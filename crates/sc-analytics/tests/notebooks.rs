//! The notebook's state and its non-persisted datasets on both backends
//! (analytics TODO A10.1–A10.2), over `plots`' ten houses.

use sc_analytics::notebook::{NotebookLanguage, NotebookState};
use sc_analytics::panel::{
    Panel, PanelBody, UsageIndex, find_inline_dataset, render_panel, save_inline_dataset,
};
use sc_analytics::plot::{Channel, DataRef, FieldDef, Layer, Mark, PlotSpec, Stat};
use sc_analytics::{Workspace, WorkspaceKind, bootstrap_workspaces, create_workspace};
use sc_dataset::{Base, Caller, DatasetDef, DatasetId, Op};
use sc_error::Result;
use serde_json::{Value as Json, json};

use crate::plots::{Fixture, both, draw, refused};

fn scatter(data: DataRef) -> PlotSpec {
    PlotSpec::single(
        data,
        Layer::new(Mark::Point, Stat::Identity)
            .with(Channel::X, FieldDef::of("area"))
            .with(Channel::Y, FieldDef::of("price")),
    )
}

/// `price > 200` over `base`: six of the ten houses.
fn dear(name: &str, base: Base) -> DatasetDef {
    DatasetDef::new(name, base).then(Op::filter("price > 200"))
}

#[tokio::test]
async fn an_inline_dataset_renders_a_plot_on_both_drivers() -> Result<()> {
    for fx in both().await? {
        // Over a table, and over a stored dataset, as a named one would be.
        let on_table = dear("d", Base::table("houses"));
        let data = draw(&fx, &scatter(DataRef::inline(on_table.clone()))).await;
        assert_eq!(data.layers[0].rows.len(), 6, "on {}", fx.backend);
        let on_dataset = dear("d", Base::dataset(fx.houses));
        let data = draw(&fx, &scatter(DataRef::inline(on_dataset))).await;
        assert_eq!(data.layers[0].rows.len(), 6, "on {}", fx.backend);

        // Its own sentence when it does not read, naming it.
        let broken = DatasetDef::new("typo", Base::table("houses")).then(Op::filter("pirce > 1"));
        let error = refused(&fx, &scatter(DataRef::inline(broken))).await;
        assert!(error.contains("pirce"), "on {}: {error}", fx.backend);

        // As a panel's data, as a summary table's and a test's are too.
        let panel = Panel::new(PanelBody::Plot {
            spec: scatter(DataRef::inline(on_table)),
        });
        let back = Panel::from_json(&serde_json::to_value(&panel).unwrap())?;
        assert_eq!(back, panel);
        let drawn = render_panel(&fx.cat, &Caller::system("test"), &panel).await?;
        assert!(drawn.error.is_none(), "on {}: {drawn:?}", fx.backend);
        let json = serde_json::to_value(&drawn).unwrap();
        assert_eq!(
            json["plot"]["layers"][0]["total"],
            json!(6),
            "on {}",
            fx.backend
        );
    }
    Ok(())
}

/// A notebook whose one code cell made `panels`.
fn notebook_with(panels: &[&Panel]) -> Json {
    let mut nb = NotebookState::new(NotebookLanguage::Javascript).to_json();
    nb["cells"] = json!([
        { "id": "intro", "kind": "text", "source": "# Dear houses" },
        { "id": "plot", "kind": "code", "source": "nb.plot(…)",
          "outputs": panels.iter().map(|p| json!({ "kind": "panel", "panel": p })).collect::<Vec<_>>(),
          "run": { "count": 1, "at": "2026-11-20T10:00:00Z" } },
    ]);
    nb
}

async fn notebook_usage(fx: &Fixture) -> Result<()> {
    bootstrap_workspaces(&fx.cat).await?;
    let on_dataset = dear("d", Base::dataset(fx.houses));
    let on_table = dear("e", Base::table("houses"));
    let a = Panel::new(PanelBody::Plot {
        spec: scatter(DataRef::inline(on_dataset.clone())),
    });
    let b = Panel::new(PanelBody::Plot {
        spec: scatter(DataRef::inline(on_table)),
    });
    let mut ws = Workspace::new("Dear houses", WorkspaceKind::Notebook, None);
    ws.state = notebook_with(&[&a, &b]);
    create_workspace(&fx.cat, &ws).await?;
    // Checked as the API checks it, and saved.
    sc_analytics::check_new_state(WorkspaceKind::Notebook, &ws.state)?;
    sc_analytics::save_workspace_state(&fx.cat, ws.id, ws.state.clone()).await?;

    // The base's delete warning lists the notebook, and so does the table's.
    let index = UsageIndex::build(&fx.cat).await?;
    let uses = index.dataset(fx.houses);
    assert_eq!(uses.len(), 1, "on {}", fx.backend);
    assert_eq!(
        (uses[0].id, uses[0].kind, uses[0].panels),
        (ws.id, WorkspaceKind::Notebook, 1)
    );
    assert_eq!(index.table("houses").len(), 1, "on {}", fx.backend);
    // A non-persisted dataset is not a stored one.
    assert!(index.dataset(on_dataset.id).is_empty());

    // Its language stays what it was created with.
    let mut python = ws.state.clone();
    python["language"] = json!("python");
    let err = sc_analytics::save_workspace_state(&fx.cat, ws.id, python)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("this one is `javascript`"),
        "{err}"
    );
    // A panel in an output that does not read is refused by position.
    let mut bad = ws.state.clone();
    bad["cells"][1]["outputs"][0]["panel"]["kind"] = json!("pie");
    let err = sc_analytics::save_workspace_state(&fx.cat, ws.id, bad)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("panel 1 of the workspace"),
        "{err}"
    );

    // Save as dataset: stored under its id, and the notebook points at it.
    let taken = save_inline_dataset(&fx.cat, ws.id, on_dataset.id, "Houses").await;
    assert!(
        taken.unwrap_err().to_string().contains("already exists"),
        "on {}",
        fx.backend
    );
    let (saved, ws) = save_inline_dataset(&fx.cat, ws.id, on_dataset.id, " Dear ").await?;
    assert_eq!((saved.id, saved.name.as_str()), (on_dataset.id, "Dear"));
    let stored = sc_dataset::load_dataset(&fx.cat, on_dataset.id)
        .await?
        .expect("stored");
    assert_eq!(stored.operations, on_dataset.operations);
    assert_eq!(
        ws.state["cells"][1]["outputs"][0]["panel"]["content"]["spec"]["data"],
        json!({ "kind": "dataset", "dataset": on_dataset.id })
    );
    assert!(find_inline_dataset(&ws, on_dataset.id).is_none());
    let err = save_inline_dataset(&fx.cat, ws.id, on_dataset.id, "Again")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no dataset"), "{err}");
    let index = UsageIndex::build(&fx.cat).await?;
    assert_eq!(index.dataset(on_dataset.id).len(), 1);
    // The plot now reads the stored dataset, which reads as it did.
    let panel = Panel::from_json(&ws.state["cells"][1]["outputs"][0]["panel"])?;
    let drawn = render_panel(&fx.cat, &Caller::system("test"), &panel).await?;
    let json = serde_json::to_value(&drawn).unwrap();
    assert_eq!(
        json["plot"]["layers"][0]["total"],
        json!(6),
        "on {}",
        fx.backend
    );

    // A base that is gone is said so, not failed on.
    let gone = Panel::new(PanelBody::Plot {
        spec: scatter(DataRef::inline(dear("g", Base::dataset(DatasetId::new())))),
    });
    let drawn = render_panel(&fx.cat, &Caller::system("test"), &gone).await?;
    assert!(
        drawn
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("built on has been deleted"),
        "{drawn:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_notebook_s_datasets_are_in_the_usage_index_and_saved_by_name() -> Result<()> {
    for fx in both().await? {
        notebook_usage(&fx).await?;
    }
    Ok(())
}

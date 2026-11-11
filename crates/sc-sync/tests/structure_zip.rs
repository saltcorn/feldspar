//! The zip transport (S6.1, §7.4): a structure zip deployed onto an instance
//! with no link is applied additively — created and updated, never deleted —
//! and onto a linked one in the branch tip's place, with the base rules.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::json;

use sc_api::schema_edit::Operation;
use sc_error::Result;
use sc_sync::{
    Class, CommitRequest, Confirm, OpClass, Outcome, Source, SourceControl, Step, StructureTree,
    SyncLink, list_deployments, read_zip, zip_bytes,
};

use crate::fixture::{Installation, code_app, git, text_field};

fn field_names(tree: &StructureTree, table: &str) -> BTreeSet<String> {
    tree.json(&format!("tables/{table}.json")).unwrap()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_owned())
        .collect()
}

/// `tree` written as a structure zip in `dir` and read back, as `feldspar
/// sync deploy --from` reads it.
fn through_a_zip(dir: &Path, tree: &StructureTree) -> StructureTree {
    let path = dir.join("structure.zip");
    std::fs::write(&path, zip_bytes(tree).unwrap()).unwrap();
    read_zip(&path).unwrap()
}

/// A: `tickets` (title, status, priority), `notes`, and the application
/// `helpdesk` with a list view.
async fn instance_a() -> Result<Installation> {
    let a = Installation::new().await?;
    a.table("tickets", &["title", "status", "priority"]).await?;
    a.table("notes", &["body"]).await?;
    a.local_store("code", |d| d).await?;
    let mut body = code_app("helpdesk", "code", "web", "web/dist");
    body["tables"] = json!(["tickets"]);
    let app = a.application(body).await?;
    let view = sc_viewpattern::View::new(app.id, "list", "List", "tickets");
    sc_viewpattern::save_view(&a.catalog, &view).await?;
    Ok(a)
}

#[tokio::test]
async fn a_structure_zip_on_an_unlinked_instance_creates_and_updates_but_deletes_nothing()
-> Result<()> {
    let a = instance_a().await?;
    let zip = through_a_zip(&a.dir, &a.tree().await?);

    // B built `tickets` itself, with a field A has not got and without one A
    // has, and has a table of its own with rows in it.
    let b = Installation::new().await?;
    b.table("tickets", &["title", "status", "legacy"]).await?;
    b.table("archive", &["body"]).await?;
    b.sql("INSERT INTO archive (body) VALUES ('kept')").await?;
    let sb = SourceControl::new(b.deployer());
    assert!(sb.link().await?.is_none());

    let planned = sb.plan_zip(&zip, None, Source::Cli).await?;
    assert!(!planned.linked);
    assert!(
        planned.plan.blockers.is_empty(),
        "{:#?}",
        planned.plan.blockers
    );
    let deletes: Vec<&str> = planned
        .plan
        .operations
        .iter()
        .filter(|op| {
            op.step == Step::Deletes
                || matches!(op.class, OpClass::MetadataDelete | OpClass::DataDestructive)
        })
        .map(|op| op.id.as_str())
        .collect();
    assert!(deletes.is_empty(), "{deletes:?}");
    let ids: BTreeSet<&str> = planned
        .plan
        .operations
        .iter()
        .map(|op| op.id.as_str())
        .collect();
    assert!(ids.contains("create_table:table:notes"), "{ids:?}");
    assert!(ids.contains("add_field:table:tickets#priority"), "{ids:?}");
    assert!(ids.contains("create:application:helpdesk"), "{ids:?}");

    let confirm = Confirm::defaults(&planned.plan);
    let receipt = sb
        .deploy_zip(&zip, &confirm, Some("operator".into()), Source::Cli)
        .await?;
    assert_eq!(receipt.result, Outcome::Converged, "{receipt:#?}");
    assert!(receipt.applied.failed.is_none(), "{receipt:#?}");

    // Created and updated…
    let there = b.tree().await?;
    assert!(there.json("tables/notes.json").is_some());
    assert!(
        there
            .json("applications/helpdesk/views/list.json")
            .is_some(),
        "{:?}",
        there.paths().collect::<Vec<_>>()
    );
    // …and nothing deleted: B's own field, its own table and that table's rows.
    assert_eq!(
        field_names(&there, "tickets"),
        ["id", "legacy", "priority", "status", "title"]
            .map(String::from)
            .into()
    );
    assert!(there.json("tables/archive.json").is_some());
    assert_eq!(b.text("SELECT body FROM archive").await, "kept");

    // Deploying the same zip again does nothing.
    let again = sb.plan_zip(&zip, None, Source::Cli).await?;
    assert!(again.plan.is_empty(), "{:#?}", again.plan.operations);

    // The ledger has the deploy, of no commit and no branch.
    let ledger = list_deployments(&b.catalog, 10).await?;
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].id, receipt.id);
    assert!(ledger[0].commit.is_none() && ledger[0].branch.is_none());
    Ok(())
}

#[tokio::test]
async fn a_structure_zip_on_a_linked_instance_takes_the_tips_place() -> Result<()> {
    // B is linked and in sync with a branch holding `tickets` and `notes`.
    let b = Installation::new().await?;
    b.table("tickets", &["title"]).await?;
    b.table("notes", &["body"]).await?;
    let path = b.dir.join("structure.git");
    std::fs::create_dir_all(&path).unwrap();
    git(&path, &["init", "-q", "--bare", "--initial-branch=main"]);
    let sb = SourceControl::new(b.deployer());
    sb.set_link(SyncLink::new(format!("file://{}", path.display()), "main"))
        .await?;
    sb.commit(&CommitRequest {
        message: "B".into(),
        ..CommitRequest::default()
    })
    .await?;

    // The zip, taken from B and then edited by hand: `notes` gone, a field
    // added to `tickets`.
    let mut tree = b.tree().await?;
    tree.remove("tables/notes.json").unwrap();
    let mut tickets = tree.json("tables/tickets.json").unwrap().clone();
    let mut extra = tickets["fields"][1].clone();
    extra["id"] = json!(uuid::Uuid::new_v4().to_string());
    extra["name"] = json!("priority");
    extra["label"] = json!("priority");
    tickets["fields"].as_array_mut().unwrap().push(extra);
    tree.insert_json("tables/tickets.json", tickets);
    let zip = through_a_zip(&b.dir, &tree);

    // A local change blocks it, as it blocks a deploy of the tip (D5).
    b.schema(vec![Operation::AddField {
        table: "notes".into(),
        field: text_field("draft"),
    }])
    .await?;
    let blocked = sb.plan_zip(&zip, None, Source::Cli).await?;
    assert!(blocked.linked);
    assert!(
        blocked
            .plan
            .blockers
            .iter()
            .any(|b| b.message.contains("uncommitted")),
        "{:#?}",
        blocked.plan.blockers
    );
    b.schema(vec![Operation::DropField {
        table: "notes".into(),
        field: "draft".into(),
    }])
    .await?;

    // With the base rules, what the zip lacks is deleted: an unticked drop.
    let planned = sb.plan_zip(&zip, None, Source::Cli).await?;
    assert!(
        planned.plan.blockers.is_empty(),
        "{:#?}",
        planned.plan.blockers
    );
    let drop = planned.plan.op("drop_table:table:notes").expect("the drop");
    assert_eq!(drop.class, OpClass::DataDestructive);
    assert!(!drop.ticked);
    let receipt = sb
        .deploy_zip(&zip, &Confirm::defaults(&planned.plan), None, Source::Cli)
        .await?;
    assert!(receipt.applied.failed.is_none(), "{receipt:#?}");
    assert!(b.catalog.get("notes")?.is_some());

    // The next base is stored, as after a deploy of the tip: the zip, except
    // that the deferred drop keeps `notes` — which the branch tip has too, so
    // it is in sync. `tickets` has the zip's field, which the tip lacks: an
    // incoming change, and not a local one.
    let status = sb.status().await?;
    let class = |key: &str| status.status.entry(key).map(|e| e.class);
    assert_eq!(
        class("table:notes"),
        Some(Class::InSync),
        "{:#?}",
        status.status
    );
    assert_eq!(
        class("table:tickets"),
        Some(Class::Incoming),
        "{:#?}",
        status.status
    );
    assert!(
        status.status.deploy_blockers().is_empty(),
        "{:#?}",
        status.status
    );
    assert!(status.base_commit.is_none());
    Ok(())
}

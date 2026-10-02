//! Plots over a fit's **output data** (analytics TODO A3.1): a frame stored
//! beside the instance, read from it rather than through SQL, its stats
//! computed in memory — whichever database the instance is stored in.

use std::collections::BTreeMap;

use sc_analytics::plot::{
    Bin, Channel, DataRef, FieldDef, Layer, Mark, PlotSpec, Rendered, Stat, render_plot,
};
use sc_error::Result;
use sc_model::{
    Column, Frame, ModelId, ModelInstance, OutputData, bootstrap_model_instances, fitted,
    save_fitted_instance_with_outputs,
};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::plots::both;

/// Ten scored rows: a fitted value, a residual, and the split.
fn rows() -> OutputData {
    let residual = [-2.0, -1.5, -1.0, -0.5, 0.0, 0.2, 0.4, 1.1, 1.6, 2.4];
    let frame = Frame::new(
        vec![
            (
                "fitted".to_owned(),
                Column::Float((1..=10).map(|i| Some(f64::from(i) * 10.0)).collect()),
            ),
            (
                "residual".to_owned(),
                Column::Float(residual.iter().copied().map(Some).collect()),
            ),
            (
                "split".to_owned(),
                Column::Str(
                    (0..10)
                        .map(|i| Some(if i < 8 { "train" } else { "test" }.to_owned()))
                        .collect(),
                ),
            ),
        ],
        Vec::new(),
    )
    .unwrap();
    // Thinned from forty: the plot says the data stands for more rows.
    OutputData { frame, total: 40 }
}

fn spec(instance: Uuid, name: &str, layer: Layer) -> PlotSpec {
    PlotSpec::single(
        DataRef::FitOutput {
            instance,
            name: name.to_owned(),
        },
        layer,
    )
}

fn plot(rendered: Rendered) -> sc_analytics::plot::PlotData {
    match rendered {
        Rendered::Plot(data) => data,
        Rendered::Refused { error, .. } => panic!("refused: {error}"),
    }
}

fn refused(rendered: Rendered) -> String {
    match rendered {
        Rendered::Refused { error, .. } => error,
        Rendered::Plot(_) => panic!("drawn"),
    }
}

fn column(data: &sc_analytics::plot::LayerData, name: &str) -> Vec<Json> {
    let i = data
        .columns
        .iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("no column {name} in {:?}", data.columns));
    data.rows.iter().map(|r| r[i].clone()).collect()
}

#[tokio::test]
async fn a_plot_over_fit_output_data_is_computed_from_the_stored_frame() -> Result<()> {
    for fx in both().await? {
        let cat = &fx.cat;
        bootstrap_model_instances(cat).await?;
        let instance = fitted(
            ModelInstance::starting(ModelId::new()),
            json!({}),
            Vec::new(),
        );
        let outputs = BTreeMap::from([("rows".to_owned(), rows())]);
        save_fitted_instance_with_outputs(cat, &instance, &[], &outputs).await?;
        let id = instance.id.0;

        // A histogram of the residuals, bins of width 1 from −2.
        let histogram = Layer::new(Mark::Bar, Stat::Count).with(
            Channel::X,
            FieldDef {
                field: "residual".into(),
                bin: Some(Bin {
                    width: Some(1.0),
                    bins: None,
                }),
            },
        );
        let data = plot(render_plot(cat, &spec(id, "rows", histogram)).await?);
        let counts: Vec<i64> = column(&data.layers[0], "y")
            .iter()
            .map(|v| v.as_f64().unwrap() as i64)
            .collect();
        assert_eq!(counts.iter().sum::<i64>(), 10, "{}", fx.backend);
        assert_eq!(counts, [2, 2, 3, 2, 1], "{}", fx.backend);

        // The points, coloured by split: all ten, none sampled.
        let points = Layer::new(Mark::Point, Stat::Identity)
            .with(Channel::X, FieldDef::of("fitted"))
            .with(Channel::Y, FieldDef::of("residual"))
            .with(Channel::Color, FieldDef::of("split"));
        let data = plot(render_plot(cat, &spec(id, "rows", points)).await?);
        assert_eq!(data.layers[0].rows.len(), 10, "{}", fx.backend);
        assert!(!data.layers[0].sampled);
        assert_eq!(
            data.domains["color"].values,
            Some(vec![json!("test"), json!("train")]),
            "{}",
            fx.backend
        );

        // A box per split: the test rows' median is 2.0.
        let boxes = Layer::new(Mark::Box, Stat::boxplot())
            .with(Channel::X, FieldDef::of("split"))
            .with(Channel::Y, FieldDef::of("residual"));
        let data = plot(render_plot(cat, &spec(id, "rows", boxes)).await?);
        let x = column(&data.layers[0], "x");
        let median = column(&data.layers[0], "y_median");
        let test = x.iter().position(|v| v == "test").unwrap();
        assert!((median[test].as_f64().unwrap() - 2.0).abs() < 1e-9);

        // A column the frame does not have is refused as any other is.
        let wrong = Layer::new(Mark::Point, Stat::Identity)
            .with(Channel::X, FieldDef::of("area"))
            .with(Channel::Y, FieldDef::of("residual"));
        let error = refused(render_plot(cat, &spec(id, "rows", wrong)).await?);
        assert!(error.contains("`area`"), "{error}");

        // Output data the fit did not store, and a fit that is gone.
        let any = || Layer::new(Mark::Bar, Stat::Count).with(Channel::X, FieldDef::of("split"));
        let error = refused(render_plot(cat, &spec(id, "draws", any())).await?);
        assert!(error.contains("no output data `draws`"), "{error}");
        let error = refused(render_plot(cat, &spec(Uuid::new_v4(), "rows", any())).await?);
        assert!(error.contains("is gone"), "{error}");
    }
    Ok(())
}

#[test]
fn a_fit_output_reference_is_json_the_spec_carries() {
    let id = Uuid::nil();
    let data = DataRef::FitOutput {
        instance: id,
        name: "rows".to_owned(),
    };
    let json = serde_json::to_value(&data).unwrap();
    assert_eq!(
        json,
        json!({ "kind": "fit_output", "instance": id, "name": "rows" })
    );
    assert_eq!(serde_json::from_value::<DataRef>(json).unwrap(), data);
}

/// The plot fixture's houses on both backends, on catalogs a dataset source
/// can hold — and the test database, which is dropped when the caller is done.
type Catalogs = Vec<(std::sync::Arc<sc_catalog::Catalog>, &'static str)>;

async fn houses_on_both() -> Result<(Catalogs, sc_test_harness::TestDb)> {
    use sc_db::DatabaseDriver;
    use std::sync::Arc;
    let db = sc_test_harness::TestDb::new().await?;
    let pg: Arc<dyn DatabaseDriver> =
        Arc::new(sc_db_postgres::PgDriver::from_pool(db.pool().clone()));
    let lite: Arc<dyn DatabaseDriver> = Arc::new(sc_db_sqlite::SqliteDriver::open_in_memory()?);
    let mut out = Vec::new();
    for (driver, backend) in [(pg, "postgres"), (lite, "sqlite")] {
        let cat = Arc::new(sc_catalog::Catalog::init(driver).await?);
        crate::plots::fill(&cat).await?;
        sc_model::bootstrap_models(&cat).await?;
        bootstrap_model_instances(&cat).await?;
        out.push((cat, backend));
    }
    Ok((out, db))
}

/// A named dataset over `houses` keeping `columns`.
async fn houses_with(
    cat: &sc_catalog::Catalog,
    name: &str,
    columns: &[&str],
) -> Result<sc_model::Dataset> {
    let select: sc_dataset::Op = serde_json::from_value(json!({
        "kind": "select",
        "params": { "columns": columns.iter().map(|c| json!({ "column": c })).collect::<Vec<_>>() }
    }))
    .unwrap();
    let def = sc_dataset::DatasetDef::over_table(name, "houses").then(select);
    sc_dataset::save_dataset(cat, &def).await?;
    let schema = sc_dataset::Schema::of_catalog(cat)?;
    let library = sc_dataset::load_library(cat).await?;
    Ok(sc_model::Dataset::resolve(&schema, &library, def.id))
}

/// Every built-in provider's outputs (analytics TODO A3.2) render over what
/// its fit stored: each table fills and each plot draws, on both backends.
#[tokio::test]
async fn the_built_in_providers_outputs_all_render() -> Result<()> {
    use sc_analytics::model_outputs::render_outputs;
    use sc_model::{
        FitStatus, Model, builtin_registry, fit_model, save_model, save_model_instance,
    };
    let (catalogs, _db) = houses_on_both().await?;
    for (cat, backend) in catalogs {
        let registry = builtin_registry()?;
        let source = sc_model::CompiledSource::new(cat.clone());
        let prices = houses_with(&cat, "Prices", &["price", "area"]).await?;
        let sales = houses_with(&cat, "Sales", &["area", "sold"]).await?;
        let cases = [
            (
                Model::new("price by area", "linear_regression", prices.clone())
                    .config("label", "price"),
                vec![
                    "coefficients",
                    "metrics",
                    "residuals_fitted",
                    "actual_predicted",
                    "qq",
                ],
            ),
            (
                Model::new("sold by area", "logistic_regression", sales).config("label", "sold"),
                vec!["coefficients", "metrics", "confusion", "calibration"],
            ),
            (
                Model::new("two clusters", "kmeans", prices).hyperparameter("k", 2),
                vec!["cluster_centres", "metrics", "cluster_sizes", "clusters"],
            ),
        ];
        for (model, expected) in cases {
            save_model(&cat, &registry, &model, None).await?;
            let started = ModelInstance::starting(model.id);
            save_model_instance(&cat, &started).await?;
            let fitted = fit_model(&cat, &registry, &source, &model, started.id, 10_000).await?;
            assert_eq!(
                fitted.status,
                FitStatus::Fitted,
                "{backend} {}: {:?}",
                model.provider,
                fitted.error()
            );
            // Every optional plot asked for too.
            let all: std::collections::BTreeSet<String> = sc_model::instance_outputs(&fitted)?
                .into_iter()
                .map(|o| o.name)
                .collect();
            let views = render_outputs(&cat, &fitted, &all).await?;
            let names: Vec<&str> = views.iter().map(|v| v.name.as_str()).collect();
            for name in &expected {
                assert!(
                    names.contains(name),
                    "{backend} {}: {names:?}",
                    model.provider
                );
            }
            for view in &views {
                assert!(
                    view.error.is_none(),
                    "{backend} {} {}: {:?}",
                    model.provider,
                    view.name,
                    view.error
                );
                if view.kind == "plot" {
                    match &view.plot {
                        Some(Rendered::Plot(data)) => assert!(
                            data.layers.iter().any(|l| !l.rows.is_empty()),
                            "{backend} {} {}: nothing drawn",
                            model.provider,
                            view.name
                        ),
                        Some(Rendered::Refused { error, .. }) => panic!(
                            "{backend} {} {}: refused: {error}",
                            model.provider, view.name
                        ),
                        None => panic!("{backend} {} {}: not drawn", model.provider, view.name),
                    }
                } else {
                    assert!(view.table.is_some(), "{} has no table", view.name);
                }
            }
        }
    }
    Ok(())
}

/// A posterior's trace, rank and density plots draw over draws shaped as the
/// fit stores them.
#[tokio::test]
async fn a_posteriors_plots_render_over_its_draws() -> Result<()> {
    use sc_analytics::model_outputs::output_spec;
    for fx in both().await? {
        let cat = &fx.cat;
        bootstrap_model_instances(cat).await?;
        let instance = fitted(
            ModelInstance::starting(ModelId::new()),
            json!({}),
            Vec::new(),
        );
        let mut parameter = Vec::new();
        let mut chain = Vec::new();
        let mut iteration = Vec::new();
        let mut value = Vec::new();
        let mut rank = Vec::new();
        for (p, name) in ["mu", "sigma"].iter().enumerate() {
            for c in 1..=2 {
                for i in 1..=50i64 {
                    parameter.push(Some((*name).to_owned()));
                    chain.push(Some(c.to_string()));
                    iteration.push(Some(i));
                    let v = p as f64 + ((i * 7 + c * 13) % 23) as f64 / 23.0;
                    value.push(Some(v));
                    rank.push(Some(((c - 1) * 50 + i) as f64));
                }
            }
        }
        let frame = Frame::new(
            vec![
                ("parameter".to_owned(), Column::Str(parameter)),
                ("chain".to_owned(), Column::Str(chain)),
                ("iteration".to_owned(), Column::Int(iteration)),
                ("value".to_owned(), Column::Float(value)),
                ("rank".to_owned(), Column::Float(rank)),
            ],
            Vec::new(),
        )
        .unwrap();
        let outputs = BTreeMap::from([("draws".to_owned(), OutputData::whole(frame))]);
        save_fitted_instance_with_outputs(cat, &instance, &[], &outputs).await?;
        for decl in sc_model::posterior_plots() {
            let sc_model::OutputKind::Plot { data, spec } = &decl.kind else {
                panic!("a plot");
            };
            let spec = output_spec(instance.id, data, spec).unwrap();
            let data = plot(render_plot(cat, &spec).await?);
            assert!(
                !data.layers[0].rows.is_empty(),
                "{} {}",
                fx.backend,
                decl.name
            );
            if decl.name == "trace" {
                assert_eq!(data.facets["wrap"], [json!("mu"), json!("sigma")]);
                assert_eq!(data.layers[0].rows.len(), 200);
            }
        }
    }
    Ok(())
}

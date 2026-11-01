//! `plugins/sklearn` — the third bundled module — installed from the directory
//! it ships in, and its estimators fitted and predicted with through the real
//! scikit-learn (TODO "Predictive models" task 7.3, 7.5).
//!
//! This is the milestone's second half of the definition of done: the same
//! dataset, a second provider, and nothing on the screen knowing that one of the
//! two answers came from Python. What runs here is everything below that screen
//! — the manifest, the registry, the columnar frame, the state that comes back
//! and the predictions it produces.
//!
//! `#[ignore]`, because installing it downloads scikit-learn, scipy and numpy
//! from PyPI. Run it with
//! `cargo test -p sc-python --features python-host --test it bundled_sklearn -- --ignored`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sc_error::Result;
use sc_model::{
    Column, Frame, ModelProviderHost, ModelRegistry, Outcome, ParameterBlock, Prediction,
};
use sc_module::{BundledModules, LoadedModule, Module, ModuleLanguage, ModuleSource};
use sc_python::pymodule::{PyModuleHost, PyModuleModelProviders};
use sc_python::{PythonEnv, PythonEnvironment, PythonRuntime, PythonSource};
use sc_types::Attrs;
use serde_json::json;

/// Say why a test did nothing, so a skip is visible rather than looking like a
/// pass.
macro_rules! skip_without {
    ($cond:expr, $why:expr) => {
        if !$cond {
            eprintln!("skipping: {}", $why);
            return Ok(());
        }
    };
}

async fn have_toolchain() -> bool {
    let bin = PathBuf::from(sc_python::DEFAULT_PYTHON_BIN);
    sc_python::have_python(&bin).await && sc_python::have_pip(&bin).await
}

fn environment_at(dir: &Path) -> PythonEnvironment {
    PythonEnvironment::new(
        &PythonEnv {
            dir: Some(dir.to_path_buf()),
            bin: None,
        },
        None,
    )
    .expect("an explicit --python-dir needs no default")
}

fn attrs(value: serde_json::Value) -> Attrs {
    value.as_object().expect("an object").clone()
}

/// A dataset with an exactly linear relationship, so a fitted regressor has a
/// right answer to be near: `price = 10 * area`, and a `region` already one-hot
/// encoded the way the host would have encoded it.
fn training_frame() -> Frame {
    let areas: Vec<f64> = (1..=40).map(f64::from).collect();
    Frame::new(
        vec![
            (
                "area".to_owned(),
                Column::Float(areas.iter().copied().map(Some).collect()),
            ),
            (
                "price".to_owned(),
                Column::Float(areas.iter().map(|a| Some(a * 10.0)).collect()),
            ),
        ],
        Vec::new(),
    )
    .expect("a rectangular frame")
}

/// The same, with the label taken off — what a provider **predicts** from.
fn feature_frame(rows: &[f64]) -> Frame {
    Frame::new(
        vec![(
            "area".to_owned(),
            Column::Float(rows.iter().copied().map(Some).collect()),
        )],
        Vec::new(),
    )
    .expect("a rectangular frame")
}

#[tokio::test]
#[ignore = "installs the bundled module, which downloads scikit-learn from PyPI"]
async fn the_bundled_sklearn_module_supplies_providers_that_fit_and_predict() -> Result<()> {
    skip_without!(
        have_toolchain().await,
        "python3 with pip is not on the PATH"
    );
    skip_without!(
        cfg!(feature = "python-host"),
        "built without an interpreter"
    );

    // Installed the way the Install button installs it: the catalog resolves the
    // id to the directory it ships in, and pip is handed a local directory.
    let catalog = BundledModules::discover(None);
    let entry = catalog.get("sklearn").expect("the sklearn module ships");
    assert_eq!(entry.language, ModuleLanguage::Python);

    let dir = std::env::temp_dir().join(format!("sc-bundled-sklearn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let installed = environment_at(&dir)
        .install(PythonSource::Local, &entry.directory.display().to_string())
        .await?;
    assert_eq!(installed.name, entry.name);

    let python = Arc::new(PythonRuntime::new().with_env(PythonEnv {
        dir: Some(dir.clone()),
        bin: None,
    }));
    let host = Arc::new(PyModuleHost::new(python));
    let manifest = host.load(&entry.name, &json!({})).await?;
    assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);
    // Five model providers, and nothing else: this module supplies no action, no
    // function and no table.
    assert_eq!(manifest.model_providers.len(), 5);
    assert!(manifest.actions.is_empty());
    assert!(manifest.functions.is_empty());
    assert!(manifest.table_providers.is_empty());

    let loaded = vec![LoadedModule {
        module: Module::new(
            &entry.name,
            ModuleSource::Bundled,
            entry.directory.display().to_string(),
        )
        .in_language(ModuleLanguage::Python),
        manifest: Some(manifest),
        config_spec: Vec::new(),
        issues: Vec::new(),
    }];
    let providers: Arc<dyn ModelProviderHost> =
        Arc::new(PyModuleModelProviders::new(&host, &loaded));

    let mut registry = ModelRegistry::new();
    registry.register_host(Arc::clone(&providers))?;
    let mut names = registry.names();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "sklearn_dbscan",
            "sklearn_gradient_boosting",
            "sklearn_ridge",
            "sklearn_svm",
            "sklearn_tsne",
        ]
    );

    // --- a regression ------------------------------------------------------
    let frame = training_frame();
    let ridge = registry.require("sklearn_ridge")?;
    // Its outcome is a *regression* over the column the configuration names,
    // resolved against the dataset's shape exactly as a built-in's is.
    let shape = sc_model::DatasetShape::of_frame("houses", &frame);
    assert_eq!(
        ridge.outcome(&shape, &attrs(json!({ "label": "price" })))?,
        Outcome::Regression {
            label: "price".to_owned()
        }
    );

    let fitted = ridge
        .fit(
            &frame,
            &attrs(json!({ "label": "price" })),
            &attrs(json!({ "alpha": 0.1 })),
        )
        .await?;
    // The state is opaque here and readable there: a pickled estimator, carried
    // as base64 because a system table with a `bytea` column would be the only
    // one.
    assert!(fitted.state["pickle"].is_string());
    assert!(fitted.state["sklearn"].is_string());
    // The parameters came back in the variants the instance screen renders, so
    // this fit shows beside a smartcore regression with no new rendering.
    match &fitted.parameters[0] {
        ParameterBlock::Table {
            name,
            columns,
            rows,
        } => {
            assert!(name.contains("Coefficients"), "{name}");
            assert_eq!(columns, &["Feature".to_owned(), "Estimate".to_owned()]);
            assert_eq!(rows[0].cells[0], json!("area"));
        }
        other => panic!("expected a coefficient table, got {other:?}"),
    }
    assert!(matches!(
        fitted.parameters[1],
        ParameterBlock::Scalar { .. }
    ));

    // And it predicts what a line through this data predicts. The relationship
    // is exact, so the tolerance is about float arithmetic and not about fit.
    let predicted = ridge
        .predict(&fitted.state, &feature_frame(&[10.0, 20.0]))
        .await?;
    assert_eq!(predicted.len(), 2);
    for (prediction, expected) in predicted.iter().zip([100.0, 200.0]) {
        match prediction {
            Prediction::Number { value } => assert!(
                (value - expected).abs() < 1.0,
                "predicted {value}, expected about {expected}"
            ),
            other => panic!("expected a number, got {other:?}"),
        }
    }

    // --- one algorithm, two outcomes ---------------------------------------
    // Gradient boosting is a regressor or a classifier depending on the type of
    // the column its configuration names, which is the case `Outcome` exists
    // for. The frame says which: the host encodes a classification's target as
    // an integer column.
    let boosting = registry.require("sklearn_gradient_boosting")?;
    assert_eq!(
        boosting.outcome(&shape, &attrs(json!({ "label": "price" })))?,
        Outcome::Regression {
            label: "price".to_owned()
        }
    );
    let fitted = boosting
        .fit(
            &frame,
            &attrs(json!({ "label": "price" })),
            &attrs(json!({ "n_estimators": 20, "learning_rate": 0.2, "max_depth": 2 })),
        )
        .await?;
    match &fitted.parameters[0] {
        ParameterBlock::Table { name, rows, .. } => {
            assert_eq!(name, "Feature importances");
            assert_eq!(rows[0].cells[0], json!("area"));
        }
        other => panic!("expected an importance table, got {other:?}"),
    }
    let predicted = boosting
        .predict(&fitted.state, &feature_frame(&[20.0]))
        .await?;
    match &predicted[0] {
        // Boosted trees interpolate rather than extrapolate, so the assertion is
        // "in the right part of the range" and not a tolerance on a line.
        Prediction::Number { value } => assert!((100.0..=300.0).contains(value), "{value}"),
        other => panic!("expected a number, got {other:?}"),
    }

    // A classification of the same frame: the label is an integer column, which
    // is how the provider tells the two apart without being handed the outcome.
    let classes = Frame::new(
        vec![
            (
                "area".to_owned(),
                Column::Float((1..=40).map(|i| Some(f64::from(i))).collect()),
            ),
            (
                "band".to_owned(),
                Column::Int((1..=40).map(|i| Some(i64::from(i > 20))).collect()),
            ),
        ],
        Vec::new(),
    )?;
    let fitted = boosting
        .fit(
            &classes,
            &attrs(json!({ "label": "band" })),
            &attrs(json!({ "n_estimators": 20 })),
        )
        .await?;
    let predicted = boosting
        .predict(&fitted.state, &feature_frame(&[2.0, 39.0]))
        .await?;
    // Class **indices**, never names: the index is what the target encoding
    // handed the provider, and mapping it back is the host's job.
    assert!(
        matches!(predicted[0], Prediction::ClassIndex { index: 0, .. }),
        "{:?}",
        predicted[0]
    );
    assert!(
        matches!(predicted[1], Prediction::ClassIndex { index: 1, .. }),
        "{:?}",
        predicted[1]
    );

    // --- clustering, with noise as cluster 0 -------------------------------
    let dbscan = registry.require("sklearn_dbscan")?;
    let points = Frame::new(
        vec![(
            "x".to_owned(),
            Column::Float(
                [0.0, 0.1, 0.2, 0.3, 5.0, 5.1, 5.2, 5.3, 50.0]
                    .into_iter()
                    .map(Some)
                    .collect(),
            ),
        )],
        Vec::new(),
    )?;
    let fitted = dbscan
        .fit(
            &points,
            &Attrs::new(),
            &attrs(json!({ "eps": 0.5, "min_samples": 3 })),
        )
        .await?;
    assert_eq!(
        fitted.parameters[0],
        ParameterBlock::scalar("Clusters found", 2.0)
    );
    let predicted = dbscan.predict(&fitted.state, &points).await?;
    assert_eq!(predicted.len(), 9);
    // The lone point at 50 is in no cluster, which this provider says as 0.
    assert_eq!(predicted[8], Prediction::Cluster { cluster: 0 });
    assert_ne!(predicted[0], predicted[4]);

    // --- an embedding ------------------------------------------------------
    let tsne = registry.require("sklearn_tsne")?;
    let fitted = tsne
        .fit(
            &points,
            &attrs(json!({ "components": 2 })),
            &attrs(json!({ "perplexity": 3.0 })),
        )
        .await?;
    assert_eq!(
        fitted.parameters[0],
        ParameterBlock::scalar("Dimensions", 2.0)
    );
    let predicted = tsne.predict(&fitted.state, &points).await?;
    match &predicted[0] {
        Prediction::Vector { values } => assert_eq!(values.len(), 2),
        other => panic!("expected a vector, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

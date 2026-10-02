//! A fit as **a job that writes a row** (TODO §8, task 3.3), against a real
//! Postgres.
//!
//! The orchestration itself is unit-tested in `sc_model::fit`; what is only real
//! against a real store is the half §8 is actually about: the instance row
//! exists before the work does, the work writes `fitted` or `failed` onto it,
//! and everything the screen and a later prediction need — the encoding, the
//! metrics, the chosen hyperparameters, the row counts — survives the round trip
//! through JSON columns.

use std::sync::Arc;

use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_model::{
    ATTR_ROWS, Column, Dataset, DatasetSource, FitResult, FitStatus, Frame, Model, ModelInstance,
    ModelProvider, ModelRegistry, Outcome, OutcomeSpec, ParameterBlock, Prediction, ROWS_OUTPUT,
    Read, Split, bootstrap_model_instances, bootstrap_models, fit_model, instance_outputs,
    load_output_data, numeric_column_field, predict_rows, require_model_instance,
    save_model_instance,
};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use serde_json::{Value as Json, json};

/// A catalog with both model tables and a `houses` table for the dataset to
/// name.
async fn setup(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_models(&cat).await?;
    bootstrap_model_instances(&cat).await?;
    cat.create_table(
        "houses",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key(),
            DataField::plain("price", TypeRef::Basic(BasicType::Float)),
            DataField::plain("area", TypeRef::Basic(BasicType::Float)),
        ],
    )
    .await?;
    Ok(cat)
}

/// A provider that fits the mean of its label and predicts it for every row.
///
/// Deterministic on purpose: the point of these tests is the row, so the
/// arithmetic must not be able to be the reason one fails.
struct Mean;

#[async_trait::async_trait]
impl ModelProvider for Mean {
    fn name(&self) -> &str {
        "mean"
    }
    fn description(&self) -> &str {
        "predicts the training mean"
    }
    fn config_declaration(&self) -> Vec<FormField> {
        vec![numeric_column_field("label", "Label").required()]
    }
    fn hyperparameters(&self) -> Vec<FormField> {
        vec![FormField::new("bias", BasicType::Float)]
    }
    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Regression {
            label: "label".to_owned(),
        }
    }
    async fn fit(&self, frame: &Frame, config: &Attrs, hyper: &Attrs) -> Result<FitResult> {
        let label = config.get("label").and_then(Json::as_str).unwrap_or("");
        let Some(Column::Float(values)) = frame.column(label) else {
            return Err(Error::msg(format!("no label `{label}` in the fit frame")));
        };
        let mean = values.iter().flatten().sum::<f64>() / values.len() as f64;
        let bias = hyper.get("bias").and_then(Json::as_f64).unwrap_or(0.0);
        Ok(FitResult::new(json!({ "prediction": mean + bias }))
            .parameter(ParameterBlock::scalar("mean", mean)))
    }
    async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>> {
        let value = state
            .get("prediction")
            .and_then(Json::as_f64)
            .ok_or_else(|| Error::msg("no fitted mean".to_owned()))?;
        Ok(vec![Prediction::number(value); frame.rows])
    }
}

/// A provider that always fails, so the failure path has something honest to
/// report.
struct Broken;

#[async_trait::async_trait]
impl ModelProvider for Broken {
    fn name(&self) -> &str {
        "broken"
    }
    fn description(&self) -> &str {
        "never fits"
    }
    fn config_declaration(&self) -> Vec<FormField> {
        Vec::new()
    }
    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Cluster
    }
    async fn fit(&self, _f: &Frame, _c: &Attrs, _h: &Attrs) -> Result<FitResult> {
        Err(Error::msg("the optimiser did not converge".to_owned()))
    }
    async fn predict(&self, _s: &Json, _f: &Frame) -> Result<Vec<Prediction>> {
        Err(Error::msg("nothing to predict with".to_owned()))
    }
}

fn registry() -> Result<ModelRegistry> {
    let mut reg = ModelRegistry::new();
    reg.register(Arc::new(Mean))?;
    reg.register(Arc::new(Broken))?;
    Ok(reg)
}

/// The dataset seam, stubbed: `n` houses whose `price` cycles 0..7 and whose
/// `region` is one of two values.
struct Houses(usize);

#[async_trait::async_trait]
impl DatasetSource for Houses {
    async fn read(&self, _ds: &Dataset, _how: &Read<'_>) -> Result<Frame> {
        let n = self.0;
        Frame::new(
            vec![
                (
                    "price".to_owned(),
                    Column::Float((0..n).map(|i| Some((i % 7) as f64)).collect()),
                ),
                (
                    "area".to_owned(),
                    Column::Float((0..n).map(|i| Some(i as f64)).collect()),
                ),
                (
                    "region".to_owned(),
                    Column::Str(
                        (0..n)
                            .map(|i| Some(if i % 2 == 0 { "north" } else { "south" }.to_owned()))
                            .collect(),
                    ),
                ),
            ],
            (0..n).map(|i| format!("int:{i}")).collect(),
        )
    }
}

fn model(provider: &str) -> Model {
    Model::new(
        "house prices",
        provider,
        Dataset::new("houses")
            .column("price", "price")
            .column("area", "area"),
    )
    .config("label", "price")
    .split(Split::new(0.6, 0.2, 0.2, 42))
}

#[tokio::test]
async fn a_fit_finishes_the_row_it_started_and_everything_it_learned_survives_the_columns()
-> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;

    // §8: the row exists before the work does, and the id is what the caller
    // was handed.
    let started = ModelInstance::starting(model("mean").id).name("first fit");
    save_model_instance(&cat, &started).await?;
    assert_eq!(
        require_model_instance(&cat, started.id).await?.status,
        FitStatus::Fitting
    );

    let finished = fit_model(
        &cat,
        &registry()?,
        &Houses(200),
        &model("mean"),
        started.id,
        1000,
    )
    .await?;
    assert_eq!(finished.status, FitStatus::Fitted);

    // And it is the *stored* row that has to carry it, not the value in hand.
    let stored = require_model_instance(&cat, started.id).await?;
    assert_eq!(stored.name, "first fit");
    assert_eq!(stored.parameters.len(), 1);
    assert_eq!(stored.parameters[0].name(), "mean");
    assert_eq!(
        stored.outcome()?,
        Outcome::Regression {
            label: "price".to_owned()
        }
    );
    // The encoding is the load-bearing one (§6): the categorical column is
    // recorded with the categories the training rows had.
    let encoding = stored.encoding()?;
    assert_eq!(encoding.feature_names(), vec!["area", "region=south"]);
    assert_eq!(
        encoding.target.as_ref().map(|t| t.column.as_str()),
        Some("price")
    );
    // The metrics are the host's, per split, over the rows each actually got.
    let metrics = sc_model::SplitMetrics::from_json(&stored.metrics)?;
    assert!(metrics.train.is_some());
    assert!(metrics.test.is_some());
    assert_eq!(
        metrics.train.as_ref().map(sc_model::Metrics::rows),
        stored.attributes[ATTR_ROWS]["train"]
            .as_u64()
            .map(|n| n as usize)
    );
    assert_eq!(stored.attributes[ATTR_ROWS]["selected"], 200);

    // The stored instance predicts, which is the whole point of storing it.
    let one = Frame::new(
        vec![
            ("price".to_owned(), Column::Float(vec![Some(0.0)])),
            ("area".to_owned(), Column::Float(vec![Some(12.0)])),
            (
                "region".to_owned(),
                Column::Str(vec![Some("north".to_owned())]),
            ),
        ],
        Vec::new(),
    )?;
    let predictions = predict_rows(&registry()?, "mean", &stored, &one).await?;
    assert_eq!(predictions.len(), 1);
    assert!(matches!(predictions[0], Prediction::Number { .. }));

    // A region the fit never saw is refused rather than answered (§6).
    let unseen = Frame::new(
        vec![
            ("price".to_owned(), Column::Float(vec![Some(0.0)])),
            ("area".to_owned(), Column::Float(vec![Some(12.0)])),
            (
                "region".to_owned(),
                Column::Str(vec![Some("west".to_owned())]),
            ),
        ],
        Vec::new(),
    )?;
    let err = predict_rows(&registry()?, "mean", &stored, &unseen)
        .await
        .expect_err("unseen category");
    assert!(err.to_string().contains("`west`"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_fit_that_fails_leaves_the_sentence_on_the_instance_and_nothing_to_predict_with()
-> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let model = model("broken");
    let started = ModelInstance::starting(model.id);
    save_model_instance(&cat, &started).await?;

    // A failed fit is `Ok`: the job's contract is to *record* what happened.
    let finished = fit_model(&cat, &registry()?, &Houses(50), &model, started.id, 1000).await?;
    assert_eq!(finished.status, FitStatus::Failed);

    let stored = require_model_instance(&cat, started.id).await?;
    assert_eq!(stored.status, FitStatus::Failed);
    assert!(
        stored
            .error()
            .is_some_and(|why| why.contains("the optimiser did not converge")),
        "{:?}",
        stored.error()
    );
    assert!(!stored.is_usable());
    assert_eq!(stored.state, Json::Null);
    assert!(stored.parameters.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_search_records_every_point_it_tried_and_the_one_it_chose() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let model = model("mean").hyperparameter("bias", json!([0.0, 6.0]));
    let started = ModelInstance::starting(model.id);
    save_model_instance(&cat, &started).await?;

    fit_model(&cat, &registry()?, &Houses(300), &model, started.id, 1000).await?;
    let stored = require_model_instance(&cat, started.id).await?;
    assert_eq!(stored.status, FitStatus::Fitted);
    // A bias of 0 predicts the training mean, which is the best a constant
    // predictor can do; 6 is worse by construction.
    assert_eq!(stored.hyperparameters.get("bias"), Some(&Json::from(0.0)));
    let search = stored.attributes[sc_model::ATTR_SEARCH]
        .as_array()
        .expect("the search is a list");
    assert_eq!(search.len(), 2);
    assert!(search.iter().all(|p| p.get("score").is_some()));
    Ok(())
}

/// What a fit shows (analytics TODO A3.1): the outputs it declares go on the
/// instance, the frames its plots read go to `_fd_model_outputs` in the same
/// transaction, and both go when the instance does.
#[tokio::test]
async fn a_fit_stores_its_outputs_and_they_go_with_the_instance() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let started = ModelInstance::starting(model("mean").id);
    save_model_instance(&cat, &started).await?;
    let finished = fit_model(
        &cat,
        &registry()?,
        &Houses(200),
        &model("mean"),
        started.id,
        1000,
    )
    .await?;
    assert_eq!(finished.status, FitStatus::Fitted);

    let stored = require_model_instance(&cat, started.id).await?;
    let outputs = instance_outputs(&stored)?;
    let names: Vec<&str> = outputs.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "statistics",
            "metrics",
            "residuals_fitted",
            "actual_predicted",
            "qq",
            "residual_histogram"
        ]
    );

    // Every scored row of every split, with the columns the model read.
    let rows = load_output_data(&cat, started.id, ROWS_OUTPUT)
        .await?
        .expect("the rows were stored");
    let counts = &stored.attributes[ATTR_ROWS];
    let scored = ["train", "validation", "test"]
        .iter()
        .map(|p| counts[p].as_u64().unwrap_or(0))
        .sum::<u64>();
    assert_eq!(rows.frame.rows as u64, scored);
    assert!(!rows.sampled());
    assert_eq!(
        rows.frame.names(),
        [
            "area",
            "region",
            "split",
            "actual",
            "fitted",
            "residual",
            "standardised_residual",
            "theoretical_quantile"
        ]
    );
    // The mean model's residuals are price − the training mean.
    let (Some(Column::Float(actual)), Some(Column::Float(fitted)), Some(Column::Float(residual))) = (
        rows.frame.column("actual"),
        rows.frame.column("fitted"),
        rows.frame.column("residual"),
    ) else {
        panic!("float columns");
    };
    for i in 0..rows.frame.rows {
        let expected = actual[i].unwrap() - fitted[i].unwrap();
        assert!((residual[i].unwrap() - expected).abs() < 1e-12);
    }
    assert!(load_output_data(&cat, started.id, "draws").await?.is_none());

    // Deleting the instance deletes its outputs.
    sc_model::delete_model_instance(&cat, &registry()?, started.id).await?;
    assert!(
        load_output_data(&cat, started.id, ROWS_OUTPUT)
            .await?
            .is_none()
    );
    Ok(())
}

/// A fit asked to stop is stopped between its stages, whichever provider it
/// is (analytics TODO A3.3) — here before it has read anything.
#[tokio::test]
async fn a_cancelled_fit_stops_at_its_next_stage() -> Result<()> {
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let ctx = sc_model::FitContext::new(&sc_model::NoProgress, &cancel);
    let err = sc_model::run_fit_with(&registry()?, &Houses(50), &model("mean"), 1000, &ctx)
        .await
        .expect_err("cancelled");
    assert!(err.to_string().ends_with(sc_model::CANCELLED), "{err}");
    Ok(())
}

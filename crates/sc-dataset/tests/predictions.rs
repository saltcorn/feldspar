//! The Model predictions operation, and the operations after it computed in
//! memory (analytics TODO A7.3), on both backends.
//!
//! The model is a stub [`ModelHost`]: `Price per area` predicts `1000 ×
//! area` with the interval ±10 % at the level asked, `Sold` predicts `"yes"`
//! for a house over 100 m² with its probability, and a model with no fit is
//! refused by name. The formulas after a prediction run in the real formula
//! evaluator.

use std::sync::Arc;

use async_trait::async_trait;
use sc_catalog::{Catalog, ModelHost, ModelSummary, PredictRows};
use sc_dataset::{
    AggregateOp, DatasetDef, GroupKey, LimitMode, LimitOp, Op, OpStatus, OrderKey, PredictOp,
    SelectColumn, SelectOp, SortKey, SortOp, Summary, SummaryFunction, WindowFunction, WindowOp,
    column_values, compile, load_library, read_stage,
};
use sc_error::{Error, Result};
use sc_types::BasicType;
use serde_json::{Value as Json, json};

use crate::fixture::{Fixture, assert_rows, both, json_rows, names, read};

/// The stub models (see the module docs).
struct Models;

#[async_trait]
impl ModelHost for Models {
    async fn predict(
        &self,
        _model: &str,
        _fit: Option<&str>,
        _table: &str,
        _rows: PredictRows<'_>,
        _detail: bool,
    ) -> Result<Vec<Json>> {
        Err(Error::msg("not by key"))
    }

    async fn describe(&self, model: &str) -> Result<ModelSummary> {
        let (types, uncertainty) = match model {
            "Price per area" => (vec![BasicType::Float], "interval"),
            "Sold" => (vec![BasicType::Text], "probabilities"),
            "Clusters" => (vec![BasicType::Int], "none"),
            other => {
                return Err(Error::not_found(format!(
                    "there is no model called `{other}`"
                )));
            }
        };
        Ok(ModelSummary {
            name: model.to_owned(),
            table: "houses".to_owned(),
            provider: "stub".to_owned(),
            prediction_types: types,
            no_prediction: None,
            active_fit: Some("fit".to_owned()),
            not_rows_of_table: None,
            uncertainty: uncertainty.to_owned(),
        })
    }

    async fn predict_inputs(
        &self,
        model: &str,
        fit: Option<&str>,
        rows: &[Json],
        level: f64,
    ) -> Result<Vec<Json>> {
        assert_eq!(fit, None);
        rows.iter()
            .map(|row| {
                let area = row["area"]
                    .as_f64()
                    .ok_or_else(|| Error::invalid("a row has no `area`, an input"))?;
                Ok(match model {
                    "Price per area" => {
                        let value = 1000.0 * area;
                        let half = value * (level - 0.8);
                        json!({ "value": value, "lower": value - half, "upper": value + half,
                                "level": level, "sd": half })
                    }
                    "Sold" => json!({
                        "value": if area > 100.0 { "yes" } else { "no" },
                        "probability": 0.75,
                    }),
                    _ => json!({ "value": 1 }),
                })
            })
            .collect()
    }
}

/// The fixture with the stub models and a formula evaluator, both torn down
/// when the test is done.
struct Predicting {
    fx: Fixture,
}

impl Drop for Predicting {
    fn drop(&mut self) {
        self.fx.cat.teardown();
    }
}

async fn predicting() -> Result<Vec<Predicting>> {
    let mut out = Vec::new();
    for fx in both().await? {
        install(&fx.cat)?;
        out.push(Predicting { fx });
    }
    Ok(out)
}

fn install(cat: &Catalog) -> Result<()> {
    cat.set_model_host(Arc::new(Models))?;
    cat.set_formula_evaluator(Arc::new(sc_expr::DenoEvaluator::new()))?;
    Ok(())
}

/// The stored datasets and `def`, with the models it names — what the
/// editor compiles an unsaved definition against.
async fn library_with(fx: &Fixture, def: &DatasetDef) -> Result<sc_dataset::Library> {
    let mut library = load_library(&fx.cat).await?;
    library.insert(def.clone());
    sc_dataset::load_models(&fx.cat, &mut library).await;
    Ok(library)
}

fn predict(model: &str, name: &str) -> Op {
    Op::Predict(PredictOp::new(model, name))
}

fn select(columns: &[&str]) -> Op {
    Op::Select(SelectOp {
        columns: columns.iter().map(|c| SelectColumn::keep(*c)).collect(),
    })
}

/// The predictions, with their interval at the default level, beside the
/// columns they were predicted from — over the dataset's own SQL, which
/// filtered the rows first.
#[tokio::test]
async fn predictions_and_their_intervals_are_added_to_every_row() -> Result<()> {
    for p in predicting().await? {
        let fx = &p.fx;
        let def = DatasetDef::over_table("priced", "houses")
            .then(Op::filter("price > 100000"))
            .then(select(&["id", "area"]))
            .then(predict("Price per area", "predicted"));
        let page = read(fx, &def, None).await?;
        assert_eq!(
            names(&page),
            [
                "id",
                "area",
                "predicted",
                "predicted_lower",
                "predicted_upper"
            ]
        );
        assert_eq!(page.total, 4, "on {}", fx.backend);
        // ±10 % at 0.9.
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![
                    json!(1),
                    json!(100.0),
                    json!(100_000.0),
                    json!(90_000.0),
                    json!(110_000.0),
                ],
                vec![
                    json!(2),
                    json!(50.0),
                    json!(50_000.0),
                    json!(45_000.0),
                    json!(55_000.0),
                ],
                vec![
                    json!(4),
                    json!(120.0),
                    json!(120_000.0),
                    json!(108_000.0),
                    json!(132_000.0),
                ],
                vec![
                    json!(5),
                    json!(40.0),
                    json!(40_000.0),
                    json!(36_000.0),
                    json!(44_000.0),
                ],
            ],
        );
        // The rows are still the houses'.
        assert!(
            matches!(page.grain, sc_dataset::Grain::Table { .. }),
            "{:?}",
            page.grain
        );

        // Another level, and no interval at all.
        let mut op = PredictOp::new("Price per area", "p");
        op.level = Some(0.95);
        let def = DatasetDef::over_table("d", "houses")
            .then(select(&["id", "area"]))
            .then(Op::Predict(op.clone()));
        let rows = json_rows(&read(fx, &def, None).await?);
        assert_rows(
            fx.backend,
            &rows[..1],
            &[vec![
                json!(1),
                json!(100.0),
                json!(100_000.0),
                json!(85_000.0),
                json!(115_000.0),
            ]],
        );
        op.interval = false;
        let def = DatasetDef::over_table("d", "houses")
            .then(select(&["id", "area"]))
            .then(Op::Predict(op));
        assert_eq!(names(&read(fx, &def, None).await?), ["id", "area", "p"]);

        // A classifier's class, with its probability.
        let def = DatasetDef::over_table("d", "houses")
            .then(select(&["id", "area"]))
            .then(predict("Sold", "sold_p"));
        let page = read(fx, &def, None).await?;
        assert_eq!(names(&page), ["id", "area", "sold_p", "sold_p_probability"]);
        assert_rows(
            fx.backend,
            &json_rows(&page)[3..4],
            &[vec![json!(4), json!(120.0), json!("yes"), json!(0.75)]],
        );
    }
    Ok(())
}

/// After the predictions the operations run in memory: a Calculated column
/// and a Filter by the formula evaluator, then Select, Sort, Aggregate and
/// Limit — each against rows worked out by hand.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn the_operations_after_a_prediction_run_in_memory() -> Result<()> {
    for p in predicting().await? {
        let fx = &p.fx;
        let base = DatasetDef::over_table("d", "houses")
            .then(select(&["id", "price", "area", "neighbourhood"]))
            .then(predict("Price per area", "predicted"))
            .then(Op::calculated("error", "price - predicted"))
            .then(Op::filter("error > 0"));
        // Houses 1 (200 000 − 100 000), 2 (150 000 − 50 000), 3 (90 000 −
        // 60 000), 4 (300 000 − 120 000) and 5 (120 000 − 40 000): all over.
        let page = read(fx, &base, None).await?;
        assert_eq!(page.total, 5, "on {}", fx.backend);
        assert_eq!(page.columns[7].name, "error");
        assert_eq!(page.columns[7].ty, sc_dataset::ColType::Float);

        let narrower = base.clone().then(Op::filter("error > 90000"));
        let rows = json_rows(&read(fx, &narrower, None).await?);
        let ids: Vec<&Json> = rows.iter().map(|r| &r[0]).collect();
        assert_eq!(ids, [&json!(1), &json!(2), &json!(4)], "on {}", fx.backend);

        // Sort by the error, largest first; then Select and rename.
        let sorted = base
            .clone()
            .then(Op::Sort(SortOp {
                keys: vec![SortKey {
                    formula: "error".into(),
                    descending: true,
                }],
            }))
            .then(Op::Select(SelectOp {
                columns: vec![
                    SelectColumn::keep("id"),
                    SelectColumn::renamed("error", "over"),
                ],
            }));
        let page = read(fx, &sorted, None).await?;
        assert_eq!(names(&page), ["id", "over"]);
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![json!(4), json!(180_000.0)],
                vec![json!(1), json!(100_000.0)],
                vec![json!(2), json!(100_000.0)],
                vec![json!(5), json!(80_000.0)],
                vec![json!(3), json!(30_000.0)],
            ],
        );

        // Aggregate by neighbourhood: the mean error and a count. North has
        // houses 1, 2 and 5; South 3 and 4.
        let grouped = base.clone().then(Op::Aggregate(AggregateOp {
            group_by: vec![GroupKey::column("neighbourhood")],
            summaries: vec![
                Summary::of("mean_error", SummaryFunction::Mean, "error"),
                Summary::count("n"),
                Summary::of("most", SummaryFunction::Max, "predicted"),
            ],
        }));
        let page = read(fx, &grouped, None).await?;
        assert_eq!(
            page.grain,
            sc_dataset::Grain::Group {
                keys: vec!["neighbourhood".into()]
            }
        );
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![json!(1), json!(280_000.0 / 3.0), json!(3), json!(100_000.0)],
                vec![json!(2), json!(105_000.0), json!(2), json!(120_000.0)],
            ],
        );
        // The neighbourhood is still a key to `neighbourhoods`.
        assert!(page.columns[0].key.is_some());

        // The top house of each neighbourhood by error, and the first two.
        let top = base.clone().then(Op::Limit(LimitOp {
            mode: LimitMode::Top,
            n: 1,
            seed: 0,
            group_by: vec!["neighbourhood".into()],
            order: vec![OrderKey::desc("error")],
        }));
        let rows = json_rows(&read(fx, &top, None).await?);
        let ids: Vec<&Json> = rows.iter().map(|r| &r[0]).collect();
        assert_eq!(ids, [&json!(1), &json!(4)], "on {}", fx.backend);
        let first = base.clone().then(Op::Limit(LimitOp {
            mode: LimitMode::First,
            n: 2,
            seed: 0,
            group_by: Vec::new(),
            order: Vec::new(),
        }));
        assert_eq!(read(fx, &first, None).await?.total, 2);
        // A sample is the same sample every time.
        let sample = base.clone().then(Op::Limit(LimitOp {
            mode: LimitMode::Sample,
            n: 3,
            seed: 7,
            group_by: Vec::new(),
            order: Vec::new(),
        }));
        let once = json_rows(&read(fx, &sample, None).await?);
        assert_eq!(once.len(), 3);
        assert_eq!(once, json_rows(&read(fx, &sample, None).await?));

        // Paging and distinct values read the same rows.
        let page = read_stage(
            &fx.cat,
            &sc_dataset::Caller::system("test"),
            &sorted,
            None,
            sc_dataset::Page {
                offset: 1,
                limit: 2,
            },
        )
        .await?;
        assert_eq!(page.total, 5);
        assert_rows(
            fx.backend,
            &json_rows(&page),
            &[
                vec![json!(1), json!(100_000.0)],
                vec![json!(2), json!(100_000.0)],
            ],
        );
        let schema = sc_dataset::Schema::of_catalog(&fx.cat)?;
        let library = library_with(fx, &grouped).await?;
        let compiled = compile(&schema, &library, &grouped, sc_dataset::Options::default());
        let values = column_values(
            &fx.cat,
            &sc_dataset::Caller::system("test"),
            compiled.last()?,
            "n",
            10,
        )
        .await?;
        assert_eq!(values.len(), 2);
    }
    Ok(())
}

/// What cannot be computed after a prediction, and a model that cannot
/// predict, are refused on the operation with a sentence; the stages before
/// still read, and a SQL read of a stage after is refused.
#[tokio::test]
async fn what_cannot_follow_a_prediction_is_refused_by_name() -> Result<()> {
    for p in predicting().await? {
        let fx = &p.fx;
        let schema = sc_dataset::Schema::of_catalog(&fx.cat)?;
        let mut def = DatasetDef::over_table("d", "houses")
            .then(predict("Price per area", "predicted"))
            .then(Op::Window(WindowOp {
                name: "previous".into(),
                function: WindowFunction::Lag,
                column: Some("predicted".into()),
                offset: None,
                partition: Vec::new(),
                order: Vec::new(),
            }));
        let library = library_with(fx, &def).await?;
        let compiled = compile(&schema, &library, &def, sc_dataset::Options::default());
        assert_eq!(compiled.operations[0].status, OpStatus::Ok);
        let report = &compiled.operations[1];
        assert_eq!(report.status, OpStatus::Invalid);
        let error = report.error.as_deref().unwrap_or_default();
        assert!(
            error.contains("a Window column cannot follow a Model predictions operation"),
            "{error}"
        );
        // The stage after the predictions is in memory, so not a query.
        let after = compiled.stage(1)?;
        assert!(after.tail().is_some());
        let refused = after.unordered_query().unwrap_err().to_string();
        assert!(refused.contains("computed in memory"), "{refused}");

        // A formula after a prediction cannot follow a key.
        def.operations.pop();
        let def = def.then(Op::calculated("hood", "neighbourhoodⱵname"));
        let compiled = compile(&schema, &library, &def, sc_dataset::Options::default());
        let error = compiled.operations[1].error.clone().unwrap_or_default();
        assert!(error.contains("follows a key"), "{error}");

        // A model that is not there.
        let def = DatasetDef::over_table("d", "houses").then(predict("Nobody", "p"));
        let library = {
            let mut l = sc_dataset::Library::new([def.clone()]);
            sc_dataset::load_models(&fx.cat, &mut l).await;
            l
        };
        let compiled = compile(&schema, &library, &def, sc_dataset::Options::default());
        let error = compiled.operations[0].error.clone().unwrap_or_default();
        assert!(error.contains("no model called `Nobody`"), "{error}");

        // An input the model needs and the rows do not have.
        let def = DatasetDef::over_table("d", "houses")
            .then(select(&["id", "price"]))
            .then(predict("Price per area", "p"));
        let err = read(fx, &def, None).await.unwrap_err().to_string();
        assert!(err.contains("no `area`"), "{err}");
    }
    Ok(())
}

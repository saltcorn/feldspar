//! Applying a fitted instance to rows (TODO §6, §10, task 3.4).
//!
//! Four steps, and the third is the one the milestone is careful about:
//!
//! 1. Check the instance can answer at all — it is `fitted`, and its outcome
//!    produces something per row.
//! 2. Encode the frame **with the instance's own encoding**, strictly: a null
//!    feature or a category the fit never saw is an error naming the column and
//!    the value, never a row of zeros (§6).
//! 3. Ask the provider, which answers in class *indices*.
//! 4. Map those back through the target encoding into class **names**, because
//!    the index is an implementation detail of that encoding and nobody's row
//!    wants to hold a `2`.
//!
//! Step 4 is why [`Prediction::ClassIndex`] and [`Prediction::Class`] are
//! different variants rather than one with a flag: a provider answers the first
//! and only this module produces the second, so "is this a name or an index" is
//! never a question about where you are in the call stack.
//!
//! **A prediction takes a frame, not a row.** A single row is a frame of one.
//! Batching is what makes a provider in another language usable at all — the
//! call is the cost, not the arithmetic — and it is what lets the fit's metric
//! pass score 50 000 rows in one call rather than in 50 000.

use sc_error::{Error, Result};
use sc_query::Expr;
use serde_json::Value as Json;

use crate::encode::{Encoding, apply_encoding};
use crate::fit::ATTR_OUTCOME;
use crate::frame::Frame;
use crate::instance::ModelInstance;
use crate::model::Model;
use crate::provider::{Outcome, Prediction};
use crate::registry::ModelRegistry;
use crate::source::{DatasetSource, Read};

impl ModelInstance {
    /// The [`Outcome`] this fit produces, as it was recorded at fit time.
    ///
    /// Read off the instance rather than recomputed, because recomputing means
    /// reading the dataset again — and would answer with *today's* column types
    /// rather than the ones this instance was fitted over.
    pub fn outcome(&self) -> Result<Outcome> {
        let json = self.attributes.get(ATTR_OUTCOME).ok_or_else(|| {
            Error::invalid(format!(
                "instance {} does not record what it produces, so it cannot be applied",
                self.id
            ))
        })?;
        serde_json::from_value(json.clone()).map_err(|e| {
            Error::invalid(format!(
                "instance {}: its recorded outcome cannot be read: {e}",
                self.id
            ))
        })
    }

    /// The encoding this fit was made with.
    pub fn encoding(&self) -> Result<Encoding> {
        if self.encoding.is_null() {
            return Err(Error::invalid(format!(
                "instance {} has no stored encoding, so it cannot be applied to a row",
                self.id
            )));
        }
        Encoding::from_json(&self.encoding)
    }
}

/// Apply `instance` to `frame`, answering one prediction per row **in row
/// order**.
///
/// `provider` is looked up from the registry by the name the *model* carries,
/// not by anything on the instance: a fit belongs to its model, and an instance
/// whose provider has been uninstalled should say that in those words rather
/// than by not being found.
pub async fn predict_rows(
    registry: &ModelRegistry,
    provider: &str,
    instance: &ModelInstance,
    frame: &Frame,
) -> Result<Vec<Prediction>> {
    if !instance.is_usable() {
        return Err(Error::invalid(match instance.error() {
            Some(why) => format!(
                "instance {} did not finish fitting, so it cannot predict: {why}",
                instance.id
            ),
            None => format!(
                "instance {} is `{}`, so it cannot predict yet",
                instance.id, instance.status
            ),
        }));
    }
    let outcome = instance.outcome()?;
    if !outcome.predicts() {
        return Err(Error::invalid(format!(
            "instance {} is {}",
            instance.id,
            no_per_row_prediction(outcome.is_posterior())
        )));
    }
    let encoding = instance.encoding()?;
    // Strict, and **features only**: at predict time a row we cannot represent
    // is an error rather than a dropped row, because dropping would answer fewer
    // predictions than there were rows and the caller lines them up against the
    // rows it asked about — but the label is not one of the things the row has
    // to be able to represent. A prediction is asked about a row whose label is
    // unknown; demanding one would make a fitted model unusable on exactly the
    // rows it exists to answer about. See [`Encoding::features_only`].
    let encoded = apply_encoding(&encoding.features_only(), frame)?;
    let provider = registry.require(provider.trim())?;
    let raw = provider
        .predict(&instance.state, &encoded.features_frame())
        .await?;
    if raw.len() != frame.rows {
        return Err(Error::msg(format!(
            "the model provider `{}` answered {} predictions for {} rows",
            provider.name(),
            raw.len(),
            frame.rows
        )));
    }
    name_classes(raw, &encoding)
}

/// What a prediction is asked **about** (task 5.4, §12).
///
/// The two are not variations on one thing, and the split is the whole reason
/// this enum exists rather than an `Option<Expr>` and an `Option<Vec<Json>>`
/// that could both be `Some`:
///
/// - [`Rows`](Subject::Rows) is a row that may not be in the table at all — the
///   admin screen's "try a row" box, and an API caller asking a what-if. Its
///   derived columns are whatever the caller typed, because there is nothing to
///   derive them from.
/// - [`Dataset`](Subject::Dataset) is rows of the model's own table, read
///   **through the dataset**, so a join path and an aggregation are computed by
///   the row layer exactly as they were at fit time. This is what a formula's
///   `predict("…")` uses, and it is why the restriction goes into the read rather
///   than being applied to what came back.
#[derive(Debug, Clone, Copy)]
pub enum Subject<'a> {
    /// Literal rows: one JSON object per row, keyed by dataset column name.
    Rows(&'a [Json]),
    /// The dataset's rows, restricted by this extra `WHERE` — `None` for all of
    /// them, bounded by the row cap.
    Dataset(Option<&'a Expr>),
}

/// Predictions, and which rows they are for.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Predictions {
    /// One per row, in row order.
    pub predictions: Vec<Prediction>,
    /// The canonical primary key of each row, where the read supplied them —
    /// empty for literal rows, which have none.
    pub keys: Vec<String>,
}

/// Apply `instance` to whatever `subject` names, answering in row order.
///
/// The one path both callers of Phase 5 go through — the `predictRows` endpoint
/// and `predict("…")` — because "read the rows, encode them the way
/// the fit was, ask the provider, name the classes" must not be written twice
/// and drift.
pub async fn predict_subject(
    registry: &ModelRegistry,
    source: &dyn DatasetSource,
    model: &Model,
    instance: &ModelInstance,
    subject: Subject<'_>,
    cap: u64,
) -> Result<Predictions> {
    let frame = match subject {
        Subject::Rows(rows) => {
            let types = instance.encoding()?.feature_types();
            Frame::from_rows(rows, &types)?
        }
        Subject::Dataset(restrict) => {
            // **Unfiltered**: the dataset's own filter chose the rows the fit
            // was computed from, and a prediction is about rows the caller
            // chose. Keeping it would make a model fitted on `sold` houses
            // unable to answer about the unsold one a trigger just inserted,
            // which is the only question anybody asks it. The derived columns
            // still come from the dataset, through the row layer, so a join path
            // and an aggregation are computed exactly as they were at fit time.
            let mut how = Read::all(cap).unfiltered();
            if let Some(expr) = restrict {
                how = how.restricted_to(expr);
            }
            source.read(&model.dataset, &how).await?
        }
    };
    let predictions = predict_rows(registry, &model.provider, instance, &frame).await?;
    Ok(Predictions {
        predictions,
        keys: frame.keys,
    })
}

/// Turn every class index into the class name the fit's encoding gave it.
///
/// A provider that answered a name directly is left alone — that is what a
/// module's provider may well do — and every other variant passes through.
pub fn name_classes(predictions: Vec<Prediction>, encoding: &Encoding) -> Result<Vec<Prediction>> {
    predictions
        .into_iter()
        .map(|prediction| match prediction {
            Prediction::ClassIndex { index, probability } => {
                let target = encoding.target.as_ref().ok_or_else(|| {
                    Error::msg(
                        "the provider answered a class, but this instance was not fitted with a \
                         label to map it through"
                            .to_owned(),
                    )
                })?;
                Ok(Prediction::class(target.class(index)?, probability))
            }
            other => Ok(other),
        })
        .collect()
}

/// The value each prediction writes into a row (§12) — what `predict()` hands
/// the row layer.
pub fn prediction_values(predictions: &[Prediction]) -> Result<Vec<Json>> {
    predictions.iter().map(Prediction::to_json).collect()
}

/// What a fit that answers nothing per row is, and what to do instead: the
/// end of every refusal to predict with one, after "… is".
///
/// A posterior is refused in its own words rather than a hypothesis test's:
/// prediction for new rows from a posterior (standalone generated quantities,
/// Stan TODO §19) is carried past the Stan milestone, and the draws in a code
/// body are the way to it meanwhile.
pub fn no_per_row_prediction(posterior: bool) -> &'static str {
    if posterior {
        "a posterior: its draws are the answer, and a posterior does not predict rows here — \
         read its draws in a code body (`m.draws(…)`, on `models.get(…)`) and compute the \
         prediction there"
    } else {
        "a hypothesis test: its parameters are the answer, and there is no per-row prediction \
         to make"
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use std::sync::Arc;

    use super::*;
    use crate::encode::{TargetEncoding, fit_encoding};
    use crate::frame::Column;
    use crate::instance::ModelInstance;
    use crate::model::ModelId;
    use crate::provider::{FitResult, ModelProvider, OutcomeSpec};
    use crate::source::Read;
    use sc_types::{Attrs, FormField};

    /// A provider that answers the class index its state names, for every row.
    struct FixedClass;

    #[async_trait]
    impl ModelProvider for FixedClass {
        fn name(&self) -> &str {
            "fixed_class"
        }

        fn description(&self) -> &str {
            "answers one class index"
        }

        fn config_declaration(&self) -> Vec<FormField> {
            vec![crate::provider::column_field("label", "Label")]
        }

        fn outcome_spec(&self) -> OutcomeSpec {
            OutcomeSpec::Classification {
                label: "label".to_owned(),
            }
        }

        async fn fit(&self, _f: &Frame, _c: &Attrs, _h: &Attrs) -> Result<FitResult> {
            Ok(FitResult::new(serde_json::json!({ "class": 1 })))
        }

        async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>> {
            let index = state.get("class").and_then(Json::as_u64).unwrap_or(0) as usize;
            Ok(vec![Prediction::class_index(index, Some(0.75)); frame.rows])
        }
    }

    fn registry() -> ModelRegistry {
        let mut registry = ModelRegistry::new();
        registry.register(Arc::new(FixedClass)).expect("register");
        registry
    }

    fn training() -> Frame {
        Frame::new(
            vec![
                (
                    "region".to_owned(),
                    Column::Str(vec![Some("north".into()), Some("south".into())]),
                ),
                ("area".to_owned(), Column::Float(vec![Some(1.0), Some(2.0)])),
                (
                    "sold".to_owned(),
                    Column::Str(vec![Some("no".into()), Some("yes".into())]),
                ),
            ],
            vec!["int:1".to_owned(), "int:2".to_owned()],
        )
        .expect("frame")
    }

    /// A fitted instance over [`training`], answering class 1.
    fn instance() -> ModelInstance {
        let outcome = Outcome::Classification {
            label: "sold".to_owned(),
            classes: None,
        };
        let encoding = fit_encoding(&training(), &outcome, false).expect("encoding");
        let mut instance = ModelInstance::starting(ModelId::new());
        instance =
            crate::instance_store::fitted(instance, serde_json::json!({ "class": 1 }), Vec::new());
        instance.encoding = encoding.to_json().expect("json");
        instance.attributes.insert(
            ATTR_OUTCOME.to_owned(),
            serde_json::to_value(Outcome::Classification {
                label: "sold".to_owned(),
                classes: encoding.classes().map(<[String]>::to_vec),
            })
            .expect("outcome"),
        );
        instance
    }

    #[tokio::test]
    async fn a_class_index_comes_back_as_the_name_the_fit_saw() {
        let rows = Frame::new(
            vec![
                ("region".to_owned(), Column::Str(vec![Some("south".into())])),
                ("area".to_owned(), Column::Float(vec![Some(3.0)])),
                ("sold".to_owned(), Column::Str(vec![Some("no".into())])),
            ],
            Vec::new(),
        )
        .expect("frame");
        let predictions = predict_rows(&registry(), "fixed_class", &instance(), &rows)
            .await
            .expect("predict");
        assert_eq!(predictions, vec![Prediction::class("yes", Some(0.75))]);
        // And it is a value a row can hold — an index never would have been.
        assert_eq!(
            prediction_values(&predictions).expect("values"),
            vec![Json::from("yes")]
        );
    }

    #[tokio::test]
    async fn a_row_with_no_label_at_all_is_what_a_prediction_is_asked_about() {
        // The label is the thing being predicted, so it is usually not there —
        // and demanding it would make a fitted model unusable on exactly the
        // rows it exists to answer about. See `Encoding::features_only`.
        let rows = Frame::new(
            vec![
                ("region".to_owned(), Column::Str(vec![Some("south".into())])),
                ("area".to_owned(), Column::Float(vec![Some(3.0)])),
            ],
            Vec::new(),
        )
        .expect("frame");
        let predictions = predict_rows(&registry(), "fixed_class", &instance(), &rows)
            .await
            .expect("predict");
        assert_eq!(predictions, vec![Prediction::class("yes", Some(0.75))]);

        // And a null one is fine for the same reason, where a *fit* would have
        // dropped the row.
        let rows = Frame::new(
            vec![
                ("region".to_owned(), Column::Str(vec![Some("south".into())])),
                ("area".to_owned(), Column::Float(vec![Some(3.0)])),
                ("sold".to_owned(), Column::Str(vec![None])),
            ],
            Vec::new(),
        )
        .expect("frame");
        assert_eq!(
            predict_rows(&registry(), "fixed_class", &instance(), &rows)
                .await
                .expect("predict"),
            vec![Prediction::class("yes", Some(0.75))]
        );
    }

    /// A prediction reads the dataset **without** its filter, and this is the
    /// case the whole arrangement exists for: a model of what houses sell for is
    /// fitted on `sold` ones and asked about the unsold one a trigger just
    /// inserted. Keeping the filter would answer "the dataset does not select
    /// this row" for every row anybody wants a prediction for, which is what
    /// running the milestone's definition of done by hand actually did.
    #[tokio::test]
    async fn a_prediction_reads_past_the_datasets_own_filter() {
        use std::sync::Mutex;

        /// The seam, stubbed, recording how it was asked.
        struct Recording(Mutex<Vec<bool>>);
        #[async_trait]
        impl crate::source::DatasetSource for Recording {
            async fn read(&self, _ds: &crate::Dataset, how: &Read<'_>) -> Result<Frame> {
                self.0.lock().expect("lock").push(how.filtered);
                Ok(training())
            }
        }

        let source = Recording(Mutex::new(Vec::new()));
        let model = crate::Model::new(
            "sold",
            "fixed_class",
            crate::Dataset::new("houses")
                .column("sold", "sold")
                .filtered("sold === true"),
        );

        // A fit reads the sample the model is about: the filter applies.
        source
            .materialise(&model.dataset, 1000)
            .await
            .expect("materialise");

        // A prediction reads rows the caller named: it does not.
        predict_subject(
            &registry(),
            &source,
            &model,
            &instance(),
            crate::Subject::Dataset(None),
            1000,
        )
        .await
        .expect("dataset");

        assert_eq!(
            *source.0.lock().expect("lock"),
            vec![true, false],
            "the fit reads filtered and the prediction reads unfiltered"
        );
    }

    #[tokio::test]
    async fn literal_rows_and_dataset_rows_are_the_two_subjects_and_agree() {
        /// The seam, stubbed: the training frame, whatever is asked.
        struct Fixed;
        #[async_trait]
        impl crate::source::DatasetSource for Fixed {
            async fn read(&self, _ds: &crate::Dataset, _how: &Read<'_>) -> Result<Frame> {
                Ok(training())
            }
        }
        let model = crate::Model::new(
            "sold",
            "fixed_class",
            crate::Dataset::new("houses").column("sold", "sold"),
        );
        let instance = instance();

        // Over the dataset: two rows, and their keys come back so a caller can
        // line the answers up against the rows it asked about.
        let over_dataset = predict_subject(
            &registry(),
            &Fixed,
            &model,
            &instance,
            crate::Subject::Dataset(None),
            1000,
        )
        .await
        .expect("dataset");
        assert_eq!(over_dataset.predictions.len(), 2);
        assert_eq!(over_dataset.keys, vec!["int:1", "int:2"]);

        // Over a literal row: the same answer, and no key, because a row that is
        // not in the table has none.
        let literal = [serde_json::json!({ "region": "north", "area": 1.0 })];
        let over_rows = predict_subject(
            &registry(),
            &Fixed,
            &model,
            &instance,
            crate::Subject::Rows(&literal),
            1000,
        )
        .await
        .expect("rows");
        assert_eq!(
            over_rows.predictions,
            vec![Prediction::class("yes", Some(0.75))]
        );
        assert!(over_rows.keys.is_empty());
    }

    #[tokio::test]
    async fn a_category_the_fit_never_saw_is_refused_rather_than_predicted() {
        let rows = Frame::new(
            vec![
                ("region".to_owned(), Column::Str(vec![Some("west".into())])),
                ("area".to_owned(), Column::Float(vec![Some(3.0)])),
                ("sold".to_owned(), Column::Str(vec![Some("no".into())])),
            ],
            Vec::new(),
        )
        .expect("frame");
        let err = predict_rows(&registry(), "fixed_class", &instance(), &rows)
            .await
            .expect_err("unseen category");
        assert!(err.to_string().contains("`west`"), "{err}");
    }

    #[tokio::test]
    async fn an_instance_that_did_not_finish_says_so_rather_than_predicting() {
        let unfinished = ModelInstance::starting(ModelId::new());
        let err = predict_rows(&registry(), "fixed_class", &unfinished, &training())
            .await
            .expect_err("still fitting");
        assert!(err.to_string().contains("cannot predict"), "{err}");

        let failed = ModelInstance::starting(ModelId::new()).failed("the dataset selects no rows");
        let err = predict_rows(&registry(), "fixed_class", &failed, &training())
            .await
            .expect_err("failed");
        assert!(err.to_string().contains("selects no rows"), "{err}");
    }

    #[tokio::test]
    async fn a_hypothesis_test_has_no_per_row_answer_to_give() {
        let mut instance = instance();
        instance.attributes.insert(
            ATTR_OUTCOME.to_owned(),
            serde_json::to_value(Outcome::Test).expect("outcome"),
        );
        let err = predict_rows(&registry(), "fixed_class", &instance, &training())
            .await
            .expect_err("a test");
        assert!(err.to_string().contains("hypothesis test"), "{err}");
    }

    /// Prediction from a posterior is carried past the Stan milestone, so a
    /// posterior is refused — as a posterior, pointing at the draws, not as a
    /// hypothesis test.
    #[tokio::test]
    async fn a_posterior_is_refused_as_a_posterior_and_pointed_at_its_draws() {
        let mut instance = instance();
        instance.attributes.insert(
            ATTR_OUTCOME.to_owned(),
            serde_json::to_value(Outcome::Posterior { prediction: None }).expect("outcome"),
        );
        let err = predict_rows(&registry(), "fixed_class", &instance, &training())
            .await
            .expect_err("a posterior")
            .to_string();
        assert!(err.contains("is a posterior"), "{err}");
        assert!(err.contains("`m.draws(…)`, on `models.get(…)`"), "{err}");
        assert!(!err.contains("hypothesis test"), "{err}");
    }

    #[test]
    fn a_class_index_outside_the_fitted_classes_is_caught_and_not_translated() {
        let encoding = Encoding {
            columns: Vec::new(),
            target: Some(TargetEncoding {
                column: "sold".to_owned(),
                classes: Some(vec!["no".to_owned(), "yes".to_owned()]),
            }),
        };
        let err = name_classes(vec![Prediction::class_index(5, None)], &encoding)
            .expect_err("out of range");
        assert!(err.to_string().contains("2 classes"), "{err}");
        // Everything else passes through untouched.
        assert_eq!(
            name_classes(vec![Prediction::number(1.5)], &encoding).expect("passthrough"),
            vec![Prediction::number(1.5)]
        );
    }

    #[tokio::test]
    async fn an_instance_with_no_stored_encoding_cannot_be_applied() {
        let mut instance = instance();
        instance.encoding = Json::Null;
        let err = predict_rows(&registry(), "fixed_class", &instance, &training())
            .await
            .expect_err("no encoding");
        assert!(err.to_string().contains("no stored encoding"), "{err}");
    }
}

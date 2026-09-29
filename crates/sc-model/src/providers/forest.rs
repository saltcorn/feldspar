//! `random_forest` — the provider whose outcome is a function of its
//! configuration (TODO task 4.4).
//!
//! One algorithm, two outcomes: a forest of regression trees when the label is a
//! measurement and a forest of classification trees when it is a category. That
//! is the case [`OutcomeSpec::Supervised`] exists for, and the alternative — a
//! `random_forest_regressor` and a `random_forest_classifier` in the picker,
//! with the admin choosing which of them their column is — pushes a question the
//! data already answers onto the person.
//!
//! The branch is taken twice, in two places, from two different facts, and they
//! have to agree:
//!
//! - **Before the fit**, `outcome` reads the *dataset* shape: `price` is a float
//!   column, so this is a regression. That is what the form renders, what the
//!   metric set is chosen by, and what `predict()` checks a target field
//!   against.
//! - **During the fit**, this provider reads the *frame*'s label column: a class
//!   index arrives as an integer column and a measurement as a float one (see
//!   [`Encoded::frame`](crate::Encoded::frame)). `fit` is handed no outcome —
//!   the seam carries a frame, a configuration and a hyperparameter point — so
//!   the encoded type is what it has, and it is the same fact arrived at from
//!   the other side.
//!
//! ## Feature importances are permutation importances
//!
//! smartcore keeps its trees private, so the impurity-decrease importance a tree
//! library usually exposes is not reachable. What is computed instead is the
//! **permutation** importance: shuffle one feature column, predict again, and
//! see how much worse the fit got. It is the more defensible measure anyway —
//! impurity importance is famously biased towards high-cardinality columns,
//! which after a one-hot is most of them — and it costs one prediction pass per
//! feature rather than a refit. The shuffle is seeded from the fit's own seed,
//! so two fits of one model report the same importances.

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;
use smartcore::ensemble::random_forest_classifier::{
    RandomForestClassifier, RandomForestClassifierParameters,
};
use smartcore::ensemble::random_forest_regressor::{
    RandomForestRegressor, RandomForestRegressorParameters,
};
use smartcore::linalg::basic::matrix::DenseMatrix;

use crate::encode::Matrix;
use crate::frame::Frame;
use crate::provider::{
    FitResult, ModelProvider, OutcomeSpec, ParameterBlock, ParameterRow, Prediction, column_field,
};
use crate::providers::{
    cell, column_setting, design, feature_names, from_state, label_is_classified, label_values,
    optional_whole_setting, to_state, whole_setting,
};

/// The configuration key naming the label.
const LABEL: &str = "label";
/// The hyperparameter holding how many trees the forest has.
const TREES: &str = "n_trees";
/// The hyperparameter bounding each tree's depth.
const MAX_DEPTH: &str = "max_depth";
/// The hyperparameter holding the smallest leaf a split may produce.
const MIN_LEAF: &str = "min_samples_leaf";
/// The configuration key holding the bootstrap seed.
const SEED: &str = "seed";

/// The two forests, as the stored state distinguishes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    /// A forest of regression trees.
    Regressor,
    /// A forest of classification trees.
    Classifier,
}

/// A fitted forest, as it is stored.
///
/// The forest itself is smartcore's own serialisation, which is what the
/// library's `serde` feature is enabled for: a forest is hundreds of trees and
/// there is no smaller honest description of one. Everything around it is this
/// crate's, because the state has to say which of the two forests it holds and
/// which columns, in which order, its trees were grown over.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct State {
    /// Which forest this is.
    kind: Kind,
    /// The feature columns, in the order the trees index them.
    features: Vec<String>,
    /// smartcore's serialised forest.
    forest: Json,
}

/// A random forest, regressor or classifier.
pub struct RandomForest;

#[async_trait]
impl ModelProvider for RandomForest {
    fn name(&self) -> &str {
        "random_forest"
    }

    fn description(&self) -> &str {
        "A random forest: a regressor over a numeric label, a classifier over any other"
    }

    fn config_declaration(&self) -> Vec<FormField> {
        vec![
            column_field(LABEL, "Label").required(),
            FormField::new(SEED, BasicType::Int)
                .label("Random seed")
                .default_value(0),
        ]
    }

    fn hyperparameters(&self) -> Vec<FormField> {
        vec![
            FormField::new(TREES, BasicType::Int)
                .label("Trees")
                .default_value(100),
            FormField::new(MAX_DEPTH, BasicType::Int)
                .label("Maximum depth (0 for no limit)")
                .default_value(0),
            FormField::new(MIN_LEAF, BasicType::Int)
                .label("Minimum samples per leaf")
                .default_value(1),
        ]
    }

    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Supervised {
            label: LABEL.to_owned(),
        }
    }

    async fn fit(&self, frame: &Frame, config: &Attrs, hyper: &Attrs) -> Result<FitResult> {
        let label = column_setting(config, LABEL)?;
        let seed = whole_setting(config, SEED, 0, 0)? as u64;
        let trees = whole_setting(hyper, TREES, 100, 1)?;
        let max_depth = optional_whole_setting(hyper, MAX_DEPTH)?;
        let min_leaf = whole_setting(hyper, MIN_LEAF, 1, 1)?;
        let features = feature_names(frame, Some(&label));
        let x = design(frame, &features)?;
        let y = label_values(frame, &label)?;
        let matrix = dense(&x)?;

        let depth = max_depth
            .map(|d| {
                u16::try_from(d).map_err(|_| {
                    Error::invalid(format!("`{MAX_DEPTH}`: {d} is deeper than a tree can be"))
                })
            })
            .transpose()?;

        // Which forest, from the label column's type — see the module docs.
        let (kind, forest, predicted) = if label_is_classified(frame, &label)? {
            let classes: Vec<i64> = y.iter().map(|v| *v as i64).collect();
            let mut parameters = RandomForestClassifierParameters::default()
                .with_n_trees(u16::try_from(trees).map_err(|_| {
                    Error::invalid(format!(
                        "`{TREES}`: {trees} is more trees than a classification forest can hold"
                    ))
                })?)
                .with_min_samples_leaf(min_leaf)
                .with_seed(seed);
            if let Some(depth) = depth {
                parameters = parameters.with_max_depth(depth);
            }
            let model = RandomForestClassifier::fit(&matrix, &classes, parameters)
                .map_err(|e| Error::invalid(format!("this forest could not be grown: {e}")))?;
            let predicted = predict_classifier(&model, &matrix)?;
            (Kind::Classifier, to_state(&model)?, predicted)
        } else {
            let mut parameters = RandomForestRegressorParameters::default()
                .with_n_trees(trees)
                .with_min_samples_leaf(min_leaf)
                .with_seed(seed);
            if let Some(depth) = depth {
                parameters = parameters.with_max_depth(depth);
            }
            let model = RandomForestRegressor::fit(&matrix, &y, parameters)
                .map_err(|e| Error::invalid(format!("this forest could not be grown: {e}")))?;
            let predicted = predict_regressor(&model, &matrix)?;
            (Kind::Regressor, to_state(&model)?, predicted)
        };

        let state = State {
            kind,
            features: features.clone(),
            forest,
        };
        let importances = importances(&state, &x, &y, seed, &predicted)?;
        Ok(FitResult::new(to_state(&state)?)
            .parameter(ParameterBlock::table(
                "Feature importances",
                ["feature", "importance", "loss increase"],
                importances,
            )?)
            .parameter(ParameterBlock::scalar("trees", trees as f64))
            .parameter(ParameterBlock::scalar("observations", x.rows() as f64)))
    }

    async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>> {
        let state: State = from_state(self.name(), state)?;
        let x = design(frame, &state.features)?;
        let values = apply(&state, &x)?;
        Ok(match state.kind {
            Kind::Regressor => values.into_iter().map(Prediction::number).collect(),
            // No probability: a forest's vote share is not one, and smartcore
            // does not expose the votes. An honest `None` beats a number that
            // looks like a confidence and is not.
            Kind::Classifier => values
                .into_iter()
                .map(|v| Prediction::class_index(v.max(0.0) as usize, None))
                .collect(),
        })
    }
}

/// A feature matrix as smartcore's.
fn dense(x: &Matrix) -> Result<DenseMatrix<f64>> {
    DenseMatrix::from_2d_vec(&x.to_rows())
        .map_err(|e| Error::msg(format!("this fit's design matrix could not be built: {e}")))
}

/// The forest's answer for every row, as numbers — a value for a regressor and a
/// class index for a classifier.
fn apply(state: &State, x: &Matrix) -> Result<Vec<f64>> {
    let matrix = dense(x)?;
    match state.kind {
        Kind::Regressor => {
            let model: RandomForestRegressor<f64, f64, DenseMatrix<f64>, Vec<f64>> =
                from_state("random_forest", &state.forest)?;
            predict_regressor(&model, &matrix)
        }
        Kind::Classifier => {
            let model: RandomForestClassifier<f64, i64, DenseMatrix<f64>, Vec<i64>> =
                from_state("random_forest", &state.forest)?;
            predict_classifier(&model, &matrix)
        }
    }
}

/// A regression forest over a matrix.
fn predict_regressor(
    model: &RandomForestRegressor<f64, f64, DenseMatrix<f64>, Vec<f64>>,
    matrix: &DenseMatrix<f64>,
) -> Result<Vec<f64>> {
    model
        .predict(matrix)
        .map_err(|e| Error::invalid(format!("this forest could not be applied: {e}")))
}

/// A classification forest over a matrix, as class indices.
fn predict_classifier(
    model: &RandomForestClassifier<f64, i64, DenseMatrix<f64>, Vec<i64>>,
    matrix: &DenseMatrix<f64>,
) -> Result<Vec<f64>> {
    Ok(model
        .predict(matrix)
        .map_err(|e| Error::invalid(format!("this forest could not be applied: {e}")))?
        .into_iter()
        .map(|c| c as f64)
        .collect())
}

/// Permutation importance, one row per feature, normalised to sum to 1.
///
/// The loss is squared error for a regressor and the misclassification rate for
/// a classifier — the same quantity the metric set reports, so an importance and
/// a metric are talking about the same thing. A feature whose shuffle makes the
/// fit *better* has a negative raw importance, which is a real and informative
/// outcome (the column carries no signal); it is floored at zero before the
/// normalisation, because a share of a total cannot be negative, and the
/// unnormalised column is kept beside it so the flooring is visible.
fn importances(
    state: &State,
    x: &Matrix,
    y: &[f64],
    seed: u64,
    baseline_predictions: &[f64],
) -> Result<Vec<ParameterRow>> {
    let width = x.width();
    let baseline = loss(state.kind, baseline_predictions, y);
    let mut raw = Vec::with_capacity(width);
    for j in 0..width {
        let shuffled = permute_column(x, j, seed.wrapping_add(j as u64))?;
        let predictions = apply(state, &shuffled)?;
        raw.push(loss(state.kind, &predictions, y) - baseline);
    }
    let total: f64 = raw.iter().map(|v| v.max(0.0)).sum();
    Ok(x.columns()
        .iter()
        .zip(&raw)
        .map(|(name, value)| {
            let share = if total > 0.0 {
                value.max(0.0) / total
            } else {
                0.0
            };
            ParameterRow::new(vec![Json::String(name.clone()), cell(share), cell(*value)])
        })
        .collect())
}

/// How wrong a set of predictions is — mean squared error for a regressor, the
/// fraction misclassified for a classifier.
fn loss(kind: Kind, predictions: &[f64], truth: &[f64]) -> f64 {
    if predictions.is_empty() {
        return 0.0;
    }
    let n = predictions.len() as f64;
    match kind {
        Kind::Regressor => {
            predictions
                .iter()
                .zip(truth)
                .map(|(p, t)| (p - t) * (p - t))
                .sum::<f64>()
                / n
        }
        Kind::Classifier => {
            predictions
                .iter()
                .zip(truth)
                .filter(|(p, t)| (*p - *t).abs() > f64::EPSILON)
                .count() as f64
                / n
        }
    }
}

/// `x` with column `j` shuffled by a seeded permutation.
///
/// Seeded rather than random: an importance table that changed between two runs
/// of the same fit would be a number nobody could quote. The generator is a
/// 64-bit xorshift, which is all a Fisher–Yates shuffle needs and avoids a
/// dependency on a crate this workspace does not otherwise use for arithmetic.
fn permute_column(x: &Matrix, j: usize, seed: u64) -> Result<Matrix> {
    let mut values = x.values().to_vec();
    let width = x.width();
    let rows = x.rows();
    let mut order: Vec<usize> = (0..rows).collect();
    let mut rng = seed | 1;
    for i in (1..rows).rev() {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        order.swap(i, (rng % (i as u64 + 1)) as usize);
    }
    let column: Vec<f64> = (0..rows).map(|i| x.values()[i * width + j]).collect();
    for (i, from) in order.iter().enumerate() {
        values[i * width + j] = column[*from];
    }
    Matrix::new(x.columns().to_vec(), values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{DatasetColumnShape, DatasetShape};
    use crate::frame::ColumnType;
    use crate::provider::Outcome;
    use crate::providers::testing::{attrs, close, floats, frame, ints, number, scalar, table};

    /// Twenty rows where `signal` decides the answer and `noise` does not.
    ///
    /// The label is built from `signal` alone, so the importance table has a
    /// right answer and the regressor has something to find.
    fn data(classified: bool) -> crate::frame::Frame {
        let signal: Vec<f64> = (0..20).map(f64::from).collect();
        let noise = [
            3., 1., 4., 1., 5., 9., 2., 6., 5., 3., 5., 8., 9., 7., 9., 3., 2., 3., 8., 4.,
        ];
        let label = if classified {
            ints(
                &signal
                    .iter()
                    .map(|s| i64::from(*s >= 10.0))
                    .collect::<Vec<_>>(),
            )
        } else {
            floats(&signal.iter().map(|s| s * 2.0).collect::<Vec<_>>())
        };
        frame(vec![
            ("signal", floats(&signal)),
            ("noise", floats(&noise)),
            ("label", label),
        ])
    }

    fn config() -> Attrs {
        attrs(&[("label", "label".into()), ("seed", 11.into())])
    }

    fn hyper() -> Attrs {
        attrs(&[("n_trees", 30.into()), ("min_samples_leaf", 1.into())])
    }

    /// A float label is a regression: the answers are numbers, and they track
    /// the label they were fitted against.
    #[tokio::test]
    async fn a_numeric_label_makes_a_regressor() {
        let rows = data(false);
        let fit = RandomForest.fit(&rows, &config(), &hyper()).await.unwrap();
        let features = frame(vec![
            ("signal", floats(&[0., 10., 19.])),
            ("noise", floats(&[3., 5., 4.])),
        ]);
        let predictions = RandomForest.predict(&fit.state, &features).await.unwrap();
        let values: Vec<f64> = predictions
            .iter()
            .map(|p| match p {
                Prediction::Number { value } => *value,
                other => panic!("a regression answers a number, not {other:?}"),
            })
            .collect();
        assert!(values[0] < values[1] && values[1] < values[2], "{values:?}");
        // A forest cannot extrapolate, but on rows it was grown over it should
        // land close to the label.
        close(values[1], 20.0, 4.0);
    }

    /// An integer label is a class index, so the same provider is a classifier —
    /// and the answers are class indices, not numbers.
    #[tokio::test]
    async fn an_integer_label_makes_a_classifier() {
        let rows = data(true);
        let fit = RandomForest.fit(&rows, &config(), &hyper()).await.unwrap();
        let features = frame(vec![
            ("signal", floats(&[1., 18.])),
            ("noise", floats(&[1., 8.])),
        ]);
        let predictions = RandomForest.predict(&fit.state, &features).await.unwrap();
        assert_eq!(
            predictions,
            vec![
                Prediction::class_index(0, None),
                Prediction::class_index(1, None),
            ]
        );
    }

    /// The outcome is a function of the *dataset*'s column type, resolved before
    /// any fit — the other half of the same decision, and the one the form and
    /// `predict()` read.
    #[test]
    fn the_declared_outcome_follows_the_labels_type() {
        let shape = |ty| DatasetShape {
            table: "t".to_owned(),
            columns: vec![DatasetColumnShape {
                name: "label".to_owned(),
                ty,
            }],
        };
        let config = attrs(&[("label", "label".into())]);
        assert_eq!(
            RandomForest
                .outcome(&shape(ColumnType::Float), &config)
                .unwrap(),
            Outcome::Regression {
                label: "label".to_owned()
            }
        );
        assert_eq!(
            RandomForest
                .outcome(&shape(ColumnType::Str), &config)
                .unwrap(),
            Outcome::Classification {
                label: "label".to_owned(),
                classes: None,
            }
        );
    }

    /// Permutation importance finds the column the label was built from, and the
    /// shares add up to one.
    #[tokio::test]
    async fn the_column_the_label_came_from_is_the_important_one() {
        let fit = RandomForest
            .fit(&data(false), &config(), &hyper())
            .await
            .unwrap();
        let (columns, rows) = table(&fit.parameters, "Feature importances");
        assert_eq!(columns, ["feature", "importance", "loss increase"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Json::String("signal".to_owned()));
        assert!(
            number(&rows[0][1]) > number(&rows[1][1]),
            "`signal` should matter more than `noise`: {rows:?}"
        );
        close(number(&rows[0][1]) + number(&rows[1][1]), 1.0, 1e-9);
        close(scalar(&fit.parameters, "trees"), 30.0, 0.0);
    }

    /// The same seed grows the same forest, so an instance's numbers are worth
    /// quoting.
    #[tokio::test]
    async fn a_seeded_forest_is_the_same_forest_twice() {
        let rows = data(false);
        let first = RandomForest.fit(&rows, &config(), &hyper()).await.unwrap();
        let second = RandomForest.fit(&rows, &config(), &hyper()).await.unwrap();
        let features = frame(vec![
            ("signal", floats(&[4., 12.])),
            ("noise", floats(&[5., 9.])),
        ]);
        assert_eq!(
            RandomForest.predict(&first.state, &features).await.unwrap(),
            RandomForest
                .predict(&second.state, &features)
                .await
                .unwrap()
        );
    }
}

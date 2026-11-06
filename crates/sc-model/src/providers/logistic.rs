//! `logistic_regression` — a classifier whose answer carries its own confidence
//! (TODO task 4.3).
//!
//! The fit is smartcore's; the two things this provider adds are the two things
//! anybody asks a logistic regression for.
//!
//! **The odds ratio.** A coefficient of 0.41 on `bedrooms` is a statement about
//! log-odds, which nobody reads directly; e^0.41 = 1.51 is "each extra bedroom
//! multiplies the odds by half again", which everybody does. Both are in the
//! table, because the ratio is the readable one and the coefficient is the one
//! that adds up.
//!
//! **The predicted probability.** smartcore's `predict` answers a class and
//! stops there. What is wanted is the class *and* how sure the fit is, because
//! "spam, 0.51" and "spam, 0.99" are different answers and a row that stored
//! only the first word could not tell them apart — so the linear predictor is
//! evaluated here and the probability of the answered class rides along on the
//! [`Prediction`] (§7). That is also why the fitted state is the coefficients
//! rather than smartcore's serialised model: what is stored is exactly what both
//! the table and the probability are computed from.
//!
//! ## The coefficient table is keyed by class **index**
//!
//! A multi-class fit has a coefficient per class, and this provider sees class
//! *indices* and not names: the names belong to the instance's target encoding,
//! which is the host's (§6). The table therefore says `class 0`, `class 1`, and
//! the encoding stored beside it on the instance is what turns those into
//! names — the same split that makes a provider answer
//! [`Prediction::ClassIndex`] and the host answer [`Prediction::Class`].

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;
use smartcore::linalg::basic::arrays::Array;
use smartcore::linalg::basic::matrix::DenseMatrix;
use smartcore::linear::logistic_regression::{
    LogisticRegression as ScLogisticRegression, LogisticRegressionParameters,
};

use crate::frame::Frame;
use crate::provider::{
    FitResult, ModelProvider, OutcomeSpec, ParameterBlock, ParameterRow, Prediction,
    categorical_column_field,
};
use crate::providers::{
    cell, column_setting, design, feature_names, from_state, label_values, number_setting, to_state,
};

/// The configuration key naming the label.
const LABEL: &str = "label";
/// The hyperparameter holding the ridge penalty.
const ALPHA: &str = "alpha";

/// A fitted logistic regression, as it is stored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct State {
    /// The feature columns, in coefficient order.
    features: Vec<String>,
    /// The **class indices** this fit saw, in the order its coefficient rows are
    /// in.
    ///
    /// Stored rather than assumed to be `0..k`: a training split can miss a
    /// class the dataset has, and answering "class 1" for what the fit called
    /// its second class would name the wrong one.
    classes: Vec<usize>,
    /// One row of coefficients per model: a single row for two classes (the
    /// log-odds of the second against the first), one per class for more.
    coefficients: Vec<Vec<f64>>,
    /// The constant term of each row.
    intercepts: Vec<f64>,
}

/// Logistic regression.
pub struct LogisticRegression;

#[async_trait]
impl ModelProvider for LogisticRegression {
    fn name(&self) -> &str {
        "logistic_regression"
    }

    fn description(&self) -> &str {
        "Logistic regression: a predicted class, its probability, and an odds ratio per column"
    }

    fn config_declaration(&self) -> Vec<FormField> {
        vec![categorical_column_field(LABEL, "Label").required()]
    }

    fn hyperparameters(&self) -> Vec<FormField> {
        vec![
            FormField::new(ALPHA, BasicType::Float)
                .label("Regularisation (ridge penalty)")
                .default_value(0.0),
        ]
    }

    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Classification {
            label: LABEL.to_owned(),
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "float-to-integer as saturates (NaN is 0); it rounds a finite count, bin or index computed from the data"
    )]
    async fn fit(&self, frame: &Frame, config: &Attrs, hyper: &Attrs) -> Result<FitResult> {
        let label = column_setting(config, LABEL)?;
        let alpha = number_setting(hyper, ALPHA, 0.0)?;
        if alpha < 0.0 {
            return Err(Error::invalid(format!(
                "`{ALPHA}`: a ridge penalty of {alpha} is negative, which rewards large \
                 coefficients instead of penalising them"
            )));
        }
        let features = feature_names(frame, Some(&label));
        let x = design(frame, &features)?;
        // The label of a classification frame is a class index (see
        // `Encoded::frame`), so this cast is exact rather than a rounding.
        let y: Vec<i64> = label_values(frame, &label)?
            .into_iter()
            .map(|v| v as i64)
            .collect();

        let matrix = DenseMatrix::from_2d_vec(&x.to_rows())
            .map_err(|e| Error::msg(format!("this fit's design matrix could not be built: {e}")))?;
        let model = ScLogisticRegression::fit(
            &matrix,
            &y,
            LogisticRegressionParameters::default().with_alpha(alpha),
        )
        .map_err(|e| {
            Error::invalid(format!("this logistic regression could not be fitted: {e}"))
        })?;

        let classes: Vec<usize> = model
            .classes()
            .iter()
            .map(|c| {
                usize::try_from(*c).map_err(|_| {
                    Error::msg(format!(
                        "this fit was handed {c} as a class index, and an index is not negative"
                    ))
                })
            })
            .collect::<Result<_>>()?;
        let (rows, width) = model.coefficients().shape();
        if width != features.len() {
            return Err(Error::msg(format!(
                "the fit answered {width} coefficients for {} features",
                features.len()
            )));
        }
        let coefficients: Vec<Vec<f64>> = (0..rows)
            .map(|r| {
                (0..width)
                    .map(|c| *model.coefficients().get((r, c)))
                    .collect()
            })
            .collect();
        let intercepts: Vec<f64> = (0..rows).map(|r| *model.intercept().get((r, 0))).collect();

        let table = coefficient_table(&features, &classes, &coefficients, &intercepts)?;
        let state = State {
            features,
            classes: classes.clone(),
            coefficients,
            intercepts,
        };
        Ok(FitResult::new(to_state(&state)?)
            .parameter(table)
            .parameter(ParameterBlock::scalar("classes", classes.len() as f64))
            .parameter(ParameterBlock::scalar("observations", x.rows() as f64)))
    }

    async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>> {
        let state: State = from_state(self.name(), state)?;
        let x = design(frame, &state.features)?;
        (0..x.rows())
            .map(|i| {
                let row = x.row(i).unwrap_or(&[]);
                let (which, probability) = classify(&state, row)?;
                let class = <[usize]>::get(&state.classes, which)
                    .copied()
                    .ok_or_else(|| {
                        Error::msg(format!(
                            "this fit answered its {}th class and was fitted with {}",
                            which + 1,
                            state.classes.len()
                        ))
                    })?;
                Ok(Prediction::class_index(class, Some(probability)))
            })
            .collect()
    }
}

/// Which of the fitted classes this row is, and the probability the fit gives
/// that answer.
///
/// Two shapes, because smartcore fits two: one row of coefficients for a binary
/// problem (a sigmoid over the single linear predictor) and one per class for a
/// multi-class one (a softmax over all of them).
fn classify(state: &State, row: &[f64]) -> Result<(usize, f64)> {
    let scores: Vec<f64> = state
        .coefficients
        .iter()
        .zip(&state.intercepts)
        .map(|(weights, intercept)| {
            intercept + weights.iter().zip(row).map(|(w, x)| w * x).sum::<f64>()
        })
        .collect();
    if scores.is_empty() {
        return Err(Error::msg("this fit stored no coefficients"));
    }
    if scores.len() == 1 {
        // Binary: the single predictor is the log-odds of the *second* class.
        let p = sigmoid(scores[0]);
        return Ok(if p > 0.5 { (1, p) } else { (0, 1.0 - p) });
    }
    let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exponentials: Vec<f64> = scores.iter().map(|s| (s - max).exp()).collect();
    let total: f64 = exponentials.iter().sum();
    let best = exponentials
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or(0, |(i, _)| i);
    Ok((best, exponentials[best] / total))
}

/// The logistic function, written so a large negative score does not overflow
/// the exponential before it underflows the answer.
fn sigmoid(z: f64) -> f64 {
    if z >= 0.0 {
        1.0 / (1.0 + (-z).exp())
    } else {
        let e = z.exp();
        e / (1.0 + e)
    }
}

/// The coefficients and their odds ratios, as the screen shows them.
///
/// A binary fit has one row per term; a multi-class fit has one per term *per
/// class*, and the class column carries the index for the reason in the module
/// docs.
fn coefficient_table(
    features: &[String],
    classes: &[usize],
    coefficients: &[Vec<f64>],
    intercepts: &[f64],
) -> Result<ParameterBlock> {
    let multi = coefficients.len() > 1;
    let mut rows = Vec::new();
    for (r, weights) in coefficients.iter().enumerate() {
        let class = <[usize]>::get(classes, r).copied().unwrap_or(r);
        let mut push = |term: &str, value: f64| {
            let mut cells = Vec::with_capacity(4);
            if multi {
                cells.push(Json::String(format!("class {class}")));
            }
            cells.push(Json::String(term.to_owned()));
            cells.push(cell(value));
            cells.push(cell(value.exp()));
            rows.push(ParameterRow { cells });
        };
        push("(intercept)", intercepts.get(r).copied().unwrap_or(0.0));
        for (term, value) in features.iter().zip(weights) {
            push(term, *value);
        }
    }
    let columns: Vec<&str> = if multi {
        vec!["class", "term", "coefficient", "odds ratio"]
    } else {
        vec!["term", "coefficient", "odds ratio"]
    };
    ParameterBlock::table("Coefficients", columns, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::testing::{attrs, close, floats, frame, ints, number, table};

    /// Ten rows on one axis, separated at 5½: the classes are exactly what the
    /// value of `x` says, which is the answer a logistic regression should find
    /// and the one a reader can check without arithmetic.
    fn separable() -> crate::frame::Frame {
        frame(vec![
            ("x", floats(&[1., 2., 3., 4., 5., 6., 7., 8., 9., 10.])),
            ("class", ints(&[0, 0, 0, 0, 0, 1, 1, 1, 1, 1])),
        ])
    }

    #[tokio::test]
    async fn it_separates_two_classes_and_says_how_sure_it_is() {
        let data = separable();
        let fit = LogisticRegression
            .fit(&data, &attrs(&[("label", "class".into())]), &Attrs::new())
            .await
            .unwrap();

        // The features frame a prediction gets has no label column.
        let features = frame(vec![(
            "x",
            floats(&[1., 2., 3., 4., 5., 6., 7., 8., 9., 10.]),
        )]);
        let predictions = LogisticRegression
            .predict(&fit.state, &features)
            .await
            .unwrap();
        assert_eq!(predictions.len(), 10);

        let truth = [0usize, 0, 0, 0, 0, 1, 1, 1, 1, 1];
        let mut right = 0;
        for (prediction, actual) in predictions.iter().zip(truth) {
            let Prediction::ClassIndex { index, probability } = prediction else {
                panic!("a provider answers a class index, not {prediction:?}");
            };
            // The probability is the one of the class that was answered, so it
            // is never the losing side of a coin flip.
            let probability = probability.expect("a logistic regression has a probability");
            assert!(
                (0.5..=1.0).contains(&probability),
                "{probability} is not the probability of the answered class"
            );
            right += usize::from(*index == actual);
        }
        assert_eq!(right, 10, "every training row should fall on its own side");

        // Row 1 is deep in class 0 and row 10 deep in class 1, so both should be
        // answered with more confidence than the rows either side of the
        // boundary.
        let ends = [&predictions[0], &predictions[9]];
        for prediction in ends {
            let Prediction::ClassIndex { probability, .. } = prediction else {
                unreachable!()
            };
            assert!(probability.unwrap_or(0.0) > 0.9, "{prediction:?}");
        }
    }

    /// The odds ratio is what the table is read for, and it is exactly the
    /// exponential of the coefficient beside it — asserted rather than assumed,
    /// because a table where the two columns disagreed would be believed.
    #[tokio::test]
    async fn the_odds_ratio_is_the_exponential_of_the_coefficient() {
        let fit = LogisticRegression
            .fit(
                &separable(),
                &attrs(&[("label", "class".into())]),
                &Attrs::new(),
            )
            .await
            .unwrap();
        let (columns, rows) = table(&fit.parameters, "Coefficients");
        assert_eq!(columns, ["term", "coefficient", "odds ratio"]);
        assert_eq!(rows.len(), 2, "an intercept and one feature");
        assert_eq!(rows[1][0], Json::String("x".to_owned()));
        for row in &rows {
            close(number(&row[2]), number(&row[1]).exp(), 1e-9);
        }
        // Bigger `x` means class 1, so the odds of it rise with `x`.
        assert!(number(&rows[1][1]) > 0.0, "{:?}", rows[1]);
        assert!(number(&rows[1][2]) > 1.0, "{:?}", rows[1]);
    }

    /// Three classes: one coefficient row per class, keyed by the class index
    /// the encoding assigned (the names are the instance's, not the provider's).
    #[tokio::test]
    async fn three_classes_get_a_coefficient_row_each() {
        let data = frame(vec![
            ("x", floats(&[1., 2., 3., 10., 11., 12., 20., 21., 22.])),
            ("class", ints(&[0, 0, 0, 1, 1, 1, 2, 2, 2])),
        ]);
        let fit = LogisticRegression
            .fit(&data, &attrs(&[("label", "class".into())]), &Attrs::new())
            .await
            .unwrap();
        let (columns, rows) = table(&fit.parameters, "Coefficients");
        assert_eq!(columns, ["class", "term", "coefficient", "odds ratio"]);
        assert_eq!(rows.len(), 6, "three classes × (intercept + x)");
        assert_eq!(rows[0][0], Json::String("class 0".to_owned()));
        assert_eq!(rows[4][0], Json::String("class 2".to_owned()));

        let features = frame(vec![("x", floats(&[1., 11., 21.]))]);
        let predictions = LogisticRegression
            .predict(&fit.state, &features)
            .await
            .unwrap();
        let answered: Vec<usize> = predictions
            .iter()
            .map(|p| match p {
                Prediction::ClassIndex { index, .. } => *index,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(answered, vec![0, 1, 2]);
    }

    /// A negative ridge penalty rewards large coefficients instead of
    /// penalising them, which is not a fit anybody meant to ask for.
    #[tokio::test]
    async fn a_negative_penalty_is_refused_by_name() {
        let hyper = attrs(&[("alpha", (-1.0).into())]);
        let err = LogisticRegression
            .fit(&separable(), &attrs(&[("label", "class".into())]), &hyper)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("alpha") && err.contains("negative"), "{err}");
    }
}

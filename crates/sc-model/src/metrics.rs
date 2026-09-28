//! Scoring a fit: **the host's job, not the provider's** (TODO §7, task 3.2).
//!
//! A provider returns a [`FitResult`](crate::FitResult) — its serialised state
//! and its parameters — and no metrics. `sc-model` computes them here, by
//! running the fitted state back over each split and scoring the predictions
//! against the truth.
//!
//! Two reasons, and both are about what the number on the screen *means*. It
//! makes providers **comparable**: the smartcore regression and the
//! scikit-learn one are scored by the same code on the same rows, so an RMSE of
//! 12 400 beside an RMSE of 11 900 is a comparison and not a coincidence of two
//! implementations' conventions. And it means a provider written in another
//! language **does not have to reimplement R²** to be a citizen here — the seam
//! carries predictions, and predictions are all the scoring needs.
//!
//! Five metric sets, one per [`Outcome`], because the outcome is the taxonomy of
//! *answers* and a metric is a statement about an answer:
//!
//! | outcome | metrics |
//! | --- | --- |
//! | regression | R², RMSE, MAE |
//! | classification | accuracy, per-class precision/recall/F1, the confusion matrix |
//! | clustering | cluster sizes, within-cluster sum of squares |
//! | dimensionality reduction | explained variance per component |
//! | hypothesis test | nothing — the parameters *are* the answer |
//!
//! ## The primary metric, and why it points one way
//!
//! A hyperparameter search scores each grid point on the validation split and
//! keeps the best (§11), so there has to be one number per outcome and **bigger
//! has to be better**. R² and accuracy already are; within-cluster sum of
//! squares is not, so [`primary`](Metrics::primary) reports its negation. A
//! search that silently minimised where it meant to maximise would pick the
//! worst point of the grid and report it as the winner, which is the kind of
//! wrong answer that looks exactly like a right one.

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use serde_json::Value as Json;

use crate::encode::{Encoded, Matrix};
use crate::provider::{Outcome, Prediction};
use crate::split::Part;

/// Serialising a metric that has no value.
///
/// R² is genuinely undefined for a constant label, and JSON has no spelling for
/// NaN — `serde_json` already writes `null`, but reading `null` back into an
/// `f64` is an error unless somebody says what it means. It means "not
/// available", so it round-trips as NaN rather than as 0, which would be a
/// number somebody could compare.
mod nullable {
    use serde::{Deserialize, Deserializer, Serializer};

    /// A metric as JSON: the number, or `null` when it is not finite.
    pub fn serialize<S: Serializer>(value: &f64, s: S) -> Result<S::Ok, S::Error> {
        if value.is_finite() {
            s.serialize_f64(*value)
        } else {
            s.serialize_none()
        }
    }

    /// A metric back off JSON, with `null` meaning "not available".
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        Ok(Option::<f64>::deserialize(d)?.unwrap_or(f64::NAN))
    }
}

/// The same, for a list of metrics.
mod nullable_list {
    use serde::{Deserialize, Deserializer, Serializer, ser::SerializeSeq};

    /// A list of metrics as JSON, with the non-finite ones `null`.
    pub fn serialize<S: Serializer>(values: &[f64], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(values.len()))?;
        for value in values {
            if value.is_finite() {
                seq.serialize_element(value)?;
            } else {
                seq.serialize_element(&Option::<f64>::None)?;
            }
        }
        seq.end()
    }

    /// A list of metrics back off JSON.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<f64>, D::Error> {
        Ok(Vec::<Option<f64>>::deserialize(d)?
            .into_iter()
            .map(|v| v.unwrap_or(f64::NAN))
            .collect())
    }
}

/// One class's precision, recall and F1, plus how many rows were actually it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClassMetrics {
    /// The class name, as the fit saw it in the data.
    pub class: String,
    /// Of the rows predicted to be this class, the fraction that were.
    #[serde(with = "nullable")]
    pub precision: f64,
    /// Of the rows that were this class, the fraction predicted to be.
    #[serde(with = "nullable")]
    pub recall: f64,
    /// The harmonic mean of the two.
    #[serde(with = "nullable")]
    pub f1: f64,
    /// How many rows actually were this class — the support, without which a
    /// recall of 1.0 over three rows reads like a recall of 1.0 over three
    /// thousand.
    pub support: usize,
}

/// What one split's rows scored (§7).
///
/// The variant is the [`Outcome`]'s, so nothing has to ask whether an accuracy
/// on a regression means anything: there is no place to put one.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "metrics", rename_all = "snake_case")]
pub enum Metrics {
    /// A regression's fit to the truth.
    Regression {
        /// Coefficient of determination: 1 − SS_res / SS_tot. Negative for a fit
        /// worse than predicting the mean, which is a real thing that happens on
        /// held-out rows and is reported rather than clamped.
        #[serde(with = "nullable")]
        r2: f64,
        /// Root mean squared error, in the label's own units.
        #[serde(with = "nullable")]
        rmse: f64,
        /// Mean absolute error, in the label's own units.
        #[serde(with = "nullable")]
        mae: f64,
        /// How many rows were scored.
        rows: usize,
    },
    /// A classification's agreement with the truth.
    Classification {
        /// The fraction of rows predicted correctly.
        #[serde(with = "nullable")]
        accuracy: f64,
        /// Per class, in the encoding's class order.
        classes: Vec<ClassMetrics>,
        /// `confusion[actual][predicted]`, in the encoding's class order.
        confusion: Vec<Vec<usize>>,
        /// How many rows were scored.
        rows: usize,
    },
    /// A clustering's shape.
    Clustering {
        /// How many rows fell in each cluster, by cluster number.
        sizes: Vec<usize>,
        /// The summed squared distance of every row from its cluster's centre —
        /// the quantity k-means minimises, computed here from the assignments so
        /// that a provider that clusters some other way is scored the same.
        #[serde(with = "nullable")]
        wcss: f64,
        /// How many rows were scored.
        rows: usize,
    },
    /// A projection's information content.
    Embedding {
        /// The fraction of the features' total variance each component carries,
        /// in component order.
        #[serde(with = "nullable_list")]
        explained_variance: Vec<f64>,
        /// How many rows were scored.
        rows: usize,
    },
    /// A hypothesis test scores nothing: its parameters are the answer.
    None,
    /// A posterior's sampler diagnostics (Stan TODO §15), computed by the host
    /// from the stored draws — stored under the `train` split, because every
    /// row the posterior was fitted from is one it saw.
    Posterior(PosteriorMetrics),
    /// An optimiser's posterior mode: one point, so no diagnostics of mixing —
    /// its log density and how long it took to get there.
    PosteriorMode(ModeMetrics),
    /// An approximation's draws (Pathfinder): independent draws, not chains, so
    /// no R̂ and no sampler diagnostics.
    PosteriorApproximation(ApproximationMetrics),
}

/// The convergence diagnostics of one posterior fit (Stan TODO §15).
///
/// The host's, like every metric: a second Bayesian provider is scored by the
/// same code (`sc_model::diagnose`), from the sampler's own variables
/// (`divergent__`, `treedepth__`, `energy__`) and the summary.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PosteriorMetrics {
    /// How many chains ran.
    pub chains: usize,
    /// Post-warmup draws per chain.
    pub draws_per_chain: usize,
    /// Divergent transitions after warmup, across all chains.
    pub divergent: usize,
    /// The same, chain by chain.
    #[serde(default)]
    pub divergent_per_chain: Vec<usize>,
    /// Iterations that stopped at `max_treedepth`.
    pub max_treedepth_hits: usize,
    /// Energy Bayesian fraction of missing information, per chain.
    #[serde(default, with = "nullable_list")]
    pub ebfmi: Vec<f64>,
    /// The worst rank-normalised split-R̂ across the parameters.
    #[serde(with = "nullable")]
    pub max_rhat: f64,
    /// The smallest bulk effective sample size.
    #[serde(with = "nullable")]
    pub min_ess_bulk: f64,
    /// The smallest tail effective sample size.
    #[serde(with = "nullable")]
    pub min_ess_tail: f64,
    /// Wall time per chain, in seconds.
    #[serde(default, with = "nullable_list")]
    pub wall_seconds: Vec<f64>,
}

/// What an optimiser's posterior mode is scored by (Stan TODO §15).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModeMetrics {
    /// The log density at the mode (`lp__`).
    #[serde(with = "nullable")]
    pub log_density: f64,
    /// The optimiser's iterations, when the provider reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<u64>,
    /// Wall time, in seconds.
    #[serde(default, with = "nullable_list")]
    pub wall_seconds: Vec<f64>,
}

/// What an approximation's draws are scored by (Stan TODO §15).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApproximationMetrics {
    /// How many draws.
    pub draws: usize,
    /// The smallest bulk effective sample size across the parameters.
    #[serde(with = "nullable")]
    pub min_ess_bulk: f64,
    /// The smallest tail effective sample size.
    #[serde(with = "nullable")]
    pub min_ess_tail: f64,
    /// Wall time, in seconds.
    #[serde(default, with = "nullable_list")]
    pub wall_seconds: Vec<f64>,
}

impl Metrics {
    /// Whether these are a posterior's (any method's) — which sit under the
    /// `train` split and rank nothing.
    pub fn is_posterior(&self) -> bool {
        matches!(
            self,
            Metrics::Posterior(_) | Metrics::PosteriorMode(_) | Metrics::PosteriorApproximation(_)
        )
    }

    /// The one number a hyperparameter search maximises (§11), or `None` for an
    /// outcome with nothing to compare.
    ///
    /// Bigger is better in every case — see the module docs for why the
    /// within-cluster sum of squares is negated here.
    pub fn primary(&self) -> Option<f64> {
        match self {
            Metrics::Regression { r2, .. } => Some(*r2),
            Metrics::Classification { accuracy, .. } => Some(*accuracy),
            Metrics::Clustering { wcss, .. } => Some(-wcss),
            Metrics::Embedding {
                explained_variance, ..
            } => Some(explained_variance.iter().sum()),
            // A posterior is not searched, so there is nothing to rank.
            Metrics::None
            | Metrics::Posterior(_)
            | Metrics::PosteriorMode(_)
            | Metrics::PosteriorApproximation(_) => None,
        }
    }

    /// What the primary metric is called, for the screen and for the search's
    /// stored scores.
    pub fn primary_name(&self) -> Option<&'static str> {
        match self {
            Metrics::Regression { .. } => Some("r2"),
            Metrics::Classification { .. } => Some("accuracy"),
            Metrics::Clustering { .. } => Some("-wcss"),
            Metrics::Embedding { .. } => Some("explained variance"),
            Metrics::None
            | Metrics::Posterior(_)
            | Metrics::PosteriorMode(_)
            | Metrics::PosteriorApproximation(_) => None,
        }
    }

    /// How many rows were scored.
    pub fn rows(&self) -> usize {
        match self {
            Metrics::Regression { rows, .. }
            | Metrics::Classification { rows, .. }
            | Metrics::Clustering { rows, .. }
            | Metrics::Embedding { rows, .. } => *rows,
            // Rows of which dataset? A posterior's data is several, and what it
            // counts is draws.
            Metrics::None
            | Metrics::Posterior(_)
            | Metrics::PosteriorMode(_)
            | Metrics::PosteriorApproximation(_) => 0,
        }
    }

    /// Score `predictions` — one per row of `encoded`, in row order — against
    /// the truth `encoded` carries.
    ///
    /// The one entry point the fit uses, so that "which metric set does a random
    /// forest over a text label get" is answered in exactly one place: by its
    /// [`Outcome`].
    pub fn of(outcome: &Outcome, predictions: &[Prediction], encoded: &Encoded) -> Result<Metrics> {
        // A posterior is scored by its draws, not by predictions over a split.
        if !outcome.predicts() || outcome.is_posterior() {
            return Ok(Metrics::None);
        }
        if predictions.len() != encoded.len() {
            return Err(Error::msg(format!(
                "the provider answered {} predictions for {} rows",
                predictions.len(),
                encoded.len()
            )));
        }
        match outcome {
            Outcome::Regression { label } => {
                let truth = encoded.target.as_ref().ok_or_else(|| {
                    Error::msg(format!(
                        "no `{label}` values to score the regression against"
                    ))
                })?;
                let predicted = numbers(predictions)?;
                Ok(regression(&predicted, truth))
            }
            Outcome::Classification { label, classes } => {
                let truth = encoded.target.as_ref().ok_or_else(|| {
                    Error::msg(format!(
                        "no `{label}` values to score the classification against"
                    ))
                })?;
                let classes = classes.as_deref().unwrap_or(&[]);
                let predicted = class_indices(predictions)?;
                let actual: Vec<usize> = truth.iter().map(|v| *v as usize).collect();
                classification(&predicted, &actual, classes)
            }
            Outcome::Cluster => {
                let assignments = clusters(predictions)?;
                Ok(clustering(&assignments, &encoded.features))
            }
            Outcome::Embedding { .. } => {
                let vectors = vectors(predictions)?;
                Ok(embedding(&vectors, &encoded.features))
            }
            Outcome::Test | Outcome::Posterior { .. } => Ok(Metrics::None),
        }
    }
}

/// R², RMSE and MAE.
fn regression(predicted: &[f64], truth: &[f64]) -> Metrics {
    let rows = predicted.len();
    if rows == 0 {
        return Metrics::Regression {
            r2: f64::NAN,
            rmse: f64::NAN,
            mae: f64::NAN,
            rows: 0,
        };
    }
    let n = rows as f64;
    let mean = truth.iter().sum::<f64>() / n;
    let ss_res: f64 = predicted
        .iter()
        .zip(truth)
        .map(|(p, t)| (t - p).powi(2))
        .sum();
    let ss_tot: f64 = truth.iter().map(|t| (t - mean).powi(2)).sum();
    let mae = predicted
        .iter()
        .zip(truth)
        .map(|(p, t)| (t - p).abs())
        .sum::<f64>()
        / n;
    Metrics::Regression {
        // A constant label has no variance to explain. R² is undefined there
        // rather than 0 or 1, and NaN is how a JSON number says so — the
        // alternative, reporting 1.0 for "predicted a constant perfectly", would
        // put a flawless-looking score on a model that learned nothing.
        r2: if ss_tot > 0.0 {
            1.0 - ss_res / ss_tot
        } else {
            f64::NAN
        },
        rmse: (ss_res / n).sqrt(),
        mae,
        rows,
    }
}

/// Accuracy, the per-class table and the confusion matrix.
fn classification(predicted: &[usize], actual: &[usize], classes: &[String]) -> Result<Metrics> {
    let k = classes.len();
    for (what, values) in [("predicted", predicted), ("actual", actual)] {
        if let Some(bad) = values.iter().find(|v| **v >= k) {
            return Err(Error::msg(format!(
                "a {what} class index ({bad}) is outside the {k} classes this model was fitted \
                 with"
            )));
        }
    }
    let rows = predicted.len();
    let mut confusion = vec![vec![0usize; k]; k];
    for (p, a) in predicted.iter().zip(actual) {
        confusion[*a][*p] += 1;
    }
    let correct: usize = (0..k).map(|i| confusion[i][i]).sum();
    let per_class = classes
        .iter()
        .enumerate()
        .map(|(i, class)| {
            let tp = confusion[i][i] as f64;
            let predicted_i: f64 = (0..k).map(|a| confusion[a][i] as f64).sum();
            let actual_i: f64 = confusion[i].iter().map(|c| *c as f64).sum();
            // A class nothing was predicted to be has no precision, and a class
            // nothing actually was has no recall. 0 rather than NaN: the reading
            // "it got none of them right" is the true one, and a table of NaNs
            // is unreadable.
            let precision = if predicted_i > 0.0 {
                tp / predicted_i
            } else {
                0.0
            };
            let recall = if actual_i > 0.0 { tp / actual_i } else { 0.0 };
            let f1 = if precision + recall > 0.0 {
                2.0 * precision * recall / (precision + recall)
            } else {
                0.0
            };
            ClassMetrics {
                class: class.clone(),
                precision,
                recall,
                f1,
                support: actual_i as usize,
            }
        })
        .collect();
    Ok(Metrics::Classification {
        accuracy: if rows == 0 {
            f64::NAN
        } else {
            correct as f64 / rows as f64
        },
        classes: per_class,
        confusion,
        rows,
    })
}

/// Cluster sizes and the within-cluster sum of squares.
///
/// The centres are recomputed from the assignments rather than read off the
/// provider's state, so a provider that clusters by some means other than
/// k-means is scored by the same rule.
fn clustering(assignments: &[usize], features: &Matrix) -> Metrics {
    let width = features.width();
    let mut sums: BTreeMap<usize, (usize, Vec<f64>)> = BTreeMap::new();
    for (i, cluster) in assignments.iter().enumerate() {
        let entry = sums
            .entry(*cluster)
            .or_insert_with(|| (0, vec![0.0; width]));
        entry.0 += 1;
        if let Some(row) = features.row(i) {
            for (acc, v) in entry.1.iter_mut().zip(row) {
                *acc += v;
            }
        }
    }
    let centres: BTreeMap<usize, Vec<f64>> = sums
        .iter()
        .map(|(cluster, (count, sum))| {
            let n = *count as f64;
            (*cluster, sum.iter().map(|s| s / n).collect())
        })
        .collect();
    let wcss = assignments
        .iter()
        .enumerate()
        .map(
            |(i, cluster)| match (features.row(i), centres.get(cluster)) {
                (Some(row), Some(centre)) => row
                    .iter()
                    .zip(centre)
                    .map(|(v, c)| (v - c).powi(2))
                    .sum::<f64>(),
                _ => 0.0,
            },
        )
        .sum();
    // Sizes are indexed by cluster number, so a cluster nothing landed in is a
    // zero and not a hole: an empty cluster is a fact about the fit.
    let highest = sums.keys().copied().max().map_or(0, |k| k + 1);
    let mut sizes = vec![0usize; highest];
    for (cluster, (count, _)) in &sums {
        sizes[*cluster] = *count;
    }
    Metrics::Clustering {
        sizes,
        wcss,
        rows: assignments.len(),
    }
}

/// The fraction of the features' total variance each component carries.
///
/// Computed from the projections and the features rather than from the
/// provider's eigenvalues, for the reason every metric here is: a projection is
/// a projection, whoever produced it, and one number that means one thing beats
/// two providers' conventions.
fn embedding(vectors: &[Vec<f64>], features: &Matrix) -> Metrics {
    let rows = vectors.len();
    let total: f64 = (0..features.width())
        .map(|j| {
            variance(
                &(0..features.rows())
                    .filter_map(|i| features.row(i).map(|r| r[j]))
                    .collect::<Vec<_>>(),
            )
        })
        .sum();
    let components = vectors.first().map_or(0, Vec::len);
    let explained_variance = (0..components)
        .map(|c| {
            let column: Vec<f64> = vectors.iter().filter_map(|v| v.get(c).copied()).collect();
            if total > 0.0 {
                variance(&column) / total
            } else {
                f64::NAN
            }
        })
        .collect();
    Metrics::Embedding {
        explained_variance,
        rows,
    }
}

/// The sample variance of `values`.
fn variance(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)
}

/// The predictions as numbers, or an error naming the row and what arrived.
fn numbers(predictions: &[Prediction]) -> Result<Vec<f64>> {
    predictions
        .iter()
        .enumerate()
        .map(|(i, p)| match p {
            Prediction::Number { value } => Ok(*value),
            other => Err(wrong_shape(i, "a number", other)),
        })
        .collect()
}

/// The predictions as class indices.
fn class_indices(predictions: &[Prediction]) -> Result<Vec<usize>> {
    predictions
        .iter()
        .enumerate()
        .map(|(i, p)| match p {
            Prediction::ClassIndex { index, .. } => Ok(*index),
            other => Err(wrong_shape(i, "a class index", other)),
        })
        .collect()
}

/// The predictions as cluster numbers.
fn clusters(predictions: &[Prediction]) -> Result<Vec<usize>> {
    predictions
        .iter()
        .enumerate()
        .map(|(i, p)| match p {
            Prediction::Cluster { cluster } => Ok(*cluster),
            other => Err(wrong_shape(i, "a cluster number", other)),
        })
        .collect()
}

/// The predictions as vectors.
fn vectors(predictions: &[Prediction]) -> Result<Vec<Vec<f64>>> {
    predictions
        .iter()
        .enumerate()
        .map(|(i, p)| match p {
            Prediction::Vector { values } => Ok(values.clone()),
            other => Err(wrong_shape(i, "a vector", other)),
        })
        .collect()
}

/// A provider answering the wrong shape for its own declared outcome. A
/// programming error in the provider, so it says which row and what came back
/// rather than "invalid prediction".
fn wrong_shape(row: usize, wanted: &str, got: &Prediction) -> Error {
    Error::msg(format!(
        "this fit's outcome needs {wanted} per row, but the provider answered {} for row {}",
        match got {
            Prediction::Number { .. } => "a number",
            Prediction::ClassIndex { .. } => "a class index",
            Prediction::Class { .. } => "a class name",
            Prediction::Cluster { .. } => "a cluster number",
            Prediction::Vector { .. } => "a vector",
        },
        row + 1
    ))
}

/// The metrics of every split of one fit — what the instance's `metrics` column
/// holds.
///
/// Per split, because "R² 0.94" is not a claim about anything until you know
/// whether it was measured on the rows the fit was computed from. A split with
/// no rows has no entry rather than an entry full of NaN.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct SplitMetrics {
    /// The rows the fit was computed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub train: Option<Metrics>,
    /// The rows a hyperparameter search scored its grid on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<Metrics>,
    /// The rows held out — the ones the number on the screen should be read off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<Metrics>,
}

impl SplitMetrics {
    /// Record one split's metrics.
    pub fn set(&mut self, part: Part, metrics: Metrics) {
        match part {
            Part::Train => self.train = Some(metrics),
            Part::Validation => self.validation = Some(metrics),
            Part::Test => self.test = Some(metrics),
        }
    }

    /// One split's metrics.
    pub fn get(&self, part: Part) -> Option<&Metrics> {
        match part {
            Part::Train => self.train.as_ref(),
            Part::Validation => self.validation.as_ref(),
            Part::Test => self.test.as_ref(),
        }
    }

    /// These metrics as the instance's `metrics` column.
    ///
    /// R² is genuinely NaN for a constant label and RMSE is genuinely infinite
    /// nowhere, but JSON has no spelling for either — so a non-finite metric is
    /// `null` on the way out, which reads as "not available" and not as a
    /// number that happens to be enormous.
    pub fn to_json(&self) -> Result<Json> {
        serde_json::to_value(self).map_err(|e| Error::msg(format!("metrics: {e}")))
    }

    /// The metrics an instance's `metrics` column holds.
    pub fn from_json(json: &Json) -> Result<SplitMetrics> {
        if json.is_null() {
            return Ok(SplitMetrics::default());
        }
        serde_json::from_value(json.clone())
            .map_err(|e| Error::invalid(format!("this instance's stored metrics: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_perfect_regression_is_r2_one_and_no_error() {
        let m = regression(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]);
        let Metrics::Regression {
            r2,
            rmse,
            mae,
            rows,
        } = m
        else {
            panic!("wrong set");
        };
        assert!((r2 - 1.0).abs() < 1e-12);
        assert_eq!(rmse, 0.0);
        assert_eq!(mae, 0.0);
        assert_eq!(rows, 3);
    }

    #[test]
    fn predicting_the_mean_is_r2_zero_and_a_worse_fit_is_negative() {
        // Truth 1, 2, 3 has mean 2 and SS_tot 2.
        let mean = regression(&[2.0, 2.0, 2.0], &[1.0, 2.0, 3.0]);
        assert!((mean.primary().unwrap() - 0.0).abs() < 1e-12);
        let worse = regression(&[3.0, 2.0, 1.0], &[1.0, 2.0, 3.0]);
        // SS_res = 4 + 0 + 4 = 8, so R² = 1 − 8/2 = −3.
        assert!((worse.primary().unwrap() + 3.0).abs() < 1e-12);
        let Metrics::Regression { rmse, mae, .. } = worse else {
            panic!("wrong set");
        };
        assert!((rmse - (8.0f64 / 3.0).sqrt()).abs() < 1e-12);
        assert!((mae - 4.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn a_constant_label_has_no_variance_to_explain_and_says_so() {
        let m = regression(&[7.0, 7.0], &[7.0, 7.0]);
        let Metrics::Regression { r2, rmse, .. } = m else {
            panic!("wrong set");
        };
        assert!(r2.is_nan());
        assert_eq!(rmse, 0.0);
    }

    #[test]
    fn a_confusion_matrix_is_actual_by_predicted_and_the_class_table_matches_it() {
        let classes = vec!["no".to_owned(), "yes".to_owned()];
        // Two actual `no` (one caught), three actual `yes` (all caught).
        let m = classification(&[0, 1, 1, 1, 1], &[0, 0, 1, 1, 1], &classes).expect("score");
        let Metrics::Classification {
            accuracy,
            classes,
            confusion,
            rows,
        } = m
        else {
            panic!("wrong set");
        };
        assert_eq!(rows, 5);
        assert!((accuracy - 0.8).abs() < 1e-12);
        assert_eq!(confusion, vec![vec![1, 1], vec![0, 3]]);
        assert_eq!(classes[0].class, "no");
        assert_eq!(classes[0].support, 2);
        assert!((classes[0].precision - 1.0).abs() < 1e-12);
        assert!((classes[0].recall - 0.5).abs() < 1e-12);
        assert!((classes[0].f1 - 2.0 / 3.0).abs() < 1e-12);
        assert!((classes[1].precision - 0.75).abs() < 1e-12);
        assert!((classes[1].recall - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_class_index_outside_the_fitted_classes_is_refused() {
        let classes = vec!["no".to_owned(), "yes".to_owned()];
        let err = classification(&[2], &[0], &classes).expect_err("out of range");
        assert!(err.to_string().contains("2 classes"), "{err}");
    }

    #[test]
    fn clustering_scores_sizes_and_the_within_cluster_sum_of_squares() {
        // Two obvious blobs, one unit apart within each.
        let features =
            Matrix::new(vec!["x".to_owned()], vec![0.0, 1.0, 10.0, 11.0, 12.0]).expect("matrix");
        let m = clustering(&[0, 0, 1, 1, 1], &features);
        let Metrics::Clustering { sizes, wcss, rows } = m.clone() else {
            panic!("wrong set");
        };
        assert_eq!(sizes, vec![2, 3]);
        assert_eq!(rows, 5);
        // Centres 0.5 and 11: 0.25 + 0.25 + 1 + 0 + 1.
        assert!((wcss - 2.5).abs() < 1e-12);
        // The search maximises, so a tighter clustering has to score higher.
        let looser = clustering(&[0, 1, 0, 1, 0], &features);
        assert!(m.primary().unwrap() > looser.primary().unwrap());
    }

    #[test]
    fn an_empty_cluster_is_a_zero_and_not_a_hole() {
        let features = Matrix::new(vec!["x".to_owned()], vec![0.0, 1.0]).expect("matrix");
        let Metrics::Clustering { sizes, .. } = clustering(&[0, 2], &features) else {
            panic!("wrong set");
        };
        assert_eq!(sizes, vec![1, 0, 1]);
    }

    #[test]
    fn explained_variance_is_the_projections_share_of_the_features_variance() {
        // One feature, and a projection that is exactly it: all of the variance.
        let features = Matrix::new(vec!["x".to_owned()], vec![1.0, 2.0, 3.0, 4.0]).expect("matrix");
        let projections: Vec<Vec<f64>> = vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0]];
        let Metrics::Embedding {
            explained_variance,
            rows,
        } = embedding(&projections, &features)
        else {
            panic!("wrong set");
        };
        assert_eq!(rows, 4);
        assert!((explained_variance[0] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_provider_answering_the_wrong_shape_is_named_with_the_row() {
        let err = numbers(&[Prediction::number(1.0), Prediction::class("yes", None)])
            .expect_err("wrong shape");
        assert!(err.to_string().contains("row 2"), "{err}");
        assert!(err.to_string().contains("a class name"), "{err}");
    }

    #[test]
    fn the_primary_metric_always_points_up() {
        assert_eq!(
            Metrics::Regression {
                r2: 0.9,
                rmse: 1.0,
                mae: 1.0,
                rows: 1
            }
            .primary(),
            Some(0.9)
        );
        assert_eq!(
            Metrics::Clustering {
                sizes: vec![1],
                wcss: 4.0,
                rows: 1
            }
            .primary(),
            Some(-4.0)
        );
        assert_eq!(Metrics::None.primary(), None);
    }

    #[test]
    fn a_non_finite_metric_is_null_on_the_way_out_and_reads_back() {
        let mut metrics = SplitMetrics::default();
        metrics.set(
            Part::Test,
            Metrics::Regression {
                r2: f64::NAN,
                rmse: 0.0,
                mae: 0.0,
                rows: 2,
            },
        );
        let json = metrics.to_json().expect("json");
        assert_eq!(json["test"]["r2"], Json::Null);
        assert!(json.get("train").is_none());
        let back = SplitMetrics::from_json(&json).expect("read");
        // NaN does not survive JSON, and does not pretend to: it comes back as
        // the null the column holds.
        let Some(Metrics::Regression { r2, rows, .. }) = back.get(Part::Test) else {
            panic!("wrong set");
        };
        assert!(r2.is_nan());
        assert_eq!(*rows, 2);
        assert_eq!(
            SplitMetrics::from_json(&Json::Null).unwrap(),
            SplitMetrics::default()
        );
    }

    #[test]
    fn a_posteriors_diagnostics_sit_under_train_and_rank_nothing() {
        let posterior = Metrics::Posterior(PosteriorMetrics {
            chains: 4,
            draws_per_chain: 1000,
            divergent: 3,
            divergent_per_chain: vec![0, 3, 0, 0],
            max_treedepth_hits: 0,
            ebfmi: vec![0.9, 0.8, f64::NAN, 1.1],
            max_rhat: 1.004,
            min_ess_bulk: 812.0,
            min_ess_tail: f64::NAN,
            wall_seconds: vec![1.5, 1.6, 1.4, 1.5],
        });
        assert_eq!(posterior.primary(), None);
        assert_eq!(posterior.rows(), 0);
        let mut metrics = SplitMetrics::default();
        metrics.set(Part::Train, posterior);
        let json = metrics.to_json().expect("json");
        assert_eq!(json["train"]["metrics"], "posterior");
        assert_eq!(json["train"]["divergent"], 3);
        assert_eq!(json["train"]["min_ess_tail"], Json::Null);
        let back = SplitMetrics::from_json(&json).expect("read");
        let Some(Metrics::Posterior(read)) = back.get(Part::Train) else {
            panic!("wrong set: {back:?}");
        };
        assert_eq!(read.divergent_per_chain, vec![0, 3, 0, 0]);
        assert!(read.ebfmi[2].is_nan() && read.min_ess_tail.is_nan());
    }

    #[test]
    fn a_mode_and_an_approximation_have_metrics_of_their_own() {
        let mode = Metrics::PosteriorMode(ModeMetrics {
            log_density: -5.0,
            iterations: Some(7),
            wall_seconds: vec![0.1],
        });
        let approx = Metrics::PosteriorApproximation(ApproximationMetrics {
            draws: 1000,
            min_ess_bulk: 950.0,
            min_ess_tail: f64::NAN,
            wall_seconds: vec![0.2],
        });
        for (metrics, tag) in [
            (mode, "posterior_mode"),
            (approx, "posterior_approximation"),
        ] {
            assert!(metrics.is_posterior() && metrics.primary().is_none());
            let mut split = SplitMetrics::default();
            split.set(Part::Train, metrics.clone());
            let json = split.to_json().expect("json");
            assert_eq!(json["train"]["metrics"], tag);
            let back = SplitMetrics::from_json(&json).expect("read");
            match (back.get(Part::Train), &metrics) {
                (Some(Metrics::PosteriorApproximation(a)), Metrics::PosteriorApproximation(_)) => {
                    assert!(a.min_ess_tail.is_nan());
                }
                (Some(read), _) => assert_eq!(read, &metrics),
                (None, _) => panic!("no train metrics"),
            }
        }
    }
}

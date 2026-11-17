//! `linear_regression` — ordinary least squares, and the four numbers beside
//! each coefficient that make it readable (TODO task 4.2).
//!
//! The fit is smartcore's: the design matrix is solved by QR, which is what
//! `LinearRegression` does internally and is more stable than forming the normal
//! equations. What is *not* smartcore's is everything a regression is actually
//! read for. GOALS asks for "a regression model where we are more interested in
//! the slope coefficient values", and a slope with no standard error is a number
//! with no way to tell whether it means anything — so this provider computes,
//! for every term, its **standard error, *t* and *p***, from the residual
//! variance and `(XᵀX)⁻¹`. That is about forty lines on top of the fit, and it
//! is the difference between a coefficient table and a coefficient.
//!
//! ## Why the intercept is a setting and not an assumption
//!
//! smartcore always fits one. Here it is a checkbox, because "the line goes
//! through the origin" is a real modelling claim — a calibration curve, a
//! proportional cost — and a model that silently added a constant would be
//! answering a different question from the one asked. The design matrix is
//! therefore built here rather than by the library, with the intercept as its
//! **first** column so that `(XᵀX)⁻¹`'s diagonal lines up with the table on the
//! screen.
//!
//! It also changes what R² means, and that is stated rather than hidden: with an
//! intercept the total sum of squares is about the mean, and without one it is
//! about zero (the uncentred R², which is what every other tool reports for a
//! no-intercept fit and is not comparable with the centred one).

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;
use smartcore::linalg::basic::arrays::Array;
use smartcore::linalg::basic::matrix::DenseMatrix;
use smartcore::linalg::traits::qr::QRDecomposable;
use statrs::distribution::{ContinuousCDF, StudentsT};

use crate::frame::Frame;
use crate::provider::{
    FitResult, ModelProvider, OutcomeSpec, ParameterBlock, ParameterRow, Prediction, Uncertainty,
    numeric_column_field,
};
use crate::providers::{
    bool_setting, cell, column_setting, design, feature_names, from_state, label_values, to_state,
};

/// The configuration key naming the label.
const LABEL: &str = "label";
/// The configuration key deciding whether a constant term is fitted.
const INTERCEPT: &str = "intercept";
/// What the intercept row is called in the coefficient table.
const INTERCEPT_TERM: &str = "(intercept)";

/// A fitted OLS regression, as it is stored.
///
/// The feature names travel with the coefficients because the matrix a
/// prediction is built from is rebuilt **by name** from the stored list, never
/// by position in whatever frame arrived — which is the same rule the encoding
/// follows and for the same reason.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct State {
    /// The feature columns, in coefficient order.
    features: Vec<String>,
    /// One coefficient per feature.
    coefficients: Vec<f64>,
    /// The constant term, or `None` for a fit without one.
    intercept: Option<f64>,
    /// (XᵀX)⁻¹, row-major over the terms (the intercept first, where there is
    /// one): with `sigma2` and `df`, what a prediction interval is computed
    /// from (analytics TODO A7.2). Absent from a fit made before intervals.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    covariance: Vec<f64>,
    /// The residual variance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sigma2: Option<f64>,
    /// The residual degrees of freedom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    df: Option<f64>,
}

/// Ordinary least squares.
pub struct LinearRegression;

#[async_trait]
impl ModelProvider for LinearRegression {
    fn name(&self) -> &str {
        "linear_regression"
    }

    fn description(&self) -> &str {
        "Ordinary least squares: a coefficient, a standard error, a t and a p for every column"
    }

    fn config_declaration(&self) -> Vec<FormField> {
        vec![
            numeric_column_field(LABEL, "Label").required(),
            FormField::new(INTERCEPT, BasicType::Bool)
                .label("Fit an intercept")
                .default_value(true),
        ]
    }

    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Regression {
            label: LABEL.to_owned(),
        }
    }

    async fn fit(&self, frame: &Frame, config: &Attrs, _hyper: &Attrs) -> Result<FitResult> {
        let label = column_setting(config, LABEL)?;
        let intercept = bool_setting(config, INTERCEPT, true);
        let features = feature_names(frame, Some(&label));
        let x = design(frame, &features)?;
        let y = label_values(frame, &label)?;

        let n = x.rows();
        let terms = terms(&features, intercept);
        let k = terms.len();
        if n <= k {
            return Err(Error::invalid(format!(
                "a regression over {k} terms needs more than {k} rows and this fit has {n}: the \
                 system is underdetermined, so no coefficient has a standard error"
            )));
        }

        // The design matrix, with the intercept — where there is one — as its
        // first column, so the coefficient table, the coefficient vector and the
        // diagonal of (XᵀX)⁻¹ are all in the same order.
        let rows: Vec<Vec<f64>> = (0..n)
            .map(|i| {
                let mut row = Vec::with_capacity(k);
                if intercept {
                    row.push(1.0);
                }
                row.extend_from_slice(x.row(i).unwrap_or(&[]));
                row
            })
            .collect();
        let matrix = dense(&rows)?;
        let beta = solve(&matrix, &y, n, k)?;

        // Residuals, and the variance every standard error is scaled by.
        let fitted: Vec<f64> = rows
            .iter()
            .map(|row| row.iter().zip(&beta).map(|(x, b)| x * b).sum())
            .collect();
        let rss: f64 = y.iter().zip(&fitted).map(|(y, f)| (y - f) * (y - f)).sum();
        let df = n - k;
        let sigma2 = rss / df as f64;

        let full = gram_inverse(&rows, k)?;
        let inverse: Vec<f64> = (0..k).map(|j| full[j * k + j]).collect();
        let t_dist = StudentsT::new(0.0, 1.0, df as f64).map_err(|e| {
            Error::msg(format!(
                "the t distribution for {df} degrees of freedom could not be built: {e}"
            ))
        })?;
        let mut coefficients = Vec::with_capacity(k);
        for (j, term) in terms.iter().enumerate() {
            let se = (sigma2 * inverse[j]).sqrt();
            let t = beta[j] / se;
            // Two-sided: the question a coefficient's p answers is "could this be
            // zero", and zero is on both sides of the estimate.
            let p = if t.is_finite() {
                2.0 * (1.0 - t_dist.cdf(t.abs()))
            } else {
                f64::NAN
            };
            coefficients.push(ParameterRow::new(vec![
                Json::String(term.clone()),
                cell(beta[j]),
                cell(se),
                cell(t),
                cell(p),
            ]));
        }

        // R² about the mean where an intercept was fitted and about zero where
        // one was not — see the module docs.
        let mean = y.iter().sum::<f64>() / n as f64;
        let tss: f64 = y
            .iter()
            .map(|v| {
                if intercept {
                    (v - mean) * (v - mean)
                } else {
                    v * v
                }
            })
            .sum();
        let r2 = if tss > 0.0 { 1.0 - rss / tss } else { f64::NAN };
        let df_total = if intercept { n - 1 } else { n };
        let adjusted = 1.0 - (1.0 - r2) * (df_total as f64) / (df as f64);

        let state = State {
            features,
            coefficients: beta[usize::from(intercept)..].to_vec(),
            intercept: intercept.then(|| beta[0]),
            covariance: full,
            sigma2: Some(sigma2),
            df: Some(df as f64),
        };
        Ok(FitResult::new(to_state(&state)?)
            .parameter(ParameterBlock::table(
                "Coefficients",
                ["term", "estimate", "std. error", "t", "p"],
                coefficients,
            )?)
            .parameter(ParameterBlock::scalar("R²", r2))
            .parameter(ParameterBlock::scalar("adjusted R²", adjusted))
            .parameter(ParameterBlock::scalar(
                "residual standard error",
                sigma2.sqrt(),
            ))
            .parameter(ParameterBlock::scalar("observations", n as f64))
            .parameter(ParameterBlock::scalar(
                "residual degrees of freedom",
                df as f64,
            )))
    }

    async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>> {
        let state: State = from_state(self.name(), state)?;
        let x = design(frame, &state.features)?;
        if state.coefficients.len() != state.features.len() {
            return Err(Error::msg(format!(
                "this fit stored {} coefficients for {} features",
                state.coefficients.len(),
                state.features.len()
            )));
        }
        Ok((0..x.rows())
            .map(|i| {
                let row = x.row(i).unwrap_or(&[]);
                let value = state.intercept.unwrap_or(0.0)
                    + row
                        .iter()
                        .zip(&state.coefficients)
                        .map(|(x, b)| x * b)
                        .sum::<f64>();
                Prediction::number(value)
            })
            .collect())
    }

    fn uncertainty(&self) -> Uncertainty {
        Uncertainty::Interval
    }

    /// The prediction interval of each row: ŷ ± t · s · √(1 + x₀ᵀ(XᵀX)⁻¹x₀),
    /// for a new observation rather than the mean. A fit from before
    /// intervals answers the point prediction.
    async fn predict_uncertain(
        &self,
        state: &Json,
        frame: &Frame,
        level: f64,
    ) -> Result<Vec<Prediction>> {
        let points = self.predict(state, frame).await?;
        let state: State = from_state(self.name(), state)?;
        let (Some(sigma2), Some(df)) = (state.sigma2, state.df) else {
            return Ok(points);
        };
        let intercept = state.intercept.is_some();
        let k = state.features.len() + usize::from(intercept);
        if state.covariance.len() != k * k || df <= 0.0 {
            return Ok(points);
        }
        let x = design(frame, &state.features)?;
        let t = StudentsT::new(0.0, 1.0, df)
            .map_err(|e| {
                Error::msg(format!(
                    "the t distribution for {df} degrees of freedom: {e}"
                ))
            })?
            .inverse_cdf(0.5 + level / 2.0);
        Ok(points
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let Some(value) = p.number_value() else {
                    return p;
                };
                let mut x0 = Vec::with_capacity(k);
                if intercept {
                    x0.push(1.0);
                }
                x0.extend_from_slice(x.row(i).unwrap_or(&[]));
                let mut leverage = 0.0;
                for a in 0..k {
                    for b in 0..k {
                        leverage += x0[a] * state.covariance[a * k + b] * x0[b];
                    }
                }
                let sd = (sigma2 * (1.0 + leverage)).sqrt();
                Prediction::distribution(value, sd, value - t * sd, value + t * sd, level)
            })
            .collect())
    }
}

/// The terms of the design matrix, in its column order.
fn terms(features: &[String], intercept: bool) -> Vec<String> {
    let mut terms = Vec::with_capacity(features.len() + 1);
    if intercept {
        terms.push(INTERCEPT_TERM.to_owned());
    }
    terms.extend(features.iter().cloned());
    terms
}

/// A row-major `Vec<Vec<f64>>` as smartcore's matrix.
fn dense(rows: &Vec<Vec<f64>>) -> Result<DenseMatrix<f64>> {
    DenseMatrix::from_2d_vec(rows)
        .map_err(|e| Error::msg(format!("this fit's design matrix could not be built: {e}")))
}

/// `β̂ = argmin ‖Xβ − y‖`, by QR through smartcore.
///
/// A rank-deficient design matrix is refused **by name**, because it has a
/// cause an admin can act on: two dataset columns that are the same column, or a
/// categorical column whose one-hot spans the intercept.
fn solve(x: &DenseMatrix<f64>, y: &[f64], n: usize, k: usize) -> Result<Vec<f64>> {
    let y = DenseMatrix::new(n, 1, y.to_vec(), false)
        .map_err(|e| Error::msg(format!("the label could not be shaped for the solver: {e}")))?;
    let solved = x.clone().qr_solve_mut(y).map_err(|e| {
        Error::invalid(format!(
            "these columns cannot be fitted by least squares ({e}); the usual cause is two \
             columns that carry the same information"
        ))
    })?;
    Ok((0..k).map(|j| *solved.get((j, 0))).collect())
}

/// The diagonal of `(XᵀX)⁻¹` — the only part of it a standard error needs.
///
/// Two things about this are deliberate. It **forms the Gram matrix**, which is
/// the one place this provider does what the fit itself avoids: the coefficients
/// come from a QR solve precisely because the normal equations are
/// ill-conditioned, but the variance of an estimate has no other spelling, and
/// it is being multiplied by a residual variance that is itself an estimate. And
/// it **inverts by Gauss–Jordan here** rather than calling the library's, because
/// smartcore's LU inverse *panics* on a singular matrix — and a design matrix
/// with two columns carrying the same information is not a bug in this server,
/// it is a dataset an admin can fix, so it has to come back as a sentence rather
/// than as a dead fit job.
fn gram_inverse(rows: &[Vec<f64>], k: usize) -> Result<Vec<f64>> {
    let mut gram = vec![vec![0.0f64; k]; k];
    for row in rows {
        for i in 0..k {
            for j in 0..k {
                gram[i][j] += row[i] * row[j];
            }
        }
    }
    invert(gram, k).ok_or_else(|| {
        Error::invalid(
            "the standard errors of these coefficients cannot be computed: the columns are \
             linearly dependent, and the usual cause is two columns that carry the same \
             information",
        )
    })
}

/// A `k`×`k` matrix inverted in place by Gauss–Jordan with partial pivoting,
/// row-major, or `None` when it is singular to the working precision.
///
/// The pivot is compared against the largest entry the matrix started with, so
/// the test is on the *conditioning* and not on an absolute size: a Gram matrix
/// of columns measured in millions has a legitimate pivot of 10¹² and one of
/// columns measured in millionths has a legitimate pivot of 10⁻¹².
fn invert(mut a: Vec<Vec<f64>>, k: usize) -> Option<Vec<f64>> {
    let scale = a
        .iter()
        .flat_map(|row| row.iter())
        .fold(0.0f64, |m, v| m.max(v.abs()));
    if scale == 0.0 || !scale.is_finite() {
        return None;
    }
    let tolerance = scale * 1e-12;
    let mut inverse: Vec<Vec<f64>> = (0..k)
        .map(|i| (0..k).map(|j| f64::from(u8::from(i == j))).collect())
        .collect();
    for column in 0..k {
        let pivot = (column..k).fold(column, |best, r| {
            if a[r][column].abs() > a[best][column].abs() {
                r
            } else {
                best
            }
        });
        if a[pivot][column].abs() <= tolerance {
            return None;
        }
        a.swap(column, pivot);
        inverse.swap(column, pivot);
        let divisor = a[column][column];
        for j in 0..k {
            a[column][j] /= divisor;
            inverse[column][j] /= divisor;
        }
        for r in 0..k {
            if r == column {
                continue;
            }
            let factor = a[r][column];
            if factor == 0.0 {
                continue;
            }
            for j in 0..k {
                a[r][j] -= factor * a[column][j];
                inverse[r][j] -= factor * inverse[column][j];
            }
        }
    }
    Some(inverse.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::testing::{attrs, close, floats, frame, number, scalar, table};

    /// The reference fit. Ten rows, two correlated columns, and a label that is
    /// almost but not exactly a linear combination of them — so the residual
    /// variance is small and non-zero, which is what makes the standard errors
    /// worth asserting.
    fn data() -> crate::frame::Frame {
        frame(vec![
            ("x1", floats(&[1., 2., 3., 4., 5., 6., 7., 8., 9., 10.])),
            ("x2", floats(&[2., 1., 4., 3., 6., 5., 8., 7., 10., 9.])),
            (
                "y",
                floats(&[3.1, 4.2, 6.0, 7.1, 9.3, 10.1, 12.4, 13.2, 15.5, 16.3]),
            ),
        ])
    }

    /// Every number in this test is `statsmodels.api.OLS(y, add_constant([x1,
    /// x2])).fit()` on the same ten rows, pasted here as a constant. Asserting
    /// against another implementation rather than against ourselves is the whole
    /// point: an OLS that agrees with itself proves nothing, and the standard
    /// errors and *p*s are code this crate wrote rather than code it called.
    #[tokio::test]
    async fn the_coefficients_standard_errors_and_ps_agree_with_statsmodels() {
        let fit = LinearRegression
            .fit(&data(), &attrs(&[("label", "y".into())]), &Attrs::new())
            .await
            .unwrap();

        let (columns, rows) = table(&fit.parameters, "Coefficients");
        assert_eq!(columns, ["term", "estimate", "std. error", "t", "p"]);
        assert_eq!(rows.len(), 3);
        // (intercept), x1, x2 — in the design matrix's own order.
        let expected = [
            (
                "(intercept)",
                1.26375,
                0.07730823048033121,
                16.346901127448824,
                7.812316981147812e-07,
            ),
            (
                "x1",
                1.2287500000000002,
                0.035903516540862726,
                34.22366716089016,
                4.714524576735755e-09,
            ),
            (
                "x2",
                0.30874999999999986,
                0.03590351654086273,
                8.599436204211456,
                5.7302408033063014e-05,
            ),
        ];
        for (row, (term, estimate, se, t, p)) in rows.iter().zip(expected) {
            assert_eq!(row[0], Json::String(term.to_owned()));
            close(number(&row[1]), estimate, 1e-9);
            close(number(&row[2]), se, 1e-9);
            close(number(&row[3]), t, 1e-7);
            close(number(&row[4]), p, 1e-12);
        }

        close(scalar(&fit.parameters, "R²"), 0.9995426414936545, 1e-12);
        close(
            scalar(&fit.parameters, "adjusted R²"),
            0.9994119676346986,
            1e-12,
        );
        close(
            scalar(&fit.parameters, "residual standard error"),
            0.1118033988749896,
            1e-12,
        );
        close(scalar(&fit.parameters, "observations"), 10.0, 0.0);
        close(
            scalar(&fit.parameters, "residual degrees of freedom"),
            7.0,
            0.0,
        );
    }

    /// Dropping the intercept is a different model and not a cosmetic switch:
    /// statsmodels fitted without the constant column gives different
    /// coefficients, different standard errors and the *uncentred* R², and this
    /// asserts all three.
    #[tokio::test]
    async fn without_an_intercept_it_is_a_different_fit_and_says_so() {
        let fit = LinearRegression
            .fit(
                &data(),
                &attrs(&[("label", "y".into()), ("intercept", false.into())]),
                &Attrs::new(),
            )
            .await
            .unwrap();
        let (_, rows) = table(&fit.parameters, "Coefficients");
        assert_eq!(rows.len(), 2, "no intercept row");
        assert_eq!(rows[0][0], Json::String("x1".to_owned()));
        close(number(&rows[0][1]), 1.3196078431372544, 1e-9);
        close(number(&rows[1][1]), 0.3996078431372556, 1e-9);
        close(number(&rows[0][2]), 0.20767066465832598, 1e-9);
        close(scalar(&fit.parameters, "R²"), 0.996982867083987, 1e-12);
    }

    /// A prediction is the stored coefficients applied to the stored feature
    /// names — so the fitted rows come back as their fitted values, and a frame
    /// whose columns arrive in another order still gets the right answer.
    #[tokio::test]
    async fn a_prediction_is_the_fit_applied_by_name() {
        let config = attrs(&[("label", "y".into())]);
        let fit = LinearRegression
            .fit(&data(), &config, &Attrs::new())
            .await
            .unwrap();
        // The same rows, with the columns swapped and the label gone.
        let reversed = frame(vec![("x2", floats(&[2., 1.])), ("x1", floats(&[1., 2.]))]);
        let predictions = LinearRegression
            .predict(&fit.state, &reversed)
            .await
            .unwrap();
        assert_eq!(predictions.len(), 2);
        let Prediction::Number { value } = predictions[0] else {
            panic!("a regression answers a number, not {:?}", predictions[0]);
        };
        close(
            value,
            1.26375 + 1.2287500000000002 + 2.0 * 0.30874999999999986,
            1e-9,
        );
    }

    /// The 90 % prediction interval (analytics TODO A7.2) against R's
    /// `predict(lm(y ~ x1 + x2), newdata, interval = "prediction", level =
    /// 0.9)` on the same ten rows, one row inside the data and one outside
    /// it: the second is wider, as an extrapolation should be.
    #[tokio::test]
    async fn a_prediction_interval_agrees_with_r() {
        let config = attrs(&[("label", "y".into())]);
        let fit = LinearRegression
            .fit(&data(), &config, &Attrs::new())
            .await
            .unwrap();
        assert_eq!(LinearRegression.uncertainty(), Uncertainty::Interval);
        let rows = frame(vec![
            ("x1", floats(&[2.5, 12.])),
            ("x2", floats(&[1., 11.])),
        ]);
        let predicted = LinearRegression
            .predict_uncertain(&fit.state, &rows, 0.9)
            .await
            .unwrap();
        let expected = [
            (4.644375, 4.384_881_590_233_173, 4.903_868_409_766_828),
            (19.405, 19.132_911_862_856_744, 19.677_088_137_143_25),
        ];
        for (p, (fit, lower, upper)) in predicted.iter().zip(expected) {
            let Prediction::Distribution {
                mean,
                sd,
                lower: lo,
                upper: up,
                level,
            } = p
            else {
                panic!("an interval, not {p:?}");
            };
            close(*mean, fit, 1e-9);
            close(*lo, lower, 1e-9);
            close(*up, upper, 1e-9);
            assert!((*level - 0.9).abs() < 1e-12);
            assert!(
                *sd > 0.1118,
                "the residual standard error is the least it can be"
            );
        }
        // A fit stored before intervals answers the point prediction.
        let mut old = fit.state.clone();
        for key in ["covariance", "sigma2", "df"] {
            old.as_object_mut().unwrap().remove(key);
        }
        let point = LinearRegression
            .predict_uncertain(&old, &rows, 0.9)
            .await
            .unwrap();
        assert!(matches!(point[0], Prediction::Number { .. }), "{point:?}");
    }

    /// More terms than rows has no least-squares answer and — more to the point
    /// — no residual degrees of freedom, so every standard error would be a
    /// division by zero. Refused by name, before the solver.
    #[tokio::test]
    async fn a_fit_with_no_residual_degrees_of_freedom_is_refused_by_name() {
        let short = frame(vec![
            ("x1", floats(&[1., 2.])),
            ("x2", floats(&[2., 1.])),
            ("y", floats(&[1., 2.])),
        ]);
        let err = LinearRegression
            .fit(&short, &attrs(&[("label", "y".into())]), &Attrs::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("underdetermined") && err.contains("standard error"),
            "{err}"
        );
    }

    /// Two columns carrying the same information make `(XᵀX)` singular. The
    /// message says what to do about it, because "matrix is rank deficient" does
    /// not.
    #[tokio::test]
    async fn duplicate_columns_are_refused_with_their_cause() {
        let duplicated = frame(vec![
            ("x1", floats(&[1., 2., 3., 4., 5.])),
            ("x1_again", floats(&[1., 2., 3., 4., 5.])),
            ("y", floats(&[2., 4., 6., 8., 10.])),
        ]);
        let err = LinearRegression
            .fit(&duplicated, &attrs(&[("label", "y".into())]), &Attrs::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("same information"), "{err}");
    }
}

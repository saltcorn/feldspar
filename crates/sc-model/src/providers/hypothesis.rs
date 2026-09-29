//! `t_test` and `anova` — the two providers that are **not** machine learning
//! (TODO tasks 4.7, 4.1).
//!
//! They are here in every build, including one made with
//! `--no-default-features`, and the reason is §13's: a hypothesis test needs a
//! distribution function and nothing else, and a server with the machine
//! learning compiled out should still be able to answer whether two groups
//! differ. They are also GOALS' fourth category of model — "statistical
//! hypothesis testing (t-test, anova etc; the main outcome is the model
//! parameters including the test statistic score)" — and that sentence is the
//! whole specification: the parameters *are* the result.
//!
//! ## Which is why the outcome is `Test`
//!
//! [`Outcome::Test`](crate::Outcome::Test) has no per-row output, and everything
//! downstream reads that: the fit takes no split (there is nothing to hold out
//! from a test statistic), fits no encoding (the configuration names *this*
//! column as the value and *that* one as the group, and a one-hot would leave
//! neither addressable), computes no metrics, and `predict()` will not offer
//! these providers at all. [`ModelProvider::predict`] here is therefore an
//! error and not an empty vector: a caller that reached it has a bug, and a
//! silent empty answer would hide it.
//!
//! ## Rows are dropped, and the count is on the screen
//!
//! The frame is the dataset's own, unencoded, so a null is possible in any
//! column a test reads. A row that is null in one of the columns *this test
//! uses* is excluded — which is what every statistics package does — and the
//! group table reports the *n* each mean was computed from, so a test over 180
//! of 200 rows never looks like a test over 200.

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;
use statrs::distribution::{ContinuousCDF, FisherSnedecor, StudentsT};

use crate::dataset::DatasetShape;
use crate::encode::{category_at, number_at};
use crate::frame::Frame;
use crate::provider::{
    FitResult, ModelProvider, OutcomeSpec, ParameterBlock, ParameterRow, Prediction, column_field,
    numeric_column_field,
};
use crate::providers::{cell, column_setting, number_setting, optional_column_setting};

/// The configuration key naming the column being tested.
const VALUE: &str = "value";
/// The configuration key naming the column the rows are grouped by.
const GROUP: &str = "group";
/// The configuration key naming the second column of a paired test.
const AGAINST: &str = "against";
/// The configuration key holding the constant a one-sample test is against.
const MU: &str = "mu";
/// The configuration key choosing which t-test this is.
const TEST: &str = "test";

/// The confidence level every interval here is reported at.
const CONFIDENCE: f64 = 0.95;

/// Which of the four t-tests a configuration asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// One sample against a constant.
    OneSample,
    /// Two independent samples with a pooled variance.
    TwoSample,
    /// Two independent samples without one — Welch's.
    Welch,
    /// Two measurements of the same rows.
    Paired,
}

impl Kind {
    /// The stable configuration value.
    fn key(self) -> &'static str {
        match self {
            Kind::OneSample => "one_sample",
            Kind::TwoSample => "two_sample",
            Kind::Welch => "welch",
            Kind::Paired => "paired",
        }
    }

    /// The kind a configuration names.
    fn parse(value: &str) -> Result<Kind> {
        [Kind::OneSample, Kind::TwoSample, Kind::Welch, Kind::Paired]
            .into_iter()
            .find(|k| k.key() == value)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "`{TEST}`: `{value}` is not a t-test; the tests are one_sample, two_sample, \
                     welch and paired"
                ))
            })
    }
}

/// Student's t-test, in its four shapes.
pub struct TTest;

#[async_trait]
impl ModelProvider for TTest {
    fn name(&self) -> &str {
        "t_test"
    }

    fn description(&self) -> &str {
        "Student's t-test: one sample against a constant, two samples, Welch's, or paired"
    }

    fn config_declaration(&self) -> Vec<FormField> {
        vec![
            FormField::new(TEST, BasicType::Text)
                .label("Test")
                .required()
                .default_value(Kind::TwoSample.key())
                .options([
                    Kind::OneSample.key(),
                    Kind::TwoSample.key(),
                    Kind::Welch.key(),
                    Kind::Paired.key(),
                ]),
            numeric_column_field(VALUE, "Value").required(),
            column_field(GROUP, "Group (two samples)"),
            numeric_column_field(AGAINST, "Second measurement (paired)"),
            FormField::new(MU, BasicType::Float)
                .label("Constant (one sample)")
                .default_value(0.0),
        ]
    }

    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Test
    }

    /// The half of the form that only this provider knows: which of the other
    /// settings the chosen test needs.
    ///
    /// A form cannot say "required, but only when `test` is `paired`", so the
    /// check is here — and it runs on save, in front of the admin, rather than
    /// at fit time in front of a failed instance.
    fn validate(&self, _shape: &DatasetShape, config: &Attrs) -> Result<()> {
        let kind = Kind::parse(&column_setting(config, TEST)?)?;
        match kind {
            Kind::TwoSample | Kind::Welch if optional_column_setting(config, GROUP).is_none() => {
                Err(Error::invalid(format!(
                    "`{GROUP}`: a {} t-test compares two groups, so it needs the column that \
                     says which group a row is in",
                    kind.key()
                )))
            }
            Kind::Paired if optional_column_setting(config, AGAINST).is_none() => {
                Err(Error::invalid(format!(
                    "`{AGAINST}`: a paired t-test compares two measurements of the same rows, so \
                     it needs the second one"
                )))
            }
            _ => Ok(()),
        }
    }

    async fn fit(&self, frame: &Frame, config: &Attrs, _hyper: &Attrs) -> Result<FitResult> {
        let kind = Kind::parse(&column_setting(config, TEST)?)?;
        let value = column_setting(config, VALUE)?;
        match kind {
            Kind::OneSample => {
                let sample = numbers(frame, &value)?;
                let mu = number_setting(config, MU, 0.0)?;
                one_sample(&value, &sample, mu)
            }
            Kind::Paired => {
                let against = column_setting(config, AGAINST)?;
                let (left, right) = pairs(frame, &value, &against)?;
                let differences: Vec<f64> = left.iter().zip(&right).map(|(a, b)| a - b).collect();
                let mut result = one_sample(&format!("{value} − {against}"), &differences, 0.0)?;
                result.parameters.push(ParameterBlock::table(
                    "Measurements",
                    ["measurement", "n", "mean", "sd"],
                    vec![summary(&value, &left), summary(&against, &right)],
                )?);
                Ok(result)
            }
            Kind::TwoSample | Kind::Welch => {
                let group = column_setting(config, GROUP)?;
                let groups = grouped(frame, &value, &group)?;
                if groups.len() != 2 {
                    return Err(Error::invalid(format!(
                        "`{GROUP}`: a t-test compares two groups and `{group}` has {} ({})",
                        groups.len(),
                        names(&groups)
                    )));
                }
                two_sample(&groups, kind == Kind::Welch)
            }
        }
    }

    async fn predict(&self, _state: &Json, _frame: &Frame) -> Result<Vec<Prediction>> {
        Err(nothing_to_predict("a t-test"))
    }
}

/// One-way analysis of variance.
pub struct Anova;

#[async_trait]
impl ModelProvider for Anova {
    fn name(&self) -> &str {
        "anova"
    }

    fn description(&self) -> &str {
        "One-way ANOVA: whether the means of three or more groups differ by more than chance"
    }

    fn config_declaration(&self) -> Vec<FormField> {
        vec![
            numeric_column_field(VALUE, "Value").required(),
            column_field(GROUP, "Group").required(),
        ]
    }

    fn outcome_spec(&self) -> OutcomeSpec {
        OutcomeSpec::Test
    }

    async fn fit(&self, frame: &Frame, config: &Attrs, _hyper: &Attrs) -> Result<FitResult> {
        let value = column_setting(config, VALUE)?;
        let group = column_setting(config, GROUP)?;
        let groups = grouped(frame, &value, &group)?;
        if groups.len() < 2 {
            return Err(Error::invalid(format!(
                "`{GROUP}`: an analysis of variance needs at least two groups and `{group}` has \
                 {}",
                groups.len()
            )));
        }
        let n: usize = groups.iter().map(|(_, v)| v.len()).sum();
        let k = groups.len();
        if n <= k {
            return Err(Error::invalid(format!(
                "an analysis of variance over {k} groups needs more than {k} rows and this \
                 dataset has {n}"
            )));
        }
        let grand = groups.iter().flat_map(|(_, v)| v.iter()).sum::<f64>() / n as f64;
        let between: f64 = groups
            .iter()
            .map(|(_, v)| {
                let m = mean(v);
                v.len() as f64 * (m - grand) * (m - grand)
            })
            .sum();
        let within: f64 = groups
            .iter()
            .flat_map(|(_, v)| {
                let m = mean(v);
                v.iter().map(move |x| (x - m) * (x - m))
            })
            .sum();
        let df_between = (k - 1) as f64;
        let df_within = (n - k) as f64;
        let ms_between = between / df_between;
        let ms_within = within / df_within;
        let f = ms_between / ms_within;
        // One-sided by construction: F is a ratio of variances, and only a large
        // one is evidence against "the group means are the same".
        let p = if f.is_finite() && f >= 0.0 && ms_within > 0.0 {
            let dist = FisherSnedecor::new(df_between, df_within).map_err(|e| {
                Error::msg(format!(
                    "the F distribution for {df_between} and {df_within} degrees of freedom \
                     could not be built: {e}"
                ))
            })?;
            1.0 - dist.cdf(f)
        } else {
            f64::NAN
        };

        Ok(FitResult::new(Json::Null)
            .parameter(ParameterBlock::table(
                "Analysis of variance",
                ["source", "sum of squares", "df", "mean square", "F", "p"],
                vec![
                    ParameterRow::new(vec![
                        Json::String("between groups".to_owned()),
                        cell(between),
                        cell(df_between),
                        cell(ms_between),
                        cell(f),
                        cell(p),
                    ]),
                    ParameterRow::new(vec![
                        Json::String("within groups".to_owned()),
                        cell(within),
                        cell(df_within),
                        cell(ms_within),
                        Json::Null,
                        Json::Null,
                    ]),
                    ParameterRow::new(vec![
                        Json::String("total".to_owned()),
                        cell(between + within),
                        cell(df_between + df_within),
                        Json::Null,
                        Json::Null,
                        Json::Null,
                    ]),
                ],
            )?)
            .parameter(group_table(&groups)?)
            .parameter(ParameterBlock::scalar("F", f))
            .parameter(ParameterBlock::scalar("p-value", p))
            .parameter(ParameterBlock::scalar("observations", n as f64)))
    }

    async fn predict(&self, _state: &Json, _frame: &Frame) -> Result<Vec<Prediction>> {
        Err(nothing_to_predict("an analysis of variance"))
    }
}

/// Why a hypothesis test has no prediction to give.
fn nothing_to_predict(what: &str) -> Error {
    Error::msg(format!(
        "{what} produces no per-row prediction: its parameters are the result, and nothing \
         should have asked it to predict"
    ))
}

/// A one-sample t-test of `sample` against `mu`, as a fit.
///
/// The paired test is this one over the differences, which is what a paired test
/// *is* — so it is one implementation and not two.
fn one_sample(label: &str, sample: &[f64], mu: f64) -> Result<FitResult> {
    let n = sample.len();
    if n < 2 {
        return Err(Error::invalid(format!(
            "a t-test needs at least two rows and `{label}` has {n}"
        )));
    }
    let m = mean(sample);
    let sd = sd(sample, m);
    let se = sd / (n as f64).sqrt();
    let df = (n - 1) as f64;
    let t = (m - mu) / se;
    let (p, low, high) = student(t, df, m - mu, se)?;
    Ok(FitResult::new(Json::Null)
        .parameter(ParameterBlock::table(
            "Sample",
            ["sample", "n", "mean", "sd"],
            vec![summary(label, sample)],
        )?)
        .parameter(ParameterBlock::scalar("t", t))
        .parameter(ParameterBlock::scalar("degrees of freedom", df))
        .parameter(ParameterBlock::scalar("p-value", p))
        .parameter(ParameterBlock::scalar("estimate", m - mu))
        .parameter(ParameterBlock::scalar("standard error", se))
        .parameter(ParameterBlock::scalar("95% CI lower", low))
        .parameter(ParameterBlock::scalar("95% CI upper", high)))
}

/// A two-sample t-test, pooled or Welch's.
///
/// The two differ in exactly two lines — the standard error and the degrees of
/// freedom — which is why they are one function and two options on a form rather
/// than two providers.
fn two_sample(groups: &[(String, Vec<f64>)], welch: bool) -> Result<FitResult> {
    let (name_a, a) = &groups[0];
    let (name_b, b) = &groups[1];
    let (na, nb) = (a.len(), b.len());
    if na < 2 || nb < 2 {
        return Err(Error::invalid(format!(
            "a two-sample t-test needs at least two rows in each group, and `{name_a}` has \
             {na} and `{name_b}` has {nb}"
        )));
    }
    let (ma, mb) = (mean(a), mean(b));
    let (va, vb) = (sd(a, ma).powi(2), sd(b, mb).powi(2));
    let (na_f, nb_f) = (na as f64, nb as f64);
    let (se, df) = if welch {
        let se2 = va / na_f + vb / nb_f;
        // Welch–Satterthwaite: the effective degrees of freedom of a difference
        // of two means whose variances are not assumed equal.
        let df =
            se2 * se2 / ((va / na_f).powi(2) / (na_f - 1.0) + (vb / nb_f).powi(2) / (nb_f - 1.0));
        (se2.sqrt(), df)
    } else {
        let pooled = ((na_f - 1.0) * va + (nb_f - 1.0) * vb) / (na_f + nb_f - 2.0);
        (
            (pooled * (1.0 / na_f + 1.0 / nb_f)).sqrt(),
            na_f + nb_f - 2.0,
        )
    };
    let difference = ma - mb;
    let t = difference / se;
    let (p, low, high) = student(t, df, difference, se)?;
    Ok(FitResult::new(Json::Null)
        .parameter(group_table(groups)?)
        .parameter(ParameterBlock::scalar("t", t))
        .parameter(ParameterBlock::scalar("degrees of freedom", df))
        .parameter(ParameterBlock::scalar("p-value", p))
        .parameter(ParameterBlock::scalar("estimate", difference))
        .parameter(ParameterBlock::scalar("standard error", se))
        .parameter(ParameterBlock::scalar("95% CI lower", low))
        .parameter(ParameterBlock::scalar("95% CI upper", high)))
}

/// The two-sided *p* of a *t*, and the confidence interval of the estimate it
/// came from.
fn student(t: f64, df: f64, estimate: f64, se: f64) -> Result<(f64, f64, f64)> {
    if !t.is_finite() || !df.is_finite() || df <= 0.0 {
        return Ok((f64::NAN, f64::NAN, f64::NAN));
    }
    let dist = StudentsT::new(0.0, 1.0, df).map_err(|e| {
        Error::msg(format!(
            "the t distribution for {df} degrees of freedom could not be built: {e}"
        ))
    })?;
    let p = 2.0 * (1.0 - dist.cdf(t.abs()));
    let critical = dist.inverse_cdf(1.0 - (1.0 - CONFIDENCE) / 2.0);
    Ok((p, estimate - critical * se, estimate + critical * se))
}

/// One row of a group table: its name, its *n*, its mean and its sd.
fn summary(name: &str, values: &[f64]) -> ParameterRow {
    let m = mean(values);
    ParameterRow::new(vec![
        Json::String(name.to_owned()),
        Json::from(values.len()),
        cell(m),
        cell(sd(values, m)),
    ])
}

/// Every group's *n*, mean and sd, as the screen shows them.
fn group_table(groups: &[(String, Vec<f64>)]) -> Result<ParameterBlock> {
    ParameterBlock::table(
        "Groups",
        ["group", "n", "mean", "sd"],
        groups
            .iter()
            .map(|(name, values)| summary(name, values))
            .collect(),
    )
}

/// The mean of a sample.
fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// The sample standard deviation — *n* − 1 in the denominator, because these are
/// samples and not populations.
fn sd(values: &[f64], mean: f64) -> f64 {
    if values.len() < 2 {
        return f64::NAN;
    }
    (values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / (values.len() - 1) as f64).sqrt()
}

/// The non-null numbers of one dataset column.
fn numbers(frame: &Frame, name: &str) -> Result<Vec<f64>> {
    let column = require(frame, name)?;
    Ok((0..frame.rows)
        .filter_map(|i| number_at(column, i))
        .collect())
}

/// The rows where **both** columns are present, as two aligned samples.
///
/// Aligned is the whole point of a paired test: dropping a null from one column
/// without dropping its partner would pair each row with a different one.
fn pairs(frame: &Frame, left: &str, right: &str) -> Result<(Vec<f64>, Vec<f64>)> {
    let a = require(frame, left)?;
    let b = require(frame, right)?;
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for i in 0..frame.rows {
        if let (Some(x), Some(y)) = (number_at(a, i), number_at(b, i)) {
            xs.push(x);
            ys.push(y);
        }
    }
    Ok((xs, ys))
}

/// The value column split by the group column, in the group names' sorted order.
///
/// Sorted so that "group A minus group B" means the same thing on two runs — the
/// sign of a *t* is half of what it says, and a difference whose direction
/// depended on row order would be unreadable.
fn grouped(frame: &Frame, value: &str, group: &str) -> Result<Vec<(String, Vec<f64>)>> {
    let values = require(frame, value)?;
    let groups = require(frame, group)?;
    let mut out: std::collections::BTreeMap<String, Vec<f64>> = std::collections::BTreeMap::new();
    for i in 0..frame.rows {
        if let (Some(v), Some(g)) = (number_at(values, i), category_at(groups, i)) {
            out.entry(g).or_default().push(v);
        }
    }
    Ok(out.into_iter().collect())
}

/// The groups a column turned out to have, for the message that says there were
/// not two of them.
fn names(groups: &[(String, Vec<f64>)]) -> String {
    if groups.is_empty() {
        return "none".to_owned();
    }
    groups
        .iter()
        .map(|(name, _)| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The frame's column under `name`, or an error naming what it does have.
fn require<'a>(frame: &'a Frame, name: &str) -> Result<&'a crate::frame::Column> {
    frame.column(name).ok_or_else(|| {
        Error::invalid(format!(
            "`{name}` is not a column of this dataset (it has {})",
            if frame.columns.is_empty() {
                "none".to_owned()
            } else {
                frame
                    .names()
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Column;
    use crate::provider::Outcome;
    use crate::providers::testing::{attrs, close, floats, frame, number, scalar, strings, table};

    /// Every reference number below is `scipy.stats`' answer for the same rows,
    /// pasted here as a constant: `ttest_1samp`, `ttest_ind` (pooled and
    /// Welch's), `ttest_rel` and `f_oneway`. A test statistic that agreed only
    /// with itself would prove nothing, and these are textbook quantities with
    /// one right answer.
    #[tokio::test]
    async fn a_one_sample_t_agrees_with_scipy() {
        let data = frame(vec![(
            "height",
            floats(&[5.1, 4.9, 6.2, 5.8, 5.5, 6.0, 5.3, 5.9]),
        )]);
        let config = attrs(&[
            ("test", "one_sample".into()),
            ("value", "height".into()),
            ("mu", 5.0.into()),
        ]);
        let fit = TTest.fit(&data, &config, &Attrs::new()).await.unwrap();

        close(scalar(&fit.parameters, "t"), 3.6032218067652964, 1e-12);
        close(scalar(&fit.parameters, "degrees of freedom"), 7.0, 0.0);
        close(
            scalar(&fit.parameters, "p-value"),
            0.00870237937625855,
            1e-12,
        );
        // scipy's interval is around the mean; ours is around the estimate,
        // which is the mean less the constant it was tested against.
        close(
            scalar(&fit.parameters, "95% CI lower"),
            0.201951504150422,
            1e-9,
        );
        close(
            scalar(&fit.parameters, "95% CI upper"),
            0.973048495849579,
            1e-9,
        );

        let (columns, rows) = table(&fit.parameters, "Sample");
        assert_eq!(columns, ["sample", "n", "mean", "sd"]);
        assert_eq!(rows[0][1], Json::from(8));
        close(number(&rows[0][2]), 5.5875, 1e-12);
        close(number(&rows[0][3]), 0.4611708700997619, 1e-12);
    }

    #[tokio::test]
    async fn a_pooled_two_sample_t_agrees_with_scipy() {
        let data = frame(vec![
            (
                "score",
                floats(&[12., 14., 11., 15., 13., 15., 17., 14., 18., 16.]),
            ),
            (
                "arm",
                strings(&["a", "a", "a", "a", "a", "b", "b", "b", "b", "b"]),
            ),
        ]);
        let config = attrs(&[
            ("test", "two_sample".into()),
            ("value", "score".into()),
            ("group", "arm".into()),
        ]);
        let fit = TTest.fit(&data, &config, &Attrs::new()).await.unwrap();

        close(scalar(&fit.parameters, "t"), -3.0, 1e-12);
        close(scalar(&fit.parameters, "degrees of freedom"), 8.0, 0.0);
        close(
            scalar(&fit.parameters, "p-value"),
            0.01707168123378265,
            1e-12,
        );
        close(
            scalar(&fit.parameters, "95% CI lower"),
            -5.306004135204166,
            1e-9,
        );
        close(
            scalar(&fit.parameters, "95% CI upper"),
            -0.6939958647958342,
            1e-9,
        );
        // The groups are in sorted order, so "a minus b" means the same thing on
        // every run and the sign of the t is readable.
        let (_, rows) = table(&fit.parameters, "Groups");
        assert_eq!(rows[0][0], Json::String("a".to_owned()));
        assert_eq!(rows[1][0], Json::String("b".to_owned()));
        close(number(&rows[0][2]), 13.0, 1e-12);
        close(number(&rows[1][2]), 16.0, 1e-12);
    }

    #[tokio::test]
    async fn welchs_t_agrees_with_scipy_where_the_spreads_differ() {
        let data = frame(vec![
            (
                "score",
                floats(&[12., 14., 11., 15., 13., 20., 25., 18., 30., 22.]),
            ),
            (
                "arm",
                strings(&["a", "a", "a", "a", "a", "b", "b", "b", "b", "b"]),
            ),
        ]);
        let config = attrs(&[
            ("test", "welch".into()),
            ("value", "score".into()),
            ("group", "arm".into()),
        ]);
        let fit = TTest.fit(&data, &config, &Attrs::new()).await.unwrap();
        close(scalar(&fit.parameters, "t"), -4.5175395145262565, 1e-12);
        // The Welch–Satterthwaite degrees of freedom are not a whole number, and
        // that is the point of the test.
        close(
            scalar(&fit.parameters, "degrees of freedom"),
            4.897501274859766,
            1e-12,
        );
        close(
            scalar(&fit.parameters, "p-value"),
            0.00661980904020938,
            1e-12,
        );
    }

    #[tokio::test]
    async fn a_paired_t_agrees_with_scipy() {
        let data = frame(vec![
            ("before", floats(&[10., 12., 9., 11., 13.])),
            ("after", floats(&[12., 15., 10., 14., 15.])),
        ]);
        let config = attrs(&[
            ("test", "paired".into()),
            ("value", "before".into()),
            ("against", "after".into()),
        ]);
        let fit = TTest.fit(&data, &config, &Attrs::new()).await.unwrap();
        close(scalar(&fit.parameters, "t"), -5.879747322073337, 1e-12);
        close(scalar(&fit.parameters, "degrees of freedom"), 4.0, 0.0);
        close(
            scalar(&fit.parameters, "p-value"),
            0.0041810721356402986,
            1e-12,
        );
        close(scalar(&fit.parameters, "estimate"), -2.2, 1e-12);
        // Both measurements are reported beside the difference, because a mean
        // difference of −2.2 says nothing about where it started.
        let (_, rows) = table(&fit.parameters, "Measurements");
        assert_eq!(rows[0][0], Json::String("before".to_owned()));
        close(number(&rows[0][2]), 11.0, 1e-12);
        close(number(&rows[1][2]), 13.2, 1e-12);
    }

    /// The textbook one-way ANOVA: three groups of six, `scipy.stats.f_oneway`
    /// for the *F* and the *p*, and the sums of squares checked by hand
    /// (84 between, 68 within).
    #[tokio::test]
    async fn a_one_way_anova_agrees_with_scipy() {
        let values: Vec<f64> = vec![
            6., 8., 4., 5., 3., 4., //
            8., 12., 9., 11., 6., 8., //
            13., 9., 11., 8., 7., 12.,
        ];
        let groups: Vec<&str> = ["one"; 6]
            .into_iter()
            .chain(["two"; 6])
            .chain(["three"; 6])
            .collect();
        let data = frame(vec![("yield", floats(&values)), ("plot", strings(&groups))]);
        let config = attrs(&[("value", "yield".into()), ("group", "plot".into())]);
        let fit = Anova.fit(&data, &config, &Attrs::new()).await.unwrap();

        close(scalar(&fit.parameters, "F"), 9.264705882352942, 1e-12);
        close(
            scalar(&fit.parameters, "p-value"),
            0.0023987773293929083,
            1e-12,
        );
        close(scalar(&fit.parameters, "observations"), 18.0, 0.0);

        let (columns, rows) = table(&fit.parameters, "Analysis of variance");
        assert_eq!(
            columns,
            ["source", "sum of squares", "df", "mean square", "F", "p"]
        );
        close(number(&rows[0][1]), 84.0, 1e-9);
        close(number(&rows[0][2]), 2.0, 0.0);
        close(number(&rows[1][1]), 68.0, 1e-9);
        close(number(&rows[1][2]), 15.0, 0.0);
        // The total row is the two of them, which is the identity the table is
        // read to check.
        close(number(&rows[2][1]), 152.0, 1e-9);
        close(number(&rows[2][2]), 17.0, 0.0);

        // Three groups, in sorted name order.
        let (_, rows) = table(&fit.parameters, "Groups");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Json::String("one".to_owned()));
        assert_eq!(rows[1][0], Json::String("three".to_owned()));
        assert_eq!(rows[2][0], Json::String("two".to_owned()));
    }

    /// A row that is null in either column of a paired test takes its partner
    /// with it: pairing a value with a different row's would be arithmetic on
    /// numbers that never went together.
    #[tokio::test]
    async fn a_paired_test_drops_a_row_whole() {
        let data = frame(vec![
            (
                "before",
                Column::Float(vec![Some(10.), Some(12.), None, Some(11.), Some(13.)]),
            ),
            (
                "after",
                Column::Float(vec![Some(12.), Some(15.), Some(10.), None, Some(15.)]),
            ),
        ]);
        let config = attrs(&[
            ("test", "paired".into()),
            ("value", "before".into()),
            ("against", "after".into()),
        ]);
        let fit = TTest.fit(&data, &config, &Attrs::new()).await.unwrap();
        let (_, rows) = table(&fit.parameters, "Sample");
        assert_eq!(rows[0][1], Json::from(3), "two of the five rows are broken");
    }

    /// The settings a test needs depend on which test it is, which no form can
    /// say — so the provider says it, on save.
    #[test]
    fn each_test_asks_for_the_settings_it_needs() {
        let shape = DatasetShape {
            table: "t".to_owned(),
            columns: Vec::new(),
        };
        let err = TTest
            .validate(
                &shape,
                &attrs(&[("test", "two_sample".into()), ("value", "v".into())]),
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`group`") && err.contains("two groups"),
            "{err}"
        );

        let err = TTest
            .validate(
                &shape,
                &attrs(&[("test", "paired".into()), ("value", "v".into())]),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("`against`"), "{err}");

        // One sample needs neither.
        assert!(
            TTest
                .validate(
                    &shape,
                    &attrs(&[("test", "one_sample".into()), ("value", "v".into())])
                )
                .is_ok()
        );

        let err = TTest
            .validate(&shape, &attrs(&[("test", "chi_squared".into())]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("chi_squared") && err.contains("welch"),
            "{err}"
        );
    }

    /// A t-test over three groups is not a t-test, and the message names the
    /// groups it found rather than saying "invalid".
    #[tokio::test]
    async fn a_two_sample_t_over_three_groups_is_refused_naming_them() {
        let data = frame(vec![
            ("v", floats(&[1., 2., 3., 4., 5., 6.])),
            ("g", strings(&["a", "a", "b", "b", "c", "c"])),
        ]);
        let config = attrs(&[
            ("test", "two_sample".into()),
            ("value", "v".into()),
            ("group", "g".into()),
        ]);
        let err = TTest
            .fit(&data, &config, &Attrs::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("has 3") && err.contains("`c`"), "{err}");
    }

    /// Nothing should ask a hypothesis test to predict, and if something does it
    /// hears about it rather than getting an empty answer back.
    #[tokio::test]
    async fn a_hypothesis_test_refuses_to_predict() {
        assert_eq!(TTest.outcome_spec(), OutcomeSpec::Test);
        assert!(!Outcome::Test.predicts());
        for provider in [&TTest as &dyn ModelProvider, &Anova] {
            let err = provider
                .predict(&Json::Null, &frame(vec![]))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("no per-row prediction"), "{err}");
        }
    }
}

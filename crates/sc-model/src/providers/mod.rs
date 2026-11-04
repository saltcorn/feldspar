//! The built-in model providers (TODO §13, Phase 4).
//!
//! Seven of them, in two groups that are deliberately not compiled together.
//!
//! **Five are machine learning**, behind the `smartcore` feature: `linear_regression`,
//! `logistic_regression`, `random_forest`, `kmeans` and `pca`. GOALS asks for
//! exactly that set — "regression, classification, clustering and dimensionality
//! reduction based on smartcore" — and asks for it behind a flag, so the flag is
//! `sc-model`'s own and it is **on by default**: `--no-default-features` is the
//! opt-out rather than something a packager has to remember.
//!
//! **Two are arithmetic**, and are there either way: `t_test` and `anova`. They
//! are GOALS' "statistical hypothesis testing" category, they need a
//! distribution function and nothing else, and a build with the machine learning
//! compiled out should still be able to answer whether two groups differ. A
//! registry holding only those two is a real state and not an empty list, which
//! is what [`BUILTINS_COMPILED_OUT`] exists to say on the screen.
//!
//! ## What the built-ins add on top of the library
//!
//! smartcore fits; it does not explain. Everything a fitted model is *read* for
//! is computed here:
//!
//! - a regression's **standard errors, *t* and *p*** for every coefficient, from
//!   the residual variance and `(XᵀX)⁻¹` — without which "a regression model
//!   where we are more interested in the slope coefficients" (GOALS) is a number
//!   with no way to tell whether it means anything;
//! - a logistic regression's **odds ratios** and the **predicted probability**
//!   that becomes the prediction's uncertainty;
//! - a forest's **feature importances**, by permutation, because smartcore keeps
//!   its trees to itself;
//! - a k-means' **cluster centres and sizes**, and a PCA's **explained variance
//!   ratio**.
//!
//! ## Two conventions every provider here follows
//!
//! **The frame is already numbers.** By the time [`ModelProvider::fit`] is
//! called the host has encoded it (§6): one-hot columns are named `region=north`,
//! dates are epoch seconds, and — where the provider asked for it — the numeric
//! columns are standardised. A provider here therefore never inspects a string,
//! and the feature *names* it stores are the encoded ones, which is what makes a
//! coefficient table say `region=north` rather than `x7`.
//!
//! **The label is a column of the frame, and its type says which fit this is.**
//! `fit` is handed a frame, a configuration and a hyperparameter point — not the
//! resolved [`Outcome`](crate::Outcome) — so `random_forest` reads the label
//! column's type: an integer column is a class index and a float column is a
//! measurement. See [`Encoded::frame`](crate::Encoded::frame), which is where
//! that is decided.

use std::sync::Arc;

use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;

use crate::provider::ModelProvider;
use crate::registry::ModelRegistry;

#[cfg(feature = "smartcore")]
use crate::encode::Matrix;
#[cfg(feature = "smartcore")]
use crate::frame::{ColumnType, Frame};
#[cfg(feature = "smartcore")]
use serde::Serialize;
#[cfg(feature = "smartcore")]
use serde::de::DeserializeOwned;

mod hypothesis;

#[cfg(feature = "smartcore")]
mod forest;
#[cfg(feature = "smartcore")]
mod kmeans;
#[cfg(feature = "smartcore")]
mod linear;
#[cfg(feature = "smartcore")]
mod logistic;
#[cfg(feature = "smartcore")]
mod pca;

/// Whether this build carries the five machine-learning providers.
///
/// A `const` rather than a `cfg!` at each call site so that the one place the
/// question is answered is here, beside the notice that explains the answer.
pub const SMARTCORE: bool = cfg!(feature = "smartcore");

/// What the admin screen says when the machine-learning built-ins are not in
/// this build (§13).
///
/// An empty provider list reads like a bug; "this build was made without them"
/// reads like a decision, which is what it is. The sentence names the flag,
/// because the person reading it is usually the person who can rebuild.
pub const BUILTINS_COMPILED_OUT: &str = "this server was built with `--no-default-features`, so the built-in machine-learning \
     model providers (linear and logistic regression, random forest, k-means and PCA) were \
     compiled out; the hypothesis tests are still here, and a module can supply more";

/// Every built-in provider this build carries, in no particular order — the
/// registry sorts them.
///
/// Two with the feature off, seven with it on.
pub fn builtin_providers() -> Vec<Arc<dyn ModelProvider>> {
    #[cfg_attr(
        not(feature = "smartcore"),
        expect(
            unused_mut,
            reason = "the five machine-learning providers are the only pushes, and they are \
                      behind the feature"
        )
    )]
    let mut providers: Vec<Arc<dyn ModelProvider>> =
        vec![Arc::new(hypothesis::TTest), Arc::new(hypothesis::Anova)];
    #[cfg(feature = "smartcore")]
    {
        providers.push(Arc::new(linear::LinearRegression));
        providers.push(Arc::new(logistic::LogisticRegression));
        providers.push(Arc::new(forest::RandomForest));
        providers.push(Arc::new(kmeans::KMeans));
        providers.push(Arc::new(pca::Pca));
    }
    providers
}

/// A registry holding every built-in — what the server starts from before a
/// module host adds its own.
pub fn builtin_registry() -> Result<ModelRegistry> {
    let mut registry = ModelRegistry::new();
    for provider in builtin_providers() {
        registry.register(provider)?;
    }
    Ok(registry)
}

// --- What every provider here needs from a frame and a configuration ---------

/// The configuration value under `key`, as the name of a dataset column.
///
/// Required: a provider whose label is missing cannot be fitted, and saying so
/// with the key's name is what lets the admin find the empty field.
pub(crate) fn column_setting(config: &Attrs, key: &str) -> Result<String> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("`{key}`: no column is named")))
}

/// The configuration value under `key`, or `None` when it was left empty.
pub(crate) fn optional_column_setting(config: &Attrs, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A boolean setting, defaulted.
#[cfg(feature = "smartcore")]
pub(crate) fn bool_setting(config: &Attrs, key: &str, default: bool) -> bool {
    config.get(key).and_then(Json::as_bool).unwrap_or(default)
}

/// A whole-number setting or hyperparameter, defaulted, and refused **by name**
/// below `min`.
///
/// Refused rather than clamped: `k = 0` clamped to 1 is a fit nobody asked for
/// reported as a success, and a grid point that cannot be fitted is recorded
/// with its sentence (§11) precisely so that this kind of thing is visible.
#[cfg(feature = "smartcore")]
pub(crate) fn whole_setting(attrs: &Attrs, key: &str, default: usize, min: usize) -> Result<usize> {
    let Some(value) = attrs.get(key) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let n = value
        .as_i64()
        .ok_or_else(|| Error::invalid(format!("`{key}`: {value} is not a whole number")))?;
    if n < min as i64 {
        return Err(Error::invalid(format!(
            "`{key}`: {n} is too small; it must be at least {min}"
        )));
    }
    usize::try_from(n).map_err(|_| Error::invalid(format!("`{key}`: {n} is too large")))
}

/// An optional whole-number setting: absent, null or zero all mean "no limit".
#[cfg(feature = "smartcore")]
pub(crate) fn optional_whole_setting(attrs: &Attrs, key: &str) -> Result<Option<usize>> {
    match whole_setting(attrs, key, 0, 0)? {
        0 => Ok(None),
        n => Ok(Some(n)),
    }
}

/// A floating-point setting or hyperparameter, defaulted.
pub(crate) fn number_setting(attrs: &Attrs, key: &str, default: f64) -> Result<f64> {
    let Some(value) = attrs.get(key) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| Error::invalid(format!("`{key}`: {value} is not a number")))
}

/// The frame's columns other than `label`, in frame order — the features.
///
/// Order is the frame's, which is the encoding's, which is the matrix's: the
/// same order at fit time and at predict time, because the stored state carries
/// this list and predict rebuilds the matrix from it by name.
#[cfg(feature = "smartcore")]
pub(crate) fn feature_names(frame: &Frame, label: Option<&str>) -> Vec<String> {
    frame
        .columns
        .iter()
        .map(|(name, _)| name)
        .filter(|name| Some(name.as_str()) != label)
        .cloned()
        .collect()
}

/// The named columns of `frame` as a matrix, refusing an empty feature set by
/// name.
#[cfg(feature = "smartcore")]
pub(crate) fn design(frame: &Frame, names: &[String]) -> Result<Matrix> {
    if names.is_empty() {
        return Err(Error::invalid(
            "this fit has no feature columns: every column of the dataset is the label",
        ));
    }
    Matrix::from_frame(frame, names)
}

/// The label column of a fit frame, as numbers.
///
/// For a classification these are class **indices** — see
/// [`Encoded::frame`](crate::Encoded::frame).
#[cfg(feature = "smartcore")]
pub(crate) fn label_values(frame: &Frame, label: &str) -> Result<Vec<f64>> {
    let names = vec![label.to_owned()];
    let matrix = Matrix::from_frame(frame, &names)?;
    matrix
        .column(label)
        .ok_or_else(|| Error::msg(format!("the label `{label}` vanished from its own matrix")))
}

/// Whether the label column of a fit frame holds class indices rather than
/// measurements — the one thing `random_forest` branches on.
#[cfg(feature = "smartcore")]
pub(crate) fn label_is_classified(frame: &Frame, label: &str) -> Result<bool> {
    let column = frame.column(label).ok_or_else(|| {
        Error::invalid(format!(
            "the label `{label}` is not a column of this frame (it has {})",
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
    })?;
    Ok(column.kind() == ColumnType::Int)
}

/// A provider's fitted state, as the JSON it is stored as.
#[cfg(feature = "smartcore")]
pub(crate) fn to_state<T: Serialize>(state: &T) -> Result<Json> {
    serde_json::to_value(state).map_err(|e| Error::msg(format!("this fit cannot be stored: {e}")))
}

/// A provider's fitted state, read back.
///
/// The error says *whose* state could not be read, because an instance's `state`
/// column is opaque to everything else and "invalid JSON" would not say which
/// provider was looking at it.
#[cfg(feature = "smartcore")]
pub(crate) fn from_state<T: DeserializeOwned>(provider: &str, state: &Json) -> Result<T> {
    serde_json::from_value(state.clone()).map_err(|e| {
        Error::invalid(format!(
            "this instance's stored `{provider}` fit cannot be read, so it cannot be applied: {e}"
        ))
    })
}

/// A number a parameter table can hold: a non-finite one becomes `null` rather
/// than a spelling JSON has no room for.
pub(crate) fn cell(value: f64) -> Json {
    serde_json::Number::from_f64(value).map_or(Json::Null, Json::Number)
}

/// What every provider's tests need to say a frame, a configuration and an
/// assertion about a parameter block, in one place rather than seven.
#[cfg(test)]
pub(crate) mod testing {
    use sc_types::Attrs;
    use serde_json::Value as Json;

    use crate::frame::{Column, Frame};
    use crate::provider::ParameterBlock;

    /// A frame of these named columns, with no keys (nothing here splits).
    pub(crate) fn frame(columns: Vec<(&str, Column)>) -> Frame {
        let rows = columns.first().map_or(0, |(_, c)| c.len());
        Frame {
            columns: columns
                .into_iter()
                .map(|(name, column)| (name.to_owned(), column))
                .collect(),
            rows,
            keys: Vec::new(),
        }
    }

    /// A float column with no nulls.
    pub(crate) fn floats(values: &[f64]) -> Column {
        Column::Float(values.iter().copied().map(Some).collect())
    }

    /// An integer column with no nulls — a class index, as an encoded
    /// classification label arrives. Only the supervised built-ins have one.
    #[cfg(feature = "smartcore")]
    pub(crate) fn ints(values: &[i64]) -> Column {
        Column::Int(values.iter().copied().map(Some).collect())
    }

    /// A text column with no nulls.
    pub(crate) fn strings(values: &[&str]) -> Column {
        Column::Str(values.iter().map(|v| Some((*v).to_owned())).collect())
    }

    /// A settings bag.
    pub(crate) fn attrs(pairs: &[(&str, Json)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    /// The named scalar parameter, or a panic naming what was there instead.
    pub(crate) fn scalar(parameters: &[ParameterBlock], name: &str) -> f64 {
        parameters
            .iter()
            .find_map(|block| match block {
                ParameterBlock::Scalar { name: n, value } if n == name => Some(*value),
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!(
                    "no scalar parameter `{name}`; there are {:?}",
                    parameters
                        .iter()
                        .map(ParameterBlock::name)
                        .collect::<Vec<_>>()
                )
            })
    }

    /// The named table parameter's headings and rows.
    pub(crate) fn table<'a>(
        parameters: &'a [ParameterBlock],
        name: &str,
    ) -> (&'a [String], Vec<Vec<Json>>) {
        parameters
            .iter()
            .find_map(|block| match block {
                ParameterBlock::Table {
                    name: n,
                    columns,
                    rows,
                } if n == name => Some((
                    columns.as_slice(),
                    rows.iter().map(|r| r.cells.clone()).collect(),
                )),
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!(
                    "no table parameter `{name}`; there are {:?}",
                    parameters
                        .iter()
                        .map(ParameterBlock::name)
                        .collect::<Vec<_>>()
                )
            })
    }

    /// One cell of a table row, as a number.
    pub(crate) fn number(cell: &Json) -> f64 {
        cell.as_f64()
            .unwrap_or_else(|| panic!("`{cell}` is not a number"))
    }

    /// `left` and `right` agree to `tolerance`, with both printed when they do
    /// not — the reference value is pasted from another tool, and "assertion
    /// failed" without the numbers is not enough to tell a bug from a rounding.
    #[track_caller]
    pub(crate) fn close(left: f64, right: f64, tolerance: f64) {
        assert!(
            (left - right).abs() <= tolerance,
            "{left} is not within {tolerance} of {right}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtins_are_registered_under_the_names_a_stored_model_references() {
        let registry = builtin_registry().unwrap();
        // The two hypothesis tests are there in every build.
        assert!(registry.get("t_test").is_some());
        assert!(registry.get("anova").is_some());
        assert_eq!(registry.len(), if SMARTCORE { 7 } else { 2 });
        for name in [
            "linear_regression",
            "logistic_regression",
            "random_forest",
            "kmeans",
            "pca",
        ] {
            assert_eq!(registry.get(name).is_some(), SMARTCORE, "{name}");
        }
        // No built-in claims to come from a module, which is what the picker
        // renders "built in" against.
        assert!(registry.kinds().iter().all(|k| k.module.is_none()));
    }

    /// Task 4.9: with the feature off there are exactly two providers, and the
    /// screen has a sentence to show instead of an empty list.
    ///
    /// `SMARTCORE` is the one place the question is answered, so the assertion
    /// is written against it rather than against a second `cfg!` that could
    /// drift from it.
    #[test]
    fn a_build_without_the_feature_still_has_the_hypothesis_tests_and_a_notice() {
        let registry = builtin_registry().unwrap();
        if SMARTCORE {
            assert_eq!(
                registry.names(),
                vec![
                    "anova",
                    "kmeans",
                    "linear_regression",
                    "logistic_regression",
                    "pca",
                    "random_forest",
                    "t_test",
                ]
            );
        } else {
            assert_eq!(registry.names(), vec!["anova", "t_test"]);
        }
        // The notice is in every build, because the screen that shows it is
        // built once and asks `SMARTCORE` at render time.
        assert!(BUILTINS_COMPILED_OUT.contains("--no-default-features"));
        assert!(BUILTINS_COMPILED_OUT.contains("hypothesis tests are still here"));
    }

    /// The whole of Phase 3 and Phase 4 in one call: a dataset with a
    /// categorical column goes through the split, the encoding, a real provider
    /// and the host's metrics, and comes back as a fit.
    ///
    /// The thing worth asserting here is the *join* — that the one-hot columns
    /// the encoding produced are the terms the coefficient table names, so a
    /// reader sees `region=north` and not `x2`.
    #[cfg(feature = "smartcore")]
    #[tokio::test]
    async fn a_builtin_fits_end_to_end_through_the_encoding_and_the_metrics() {
        use crate::dataset::Dataset;
        use crate::frame::{Column, Frame};
        use crate::model::Model;
        use crate::provider::ParameterBlock;
        use crate::source::{DatasetSource, Read};
        use crate::split::Split;

        /// The seam, stubbed: one fixed frame, as `fit`'s own tests do it.
        struct Fixed(Frame);
        #[async_trait::async_trait]
        impl DatasetSource for Fixed {
            async fn read(&self, _dataset: &Dataset, _how: &Read<'_>) -> Result<Frame> {
                Ok(self.0.clone())
            }
        }

        let n = 60;
        let regions = ["north", "south", "east"];
        let frame = Frame::new(
            vec![
                (
                    "area".to_owned(),
                    Column::Float((0..n).map(|i| Some(50.0 + i as f64)).collect()),
                ),
                (
                    "region".to_owned(),
                    Column::Str((0..n).map(|i| Some(regions[i % 3].to_owned())).collect()),
                ),
                (
                    "price".to_owned(),
                    Column::Float(
                        (0..n)
                            .map(|i| Some(1000.0 + 3.0 * (50.0 + i as f64) + 10.0 * (i % 3) as f64))
                            .collect(),
                    ),
                ),
            ],
            (0..n).map(|i| format!("int:{i}")).collect(),
        )
        .unwrap();

        let model = Model::new(
            "prices",
            "linear_regression",
            Dataset::new("house")
                .column("area", "area")
                .column("region", "region")
                .column("price", "price"),
        )
        .config("label", "price")
        .split(Split::new(0.7, 0.0, 0.3, 5));

        let fit = crate::fit::run_fit(&builtin_registry().unwrap(), &Fixed(frame), &model, 1000)
            .await
            .unwrap();

        // A regression, scored by the host on rows the fit never saw.
        assert_eq!(
            fit.outcome,
            crate::provider::Outcome::Regression {
                label: "price".to_owned()
            }
        );
        let test = fit.metrics.test.as_ref().unwrap();
        assert!(
            test.primary().unwrap_or(0.0) > 0.99,
            "the label is a linear function of the features: {test:?}"
        );

        // The encoding's one-hot names are the coefficient table's terms.
        let encoding = fit.encoding.as_ref().unwrap();
        assert!(
            encoding
                .feature_names()
                .contains(&"region=north".to_owned()),
            "{:?}",
            encoding.feature_names()
        );
        let Some(ParameterBlock::Table { rows, .. }) =
            fit.parameters.iter().find(|b| b.name() == "Coefficients")
        else {
            panic!("no coefficient table in {:?}", fit.parameters);
        };
        let terms: Vec<String> = rows
            .iter()
            .filter_map(|r| r.cells.first().and_then(Json::as_str).map(str::to_owned))
            .collect();
        assert_eq!(
            terms,
            vec![
                "(intercept)".to_owned(),
                "area".to_owned(),
                "region=north".to_owned(),
                "region=south".to_owned(),
            ],
            "the baseline category is not a term, and the rest are named"
        );
    }

    #[cfg(feature = "smartcore")]
    #[test]
    fn a_whole_setting_is_refused_by_name_rather_than_clamped() {
        let mut attrs = Attrs::new();
        attrs.insert("k".to_owned(), Json::from(0));
        let err = whole_setting(&attrs, "k", 3, 2).unwrap_err().to_string();
        assert!(err.contains("`k`") && err.contains("at least 2"), "{err}");
        attrs.insert("k".to_owned(), Json::from("eight"));
        let err = whole_setting(&attrs, "k", 3, 2).unwrap_err().to_string();
        assert!(err.contains("not a whole number"), "{err}");
        // Absent and null both fall back to the default.
        assert_eq!(whole_setting(&Attrs::new(), "k", 3, 2).unwrap(), 3);
        attrs.insert("k".to_owned(), Json::Null);
        assert_eq!(whole_setting(&attrs, "k", 3, 2).unwrap(), 3);
    }
}

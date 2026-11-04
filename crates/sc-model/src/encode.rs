//! Turning a frame into numbers, **once**, and carrying the result on the
//! instance (TODO §6, task 3.1).
//!
//! A model provider wants numbers. A dataset column is a string, a boolean, a
//! date or a float. The translation — one-hot for a categorical feature, a label
//! mapping for a classification target, an epoch-seconds cast for a date,
//! standardisation where a provider asks for it — happens here, and the result
//! is **stored on the instance**.
//!
//! This is the single most load-bearing decision in the milestone, because the
//! failure it prevents is silent. If prediction re-derived the one-hot column
//! order from whatever categories happen to be in the rows being predicted, a
//! model fitted when `region` had four values and applied to a batch containing
//! three would put every coefficient against the wrong column and return
//! confident nonsense. Fitting the encoding once and carrying it means a
//! prediction is encoded **the way the fit was**, or it fails.
//!
//! And it fails loudly:
//!
//! - **A category at predict time that was not present at fit time is an
//!   error** naming the column and the value — not a row of zeros, which is the
//!   industry's usual answer and is a prediction from a model that was never
//!   shown this input.
//! - **A null in a feature** is a dropped row at fit time
//!   ([`apply_encoding_dropping`], counted and reported on the instance) and an
//!   error at predict time ([`apply_encoding`]). At fit time dropping is a
//!   defensible sample restriction that we report; at predict time it would mean
//!   answering a question about a row we cannot represent.
//!
//! ## One-hot is reference-coded, and the whole list is still stored
//!
//! A categorical column with *k* values becomes *k − 1* columns: the first
//! category (in sorted order) is the **baseline** and is the row of zeros. The
//! alternative — a column per category — makes the design matrix rank-deficient
//! the moment an intercept is fitted beside it, and `(XᵀX)⁻¹` is exactly what
//! the standard errors, *t* and *p* of §13 are computed from. What is *stored*
//! is the full category list, including the baseline, because that is what makes
//! "was this value seen at fit time" answerable — a value missing from the list
//! is refused, and the baseline is not a hole in the list to fall into.
//!
//! ## What the provider actually receives
//!
//! [`Encoded`] carries both shapes, because both are wanted: a [`Matrix`] —
//! row-major `Vec<f64>` plus its column names, which is what a provider's
//! arithmetic works in — and a [`Frame`], which is what
//! [`ModelProvider::fit`](crate::ModelProvider::fit) takes and what crosses a
//! module seam. They are the same numbers.

use std::collections::BTreeSet;

use sc_error::{Error, Result};
use serde_json::Value as Json;

use crate::frame::{Column, ColumnType, Frame};
use crate::provider::Outcome;

/// A row-major matrix of `f64` with named columns — what every provider wants.
///
/// Row-major because a provider iterates rows (`smartcore` takes
/// `Vec<Vec<f64>>`, `numpy.asarray` takes the same shape) and because the
/// encoding produces a row at a time. The names travel with it so a coefficient
/// table can say `region=north` rather than `x7`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Matrix {
    columns: Vec<String>,
    values: Vec<f64>,
    rows: usize,
}

impl Matrix {
    /// A matrix of `values`, row-major, `columns.len()` wide.
    ///
    /// Checked rather than trusted: a `values` length that is not a multiple of
    /// the width is a matrix whose last row is short, which is a wrong answer
    /// rather than a crash.
    pub fn new(columns: Vec<String>, values: Vec<f64>) -> Result<Matrix> {
        let width = columns.len();
        if width == 0 {
            if !values.is_empty() {
                return Err(Error::msg(
                    "a matrix with no columns cannot hold values".to_owned(),
                ));
            }
            return Ok(Matrix {
                columns,
                values,
                rows: 0,
            });
        }
        if values.len() % width != 0 {
            return Err(Error::msg(format!(
                "a matrix {width} columns wide cannot hold {} values",
                values.len()
            )));
        }
        let rows = values.len() / width;
        Ok(Matrix {
            columns,
            values,
            rows,
        })
    }

    /// The column names, in order.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// How many columns wide.
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    /// How many rows tall.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Whether it has no rows.
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Every value, row-major.
    pub fn values(&self) -> &[f64] {
        &self.values
    }

    /// Row `i`, or `None` past the end.
    pub fn row(&self, i: usize) -> Option<&[f64]> {
        if i >= self.rows {
            return None;
        }
        let width = self.width();
        Some(&self.values[i * width..(i + 1) * width])
    }

    /// The rows as a vector of vectors — the shape `smartcore` takes.
    pub fn to_rows(&self) -> Vec<Vec<f64>> {
        (0..self.rows)
            .map(|i| self.row(i).unwrap_or(&[]).to_vec())
            .collect()
    }

    /// The column under `name`, as a vector down the rows.
    pub fn column(&self, name: &str) -> Option<Vec<f64>> {
        let j = self.columns.iter().position(|c| c == name)?;
        let width = self.width();
        Some((0..self.rows).map(|i| self.values[i * width + j]).collect())
    }

    /// This matrix as a [`Frame`] of `Float` columns — what crosses the provider
    /// seam.
    pub fn to_frame(&self) -> Frame {
        let width = self.width();
        let columns = self
            .columns
            .iter()
            .enumerate()
            .map(|(j, name)| {
                (
                    name.clone(),
                    Column::Float(
                        (0..self.rows)
                            .map(|i| Some(self.values[i * width + j]))
                            .collect(),
                    ),
                )
            })
            .collect();
        Frame {
            columns,
            rows: self.rows,
            keys: Vec::new(),
        }
    }

    /// The named columns of `frame` as a matrix — how a provider builds its
    /// design matrix from the frame it was handed.
    ///
    /// Every named column must exist and be numeric with no nulls, which is what
    /// an encoded frame always is: this is the inverse of
    /// [`to_frame`](Matrix::to_frame) and not a second encoder.
    pub fn from_frame(frame: &Frame, columns: &[String]) -> Result<Matrix> {
        let mut values = Vec::with_capacity(frame.rows * columns.len());
        let picked: Vec<&Column> = columns
            .iter()
            .map(|name| {
                frame.column(name).ok_or_else(|| {
                    Error::msg(format!("the frame has no column `{name}` to encode"))
                })
            })
            .collect::<Result<_>>()?;
        for i in 0..frame.rows {
            for (name, column) in columns.iter().zip(&picked) {
                values.push(number_at(column, i).ok_or_else(|| {
                    Error::msg(format!("column `{name}` is not a number at row {}", i + 1))
                })?);
            }
        }
        Matrix::new(columns.to_vec(), values)
    }
}

/// How one dataset column becomes one or more matrix columns (§6).
///
/// Each variant carries **everything the fit learned** about that column, which
/// is the point: applying it later is a lookup and never a re-derivation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "encoding", rename_all = "snake_case")]
pub enum ColumnEncoding {
    /// A number used as it stands. `Bool` counts: `true` is 1 and `false` is 0.
    Passthrough {
        /// The dataset column.
        column: String,
    },
    /// A number centred and scaled by the constants the fit computed.
    ///
    /// `sd` is never 0: a column that was constant on the training rows is
    /// recorded with `sd = 1`, so it encodes to zeros rather than to infinities.
    Standardised {
        /// The dataset column.
        column: String,
        /// The training rows' mean.
        mean: f64,
        /// The training rows' standard deviation, or 1 for a constant column.
        sd: f64,
    },
    /// A category, reference-coded: `categories.len() - 1` columns, with
    /// `categories[0]` as the baseline row of zeros (see the module docs).
    OneHot {
        /// The dataset column.
        column: String,
        /// Every category seen at fit time, sorted. A value not in this list is
        /// refused by name at apply time.
        categories: Vec<String>,
    },
    /// A date or timestamp as epoch seconds.
    ///
    /// Its own variant rather than a
    /// [`Passthrough`](ColumnEncoding::Passthrough) so the instance records that
    /// this column *was* a date: a
    /// column that arrives as an integer next time is a different column, and
    /// silently encoding it the same way is exactly the fit/predict divergence
    /// this module exists to prevent.
    Epoch {
        /// The dataset column.
        column: String,
    },
}

impl ColumnEncoding {
    /// The dataset column this encodes.
    pub fn column(&self) -> &str {
        match self {
            ColumnEncoding::Passthrough { column }
            | ColumnEncoding::Standardised { column, .. }
            | ColumnEncoding::OneHot { column, .. }
            | ColumnEncoding::Epoch { column } => column,
        }
    }

    /// The matrix columns it produces, in order.
    pub fn names(&self) -> Vec<String> {
        match self {
            ColumnEncoding::OneHot { column, categories } => categories
                .iter()
                .skip(1)
                .map(|c| format!("{column}={c}"))
                .collect(),
            other => vec![other.column().to_owned()],
        }
    }

    /// How many matrix columns wide it is.
    pub fn width(&self) -> usize {
        match self {
            ColumnEncoding::OneHot { categories, .. } => categories.len().saturating_sub(1),
            _ => 1,
        }
    }

    /// The column types this encoding can be applied to — checked at apply time
    /// so a column that changed shape between the fit and the prediction is
    /// caught rather than reinterpreted.
    fn accepts(&self, ty: ColumnType) -> bool {
        match self {
            ColumnEncoding::Passthrough { .. } | ColumnEncoding::Standardised { .. } => matches!(
                ty,
                ColumnType::Float | ColumnType::Int | ColumnType::Bool | ColumnType::Null
            ),
            ColumnEncoding::OneHot { .. } => !matches!(ty, ColumnType::Date),
            ColumnEncoding::Epoch { .. } => matches!(ty, ColumnType::Date | ColumnType::Null),
        }
    }

    /// This column's contribution to one row, appended to `out`.
    ///
    /// `Ok(false)` means "this row cannot be represented" — a null, or a
    /// category the fit never saw — and the caller decides whether that is a
    /// dropped row or an error.
    fn encode_row(&self, column: &Column, i: usize, out: &mut Vec<f64>) -> Result<bool> {
        if column.is_null_at(i) {
            return Ok(false);
        }
        match self {
            ColumnEncoding::Passthrough { column: name }
            | ColumnEncoding::Epoch { column: name } => {
                let v = number_at(column, i).ok_or_else(|| self.not_a_number(name, i))?;
                out.push(v);
            }
            ColumnEncoding::Standardised {
                column: name,
                mean,
                sd,
            } => {
                let v = number_at(column, i).ok_or_else(|| self.not_a_number(name, i))?;
                out.push((v - mean) / sd);
            }
            ColumnEncoding::OneHot { categories, .. } => {
                let Some(value) = category_at(column, i) else {
                    return Ok(false);
                };
                let Some(found) = categories.iter().position(|c| *c == value) else {
                    return Ok(false);
                };
                for (k, _) in categories.iter().enumerate().skip(1) {
                    out.push(if k == found { 1.0 } else { 0.0 });
                }
            }
        }
        Ok(true)
    }

    /// The error a numeric encoding gives when the column is not numeric after
    /// all — a shape check that
    /// [`accepts`](ColumnEncoding::accepts) has already made unlikely, kept
    /// because "unreachable" and "unchecked" are different things.
    fn not_a_number(&self, name: &str, i: usize) -> Error {
        Error::invalid(format!(
            "column `{name}` was a number when this model was fitted, but row {} is not",
            i + 1
        ))
    }

    /// Why a row failed this encoding, for the strict path's message.
    fn refusal(&self, column: &Column, i: usize) -> String {
        let name = self.column();
        if column.is_null_at(i) {
            return format!(
                "column `{name}` is null in row {}, and a model cannot be applied to a row it \
                 cannot represent",
                i + 1
            );
        }
        match self {
            ColumnEncoding::OneHot { categories, .. } => {
                let value = category_at(column, i).unwrap_or_default();
                format!(
                    "column `{name}` has the value `{value}`, which was not present when this \
                     model was fitted (it saw {})",
                    categories
                        .iter()
                        .map(|c| format!("`{c}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            _ => format!("column `{name}` cannot be encoded at row {}", i + 1),
        }
    }
}

/// How the label becomes what a provider is trained against (§6).
///
/// One type for both supervised outcomes, because "which column is the label" is
/// the question every supervised fit asks and only *classification* adds a map.
/// A regression's target is already a number, so `classes` is `None` and the
/// values pass through.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TargetEncoding {
    /// The dataset column being predicted.
    pub column: String,
    /// The classes in the order the fit assigned them indices, or `None` for a
    /// regression.
    ///
    /// This is the map a [`Prediction::ClassIndex`](crate::Prediction) is turned
    /// back into a name through: the index is an implementation detail of *this*
    /// encoding, so the list has to travel with the instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classes: Option<Vec<String>>,
}

impl TargetEncoding {
    /// The class name at `index`, or an error naming the range — a provider that
    /// answered an index outside its own fitted classes is a bug we do not want
    /// to translate into a plausible-looking class name.
    pub fn class(&self, index: usize) -> Result<&str> {
        let classes = self.classes.as_ref().ok_or_else(|| {
            Error::msg(format!(
                "the target `{}` is a number, so class {index} means nothing",
                self.column
            ))
        })?;
        classes.get(index).map(String::as_str).ok_or_else(|| {
            Error::msg(format!(
                "class {index} is outside the {} classes this model was fitted with",
                classes.len()
            ))
        })
    }
}

/// Everything one fit learned about turning its frame into numbers.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Encoding {
    /// The feature columns, in matrix order.
    pub columns: Vec<ColumnEncoding>,
    /// The label, for a supervised fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetEncoding>,
}

impl Encoding {
    /// The matrix column names this encoding produces, in order.
    pub fn feature_names(&self) -> Vec<String> {
        self.columns
            .iter()
            .flat_map(ColumnEncoding::names)
            .collect()
    }

    /// How wide the encoded matrix is.
    pub fn width(&self) -> usize {
        self.columns.iter().map(ColumnEncoding::width).sum()
    }

    /// The classes this fit saw, for a classification.
    pub fn classes(&self) -> Option<&[String]> {
        self.target.as_ref().and_then(|t| t.classes.as_deref())
    }

    /// This encoding with its label **dropped** — what a prediction applies.
    ///
    /// A prediction is asked about a row whose label is not known, and usually
    /// is not there at all: that is what a prediction *is*. Encoding the target
    /// would demand a value the caller cannot have, and refusing the row for not
    /// having one would make a fitted model unusable on exactly the rows it was
    /// fitted to answer about. The label matters again on the way *back*, where
    /// [`name_classes`](crate::name_classes) maps a class index through the
    /// target this drops — so this is a narrowing of the apply step and not of
    /// the instance.
    pub fn features_only(&self) -> Encoding {
        Encoding {
            columns: self.columns.clone(),
            target: None,
        }
    }

    /// The dataset columns this encoding reads and the type each was fitted as
    /// — what [`Frame::from_rows`](crate::Frame::from_rows) needs to read a
    /// literal row the way the fit read the table.
    ///
    /// The type is the encoding's own claim rather than a recorded copy of the
    /// dataset's shape, which is the point: a one-hot column *is* a category and
    /// an epoch column *is* a date, whatever the row being predicted happens to
    /// look like.
    pub fn feature_types(&self) -> Vec<(String, ColumnType)> {
        self.columns
            .iter()
            .map(|column| {
                let ty = match column {
                    ColumnEncoding::Passthrough { .. } | ColumnEncoding::Standardised { .. } => {
                        ColumnType::Float
                    }
                    ColumnEncoding::OneHot { .. } => ColumnType::Str,
                    ColumnEncoding::Epoch { .. } => ColumnType::Date,
                };
                (column.column().to_owned(), ty)
            })
            .collect()
    }
}

/// An encoded frame: the same numbers as a matrix and as a frame, plus which
/// rows of the source survived.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Encoded {
    /// The features, row-major.
    pub features: Matrix,
    /// The label, for a supervised fit: the value for a regression, the class
    /// **index** for a classification.
    pub target: Option<Vec<f64>>,
    /// The name the target column is known by — the dataset column's own, so a
    /// provider's configuration still addresses it.
    pub target_name: Option<String>,
    /// Whether the target is a **class index** rather than a measurement.
    ///
    /// It is what makes the target column of [`frame`](Encoded::frame) an
    /// integer column, which is how a provider whose outcome depends on its
    /// label's type — `random_forest` is the case the design names — tells a
    /// classification from a regression. See that method.
    pub classified: bool,
    /// Which rows of the source frame are in here, in order. Shorter than the
    /// source when nulls were dropped.
    pub rows: Vec<usize>,
    /// How many rows the encoding could not represent and dropped.
    pub dropped: usize,
}

impl Encoded {
    /// How many rows survived.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether nothing survived.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The features alone, as the frame a provider **predicts** from.
    ///
    /// No target column, because at predict time there is no label — and a
    /// provider that quietly read one during scoring would score itself against
    /// the answer.
    pub fn features_frame(&self) -> Frame {
        self.features.to_frame()
    }

    /// The features **and** the target, as the frame a provider **fits** from.
    ///
    /// The target column is an **integer** column for a classification and a
    /// float column for a regression, and that is load-bearing rather than
    /// cosmetic: [`ModelProvider::fit`](crate::ModelProvider::fit) is handed a
    /// frame, a configuration and a hyperparameter point and *not* the resolved
    /// [`Outcome`], so a provider that is a regressor or a classifier depending
    /// on its label — `random_forest` — reads the label's type off this column.
    /// It is also simply true: a class index is not a measurement, and encoding
    /// it as one would be the only place in this crate where a category is
    /// spelled as a float.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "float-to-integer as saturates (NaN is 0); it rounds a finite count, bin or index computed from the data"
    )]
    pub fn frame(&self) -> Frame {
        let mut frame = self.features.to_frame();
        if let (Some(name), Some(target)) = (&self.target_name, &self.target) {
            let column = if self.classified {
                Column::Int(target.iter().map(|v| Some(*v as i64)).collect())
            } else {
                Column::Float(target.iter().copied().map(Some).collect())
            };
            frame.columns.push((name.clone(), column));
        }
        frame
    }
}

/// Fit an encoding to `frame` — which is the **training** frame, and only the
/// training frame.
///
/// Category lists and standardisation constants are the training rows', so a
/// category that appears only in the test rows is *unseen*, which is what the
/// metric it scores should reflect. Fitting them over everything would leak the
/// held-out rows into the fit, quietly, in the one place nobody looks.
///
/// `standardise` is the provider's answer to
/// [`ModelProvider::standardise`](crate::ModelProvider::standardise): a k-means
/// or a PCA over unscaled columns is a fit dominated by whichever column happens
/// to be measured in larger units.
pub fn fit_encoding(frame: &Frame, outcome: &Outcome, standardise: bool) -> Result<Encoding> {
    let label = outcome.label();
    let mut columns = Vec::new();
    for (name, column) in &frame.columns {
        if Some(name.as_str()) == label {
            continue;
        }
        columns.push(fit_column(name, column, standardise)?);
    }
    let target = match outcome {
        Outcome::Regression { label } => {
            let column = require_column(frame, label)?;
            if !column.kind().is_numeric() {
                return Err(Error::invalid(format!(
                    "the label `{label}` is {} and a regression needs a number",
                    column.kind().name()
                )));
            }
            Some(TargetEncoding {
                column: label.clone(),
                classes: None,
            })
        }
        Outcome::Classification { label, .. } => {
            let column = require_column(frame, label)?;
            let classes: Vec<String> = (0..column.len())
                .filter_map(|i| category_at(column, i))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            if classes.len() < 2 {
                return Err(Error::invalid(format!(
                    "the label `{label}` takes {} value on the training rows, so there is \
                     nothing to classify",
                    if classes.is_empty() { "no" } else { "only one" }
                )));
            }
            Some(TargetEncoding {
                column: label.clone(),
                classes: Some(classes),
            })
        }
        _ => None,
    };
    if columns.is_empty() {
        return Err(Error::invalid(
            "this dataset has no feature columns: a fit needs at least one column that is not \
             the label",
        ));
    }
    Ok(Encoding { columns, target })
}

/// The encoding one feature column gets.
fn fit_column(name: &str, column: &Column, standardise: bool) -> Result<ColumnEncoding> {
    match column.kind() {
        ColumnType::Null => Err(Error::invalid(format!(
            "column `{name}` is null in every training row, so it carries no information: remove \
             it from the dataset or widen the filter"
        ))),
        ColumnType::Str => Ok(ColumnEncoding::OneHot {
            column: name.to_owned(),
            categories: (0..column.len())
                .filter_map(|i| category_at(column, i))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        }),
        ColumnType::Date if !standardise => Ok(ColumnEncoding::Epoch {
            column: name.to_owned(),
        }),
        _ if standardise => {
            let values: Vec<f64> = (0..column.len())
                .filter_map(|i| number_at(column, i))
                .collect();
            let (mean, sd) = mean_and_sd(&values);
            Ok(ColumnEncoding::Standardised {
                column: name.to_owned(),
                mean,
                sd,
            })
        }
        _ => Ok(ColumnEncoding::Passthrough {
            column: name.to_owned(),
        }),
    }
}

/// The mean and (sample) standard deviation of `values`, with a constant column
/// reported as `sd = 1` so it encodes to zeros rather than to infinities.
fn mean_and_sd(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 1.0);
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = if values.len() < 2 {
        0.0
    } else {
        values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)
    };
    let sd = variance.sqrt();
    (mean, if sd > 0.0 && sd.is_finite() { sd } else { 1.0 })
}

/// Apply `encoding` to `frame`, **refusing** any row it cannot represent (§6).
///
/// This is the predict-time path. A null feature or a category the fit never saw
/// is an error naming the column and the value, because the alternative — a row
/// of zeros — is a confident answer from a model that was never shown this
/// input, and because dropping the row would silently return fewer predictions
/// than there were rows.
pub fn apply_encoding(encoding: &Encoding, frame: &Frame) -> Result<Encoded> {
    apply(encoding, frame, false)
}

/// Apply `encoding` to `frame`, **dropping** the rows it cannot represent and
/// counting them.
///
/// This is the fit-time path, including the passes that score the fitted state
/// back over each split. At fit time a dropped row is a defensible sample
/// restriction that the instance reports; at predict time it would mean
/// answering a question about a row we cannot represent, which is why the two
/// are different functions and not a flag somebody could get wrong.
pub fn apply_encoding_dropping(encoding: &Encoding, frame: &Frame) -> Result<Encoded> {
    apply(encoding, frame, true)
}

/// The one implementation behind the two entry points.
fn apply(encoding: &Encoding, frame: &Frame, drop: bool) -> Result<Encoded> {
    // Resolve every column once, and check its shape once, rather than per row.
    let mut sources = Vec::with_capacity(encoding.columns.len());
    for column in &encoding.columns {
        let source = require_column(frame, column.column())?;
        if !column.accepts(source.kind()) {
            return Err(Error::invalid(format!(
                "column `{}` is {} now but was {} when this model was fitted",
                column.column(),
                source.kind().name(),
                fitted_as(column)
            )));
        }
        sources.push(source);
    }
    let target_source = match &encoding.target {
        Some(target) => Some((target, require_column(frame, &target.column)?)),
        None => None,
    };

    let width = encoding.width();
    let mut values = Vec::with_capacity(frame.rows * width);
    let mut target_values = Vec::new();
    let mut rows = Vec::new();
    let mut dropped = 0usize;
    let mut row = Vec::with_capacity(width);
    for i in 0..frame.rows {
        row.clear();
        let mut ok = true;
        for (column, source) in encoding.columns.iter().zip(&sources) {
            if !column.encode_row(source, i, &mut row)? {
                if !drop {
                    return Err(Error::invalid(column.refusal(source, i)));
                }
                ok = false;
                break;
            }
        }
        let mut target = None;
        if ok && let Some((encoding, source)) = &target_source {
            match encode_target(encoding, source, i) {
                Some(v) => target = Some(v),
                None => {
                    if !drop {
                        return Err(Error::invalid(target_refusal(encoding, source, i)));
                    }
                    ok = false;
                }
            }
        }
        if !ok {
            dropped += 1;
            continue;
        }
        values.extend_from_slice(&row);
        if let Some(v) = target {
            target_values.push(v);
        }
        rows.push(i);
    }

    Ok(Encoded {
        features: Matrix::new(encoding.feature_names(), values)?,
        target: encoding.target.as_ref().map(|_| target_values),
        target_name: encoding.target.as_ref().map(|t| t.column.clone()),
        classified: encoding
            .target
            .as_ref()
            .is_some_and(|t| t.classes.is_some()),
        rows,
        dropped,
    })
}

/// The label's number at row `i`: the value for a regression, the class index
/// for a classification, and `None` for a row that cannot be represented.
fn encode_target(encoding: &TargetEncoding, column: &Column, i: usize) -> Option<f64> {
    if column.is_null_at(i) {
        return None;
    }
    match &encoding.classes {
        None => number_at(column, i),
        Some(classes) => {
            let value = category_at(column, i)?;
            classes.iter().position(|c| *c == value).map(|k| k as f64)
        }
    }
}

/// Why a label value was refused, for the strict path.
fn target_refusal(encoding: &TargetEncoding, column: &Column, i: usize) -> String {
    let name = &encoding.column;
    if column.is_null_at(i) {
        return format!("the label `{name}` is null in row {}", i + 1);
    }
    match &encoding.classes {
        Some(classes) => format!(
            "the label `{name}` has the value `{}` in row {}, which was not present when this \
             model was fitted (it saw {})",
            category_at(column, i).unwrap_or_default(),
            i + 1,
            classes
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => format!("the label `{name}` is not a number in row {}", i + 1),
    }
}

/// What an encoding says its column was, for the shape-changed message.
fn fitted_as(encoding: &ColumnEncoding) -> &'static str {
    match encoding {
        ColumnEncoding::Passthrough { .. } | ColumnEncoding::Standardised { .. } => "a number",
        ColumnEncoding::OneHot { .. } => "a category",
        ColumnEncoding::Epoch { .. } => "a date",
    }
}

/// The frame's column under `name`, or an error naming what the frame does have.
fn require_column<'a>(frame: &'a Frame, name: &str) -> Result<&'a Column> {
    frame.column(name).ok_or_else(|| {
        Error::invalid(format!(
            "this model was fitted with a column `{name}`, which is not in these rows (they have \
             {})",
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

/// A column's value at row `i` as a number, where it has one. `Bool` counts:
/// `true` is 1 and `false` is 0, which is the encoding everybody expects and the
/// only one that keeps a boolean feature usable without a one-hot.
pub(crate) fn number_at(column: &Column, i: usize) -> Option<f64> {
    match column {
        Column::Float(v) => v.get(i).copied().flatten(),
        Column::Int(v) => v.get(i).copied().flatten().map(|x| x as f64),
        Column::Date(v) => v.get(i).copied().flatten().map(|x| x as f64),
        Column::Bool(v) => v
            .get(i)
            .copied()
            .flatten()
            .map(|b| if b { 1.0 } else { 0.0 }),
        Column::Str(_) | Column::Null(_) => None,
    }
}

/// A column's value at row `i` as the text a category is keyed by.
///
/// Every non-null value has one, because a category is a *label* and not a
/// reading: `true`, `3` and `north` are all perfectly good class names, and a
/// classification over an integer column is a thing people do.
pub(crate) fn category_at(column: &Column, i: usize) -> Option<String> {
    match column {
        Column::Str(v) => v.get(i).cloned().flatten(),
        Column::Int(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Float(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Bool(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Date(v) => v.get(i).copied().flatten().map(|x| x.to_string()),
        Column::Null(_) => None,
    }
}

/// The JSON an [`Encoding`] is stored as on the instance.
impl Encoding {
    /// This encoding as the instance's `encoding` column.
    pub fn to_json(&self) -> Result<Json> {
        serde_json::to_value(self).map_err(|e| Error::msg(format!("encoding: {e}")))
    }

    /// The encoding an instance's `encoding` column holds.
    pub fn from_json(json: &Json) -> Result<Encoding> {
        serde_json::from_value(json.clone()).map_err(|e| {
            Error::invalid(format!(
                "this instance's stored encoding cannot be read, so it cannot be applied: {e}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(columns: Vec<(&str, Column)>) -> Frame {
        let rows = columns.first().map_or(0, |(_, c)| c.len());
        Frame::new(
            columns
                .into_iter()
                .map(|(n, c)| (n.to_owned(), c))
                .collect(),
            (0..rows).map(|i| format!("int:{i}")).collect(),
        )
        .expect("frame")
    }

    fn houses() -> Frame {
        frame(vec![
            (
                "price",
                Column::Float(vec![Some(100.0), Some(200.0), Some(300.0), Some(400.0)]),
            ),
            (
                "beds",
                Column::Int(vec![Some(1), Some(2), Some(3), Some(4)]),
            ),
            (
                "region",
                Column::Str(vec![
                    Some("north".into()),
                    Some("south".into()),
                    Some("north".into()),
                    Some("east".into()),
                ]),
            ),
        ])
    }

    #[test]
    fn a_categorical_column_is_reference_coded_and_the_baseline_is_kept_in_the_list() {
        let encoding = fit_encoding(
            &houses(),
            &Outcome::Regression {
                label: "price".into(),
            },
            false,
        )
        .expect("fit");
        // `beds` passes through; `region` becomes two columns for three
        // categories, with the sorted first as the baseline.
        assert_eq!(
            encoding.feature_names(),
            vec!["beds", "region=north", "region=south"]
        );
        let ColumnEncoding::OneHot { categories, .. } = &encoding.columns[1] else {
            panic!("expected a one-hot, got {:?}", encoding.columns[1]);
        };
        assert_eq!(categories, &["east", "north", "south"]);

        let encoded = apply_encoding(&encoding, &houses()).expect("apply");
        assert_eq!(encoded.features.width(), 3);
        // The baseline row (`east`) is the row of zeros.
        assert_eq!(encoded.features.row(3), Some(&[4.0, 0.0, 0.0][..]));
        assert_eq!(encoded.features.row(0), Some(&[1.0, 1.0, 0.0][..]));
        assert_eq!(encoded.target, Some(vec![100.0, 200.0, 300.0, 400.0]));
    }

    #[test]
    fn a_category_the_fit_never_saw_is_refused_by_name_and_not_encoded_as_zeros() {
        let encoding = fit_encoding(
            &houses(),
            &Outcome::Regression {
                label: "price".into(),
            },
            false,
        )
        .expect("fit");
        let unseen = frame(vec![
            ("price", Column::Float(vec![Some(1.0)])),
            ("beds", Column::Int(vec![Some(2)])),
            ("region", Column::Str(vec![Some("west".into())])),
        ]);
        let err = apply_encoding(&encoding, &unseen).expect_err("unseen category");
        assert!(err.to_string().contains("`west`"), "{err}");
        assert!(err.to_string().contains("`region`"), "{err}");
        // The fit-time path drops it instead, and counts it.
        let dropped = apply_encoding_dropping(&encoding, &unseen).expect("drop");
        assert_eq!(dropped.dropped, 1);
        assert!(dropped.is_empty());
    }

    #[test]
    fn a_null_feature_is_dropped_at_fit_time_and_refused_at_predict_time() {
        let with_null = frame(vec![
            ("price", Column::Float(vec![Some(1.0), Some(2.0)])),
            ("beds", Column::Int(vec![Some(2), None])),
            (
                "region",
                Column::Str(vec![Some("north".into()), Some("north".into())]),
            ),
        ]);
        let encoding = fit_encoding(
            &houses(),
            &Outcome::Regression {
                label: "price".into(),
            },
            false,
        )
        .expect("fit");
        let err = apply_encoding(&encoding, &with_null).expect_err("null");
        assert!(err.to_string().contains("`beds` is null in row 2"), "{err}");
        let dropped = apply_encoding_dropping(&encoding, &with_null).expect("drop");
        assert_eq!(dropped.dropped, 1);
        assert_eq!(dropped.rows, vec![0]);
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn standardisation_uses_the_training_constants_and_a_constant_column_encodes_to_zero() {
        let train = frame(vec![
            ("x", Column::Float(vec![Some(1.0), Some(3.0)])),
            ("k", Column::Float(vec![Some(7.0), Some(7.0)])),
        ]);
        let encoding = fit_encoding(&train, &Outcome::Cluster, true).expect("fit");
        let ColumnEncoding::Standardised { mean, sd, .. } = &encoding.columns[0] else {
            panic!("expected standardised");
        };
        assert!((mean - 2.0).abs() < 1e-12);
        assert!((sd - std::f64::consts::SQRT_2).abs() < 1e-12);
        // A test row outside the training range is scaled by the *training*
        // constants, which is the whole point.
        let test = frame(vec![
            ("x", Column::Float(vec![Some(5.0)])),
            ("k", Column::Float(vec![Some(7.0)])),
        ]);
        let encoded = apply_encoding(&encoding, &test).expect("apply");
        let row = encoded.features.row(0).expect("row");
        assert!((row[0] - (5.0 - 2.0) / std::f64::consts::SQRT_2).abs() < 1e-12);
        assert_eq!(row[1], 0.0);
    }

    #[test]
    fn a_date_column_is_epoch_seconds_and_says_so_on_the_instance() {
        let train = frame(vec![
            ("when", Column::Date(vec![Some(0), Some(86_400)])),
            ("x", Column::Float(vec![Some(1.0), Some(2.0)])),
        ]);
        let encoding = fit_encoding(&train, &Outcome::Cluster, false).expect("fit");
        assert!(matches!(encoding.columns[0], ColumnEncoding::Epoch { .. }));
        // The same column arriving as an integer next time is a different
        // column, and is refused rather than reinterpreted.
        let changed = frame(vec![
            ("when", Column::Int(vec![Some(0)])),
            ("x", Column::Float(vec![Some(1.0)])),
        ]);
        let err = apply_encoding(&encoding, &changed).expect_err("shape changed");
        assert!(err.to_string().contains("was a date"), "{err}");
    }

    #[test]
    fn a_classification_target_is_a_stored_label_map_and_round_trips() {
        let train = frame(vec![
            ("x", Column::Float(vec![Some(1.0), Some(2.0), Some(3.0)])),
            (
                "sold",
                Column::Str(vec![
                    Some("yes".into()),
                    Some("no".into()),
                    Some("yes".into()),
                ]),
            ),
        ]);
        let encoding = fit_encoding(
            &train,
            &Outcome::Classification {
                label: "sold".into(),
                classes: None,
            },
            false,
        )
        .expect("fit");
        assert_eq!(
            encoding.classes(),
            Some(&["no".to_owned(), "yes".to_owned()][..])
        );
        let encoded = apply_encoding(&encoding, &train).expect("apply");
        assert_eq!(encoded.target, Some(vec![1.0, 0.0, 1.0]));
        let target = encoding.target.as_ref().expect("target");
        assert_eq!(target.class(0).expect("class"), "no");
        assert!(target.class(2).is_err());
        // And it survives the instance's JSON column.
        let back = Encoding::from_json(&encoding.to_json().expect("json")).expect("read");
        assert_eq!(back, encoding);
    }

    #[test]
    fn a_column_of_nothing_but_nulls_is_refused_rather_than_dropping_every_row() {
        let train = frame(vec![
            ("x", Column::Float(vec![Some(1.0), Some(2.0)])),
            ("nothing", Column::Null(2)),
        ]);
        let err = fit_encoding(&train, &Outcome::Cluster, false).expect_err("all null");
        assert!(err.to_string().contains("`nothing`"), "{err}");
    }

    #[test]
    fn a_fit_frame_carries_the_label_and_a_predict_frame_does_not() {
        let encoding = fit_encoding(
            &houses(),
            &Outcome::Regression {
                label: "price".into(),
            },
            false,
        )
        .expect("fit");
        let encoded = apply_encoding(&encoding, &houses()).expect("apply");
        assert!(encoded.frame().column("price").is_some());
        assert!(encoded.features_frame().column("price").is_none());
        // And the frame is the matrix, so a provider can go either way.
        let names = encoding.feature_names();
        let back = Matrix::from_frame(&encoded.features_frame(), &names).expect("matrix");
        assert_eq!(back, encoded.features);
    }

    #[test]
    fn a_matrix_whose_values_do_not_fill_its_rows_is_refused() {
        let err =
            Matrix::new(vec!["a".into(), "b".into()], vec![1.0, 2.0, 3.0]).expect_err("ragged");
        assert!(err.to_string().contains("2 columns"), "{err}");
    }
}

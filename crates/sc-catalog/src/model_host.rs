//! Models, from below them (milestone 31 §4): the seam a formula's `predict()`
//! and a code body's model handle reach a fitted model through.
//!
//! `sc-model` sits above this crate — it reads datasets through the row layer —
//! so nothing here can call `predict_subject`. What needs a prediction is
//! [`prefetch_bindings`](crate::prefetch_bindings) (a formula's hoisted
//! `predict("…")`), the read path's calculated fields and the code host, and
//! what all three already hold is a [`Catalog`](crate::Catalog). So the trait is
//! declared here, speaking JSON, and `sc-server`'s `ModelServices` installs its
//! implementation — the arrangement [`module_functions`] already has, for the
//! same reason.
//!
//! [`module_functions`]: crate::Catalog::module_functions

use async_trait::async_trait;
use sc_error::Result;
use sc_types::BasicType;
use serde_json::Value as Json;

/// Which rows of the model's table a prediction is about.
#[derive(Debug, Clone, Copy)]
pub enum PredictRows<'a> {
    /// Primary keys of rows of the table: each is read **through the model's
    /// dataset**, by key and unfiltered, so a join path and an aggregation are
    /// computed as they were at fit time — and a row the dataset's filter
    /// excludes (the unsold house) is still answered.
    Keys(&'a [Json]),
    /// Rows that are not (or not yet) rows of the table: one object each,
    /// keyed by the dataset's column names, supplying every feature.
    Values(&'a [Json]),
}

/// What a caller checking a `predict("…")` before it is saved needs to know
/// about a model — without a provider or a dataset of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSummary {
    /// The model's name.
    pub name: String,
    /// The table its main dataset is over — the only table whose rows it
    /// predicts.
    pub table: String,
    /// The provider it is fitted by.
    pub provider: String,
    /// The basic types a prediction of it can be: one for most providers,
    /// two for a provider that is a regression or a classification depending
    /// on its label's type. **Empty** for one that answers nothing per row.
    pub prediction_types: Vec<BasicType>,
    /// Why it answers nothing per row, as the end of a sentence beginning
    /// "`name` is …" — `None` when it predicts.
    pub no_prediction: Option<String>,
    /// The id of its active fit, `None` when no fit is active — in which
    /// case every prediction fails until one is.
    pub active_fit: Option<String>,
    /// Why a row of `table` is not an input the model understands, when its
    /// dataset changes the grain — "each row is one combination of
    /// `neighbourhood`" (analytics TODO A1.9). `None` when its rows are rows
    /// of `table`.
    pub not_rows_of_table: Option<String>,
}

impl ModelSummary {
    /// Whether the model answers anything per row.
    pub fn predicts(&self) -> bool {
        self.no_prediction.is_none() && !self.prediction_types.is_empty()
    }
}

/// The models of this server, as a formula and a code body reach them.
#[async_trait]
pub trait ModelHost: Send + Sync {
    /// Predict `rows` of `table` with `model`'s active fit, or with the fit
    /// `fit` names, answering one value per row **in the order asked**.
    ///
    /// A value is the plain prediction a row would hold — a number, a class
    /// name, a cluster index or a vector — or, with `detail`,
    /// `{ "value": …, "probability": … }`, the probability present for a
    /// class that has one. A model of another table than `table`, a model with
    /// no active fit, a key no row has and a literal row missing a feature are
    /// each an error naming it.
    async fn predict(
        &self,
        model: &str,
        fit: Option<&str>,
        table: &str,
        rows: PredictRows<'_>,
        detail: bool,
    ) -> Result<Vec<Json>>;

    /// What a formula's save check needs to know about `model`.
    async fn describe(&self, model: &str) -> Result<ModelSummary>;
}

/// The save check every formula that calls `predict("…")` goes through
/// (milestone 31 §4): each model exists, is a model of `table` — the table the
/// formula ranges over — and answers something per row.
///
/// Answers each model's [`ModelSummary`], in the order of the calls, for the
/// checks only some callers make: a calculated field's declared type against
/// [`prediction_types`](ModelSummary::prediction_types), and the notice that
/// a model has no active fit yet.
///
/// Async, and against the catalog rather than a shape, because models are
/// rows: a formula's syntax is checked by `Formula::validate`, and this is the
/// half that needs the models table.
pub async fn check_model_calls(
    catalog: &crate::Catalog,
    table: &str,
    analysis: &sc_expr::Analysis,
) -> Result<Vec<ModelSummary>> {
    if analysis.model_calls.is_empty() {
        return Ok(Vec::new());
    }
    let Some(host) = catalog.model_host() else {
        let call = analysis.model_calls.first().map_or("predict", |c| &c.key);
        return Err(sc_error::Error::invalid(format!(
            "this formula calls `{call}`, and this server has no model support to answer it"
        )));
    };
    let mut out = Vec::with_capacity(analysis.model_calls.len());
    for call in &analysis.model_calls {
        let summary = host.describe(&call.model).await?;
        if summary.table != table {
            return Err(sc_error::Error::invalid(format!(
                "`{}` is a model of `{}`, and this formula is on `{table}`: `{}` predicts the \
                 row the formula ranges over, so the two must be the same table",
                summary.name, summary.table, call.key
            )));
        }
        if let Some(grain) = summary.not_rows_of_table.as_deref() {
            return Err(sc_error::Error::invalid(format!(
                "`{}` cannot predict a row of `{table}`: its dataset changes what a row is \
                 ({grain}), so a row of the table is not an input it understands",
                summary.name
            )));
        }
        if let Some(why) = summary.no_prediction.as_deref() {
            return Err(sc_error::Error::invalid(format!(
                "`{}` is {why}",
                summary.name
            )));
        }
        if !summary.predicts() {
            return Err(sc_error::Error::invalid(format!(
                "`{}` answers nothing per row, so `{}` has nothing to compute",
                summary.name, call.key
            )));
        }
        out.push(summary);
    }
    Ok(out)
}

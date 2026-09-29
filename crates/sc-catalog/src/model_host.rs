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

//! The [`ModelInstance`]: **one fit** — its parameters, its metrics, its
//! encoding and the state that can be applied again (TODO §1, §6, §8, §15).
//!
//! The instances of a model are its history. They are comparable on purpose:
//! same dataset, same split (the hash of §5 keeps every old row on the side it
//! was on), different settings — which is the entire reason anybody looks at two
//! of them.
//!
//! **At most one instance per model is `active`**, and that is what lets a
//! trigger name a *model* rather than a fit: the admin refits, marks the new
//! instance active, and every `predict("…")` follows without being edited.
//!
//! **A fit is a job, not a request** (§8). The row is created with
//! [`FitStatus::Fitting`] and the id returned immediately; a spawned task writes
//! [`Fitted`](FitStatus::Fitted) or [`Failed`](FitStatus::Failed) when it
//! finishes, and the screen polls. There is no in-memory job registry, because
//! the row is the registry — and two consequences follow, both stated rather
//! than discovered: a fit **does not survive a restart** (boot reaps every row
//! still `fitting`, see
//! [`reap_fitting_instances`](crate::reap_fitting_instances)), and there is **no
//! cancel**, because stopping a fit means stopping a `smartcore` call or a
//! Python call mid-flight. The bound that exists is the row cap, and it is the
//! honest one.

use chrono::{DateTime, Utc};
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::model::ModelId;
use crate::provider::ParameterBlock;

/// Identifies one fit: the UUID primary key of its `_fd_model_instances` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct InstanceId(pub Uuid);

impl InstanceId {
    /// Mint an id for a new instance.
    pub fn new() -> InstanceId {
        InstanceId(Uuid::new_v4())
    }
}

impl Default for InstanceId {
    fn default() -> Self {
        InstanceId::new()
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The attribute holding the sentence a failed fit failed with (§15).
///
/// An attribute rather than a column because it is present only on the rows that
/// failed — which is exactly the rule §9 gives for what belongs in `attributes`
/// and what belongs in a column of its own.
pub const ATTR_ERROR: &str = "error";

/// What a fit reaped at boot is failed with (§8).
pub const RESTARTED: &str = "the server restarted while this fit was running";

/// Where a fit has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitStatus {
    /// Running, or spawned and about to. A row in this state at boot never
    /// finishes, and [`reap_fitting_instances`](crate::reap_fitting_instances)
    /// says so rather than leaving it saying `fitting` for ever.
    Fitting,
    /// Finished: it has parameters, metrics, an encoding and a state.
    Fitted,
    /// It did not finish. The sentence is in [`ATTR_ERROR`].
    Failed,
}

impl FitStatus {
    /// The stored spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            FitStatus::Fitting => "fitting",
            FitStatus::Fitted => "fitted",
            FitStatus::Failed => "failed",
        }
    }

    /// Parse a stored spelling, strictly — a status nobody recognises is an
    /// error rather than "probably failed", because the two readings differ in
    /// whether the instance may serve a prediction.
    pub fn parse(s: &str) -> Result<FitStatus> {
        match s {
            "fitting" => Ok(FitStatus::Fitting),
            "fitted" => Ok(FitStatus::Fitted),
            "failed" => Ok(FitStatus::Failed),
            other => Err(Error::invalid(format!(
                "unknown fit status `{other}`; expected `fitting`, `fitted` or `failed`"
            ))),
        }
    }

    /// Whether an instance in this state can answer a prediction.
    pub fn is_usable(&self) -> bool {
        matches!(self, FitStatus::Fitted)
    }
}

impl std::fmt::Display for FitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One fit of one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInstance {
    /// Stable identity: the UUID of its `_fd_model_instances` row.
    pub id: InstanceId,
    /// The model this is a fit of.
    pub model: ModelId,
    /// A short name for the instance list. Empty means "none given", and the
    /// screen falls back to the creation time.
    pub name: String,
    /// Human-readable description (§9).
    pub description: String,
    /// Where the fit got to.
    pub status: FitStatus,
    /// When the fit was started — what the instance list is ordered by, newest
    /// first.
    pub created: DateTime<Utc>,
    /// Whether this is the model's **active** instance: the one a trigger naming
    /// the model predicts with. At most one per model, enforced on save.
    pub active: bool,
    /// The provider's serialised fit, opaque to everything but the provider
    /// (§10). `Json::Null` until the fit finishes.
    pub state: Json,
    /// The provider's parameters, in the order they should be shown (§7).
    pub parameters: Vec<ParameterBlock>,
    /// The **host's** metrics, computed by scoring the fitted state back over
    /// each split (§7).
    ///
    /// Typed as JSON until Phase 3's `Metrics` exists; it is written and read by
    /// nothing else in the meantime, and the column is the same either way.
    pub metrics: Json,
    /// The encoding **fitted with this instance** — the one-hot category lists,
    /// the standardisation constants, the label map (§6).
    ///
    /// The single most load-bearing thing on the row: a prediction is encoded
    /// the way its fit was, or it fails. Re-deriving the one-hot column order
    /// from whatever categories happen to be in the rows being predicted would
    /// put every coefficient against the wrong column and return confident
    /// nonsense. Typed as JSON until Phase 3's `Encoding` exists.
    pub encoding: Json,
    /// The hyperparameter point this fit actually used — one value per
    /// hyperparameter, never a list. The winner of the grid, where there was one.
    pub hyperparameters: Attrs,
    /// Sparse per-instance values (§9): [`ATTR_ERROR`], and the grid's scores.
    pub attributes: Attrs,
}

impl ModelInstance {
    /// A new instance of `model`, **created in [`FitStatus::Fitting`]** — which
    /// is how every fit begins (§8): the row is written first and the id
    /// returned, and the work happens on a spawned task.
    pub fn starting(model: ModelId) -> ModelInstance {
        ModelInstance {
            id: InstanceId::new(),
            model,
            name: String::new(),
            description: String::new(),
            status: FitStatus::Fitting,
            created: Utc::now(),
            active: false,
            state: Json::Null,
            parameters: Vec::new(),
            metrics: Json::Null,
            encoding: Json::Null,
            hyperparameters: Attrs::new(),
            attributes: Attrs::new(),
        }
    }

    /// Set the name.
    pub fn name(mut self, name: impl Into<String>) -> ModelInstance {
        self.name = name.into();
        self
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> ModelInstance {
        self.description = description.into();
        self
    }

    /// Mark this instance active — the model's one instance a trigger predicts
    /// with.
    pub fn activated(mut self) -> ModelInstance {
        self.active = true;
        self
    }

    /// This instance, failed with `why`.
    ///
    /// The sentence goes in [`ATTR_ERROR`], and the state, parameters and
    /// metrics are cleared: a failed fit has nothing to apply and nothing to
    /// read, and leaving a half-written state behind would make an instance that
    /// looks predictable and is not.
    pub fn failed(mut self, why: impl Into<String>) -> ModelInstance {
        self.status = FitStatus::Failed;
        self.active = false;
        self.state = Json::Null;
        self.parameters = Vec::new();
        self.metrics = Json::Null;
        self.attributes
            .insert(ATTR_ERROR.to_owned(), Json::String(why.into()));
        self
    }

    /// Why this fit failed, for one that did.
    pub fn error(&self) -> Option<&str> {
        self.attributes.get(ATTR_ERROR).and_then(Json::as_str)
    }

    /// Whether this instance can answer a prediction.
    pub fn is_usable(&self) -> bool {
        self.status.is_usable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fit_begins_as_a_row_saying_fitting() {
        // §8: the row is the job registry, so it exists before the work does.
        let instance = ModelInstance::starting(ModelId::new());
        assert_eq!(instance.status, FitStatus::Fitting);
        assert!(!instance.is_usable());
        assert!(!instance.active);
        assert_eq!(instance.state, Json::Null);
    }

    #[test]
    fn failing_clears_everything_that_would_make_it_look_predictable() {
        let mut instance = ModelInstance::starting(ModelId::new()).activated();
        instance.state = Json::from("a half-written fit");
        instance.parameters = vec![ParameterBlock::scalar("r2", 0.9)];
        let instance = instance.failed("the dataset selects no rows");
        assert_eq!(instance.status, FitStatus::Failed);
        assert!(!instance.active);
        assert_eq!(instance.state, Json::Null);
        assert!(instance.parameters.is_empty());
        assert_eq!(instance.error(), Some("the dataset selects no rows"));
    }

    #[test]
    fn a_status_nobody_recognises_is_an_error_and_not_a_guess() {
        assert_eq!(FitStatus::parse("fitted").unwrap(), FitStatus::Fitted);
        let err = FitStatus::parse("done").unwrap_err();
        assert!(err.to_string().contains("done"), "{err}");
        assert_eq!(FitStatus::Fitting.to_string(), "fitting");
    }
}

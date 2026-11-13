//! The [`WorkflowEngine`] seam: what a trigger whose body is a **workflow** is
//! run by (design §10.3).
//!
//! Dispatch is layer 6 and the engine is layer 7, so [`TriggerDispatcher`] cannot
//! name `sc-workflow` — the same shape, and the same reason, as
//! [`TableEvents`](sc_catalog::TableEvents): the row layer cannot name a trigger
//! either, so the catalog holds a seam the dispatcher installs itself into. Here
//! the direction is one level up: the dispatcher holds a seam `sc-workflow`
//! installs itself into.
//!
//! One method, because there is one thing dispatch needs of the engine: *start a
//! run of this workflow for this event, and answer what happened*. Everything
//! else the engine does — advancing a run, suspending it, resuming it, retrying
//! it — is driven by the queue and by the admin API, neither of which fires
//! triggers.
//!
//! **Absent, a workflow trigger refuses by name.** A process that installs no
//! engine (a build tool, a unit test) runs every action trigger normally and
//! tells the caller, of a workflow, that nothing here can run one — rather than
//! returning success for a run that was never started, which is the silent
//! failure principle 5 forbids.

use sc_catalog::Catalog;
use sc_error::Result;
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::event::Event;
use crate::trigger::Trigger;

/// What starting a workflow run answers: the run's id and the state it is in
/// after the engine has taken it as far as it can right now.
///
/// **Not the workflow's result.** A workflow body does not return a value at the
/// end of `run` — it may suspend for a day waiting for a person (§10.3) — so what
/// a caller gets back is something *addressable*: an id to look the run up by,
/// and where it has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStarted {
    /// The `_fd_runs` id of the run that was started.
    pub run: Uuid,
    /// The run's state, as `_fd_runs.state` spells it (`running`, `waiting`,
    /// `done`, `failed`, `aborted`).
    ///
    /// A string rather than the `RunState` enum because that type lives beside
    /// the runs table in layer 7, above this crate: the seam exists precisely so
    /// this crate does not name it. The engine's own callers get the enum.
    pub state: String,
}

impl WorkflowStarted {
    /// The answer a directly-run trigger's caller receives.
    pub fn to_json(&self) -> Json {
        json!({ "run": self.run, "state": self.state })
    }
}

/// The engine that runs a trigger whose body is a [`Workflow`](crate::TriggerBody::Workflow).
///
/// Object-safe and installed at boot, exactly as the action registry is: which
/// engine is behind it is a property of the process, not of the trigger.
#[async_trait::async_trait]
pub trait WorkflowEngine: Send + Sync {
    /// Start a run of `trigger`'s workflow for `event`, and answer what happened.
    ///
    /// `chain` is what [`Event::firing`](crate::Event::firing) returned — the
    /// trigger names that led here, including this one — so a run's own writes
    /// carry it and the cascade bound applies through a workflow exactly as it
    /// does through an action.
    ///
    /// The engine pins the run to the workflow's **current version** at this
    /// moment, which is what lets the workflow be edited while the run is still
    /// going (§10.3, decision 2).
    async fn start(
        &self,
        catalog: &Catalog,
        trigger: &Trigger,
        event: &Event,
        chain: Vec<String>,
    ) -> Result<WorkflowStarted>;

    /// A fingerprint of `trigger`'s **current workflow definition**, which an
    /// approval to run as system is bound to with the rest of its
    /// configuration (TODO-type-hardening H8.10,
    /// [`Trigger::configuration_hash`](crate::Trigger::configuration_hash)):
    /// a new version makes the approval lapse. `None` when there is none.
    async fn fingerprint(&self, catalog: &Catalog, trigger: &Trigger) -> Result<Option<String>> {
        let _ = (catalog, trigger);
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_started_run_answers_is_addressable_rather_than_a_result() {
        let started = WorkflowStarted {
            run: Uuid::nil(),
            state: "waiting".to_owned(),
        };
        let json = started.to_json();
        assert_eq!(json["run"], json!(Uuid::nil()));
        assert_eq!(json["state"], json!("waiting"));
    }
}

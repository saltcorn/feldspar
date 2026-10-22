//! The **workflow half of an `_fd_runs` row** (§10.3, §11.4): what a run of a
//! workflow carries beyond what an agent run does, and how the machine's state
//! gets in and out of it.
//!
//! The table is `sc-agent`'s, and deliberately so — a chat session and a durable
//! workflow run are the same problem (a context written after every step and read
//! back to carry on), and giving them two tables would mean writing the resume
//! path twice. What is here is only the part that is a *workflow's*:
//!
//! - the [`WorkflowRun`] machine state, in `context`;
//! - the trigger's **id**, so the pinned version can be found ([`ATTR_WORKFLOW`]);
//! - the **event** that started the run, whole ([`ATTR_EVENT`]), so a run resumed
//!   the next day still reads `row.total` as the row that started it;
//! - the **chain** of triggers that led here ([`ATTR_CHAIN`]), so a step's writes
//!   are bounded by `MAX_DEPTH` exactly as an action's are.
//!
//! ## Why the event is stored rather than rebuilt
//!
//! Because there is nothing to rebuild it from. The row that fired the trigger
//! may have been deleted, updated twice, or never existed (a `login` event's user
//! is not a row of anything). A suspended run that woke up and re-read the world
//! would be a run whose formulas mean something different from what they meant
//! when it started, which is the same mistake as un-pinning its version.
//!
//! [`Event`] is not `Serialize` — it is a live value passed down a call stack —
//! so [`StoredEvent`] is its JSON shape, and the round trip is tested.

use chrono::{DateTime, Utc};
use sc_action::{Event, EventKind, ROLE_PUBLIC, Trigger, TriggerId};
use sc_agent::{Run, RunId, RunKind, RunState};
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::machine::{Conclusion, PendingForm, WorkflowRun};

/// The run attribute holding the **trigger's id**, as a string.
///
/// The run's `subject` is the trigger's *name*, so a finished run stays readable
/// after the trigger it was of is gone; the id is what
/// `_fd_workflow_versions.workflow` is keyed by, and a run that could not name it
/// could not load the version it is pinned to.
pub const ATTR_WORKFLOW: &str = "workflow";

/// The run attribute holding the [`StoredEvent`] the run was started for.
pub const ATTR_EVENT: &str = "event";

/// The run attribute holding the chain of trigger names that led here — what
/// [`Event::firing`] returned, including this trigger.
pub const ATTR_CHAIN: &str = "chain";

/// One event, in the shape a run row can hold it.
///
/// A struct of its own rather than a `serde` derive on [`Event`]: the event model
/// is a live value with a `Result`-returning `firing` and a role that is not part
/// of what a formula reads, and deriving serde on it would make an internal type
/// a wire contract by accident. Here the contract is deliberate and is one
/// conversion each way, tested.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StoredEvent {
    /// The event kind's stored spelling (`insert`, `login`, …).
    pub kind: String,
    /// The table, for a table event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The row the event is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<Json>,
    /// The row as it was before an update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_row: Option<Json>,
    /// The payload, `Json::Null` where there is none.
    #[serde(default)]
    pub payload: Json,
    /// The caller's role.
    #[serde(default = "public_role")]
    pub role: u8,
    /// The caller's fields, or `None` for an anonymous event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<Json>,
    /// The application whose request raised it, `{ id, url, scheme }`
    /// (TODO-payments P1.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<Json>,
    /// The caller's guest session public id (TODO-payments P1.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

fn public_role() -> u8 {
    ROLE_PUBLIC
}

impl StoredEvent {
    /// Everything of `event` a run needs to keep.
    ///
    /// The chain is **not** kept here: it is the run's own ([`ATTR_CHAIN`]),
    /// because what a step's write descends from is the trigger that started the
    /// run and the step, not the event's own history rewritten.
    pub fn of(event: &Event) -> StoredEvent {
        StoredEvent {
            kind: event.kind.as_str().to_owned(),
            channel: event.channel.clone(),
            row: event.row.clone(),
            old_row: event.old_row.clone(),
            payload: event.payload.clone(),
            role: event.role,
            user: event.user.clone(),
            app: event.app.as_ref().map(sc_catalog::CallingApp::to_json),
            session: event.session.clone(),
        }
    }

    /// The event again, for the formulas of a step that is running now.
    ///
    /// Strict about the kind, like every other stored spelling: an event kind
    /// nothing implements must not be read as some other kind, because `row` and
    /// `old` are in scope exactly where the kind says they are.
    pub fn to_event(&self) -> Result<Event> {
        let mut event = Event::new(EventKind::parse(&self.kind)?);
        event.channel = self.channel.clone();
        event.row = self.row.clone();
        event.old_row = self.old_row.clone();
        event.payload = self.payload.clone();
        event.role = self.role;
        event.user = self.user.clone();
        event.app = self
            .app
            .as_ref()
            .and_then(sc_catalog::CallingApp::from_json);
        event.session = self.session.clone();
        Ok(event)
    }
}

/// A new run of `trigger`'s workflow, pinned to `version`, for `event`.
///
/// `chain` is what [`Event::firing`] returned — the trigger names that led here,
/// including this one — and it is kept on the run rather than derived later,
/// because a run that resumes tomorrow has no call stack to derive it from and a
/// cascade bound that only held while the request was open would not be a bound.
pub fn new_run(
    trigger: &Trigger,
    version: u32,
    event: &Event,
    chain: Vec<String>,
    state: &WorkflowRun,
) -> Run {
    let now = Utc::now();
    let mut run = Run {
        id: RunId::new(),
        kind: RunKind::Workflow,
        subject: trigger.name.clone(),
        description: describe(trigger, event),
        state: RunState::Running,
        error: None,
        context: serde_json::to_value(state).unwrap_or(Json::Null),
        user: event_user(event),
        subject_version: Some(version),
        // Runnable now: this run wants the engine at once, which is what a
        // non-null `wake_at` in the past means to the queue.
        wake_at: Some(now),
        lease_until: None,
        claimed_by: None,
        attributes: Attrs::new(),
        created_at: now,
        updated_at: now,
    };
    run.attributes
        .insert(ATTR_WORKFLOW.to_owned(), trigger.id.to_string().into());
    run.attributes.insert(
        ATTR_EVENT.to_owned(),
        serde_json::to_value(StoredEvent::of(event)).unwrap_or(Json::Null),
    );
    run.attributes.insert(
        ATTR_CHAIN.to_owned(),
        Json::Array(chain.into_iter().map(Json::String).collect()),
    );
    run
}

/// One line for a run list: what happened, and to what.
fn describe(trigger: &Trigger, event: &Event) -> String {
    match &event.channel {
        Some(channel) => format!("`{}` on {} of {channel}", trigger.name, event.kind),
        None => format!("`{}` on {}", trigger.name, event.kind),
    }
}

/// The caller's own id, for `_fd_runs.user_id` — who the run is on behalf of.
fn event_user(event: &Event) -> Option<Uuid> {
    event
        .user
        .as_ref()
        .and_then(|u| u.get("id"))
        .and_then(Json::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
}

/// The machine state this run carries.
///
/// Strict, for the reason an agent run's is: a run read as something other than
/// what was stored would be resumed as something other than what was running.
pub fn run_state(run: &Run) -> Result<WorkflowRun> {
    if run.kind != RunKind::Workflow {
        return Err(Error::invalid(format!(
            "run {} is a {} run, not a workflow run",
            run.id, run.kind
        )));
    }
    serde_json::from_value(run.context.clone()).map_err(|e| {
        Error::invalid(format!(
            "run {}: its stored workflow state is unreadable: {e}",
            run.id
        ))
    })
}

/// The trigger whose workflow this run is of.
pub fn run_workflow_id(run: &Run) -> Result<TriggerId> {
    let raw = run
        .attributes
        .get(ATTR_WORKFLOW)
        .and_then(Json::as_str)
        .ok_or_else(|| {
            Error::invalid(format!(
                "run {} does not say which workflow it is of",
                run.id
            ))
        })?;
    Uuid::parse_str(raw)
        .map(TriggerId)
        .map_err(|e| Error::invalid(format!("run {}: `{raw}` is not a workflow id: {e}", run.id)))
}

/// The version this run is pinned to.
pub fn run_version(run: &Run) -> Result<u32> {
    run.subject_version.ok_or_else(|| {
        Error::invalid(format!(
            "run {} is a workflow run and is pinned to no version",
            run.id
        ))
    })
}

/// The event this run was started for.
pub fn run_event(run: &Run) -> Result<Event> {
    let stored = run.attributes.get(ATTR_EVENT).ok_or_else(|| {
        Error::invalid(format!(
            "run {} does not carry the event that started it",
            run.id
        ))
    })?;
    let stored: StoredEvent = serde_json::from_value(stored.clone()).map_err(|e| {
        Error::invalid(format!(
            "run {}: its stored event is unreadable: {e}",
            run.id
        ))
    })?;
    stored.to_event()
}

/// The chain of triggers that led to this run — the bound a step's own writes
/// descend from.
pub fn run_chain(run: &Run) -> Vec<String> {
    match run.attributes.get(ATTR_CHAIN) {
        Some(Json::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        // A run with no chain recorded is one whose writes descend from the run
        // itself, which is the honest reading and still bounded: the subject is
        // pushed on at the step.
        _ => vec![run.subject.clone()],
    }
}

/// Write the machine's state into the run, and move the row's own columns to
/// match it — the one place `context`, `state`, `error` and `wake_at` are set,
/// because they cannot be allowed to disagree.
///
/// A context that says the run finished beside a state that says `running` is a
/// row the queue would pick up forever.
pub fn record(run: &mut Run, state: &WorkflowRun, now: DateTime<Utc>) {
    run.context = serde_json::to_value(state).unwrap_or(Json::Null);
    run.updated_at = now;
    match state.conclusion() {
        Some(Conclusion::Finished) => {
            run.state = RunState::Done;
            run.error = None;
            run.wake_at = None;
        }
        Some(Conclusion::Failed { step, error }) => {
            run.state = RunState::Failed;
            run.error = Some(format!("step `{step}`: {error}"));
            run.wake_at = None;
        }
        None if state.is_suspended() => {
            run.state = RunState::Waiting;
            // `None` here is what "no clock will make this runnable" means: a
            // form nobody has answered yet. A `Wait` and a retry both name an
            // instant.
            run.wake_at = state.wake_at();
        }
        None => {
            run.state = RunState::Running;
            run.wake_at = Some(now);
        }
    }
}

/// The form this run is waiting for somebody to fill in, if it is waiting for
/// one (§10.3, phase 4.2).
///
/// Read out of the machine state rather than kept in a column of its own,
/// because the state is where it already is and a second copy would be a second
/// thing to keep in step. What the *row* says about it is `state = waiting` with
/// `wake_at` NULL, which is the query "who is waiting on a person" needs and is
/// all it needs.
pub fn run_pending_form(run: &Run) -> Result<Option<PendingForm>> {
    Ok(run_state(run)?.pending_form().cloned())
}

/// The step this run is on, or `None` once it is over — what a run list shows
/// beside its state.
pub fn run_current_step(run: &Run) -> Option<String> {
    run_state(run)
        .ok()
        .and_then(|state| state.current_step().map(str::to_owned))
}

/// Stop a run for good, at somebody's request rather than because of anything it
/// did (§10.3, phase 4.4).
///
/// `aborted` is a fifth state beside `running`, `waiting`, `done` and `failed`,
/// and it is deliberately not `failed`: nothing went wrong, an admin decided.
/// The reason goes on the same column a failure's does, prefixed with what it
/// is, because the column is "why did this run stop" and an operator reading it
/// wants one answer.
///
/// The machine state is left exactly as it was. That is the record of where the
/// run had got to, which is what somebody looking at a cancelled run wants to
/// see — and the queue cannot pick the row up again, because `aborted` is not
/// one of the states it looks for.
pub fn mark_aborted(run: &mut Run, reason: Option<&str>, now: DateTime<Utc>) {
    run.state = RunState::Aborted;
    run.error = Some(match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(reason) => format!("cancelled: {reason}"),
        None => "cancelled".to_owned(),
    });
    run.wake_at = None;
    run.updated_at = now;
    release(run);
}

/// Let go of the lease: nothing is working on this run any more.
///
/// Called on the same write that records a run's suspension or its end, so a
/// finished run never sits leased and a suspended one is claimable the instant
/// its deadline arrives rather than when the lease it no longer needs runs out.
pub fn release(run: &mut Run) {
    run.lease_until = None;
    run.claimed_by = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn insert_event() -> Event {
        Event::new(EventKind::Insert)
            .on("orders")
            .row(json!({ "id": 7, "total": 120 }))
            .payload(json!({ "note": "from the shop" }))
            .caller(
                40,
                Some(json!({ "id": Uuid::nil().to_string(), "email": "a@b.c" })),
            )
            .from_origin(sc_catalog::Origin {
                app: Some(sc_catalog::CallingApp {
                    id: Uuid::nil(),
                    url: "https://shop.example.com".into(),
                    scheme: Some("myshop".into()),
                }),
                session: Some("gs_1".into()),
            })
    }

    #[test]
    fn an_event_survives_the_round_trip_through_a_run_row() {
        let event = insert_event();
        let stored = StoredEvent::of(&event);
        let json = serde_json::to_value(&stored).unwrap();
        let back: StoredEvent = serde_json::from_value(json).unwrap();
        let again = back.to_event().unwrap();
        // Everything a formula can read is the same on the other side; only the
        // chain is the run's own rather than the event's.
        assert_eq!(again.kind, event.kind);
        assert_eq!(again.channel, event.channel);
        assert_eq!(again.row, event.row);
        assert_eq!(again.old_row, event.old_row);
        assert_eq!(again.payload, event.payload);
        // And where it came from (TODO-payments P1.3, P1.5).
        assert_eq!(again.app, event.app);
        assert_eq!(again.session, event.session);
        assert_eq!(again.role, event.role);
        assert_eq!(again.user, event.user);
    }

    #[test]
    fn an_unknown_event_kind_is_refused_rather_than_guessed() {
        let stored = StoredEvent {
            kind: "teatime".to_owned(),
            ..StoredEvent::default()
        };
        let err = stored.to_event().unwrap_err();
        assert!(err.to_string().contains("teatime"), "{err}");
    }

    #[test]
    fn a_new_run_carries_its_workflow_its_event_and_its_chain() {
        let trigger = Trigger::with_body(
            TriggerId::new(),
            "approve",
            EventKind::Insert,
            sc_action::TriggerBody::Workflow,
        )
        .on("orders");
        let event = insert_event();
        let workflow = crate::Workflow::empty(trigger.id);
        let state = WorkflowRun::new(&workflow);
        let run = new_run(&trigger, 3, &event, vec!["approve".to_owned()], &state);
        assert_eq!(run.kind, RunKind::Workflow);
        assert_eq!(run.subject, "approve");
        assert_eq!(run.subject_version, Some(3));
        assert_eq!(run.state, RunState::Running);
        // Runnable now, and leased by nobody.
        assert!(run.wake_at.is_some() && run.lease_until.is_none());
        assert_eq!(run_workflow_id(&run).unwrap(), trigger.id);
        assert_eq!(run_version(&run).unwrap(), 3);
        assert_eq!(run_chain(&run), vec!["approve".to_owned()]);
        assert_eq!(run_event(&run).unwrap().row, event.row);
        assert_eq!(run_state(&run).unwrap(), state);
        // The caller's own id, so an audit can answer "as whom?".
        assert_eq!(run.user, Some(Uuid::nil()));
    }

    #[test]
    fn recording_moves_the_row_to_match_the_state() {
        let trigger = Trigger::with_body(
            TriggerId::new(),
            "w",
            EventKind::None,
            sc_action::TriggerBody::Workflow,
        );
        let workflow = crate::Workflow::empty(trigger.id);
        let mut state = WorkflowRun::new(&workflow);
        let mut run = new_run(&trigger, 1, &Event::new(EventKind::None), vec![], &state);
        let now = Utc::now();

        // The empty start step runs and the run finishes.
        state.next_step(&workflow, now);
        record(&mut run, &state, now);
        assert_eq!(run.state, RunState::Done);
        assert_eq!(run.wake_at, None);
        assert_eq!(run.error, None);
        assert_eq!(run_state(&run).unwrap(), state);
    }
}

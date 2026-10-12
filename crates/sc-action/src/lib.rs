//! Actions and triggers (layer 6; technical design §10.1–§10.2).
//!
//! This crate owns the **event → action** half of the trigger system: what can
//! happen ([`Event`]), what can be done about it ([`Action`]), what one run of an
//! action can see ([`ActionContext`]), which actions exist ([`ActionRegistry`]),
//! and the [`Trigger`] that binds an event to a configured action — its storage in
//! `_fd_triggers` ([`save_trigger`] and friends), its
//! [validation](validate_trigger), and the cached live set ([`Triggers`]) an event
//! is matched against. The dispatch that actually fires one lands in a later
//! phase; the event model is deliberately independent of the trigger, so the row
//! layer can emit events without knowing whether anything listens.
//!
//! It holds no actions itself — the core set is a crate of its own
//! (`sc-core-actions`, above the row layer, because three of them write rows) —
//! but it does hold the machinery *every* action's configuration goes through:
//! what a configured formula may name ([`action_shape`], [`EVENT_SCOPE`]), what an
//! event binds it to ([`EventBindings`]), and how a setting is parsed
//! ([`config_str`], [`formula_map`], [`check_formula`] and friends). So a plugin's
//! action needs this crate and nothing else.
//!
//! Three design commitments are expressed as types here rather than as prose:
//!
//! - **An action is one elementary step.** Control flow is the workflow engine's
//!   (§10.3), which is why [`Action::run`] returns a value and takes no branch:
//!   the small built-in set GOALS asks for is a consequence of that split.
//! - **A workflow is a trigger body, not a new entity.** [`TriggerBody`] is
//!   `Action { action, configuration }` or `Workflow`, so a workflow inherits
//!   this crate's event, `only_if`, role floor, enabled flag, timing and
//!   exposure with no second copy of any of them. What runs one is
//!   [`WorkflowEngine`], a seam the dispatcher holds and layer 7 implements.
//! - **Configuration is data.** An action declares its settings as
//!   [`FormField`](sc_types::FormField)s, so the admin UI renders a form for an
//!   action it has never heard of and save-time validation checks the values
//!   against the same declaration.
//! - **The caller travels as JSON.** An event carries a role and the user's
//!   fields, not a `sc_auth::User` — that object is exactly what the formula
//!   language binds `user` to, and it keeps this crate implementable from a guest
//!   language.
//!
//! Layering: this is the lowest crate that both holds a [`Catalog`](sc_catalog::Catalog)
//! and needs the formula language, and nothing below it may depend on it. Two
//! crates above it do. `sc-core-actions` holds the built-in actions that **write
//! rows** (`insert_row`/`update_rows`/`delete_rows`), which must go through the
//! row layer so a trigger's write is coerced, validated and observable exactly
//! like an API caller's; the rule is "an action lives where the things it needs
//! are". `sc-api` projects an application's exposed triggers as endpoints and runs
//! them through [`TriggerDispatcher`] (§13.4). Emitting runs the other way
//! regardless: the row layer raises an event through a seam the catalog holds
//! (Phase 4), never by calling in here.

mod action;
mod dispatch;
mod engine;
mod event;
mod observer;
mod registry;
mod schedule;
mod scheduler;
mod scope;
mod store;
mod trigger;
mod triggers;
mod validate;

pub use action::{Action, ActionContext, ConfigCheck, FileUrls};
pub use dispatch::{
    ActionServices, TestRun, TriggerDispatcher, TriggerRun, fire_trigger, fire_trigger_in,
    fire_trigger_with,
};
pub use engine::{WorkflowEngine, WorkflowStarted};
pub use event::{EVENT_KINDS, Event, EventKind, MAX_DEPTH, ROLE_PUBLIC};
pub use observer::TriggerObserver;
pub use registry::ActionRegistry;
pub use schedule::{ATTR_DAY_OF_WEEK, ATTR_HOUR, ATTR_MINUTE, OFTEN_MINUTES, Schedule, day_name};
pub use scheduler::Scheduler;
pub use scope::{
    EVENT_SCOPE, EventBindings, action_shape, check_formula, check_template, config_flag,
    config_str, event_formula_value, formula_map, optional_formula, optional_template,
    render_event_template, required_formula, required_template, step_shape, template_scope,
    typed_value,
};
pub use store::{
    COL_ACTION, COL_BODY, TRIGGERS_TABLE, bootstrap_triggers, delete_trigger, list_triggers,
    load_trigger, load_trigger_by_name, record_trigger_run, save_trigger,
};
pub use trigger::{ATTR_ENABLED, Trigger, TriggerBody, TriggerId};
pub use triggers::{TriggerIssue, Triggers};
pub use validate::{trigger_shape, validate_trigger};

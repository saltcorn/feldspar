//! The [`Trigger`]: what binds an [`Event`](crate::Event) to a configured action
//! (design §10.2).
//!
//! Pure data, like an [`Application`](sc_app::Application) — the row it is stored
//! as lives in [`store`](crate::store), the validation it must pass in
//! [`validate`](crate::validate), and the firing in the dispatcher. A trigger
//! knows nothing about how it is run, which is what lets the same record grow a
//! workflow body later (§10.3) without reshaping anything around it.
//!
//! One trigger = one event + one **body**, and a body is either one configured
//! action or a workflow ([`TriggerBody`], §10.3). Not a list of actions: a
//! sequence of steps is a *workflow*, and conflating the two is what made v1's
//! execution path hard to reason about. Two triggers on the same event is how
//! you get two things done today.
//!
//! The body being an enum is what makes a workflow **a trigger body rather than
//! a new top-level entity**: a workflow inherits its event, its `only_if`, its
//! `min_role`, its enabled flag, its periodic timing, its exposure through an
//! application, and the admin's Run button, with no second copy of any of them
//! to keep in step.

use chrono::{DateTime, Utc};
use sc_error::{Error, Result};
use sc_types::Attrs;
use sc_types::Role;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::event::EventKind;

/// Identifies a trigger: the UUID primary key of its `_fd_triggers` row (§9).
///
/// Serialises as the bare UUID — no wrapper object — because that is what it is
/// wherever it crosses a boundary: a workflow version's stored JSON names the
/// trigger it belongs to, and the API answers the same string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TriggerId(pub Uuid);

impl TriggerId {
    /// Mint an id for a new trigger.
    pub fn new() -> TriggerId {
        TriggerId(Uuid::new_v4())
    }
}

impl Default for TriggerId {
    fn default() -> Self {
        TriggerId::new()
    }
}

impl std::fmt::Display for TriggerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The attribute holding whether a trigger is enabled — sparse (§9's rule),
/// because the answer is `true` for almost every trigger almost always.
pub const ATTR_ENABLED: &str = "enabled";

/// What a trigger does when it fires: one configured action, or a **workflow**.
///
/// The discriminator is stored in `_fd_triggers.body`, and reading is strict in
/// both directions — a `workflow` body naming an action is as much an error as
/// an `action` body without one — because the two are run by different engines
/// and a half-understood body is one that would run the wrong thing.
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerBody {
    /// One registered action with its own configuration: what every trigger was
    /// before §10.3, and still the common case.
    Action {
        /// The registered name of the action to run.
        action: String,
        /// That action's configuration, keyed by its `config_spec` field names.
        configuration: Attrs,
    },
    /// A **workflow**: a program of steps, versioned in `_fd_workflow_versions`
    /// and advanced durably by the engine (§10.3). Nothing is stored on the
    /// trigger row itself, because the steps are the version's.
    Workflow,
}

impl TriggerBody {
    /// The stored spelling of an action body's discriminator.
    pub const ACTION: &'static str = "action";
    /// The stored spelling of a workflow body's discriminator.
    pub const WORKFLOW: &'static str = "workflow";

    /// The stored discriminator.
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerBody::Action { .. } => TriggerBody::ACTION,
            TriggerBody::Workflow => TriggerBody::WORKFLOW,
        }
    }

    /// Rebuild a body from the three columns that carry it, **strictly**.
    ///
    /// Every disagreement between the discriminator and the other two is an
    /// error naming what is wrong, never a body silently repaired: a `workflow`
    /// row that still carries an action name is a row somebody edited half way,
    /// and running its action would be running the thing the admin replaced.
    pub fn parse(body: &str, action: Option<&str>, configuration: Attrs) -> Result<TriggerBody> {
        let action = action.map(str::trim).filter(|a| !a.is_empty());
        match body {
            TriggerBody::ACTION => match action {
                Some(action) => Ok(TriggerBody::Action {
                    action: action.to_owned(),
                    configuration,
                }),
                None => Err(Error::invalid(
                    "an `action` body must name the action it runs",
                )),
            },
            TriggerBody::WORKFLOW => match action {
                Some(action) => Err(Error::invalid(format!(
                    "a `workflow` body has no action, but `{action}` was given as one"
                ))),
                None if !configuration.is_empty() => Err(Error::invalid(
                    "a `workflow` body has no configuration; its steps are the \
                     workflow version's",
                )),
                None => Ok(TriggerBody::Workflow),
            },
            other => Err(Error::invalid(format!(
                "unknown trigger body `{other}`; expected `{}` or `{}`",
                TriggerBody::ACTION,
                TriggerBody::WORKFLOW
            ))),
        }
    }

    /// The action this body runs, or `None` for a workflow.
    pub fn action(&self) -> Option<&str> {
        match self {
            TriggerBody::Action { action, .. } => Some(action.trim()),
            TriggerBody::Workflow => None,
        }
    }

    /// The action's configuration, or `None` for a workflow.
    pub fn configuration(&self) -> Option<&Attrs> {
        match self {
            TriggerBody::Action { configuration, .. } => Some(configuration),
            TriggerBody::Workflow => None,
        }
    }

    /// Whether this is a workflow body.
    pub fn is_workflow(&self) -> bool {
        matches!(self, TriggerBody::Workflow)
    }
}

/// One event bound to one body: a configured action, or a workflow.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    /// Stable identity: the UUID of its `_fd_triggers` row.
    pub id: TriggerId,
    /// The unique, human-facing name. It is what an application's exposed subset,
    /// an API path (`POST /actions/{name}`) and a "run" button all reference, so
    /// it is the key, and renaming one breaks those references deliberately
    /// rather than silently.
    pub name: String,
    /// Human-readable description (§9 requires one on every metadata row; the
    /// empty string means "none given").
    pub description: String,
    /// The event that fires it.
    pub when: EventKind,
    /// What the event is about: the table name for a table event, `None`
    /// otherwise. Validation enforces that correspondence.
    pub channel: Option<String>,
    /// The **"only if"** formula: a predicate over the affected row (and `user`,
    /// `row`, `old`) that must be true for the action to run. `None` means
    /// always. Table events only — nothing else has a row to test.
    pub only_if: Option<String>,
    /// What fires: one configured action, or a workflow (§10.3).
    pub body: TriggerBody,
    /// The role floor for running this trigger through an API. `None` is
    /// **admin-only** (the safe reading: a trigger nobody has thought about the
    /// access of is not public), which Phase 7's endpoint projection applies.
    pub min_role: Option<Role>,
    /// Sparse per-trigger values (§9): [`ATTR_ENABLED`] and the periodic timing
    /// ([`Schedule`](crate::Schedule)).
    pub attributes: Attrs,
    /// When the scheduler last fired this trigger, for a periodic one (§10.2).
    ///
    /// **Not part of the admin's definition**, which is why it is a field of its
    /// own rather than an attribute and why [`save_trigger`](crate::save_trigger)
    /// never writes it: it is the scheduler's bookkeeping, written only by
    /// [`record_trigger_run`](crate::record_trigger_run). An admin editing a
    /// trigger's action at 3pm must not thereby tell the scheduler the daily job
    /// ran at 3pm — or, worse, that it never ran at all.
    pub last_run_at: Option<DateTime<Utc>>,
}

impl Trigger {
    /// A **new** trigger with a fresh id: an event, an action, and nothing else
    /// set.
    pub fn new(name: impl Into<String>, when: EventKind, action: impl Into<String>) -> Trigger {
        Trigger::with_id(TriggerId::new(), name, when, action)
    }

    /// Reconstruct an existing trigger, which already has an id — what
    /// [`load`](crate::load_trigger) and an update path use.
    pub fn with_id(
        id: TriggerId,
        name: impl Into<String>,
        when: EventKind,
        action: impl Into<String>,
    ) -> Trigger {
        Trigger::with_body(
            id,
            name,
            when,
            TriggerBody::Action {
                action: action.into(),
                configuration: Attrs::new(),
            },
        )
    }

    /// A **new workflow** trigger with a fresh id: an event, a workflow body, and
    /// nothing else set. Its steps live in `_fd_workflow_versions` under its id.
    pub fn workflow(name: impl Into<String>, when: EventKind) -> Trigger {
        Trigger::with_body(TriggerId::new(), name, when, TriggerBody::Workflow)
    }

    /// A trigger with the body given — the general constructor the two above are
    /// spellings of, and what a reader rebuilding a stored row uses.
    pub fn with_body(
        id: TriggerId,
        name: impl Into<String>,
        when: EventKind,
        body: TriggerBody,
    ) -> Trigger {
        Trigger {
            id,
            name: name.into(),
            description: String::new(),
            when,
            channel: None,
            only_if: None,
            body,
            min_role: None,
            attributes: Attrs::new(),
            last_run_at: None,
        }
    }

    /// The action this trigger runs, or `None` for a workflow body.
    pub fn action(&self) -> Option<&str> {
        self.body.action()
    }

    /// The action's configuration, or `None` for a workflow body.
    pub fn configuration(&self) -> Option<&Attrs> {
        self.body.configuration()
    }

    /// Whether this trigger's body is a workflow.
    pub fn is_workflow(&self) -> bool {
        self.body.is_workflow()
    }

    /// Replace the action this trigger runs, keeping its configuration.
    ///
    /// Refused on a workflow body rather than silently turning a workflow into
    /// an action: the steps would still be stored, still be versioned, and no
    /// longer run — which is the kind of quiet loss an admin discovers weeks
    /// later.
    pub fn set_action(&mut self, action: impl Into<String>) -> Result<()> {
        match &mut self.body {
            TriggerBody::Action { action: slot, .. } => {
                *slot = action.into();
                Ok(())
            }
            TriggerBody::Workflow => Err(Error::invalid(format!(
                "trigger `{}` is a workflow; its steps are edited as a workflow, \
                 not by naming an action",
                self.name
            ))),
        }
    }

    /// Replace the action's configuration. Refused on a workflow body, for the
    /// reason [`set_action`](Trigger::set_action) is.
    pub fn set_configuration(&mut self, configuration: Attrs) -> Result<()> {
        match &mut self.body {
            TriggerBody::Action {
                configuration: slot,
                ..
            } => {
                *slot = configuration;
                Ok(())
            }
            TriggerBody::Workflow => Err(Error::invalid(format!(
                "trigger `{}` is a workflow; a workflow has no action \
                 configuration — its steps carry their own",
                self.name
            ))),
        }
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> Trigger {
        self.description = description.into();
        self
    }

    /// Set the channel — the table, for a table event.
    pub fn on(mut self, channel: impl Into<String>) -> Trigger {
        self.channel = Some(channel.into());
        self
    }

    /// Set the "only if" formula.
    pub fn only_if(mut self, formula: impl Into<String>) -> Trigger {
        self.only_if = Some(formula.into());
        self
    }

    /// Set one configuration value — an **action body's** builder, as the two
    /// below it are.
    ///
    /// A workflow body has no configuration and is left unchanged; the assertion
    /// makes that a loud mistake in a debug build rather than a value that
    /// quietly went nowhere. It cannot be reached from a stored trigger, because
    /// a body is chosen before its settings are.
    pub fn config(mut self, key: impl Into<String>, value: impl Into<Json>) -> Trigger {
        debug_assert!(
            !self.body.is_workflow(),
            "a workflow body has no action configuration"
        );
        if let TriggerBody::Action { configuration, .. } = &mut self.body {
            configuration.insert(key.into(), value.into());
        }
        self
    }

    /// Set the whole configuration of an action body.
    pub fn with_configuration(mut self, configuration: Attrs) -> Trigger {
        debug_assert!(
            !self.body.is_workflow(),
            "a workflow body has no action configuration"
        );
        if let TriggerBody::Action {
            configuration: slot,
            ..
        } = &mut self.body
        {
            *slot = configuration;
        }
        self
    }

    /// Set the role floor for running this trigger through an API.
    pub fn min_role(mut self, role: Role) -> Trigger {
        self.min_role = Some(role);
        self
    }

    /// Set a timing attribute (see [`Schedule`](crate::Schedule)), returning
    /// `self` for chaining.
    pub fn timing(mut self, key: impl Into<String>, value: u32) -> Trigger {
        self.attributes.insert(key.into(), Json::from(value));
        self
    }

    /// Whether the trigger fires. Absent means **enabled**: a trigger is created
    /// to run, and the attribute exists so an admin can switch one off without
    /// deleting it (and losing its configuration).
    pub fn is_enabled(&self) -> bool {
        self.attributes
            .get(ATTR_ENABLED)
            .and_then(Json::as_bool)
            .unwrap_or(true)
    }

    /// Enable or disable the trigger. Enabling **removes** the key rather than
    /// storing `true`, so an untouched trigger carries no residue (§9's sparse
    /// rule, as `TableMeta`'s accessors do).
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled {
            self.attributes.remove(ATTR_ENABLED);
        } else {
            self.attributes
                .insert(ATTR_ENABLED.into(), Json::Bool(false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_trigger_is_built_from_an_event_and_an_action() {
        let t = Trigger::new("audit", EventKind::Insert, "insert_row")
            .description("write an audit row")
            .on("books")
            .only_if("pages > 100")
            .config("table", "audit")
            .min_role(Role::ADMIN);
        assert_eq!(t.name, "audit");
        assert_eq!(t.when, EventKind::Insert);
        assert_eq!(t.channel.as_deref(), Some("books"));
        assert_eq!(t.only_if.as_deref(), Some("pages > 100"));
        assert_eq!(t.configuration().unwrap()["table"], json!("audit"));
        assert_eq!(t.action(), Some("insert_row"));
        assert!(!t.is_workflow());
        assert_eq!(t.min_role, Some(Role::ADMIN));
        // Nothing was set for these, and they read as their defaults.
        assert!(t.attributes.is_empty());
        assert!(t.is_enabled());
    }

    #[test]
    fn enabling_leaves_no_residue_but_disabling_is_recorded() {
        let mut t = Trigger::new("t", EventKind::None, "fetch");
        t.set_enabled(false);
        assert!(!t.is_enabled());
        assert_eq!(t.attributes[ATTR_ENABLED], json!(false));
        t.set_enabled(true);
        assert!(t.is_enabled());
        assert!(!t.attributes.contains_key(ATTR_ENABLED));
    }

    #[test]
    fn a_workflow_body_carries_no_action_and_refuses_to_be_given_one() {
        let mut t = Trigger::workflow("approve_order", EventKind::Insert).on("orders");
        assert!(t.is_workflow());
        assert_eq!(t.action(), None);
        assert_eq!(t.configuration(), None);
        // Everything else a trigger is, a workflow still is — that is the whole
        // point of it being a body rather than a new entity.
        assert_eq!(t.when, EventKind::Insert);
        assert_eq!(t.channel.as_deref(), Some("orders"));
        assert!(t.is_enabled());
        // And it is not quietly convertible into an action trigger.
        let err = t.set_action("insert_row").unwrap_err().to_string();
        assert!(err.contains("is a workflow"), "{err}");
        let err = t.set_configuration(Attrs::new()).unwrap_err().to_string();
        assert!(err.contains("is a workflow"), "{err}");
    }

    #[test]
    fn a_stored_body_is_read_strictly_in_both_directions() {
        let action = TriggerBody::parse("action", Some("fetch"), Attrs::new()).unwrap();
        assert_eq!(action.action(), Some("fetch"));
        assert_eq!(action.as_str(), "action");
        let workflow = TriggerBody::parse("workflow", None, Attrs::new()).unwrap();
        assert_eq!(workflow, TriggerBody::Workflow);
        assert_eq!(workflow.as_str(), "workflow");

        // An action body with no action, a workflow body with one, a workflow
        // body carrying settings, and a body nobody has heard of.
        let err = TriggerBody::parse("action", None, Attrs::new()).unwrap_err();
        assert!(err.to_string().contains("must name the action"), "{err}");
        let err = TriggerBody::parse("action", Some("  "), Attrs::new()).unwrap_err();
        assert!(err.to_string().contains("must name the action"), "{err}");
        let err = TriggerBody::parse("workflow", Some("fetch"), Attrs::new()).unwrap_err();
        assert!(err.to_string().contains("has no action"), "{err}");
        let mut config = Attrs::new();
        config.insert("url".into(), json!("https://example.com"));
        let err = TriggerBody::parse("workflow", None, config).unwrap_err();
        assert!(err.to_string().contains("no configuration"), "{err}");
        let err = TriggerBody::parse("agent", Some("fetch"), Attrs::new()).unwrap_err();
        assert!(err.to_string().contains("unknown trigger body"), "{err}");
    }

    #[test]
    fn a_fresh_trigger_has_its_own_id() {
        let a = Trigger::new("a", EventKind::None, "fetch");
        let b = Trigger::new("b", EventKind::None, "fetch");
        assert_ne!(a.id, b.id);
        // Reconstructing keeps the id — the update path depends on it.
        let same = Trigger::with_id(a.id, "a", EventKind::None, "fetch");
        assert_eq!(same.id, a.id);
    }
}

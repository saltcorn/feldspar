//! The action registry: which actions exist, and how a stored name becomes one.
//!
//! A map rather than a `match` (the shape `sc-files`' backend registry and
//! `sc-app`'s framework registry use) because an action is a **trait object with
//! behaviour**, and because the set is meant to grow from outside this crate: a
//! plugin — including a guest-language one behind an `sc-code` shim (§2.1) —
//! registers its actions into the same map the built-ins live in, and the admin
//! UI renders every one of them from its [`config_spec`](Action::config_spec)
//! with no per-action special case.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{merge_secrets, redact_attrs};

use crate::action::Action;
use crate::trigger::Trigger;

/// The actions available to triggers, by name.
///
/// Built once (at boot, and in tests) and then read-only, so it is cheap to share
/// behind an `Arc`. Ordered by name (`BTreeMap`) so the admin UI's picker and
/// every error message that lists the alternatives are stable rather than
/// hash-ordered.
#[derive(Clone, Default)]
pub struct ActionRegistry {
    actions: BTreeMap<String, Arc<dyn Action>>,
}

impl ActionRegistry {
    /// An empty registry.
    ///
    /// The starting point for everything: `sc_core_actions::builtin_actions()`
    /// fills one with the core set (design §10.1 — deliberately few), a plugin
    /// adds its own to that, and a test starts from one with just the action it is
    /// about. This crate deliberately registers **nothing** itself: it defines
    /// what an action *is*, and an empty-but-named constructor pretending to be
    /// the built-in set is exactly the placeholder that goes stale.
    pub fn new() -> ActionRegistry {
        ActionRegistry::default()
    }

    /// Register `action` under its own [`name`](Action::name).
    ///
    /// A duplicate name is refused rather than overwritten: two implementations
    /// claiming one name means every stored trigger referencing it is ambiguous,
    /// and silently letting the last registration win would make which one runs
    /// depend on load order.
    pub fn register(&mut self, action: Arc<dyn Action>) -> Result<()> {
        let name = action.name().to_owned();
        if self.actions.contains_key(&name) {
            return Err(Error::config(format!(
                "two actions are registered under the name `{name}`"
            )));
        }
        self.actions.insert(name, action);
        Ok(())
    }

    /// The action registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Action>> {
        self.actions.get(name)
    }

    /// The action registered under `name`, or a configuration error naming it and
    /// the registered alternatives.
    ///
    /// This is the lookup every trigger goes through — on save (Phase 2) and at
    /// fire time — because a trigger whose action nothing implements must be
    /// reported, never silently skipped.
    pub fn require(&self, name: &str) -> Result<&Arc<dyn Action>> {
        self.get(name).ok_or_else(|| {
            Error::config(format!(
                "unknown action `{name}`; the registered actions are {}",
                if self.actions.is_empty() {
                    "(none)".to_owned()
                } else {
                    self.names().join(", ")
                }
            ))
        })
    }

    /// Every registered action's name, in order — what the admin UI lists.
    pub fn names(&self) -> Vec<&str> {
        self.actions.keys().map(String::as_str).collect()
    }

    /// Every registered action, in name order — what the admin API's
    /// `listActions` projects into names, descriptions and config specs.
    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn Action>> {
        self.actions.values()
    }

    /// How many actions are registered.
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// `trigger` as a client sees it: every [`secret`](sc_types::FormField::secret)
    /// setting of its action replaced by the sentinel (§11.1), against the spec
    /// the trigger's own table gives ([`config_spec_for`](Action::config_spec_for)).
    ///
    /// For every surface that shows a trigger to somebody — the admin API, the
    /// agent's tools — and for none that keeps it: a backup writes the stored
    /// value, because a restore has nothing else to put back. A workflow body,
    /// and an action nothing implements, are returned as they are.
    pub fn redact_trigger(&self, catalog: &Catalog, trigger: &Trigger) -> Trigger {
        let mut out = trigger.clone();
        if let (Some(configuration), Some(action)) = (
            trigger.configuration(),
            trigger.action().and_then(|a| self.get(a)),
        ) {
            let spec = action.config_spec_for(catalog, trigger.channel.as_deref());
            // Infallible: the body is an action, or there would be no configuration.
            let _ = out.set_configuration(redact_attrs(&spec, configuration));
        }
        out
    }

    /// The other half of [`redact_trigger`](ActionRegistry::redact_trigger):
    /// wherever `submitted` sends a secret setting back as the sentinel, put
    /// back what `stored` holds (or drop it when nothing is stored), so that
    /// saving a trigger the editor showed masked keeps its password.
    pub fn merge_trigger_secrets(
        &self,
        catalog: &Catalog,
        stored: Option<&Trigger>,
        submitted: &mut Trigger,
    ) -> Result<()> {
        let Some(action) = submitted.action().and_then(|a| self.get(a)) else {
            return Ok(());
        };
        let Some(configuration) = submitted.configuration() else {
            return Ok(());
        };
        let spec = action.config_spec_for(catalog, submitted.channel.as_deref());
        // A stored value belongs to the stored *action*: switching a trigger
        // from one action to another carries no secret across.
        let previous = stored
            .filter(|t| t.action() == submitted.action())
            .and_then(Trigger::configuration)
            .cloned()
            .unwrap_or_default();
        let merged = merge_secrets(&spec, &previous, configuration);
        submitted.set_configuration(merged)
    }
}

impl std::fmt::Debug for ActionRegistry {
    /// Names only: an action is a trait object with no meaningful `Debug`, and the
    /// names are the whole of what a reader wants from a registry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ActionRegistry")
            .field(&self.names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ActionContext;
    use sc_types::FormField;
    use serde_json::Value as Json;

    /// Two trivial actions, differing only in name, to exercise ordering and the
    /// duplicate refusal.
    struct Named(&'static str);

    #[async_trait::async_trait]
    impl Action for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test action"
        }
        fn config_spec(&self) -> Vec<FormField> {
            Vec::new()
        }
        async fn run(&self, _ctx: &mut ActionContext<'_>) -> Result<Json> {
            Ok(Json::Null)
        }
    }

    fn registry(names: &[&'static str]) -> ActionRegistry {
        let mut reg = ActionRegistry::new();
        for name in names {
            reg.register(Arc::new(Named(name))).unwrap();
        }
        reg
    }

    #[test]
    fn an_action_is_found_by_the_name_it_declares() {
        let reg = registry(&["insert_row", "fetch"]);
        assert_eq!(reg.len(), 2);
        assert!(!reg.is_empty());
        assert_eq!(reg.require("fetch").unwrap().name(), "fetch");
        assert!(reg.get("insert_row").is_some());
        // Ordered by name, not by registration order, so every listing is stable.
        assert_eq!(reg.names(), vec!["fetch", "insert_row"]);
        assert_eq!(reg.all().count(), 2);
    }

    #[test]
    fn an_unknown_action_names_itself_and_the_alternatives() {
        let reg = registry(&["insert_row", "fetch"]);
        let err = reg.require("send_email").err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("send_email"), "{msg}");
        assert!(msg.contains("insert_row") && msg.contains("fetch"), "{msg}");
        // A trigger naming an action nobody implements is the app builder's
        // problem to fix, not a Saltcorn bug (§16's split).
        assert_eq!(err.kind(), sc_error::ErrorKind::Application);
    }

    #[test]
    fn an_empty_registry_still_explains_itself() {
        let reg = ActionRegistry::new();
        assert!(reg.is_empty());
        let msg = reg.require("insert_row").err().unwrap().to_string();
        assert!(
            msg.contains("insert_row") && msg.contains("(none)"),
            "{msg}"
        );
    }

    #[test]
    fn a_duplicate_name_is_refused_rather_than_overwriting() {
        let mut reg = registry(&["fetch"]);
        let err = reg.register(Arc::new(Named("fetch"))).unwrap_err();
        assert!(err.to_string().contains("fetch"), "{err}");
        // The first registration is still the one that answers.
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn the_debug_rendering_is_the_names() {
        let reg = registry(&["fetch", "insert_row"]);
        assert_eq!(
            format!("{reg:?}"),
            "ActionRegistry([\"fetch\", \"insert_row\"])"
        );
    }
}

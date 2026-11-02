//! The trait registry: which agent traits exist, and how a stored name becomes
//! one.
//!
//! `sc-action`'s `ActionRegistry`'s twin, and deliberately so — a
//! map rather than a `match`, ordered by name, duplicates refused — because the
//! reasons are the same: the set is meant to grow from outside this crate, and
//! the admin UI must render a trait it has never heard of from its
//! [`config_spec`](crate::AgentTrait::config_spec) alone.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_error::{Error, Result};

use crate::agent_trait::AgentTrait;
use crate::view::{HostCapabilities, ViewServices};

/// The traits available to agents, by name.
///
/// Built once (at boot, and in tests) and then read-only, so it is cheap to share
/// behind an `Arc`. Ordered by name so the admin UI's picker and every error
/// message that lists the alternatives are stable rather than hash-ordered.
#[derive(Clone, Default)]
pub struct AgentRegistry {
    traits: BTreeMap<String, Arc<dyn AgentTrait>>,
    /// What the server found on its host, for configuration checks that depend
    /// on it (a browser for `view_app`).
    host: HostCapabilities,
    /// The server's preview and browser capabilities, installed once it has
    /// built them (TODO §7b). Shared by every clone, so a runner made from any
    /// handle on this registry reaches them.
    view: Arc<ViewServices>,
}

impl AgentRegistry {
    /// An empty registry.
    ///
    /// `sc_agent_builtins::builtin_traits()` fills one with the core set, a plugin
    /// adds its own, and a test starts from one with just the trait it is about.
    /// This crate registers **nothing** itself: it defines what a trait *is*, and
    /// an empty-but-named constructor pretending to be the built-in set is
    /// exactly the placeholder that goes stale.
    pub fn new() -> AgentRegistry {
        AgentRegistry::default()
    }

    /// The same registry on a host with `host`'s capabilities.
    pub fn with_host(mut self, host: HostCapabilities) -> AgentRegistry {
        self.host = host;
        self
    }

    /// What the server found on its host.
    pub fn host(&self) -> &HostCapabilities {
        &self.host
    }

    /// The server's preview and browser capabilities, where it has installed
    /// them.
    pub fn view_services(&self) -> &Arc<ViewServices> {
        &self.view
    }

    /// Register `trait_` under its own [`name`](AgentTrait::name).
    ///
    /// A duplicate name is refused rather than overwritten: two implementations
    /// claiming one name means every stored agent referencing it is ambiguous,
    /// and letting the last registration win would make which one runs depend on
    /// load order.
    pub fn register(&mut self, trait_: Arc<dyn AgentTrait>) -> Result<()> {
        let name = trait_.name().to_owned();
        if self.traits.contains_key(&name) {
            return Err(Error::config(format!(
                "two agent traits are registered under the name `{name}`"
            )));
        }
        self.traits.insert(name, trait_);
        Ok(())
    }

    /// The trait registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn AgentTrait>> {
        self.traits.get(name)
    }

    /// The trait registered under `name`, or a configuration error naming it and
    /// the registered alternatives.
    ///
    /// This is the lookup every agent goes through — on save and on load —
    /// because an agent naming a trait nothing implements must be reported, never
    /// silently run without it. An agent quietly missing the tool it was built
    /// around would answer confidently from nothing.
    pub fn require(&self, name: &str) -> Result<&Arc<dyn AgentTrait>> {
        self.get(name).ok_or_else(|| {
            Error::config(format!(
                "unknown agent trait `{name}`; the registered traits are {}",
                if self.traits.is_empty() {
                    "(none)".to_owned()
                } else {
                    self.names().join(", ")
                }
            ))
        })
    }

    /// Every registered trait's name, in order — what the admin UI lists.
    pub fn names(&self) -> Vec<&str> {
        self.traits.keys().map(String::as_str).collect()
    }

    /// Every registered trait, in name order — what the admin API's `agentTraits`
    /// projects into names, descriptions and config specs.
    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn AgentTrait>> {
        self.traits.values()
    }

    /// How many traits are registered.
    pub fn len(&self) -> usize {
        self.traits.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.traits.is_empty()
    }
}

impl std::fmt::Debug for AgentRegistry {
    /// Names only: a trait is an object with no meaningful `Debug`, and the names
    /// are the whole of what a reader wants from a registry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AgentRegistry").field(&self.names()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_trait::TraitContext;
    use sc_llm::ToolSpec;
    use sc_types::{Attrs, FormField};
    use serde_json::Value as Json;

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl AgentTrait for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test trait"
        }
        fn config_spec(&self) -> Vec<FormField> {
            Vec::new()
        }
        fn tools(&self, _cx: &crate::ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn call(
            &self,
            _config: &Attrs,
            _tool: &str,
            _args: &Json,
            _ctx: &mut TraitContext<'_>,
        ) -> Result<Json> {
            Ok(Json::Null)
        }
    }

    fn registry(names: &[&'static str]) -> AgentRegistry {
        let mut reg = AgentRegistry::new();
        for name in names {
            reg.register(Arc::new(Named(name))).unwrap();
        }
        reg
    }

    #[test]
    fn a_trait_is_found_by_the_name_it_declares() {
        let reg = registry(&["query_table", "run_trigger"]);
        assert_eq!(reg.len(), 2);
        assert!(!reg.is_empty());
        assert_eq!(reg.require("run_trigger").unwrap().name(), "run_trigger");
        assert!(reg.get("query_table").is_some());
        assert_eq!(reg.names(), vec!["query_table", "run_trigger"]);
        assert_eq!(reg.all().count(), 2);
    }

    #[test]
    fn an_unknown_trait_names_itself_and_the_alternatives() {
        let reg = registry(&["query_table"]);
        let err = reg.require("read_file").err().unwrap();
        let msg = err.to_string();
        assert!(
            msg.contains("read_file") && msg.contains("query_table"),
            "{msg}"
        );
    }

    #[test]
    fn an_empty_registry_still_explains_itself() {
        let reg = AgentRegistry::new();
        let msg = reg.require("query_table").err().unwrap().to_string();
        assert!(
            msg.contains("query_table") && msg.contains("(none)"),
            "{msg}"
        );
    }

    #[test]
    fn a_duplicate_name_is_refused_rather_than_overwriting() {
        let mut reg = registry(&["query_table"]);
        let err = reg.register(Arc::new(Named("query_table"))).unwrap_err();
        assert!(err.to_string().contains("query_table"), "{err}");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn the_debug_rendering_is_the_names() {
        let reg = registry(&["query_table", "run_trigger"]);
        assert_eq!(
            format!("{reg:?}"),
            "AgentRegistry([\"query_table\", \"run_trigger\"])"
        );
    }
}

//! Bringing the agents up at boot (§11.2, §11.4).
//!
//! The twin of [`triggers`](crate::triggers), and for the same reason: the trait
//! seam is in `sc-agent` (layer 7), the built-in traits are in `sc-agent-builtins`
//! (layer 9, above the row layer so a trait's write goes through `sc-api`), and
//! neither knows about the other. This is where they are put together, once, by
//! the process that is going to serve requests.
//!
//! What is assembled here is [`AgentServices`]: the trait registry every agent's
//! configuration is validated against, and **how an agent's provider is
//! connected**. The second is a seam rather than a call because every test of
//! the chat socket has to drive a whole turn without a vendor and without
//! spending a token (decision 7) — see
//! [`ProviderConnector`](sc_agent::ProviderConnector), which lives in `sc-agent`
//! because the `run_agent` action (§11.5) needs the same seam for the same
//! reason and sits below this crate.

use std::sync::Arc;

use sc_agent::{
    AgentRegistry, AgentTrait, LiveRegistry, ProviderConnector, StoredProviders, bootstrap_agents,
    bootstrap_runs,
};
use sc_catalog::Catalog;
use sc_error::{Context, Result};

/// Everything the agent surface needs that is not a request: which traits exist,
/// and how to reach a model.
///
/// Cloneable and shared, like the trigger dispatcher, because the admin API's
/// handlers and the chat socket must validate against the *same* registry — an
/// agent that saved against one trait set and ran against another would be
/// refused in one place and accepted in the other.
///
/// The registry is the built-in traits plus whatever the installed modules
/// supply, so it is **replaced** on every module change
/// ([`install_module_traits`](Self::install_module_traits)), and every clone
/// sees the replacement.
#[derive(Clone)]
pub struct AgentServices {
    /// What a module change rebuilds the registry from: the built-ins, on this
    /// host, with the view services every rebuilt copy shares.
    base: Arc<AgentRegistry>,
    registry: LiveRegistry,
    providers: Arc<dyn ProviderConnector>,
}

impl AgentServices {
    /// Services over `registry`, connecting providers from their stored records.
    pub fn new(registry: Arc<AgentRegistry>) -> AgentServices {
        AgentServices {
            base: Arc::clone(&registry),
            registry: LiveRegistry::new(registry),
            providers: Arc::new(StoredProviders),
        }
    }

    /// Resolve providers some other way — the seam [`ProviderConnector`]
    /// describes.
    pub fn with_providers(mut self, providers: Arc<dyn ProviderConnector>) -> AgentServices {
        self.providers = providers;
        self
    }

    /// The traits an agent's configuration is validated against, as they
    /// stand now.
    pub fn registry(&self) -> Arc<AgentRegistry> {
        self.registry.current()
    }

    /// The server's preview and browser capabilities — the same ones in every
    /// registry a module change builds, since each is a copy of the base.
    pub fn view_services(&self) -> &Arc<sc_agent::ViewServices> {
        self.base.view_services()
    }

    /// The handle on the registry that follows module changes — what the
    /// `run_agent` action holds, so it runs an agent over a module's trait
    /// installed after the actions were built.
    pub fn live_registry(&self) -> &LiveRegistry {
        &self.registry
    }

    /// Replace the registry with the built-ins plus `traits` — the agent traits
    /// the modules supply, as a module change has just loaded them. A trait
    /// whose name is taken (by a built-in, or a module that came first) is not
    /// registered; the sentence saying so is returned, and the rest are.
    pub fn install_module_traits(&self, traits: Vec<Arc<dyn AgentTrait>>) -> Vec<String> {
        let mut registry = (*self.base).clone();
        let mut problems = Vec::new();
        for trait_ in traits {
            let name = trait_.name().to_owned();
            if let Err(e) = registry.register(trait_) {
                problems.push(format!("the agent trait `{name}` is not available: {e}"));
            }
        }
        self.registry.set(Arc::new(registry));
        problems
    }

    /// How this server connects an agent's provider.
    pub fn providers(&self) -> &Arc<dyn ProviderConnector> {
        &self.providers
    }
}

/// Assemble the agent services a server runs with: the built-in traits, and the
/// two tables an agent and its runs are stored in.
///
/// Fails only on what a server must not start without — the built-in trait set
/// not assembling (a duplicate registration, which is a bug) or the tables not
/// being creatable. A **stored agent** that does not validate is not one of
/// those: it is dropped from the live set with its reason reported (by
/// [`report_unusable_agents`], once the modules' traits are in), stays listed
/// and stays editable, exactly as a trigger that does not validate is.
pub async fn install_agents(catalog: &Arc<Catalog>) -> Result<AgentServices> {
    install_agents_on(catalog, sc_agent::HostCapabilities::default()).await
}

/// [`install_agents`] on a host with `host`'s capabilities: what the boot path
/// found (a browser for `view_app`), so an agent whose grant the host cannot
/// honour is reported and dropped from the live set.
pub async fn install_agents_on(
    catalog: &Arc<Catalog>,
    host: sc_agent::HostCapabilities,
) -> Result<AgentServices> {
    bootstrap_agents(catalog)
        .await
        .context("ensuring the agents table exists")?;
    bootstrap_runs(catalog)
        .await
        .context("ensuring the runs table exists")?;
    sc_agent_builtins::bootstrap_agent_memory(catalog)
        .await
        .context("ensuring the agent memory table exists")?;
    let registry = sc_agent_builtins::builtin_traits()
        .context("registering the built-in traits")?
        .with_host(host);
    Ok(AgentServices::new(Arc::new(registry)))
}

/// Say which stored agents are not usable, and why — once the registry is
/// whole: after the modules have loaded, so an agent given a module's trait is
/// not reported as naming an unknown one at every boot.
pub async fn report_unusable_agents(catalog: &Catalog, services: &AgentServices) -> Result<()> {
    for issue in sc_agent::validate::Agents::load(catalog, &services.registry())
        .await?
        .issues()
    {
        eprintln!(
            "feldspar: agent `{}` is stored but not usable: {}",
            issue.agent, issue.problem
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_services_carry_the_built_in_traits_and_the_stored_connector() {
        let services = AgentServices::new(Arc::new(
            sc_agent_builtins::builtin_traits().expect("the built-in traits assemble"),
        ));
        assert!(services.registry().get("query_table").is_some());
        assert!(services.registry().get("run_trigger").is_some());
    }

    /// A module's trait, standing in for one an agent skill becomes.
    struct Named(&'static str);

    #[async_trait::async_trait]
    impl AgentTrait for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "from a module"
        }
        fn config_spec(&self) -> Vec<sc_types::FormField> {
            Vec::new()
        }
        fn tools(
            &self,
            _: &sc_agent::ToolsContext<'_>,
            _: &sc_types::Attrs,
        ) -> Vec<sc_llm::ToolSpec> {
            Vec::new()
        }
        async fn call(
            &self,
            _: &sc_types::Attrs,
            _: &str,
            _: &serde_json::Value,
            _: &mut sc_agent::TraitContext<'_>,
        ) -> Result<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
    }

    #[test]
    fn a_module_change_replaces_the_traits_for_every_holder() {
        let services = AgentServices::new(Arc::new(
            sc_agent_builtins::builtin_traits().expect("the built-in traits assemble"),
        ));
        let held = services.clone();
        let live = services.live_registry().clone();
        let problems = services.install_module_traits(vec![
            Arc::new(Named("run_bash")),
            // A built-in's name stays the built-in's.
            Arc::new(Named("query_table")),
        ]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("query_table"), "{problems:?}");
        assert!(held.registry().get("run_bash").is_some());
        assert!(live.current().get("run_bash").is_some());
        assert_eq!(
            held.registry()
                .require("query_table")
                .unwrap()
                .description(),
            sc_agent_builtins::builtin_traits()
                .unwrap()
                .require("query_table")
                .unwrap()
                .description()
        );
        // Removed with its module: the next change installs the set without it.
        assert!(services.install_module_traits(Vec::new()).is_empty());
        assert!(held.registry().get("run_bash").is_none());
        assert!(held.registry().get("query_table").is_some());
    }
}

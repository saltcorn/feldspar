//! Validating an [`Agent`] before it is stored — and again when it is loaded.
//!
//! Every way an agent can be wrong is checked in one place, and checked **on
//! save**, because that is when the admin is standing in front of the form: a
//! provider that is not connected, a trait nothing implements, a configuration
//! the trait does not declare, a table the trait names that no longer exists, two
//! enabled traits whose tools would collide. Discovering any of those mid-answer
//! means an agent that fails — or, worse, quietly answers without the tool it was
//! built around.
//!
//! The same function runs at **load** ([`Agents`]), where an agent that no longer
//! validates is dropped from the live set with its reason kept. That is the
//! fail-closed reading a trigger's load takes, for the same reason.

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::validate_attrs;

use crate::agent::{Agent, EnabledTrait, ModelRef, ModelRole};
use crate::agent_trait::{ToolsContext, TraitCheck};
use crate::registry::AgentRegistry;
use crate::store::list_agents;

/// The longest tool name both vendors accept, and the character set they accept
/// it in.
///
/// Checked here rather than left to the provider because the failure is
/// otherwise a 400 in the middle of a conversation, naming a tool the admin never
/// typed — the name was derived from their table's, and the connection between
/// the two is exactly what an error at the vendor cannot make.
const MAX_TOOL_NAME: usize = 64;

/// Check everything about `agent` that can be checked without running it.
///
/// Called by [`save_agent`](crate::save_agent) and by [`Agents::load`] . The
/// error names the agent first, then the problem, because the admin is looking at
/// a list of agents when they see it.
pub async fn validate_agent(
    catalog: &Catalog,
    registry: &AgentRegistry,
    agent: &Agent,
) -> Result<()> {
    let name = agent.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("an agent needs a name"));
    }
    let problem = |msg: String| Error::invalid(format!("agent `{name}`: {msg}"));

    if agent.provider.trim().is_empty() {
        return Err(problem("no LLM provider is set".to_owned()));
    }
    // The agent's own model and each role's (TODO §3) resolve the same way. A
    // role left unset is the agent's own model, which is already checked.
    let (backend, executor) = resolve_model(
        catalog,
        &ModelRef::new(agent.provider.trim(), agent.model.as_deref()),
    )
    .await
    .map_err(|e| reword(e, &problem, None))?;
    let mut priced = vec![(ModelRole::Executor, executor.prices())];
    for role in [ModelRole::Strong, ModelRole::Cheap] {
        let named = agent
            .configured_role(role)
            .map_err(|e| problem(e.to_string()))?;
        if let Some(named) = named {
            let (_, model) = resolve_model(catalog, &named)
                .await
                .map_err(|e| reword(e, &problem, Some(role)))?;
            priced.push((role, model.prices()));
        }
    }
    // Each model a person may pick (AS10.4) resolves as a role's does, and is
    // priced against the budgets as the executor it stands in for.
    for picked in agent
        .pickable_models()
        .map_err(|e| problem(e.to_string()))?
    {
        let (_, model) = resolve_model(catalog, &picked.model_ref())
            .await
            .map_err(|e| match e.repr() {
                sc_error::Repr::NotFound(msg) => {
                    problem(format!("its pickable model `{}`: {msg}", picked.value()))
                }
                _ => e,
            })?;
        priced.push((ModelRole::Executor, model.prices()));
    }

    // A cost budget cannot be kept against a model whose cost is unknown, so
    // it is refused here rather than silently never reached.
    if agent.max_cost().is_some() {
        let unpriced: Vec<String> = priced
            .iter()
            .filter(|(_, prices)| prices.input.is_none() || prices.output.is_none())
            .map(|(role, _)| format!("`{role}`"))
            .collect();
        if !unpriced.is_empty() {
            return Err(problem(format!(
                "a cost budget needs every model the agent calls to have input and \
                 output prices, and the {} model has none; set the prices on the \
                 model, or clear `max_cost`",
                unpriced.join(" and ")
            )));
        }
    }

    // A rate per dollar is kept against the models' prices, the way a cost
    // budget is; a rate per token is the fallback for a model without them.
    if let Some(meter) = agent.meter()
        && meter.needs_prices()
    {
        let unpriced: Vec<String> = priced
            .iter()
            .filter(|(_, prices)| !prices.is_priced())
            .map(|(role, _)| format!("`{role}`"))
            .collect();
        if !unpriced.is_empty() {
            return Err(problem(format!(
                "charging LLM credits per dollar needs every model the agent calls to \
                 have input and output prices, and the {} model has none; set the prices \
                 on the model, or charge per 1,000 tokens as well",
                unpriced.join(" and ")
            )));
        }
    }

    if let Some(role) = agent.min_role
        && !(1..=100).contains(&role)
    {
        return Err(problem(format!(
            "`min_role` must be a role between 1 and 100, got {role}"
        )));
    }

    // Each enabled trait: the name resolves, the values are of the declared
    // shapes, and then the trait's own check — which is the only one that can
    // know whether the table it names still exists.
    //
    // Numbered, because a trait may be enabled more than once and "trait
    // `query_table`" would otherwise not say which of them is wrong.
    let capabilities = executor.capabilities(&backend);
    let mut tool_names: Vec<(String, String)> = Vec::new();
    for (i, enabled) in agent.traits.iter().enumerate() {
        let position = i + 1;
        let where_ = |msg: String| {
            problem(format!(
                "trait {position} (`{}`): {msg}",
                enabled.trait_.trim()
            ))
        };
        validate_trait(catalog, registry, name, enabled)
            .await
            .map_err(|e| where_(e.to_string()))?;
        let trait_ = registry.require(enabled.trait_.trim())?;

        // Over every mode: a tool offered only while planning must still not
        // collide with another trait's. The same tool in two modes is one tool.
        let mut offered: Vec<sc_llm::ToolSpec> = Vec::new();
        for mode in crate::run::RunMode::ALL {
            let cx = ToolsContext::new(catalog, mode, &capabilities);
            for tool in trait_.tools(&cx, &enabled.config) {
                if !offered.iter().any(|t| t.name == tool.name) {
                    offered.push(tool);
                }
            }
        }
        for tool in offered {
            if tool.name.is_empty()
                || tool.name.len() > MAX_TOOL_NAME
                || !tool
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(where_(format!(
                    "the tool name `{}` is not one a provider will accept \
                     (up to {MAX_TOOL_NAME} letters, digits, `_` or `-`)",
                    tool.name
                )));
            }
            // A collision is refused here, where it is fixable by renaming or
            // dropping one of the two, rather than discovered when the model
            // picks whichever of them the request happened to carry last.
            if let Some((_, other)) = tool_names.iter().find(|(name, _)| *name == tool.name) {
                return Err(where_(format!(
                    "its tool `{}` has the same name as one from {other}; \
                     each enabled trait's tools must be distinguishable",
                    tool.name
                )));
            }
            tool_names.push((tool.name, format!("trait {position}")));
        }
    }

    Ok(())
}

/// Check one enabled trait on its own: the name resolves, the values are of the
/// declared shapes, and the trait's own check passes. What
/// [`validate_agent`] runs for each trait, without the collision check that
/// needs the others — so a caller can tell *which* traits keep an agent from
/// saving (a restore drops those and keeps the agent).
pub async fn validate_trait(
    catalog: &Catalog,
    registry: &AgentRegistry,
    agent: &str,
    enabled: &EnabledTrait,
) -> Result<()> {
    let trait_ = registry.require(enabled.trait_.trim())?;
    validate_attrs(&trait_.config_spec(), &enabled.config)?;
    trait_
        .validate_config(&TraitCheck {
            catalog,
            config: &enabled.config,
            agent,
            host: registry.host(),
        })
        .await
}

/// The provider's backend and the model row `named` resolves to.
async fn resolve_model(
    catalog: &Catalog,
    named: &ModelRef,
) -> Result<(String, sc_llm::LlmModelDef)> {
    // The provider must be *connected*, not merely named: an agent pointing at a
    // provider nobody configured cannot answer, and the admin who deleted the
    // provider is the one who needs telling.
    let provider = named.provider.trim();
    let Some(provider_def) = sc_llm::load_llm_provider_by_name(catalog, provider).await? else {
        return Err(Error::not_found(format!(
            "no LLM provider named `{provider}`"
        )));
    };
    // And the model must be a row under it: the one named, or the provider's
    // default when none is. The two failures are fixed in different places, so
    // the message says which it is.
    let model = sc_llm::require_llm_model(catalog, &provider_def, named.model.as_deref()).await?;
    Ok((provider_def.backend, model))
}

/// A resolution failure as the agent's problem, naming the role when it is one.
fn reword(e: Error, problem: &impl Fn(String) -> Error, role: Option<ModelRole>) -> Error {
    match e.repr() {
        sc_error::Repr::NotFound(msg) => problem(match role {
            Some(role) => format!("its `{role}` role: {msg}"),
            None => msg.clone(),
        }),
        _ => e,
    }
}

/// Why one stored agent is not in the live set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIssue {
    /// The agent's name, as stored.
    pub agent: String,
    /// What is wrong with it, in the words the validator used.
    pub problem: String,
}

/// The agents that can run, plus the ones that cannot and why.
///
/// `sc-action`'s `Triggers`'s twin. An agent that fails validation is
/// **not** deleted and **not** hidden: it stays stored, stays listed and stays
/// editable, because editing it is the repair. What it does not do is answer.
#[derive(Debug, Clone, Default)]
pub struct Agents {
    agents: Vec<Agent>,
    issues: Vec<AgentIssue>,
}

impl Agents {
    /// An empty set — a catalog with no `_fd_agents` table, and the starting
    /// point for a test.
    pub fn empty() -> Agents {
        Agents::default()
    }

    /// Load and validate every stored agent.
    ///
    /// A catalog with no `_fd_agents` table yields an empty set rather than an
    /// error: that table's absence *means* "no agents have ever been defined".
    pub async fn load(catalog: &Catalog, registry: &AgentRegistry) -> Result<Agents> {
        if catalog.get(crate::AGENTS_TABLE)?.is_none() {
            return Ok(Agents::empty());
        }
        let mut out = Agents::default();
        for agent in list_agents(catalog).await? {
            match validate_agent(catalog, registry, &agent).await {
                Ok(()) => out.agents.push(agent),
                Err(e) => out.issues.push(AgentIssue {
                    agent: agent.name.clone(),
                    problem: e.to_string(),
                }),
            }
        }
        Ok(out)
    }

    /// Reload in place, so a live handle picks up a save or a delete.
    pub async fn reload(&mut self, catalog: &Catalog, registry: &AgentRegistry) -> Result<()> {
        *self = Agents::load(catalog, registry).await?;
        Ok(())
    }

    /// The agents that can run, ordered by name.
    pub fn all(&self) -> &[Agent] {
        &self.agents
    }

    /// The stored agents that cannot, with their reasons.
    pub fn issues(&self) -> &[AgentIssue] {
        &self.issues
    }

    /// The agent named `name`, if it is in the live set.
    pub fn by_name(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// The agent named `name`, or an error that distinguishes the two ways it can
    /// be missing — never defined, or defined and invalid.
    ///
    /// The distinction is the whole point: "no agent named `librarian`" sends the
    /// admin looking for a typo, and "agent `librarian` is not usable: no LLM
    /// provider named `anthropic`" sends them to the thing that is actually
    /// broken.
    pub fn require(&self, name: &str) -> Result<&Agent> {
        if let Some(agent) = self.by_name(name) {
            return Ok(agent);
        }
        match self.issues.iter().find(|i| i.agent == name) {
            Some(issue) => Err(Error::invalid(format!(
                "agent `{name}` is not usable: {}",
                issue.problem
            ))),
            None => Err(Error::not_found(format!("no agent named `{name}`"))),
        }
    }
}

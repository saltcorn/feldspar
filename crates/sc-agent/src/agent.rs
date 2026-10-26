//! The [`Agent`]: a provider, a system prompt and a set of enabled traits
//! (design §11.2).
//!
//! Pure data, like a `Trigger` — the row it is stored as
//! lives in [`store`](crate::store), the validation it must pass in
//! [`validate`](crate::validate), and the running in [`machine`](crate::machine)
//! and [`driver`](crate::driver). An agent knows nothing about how it is run,
//! which is what lets the same record be chatted with, fired from a trigger and
//! (later) resumed by the workflow engine without reshaping anything.
//!
//! **An agent is its own record, not a trigger's configuration** (§11's third
//! separation). `run_agent` is the one registered action that runs one, so an
//! agent becomes a trigger body through machinery that already exists and stays
//! editable as an agent rather than as a blob inside a trigger's
//! `configuration`.

use sc_types::Attrs;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

/// Identifies an agent: the UUID primary key of its `_fd_agents` row (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentId(pub Uuid);

impl AgentId {
    /// Mint an id for a new agent.
    pub fn new() -> AgentId {
        AgentId(Uuid::new_v4())
    }
}

impl Default for AgentId {
    fn default() -> Self {
        AgentId::new()
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The attribute holding the sampling temperature — sparse (§9's rule): absent
/// means the provider's own default, which is not the same as any number we
/// could store in its place.
pub const ATTR_TEMPERATURE: &str = "temperature";
/// The attribute capping the tokens one *answer* may generate.
pub const ATTR_MAX_TOKENS: &str = "max_tokens";
/// The attribute capping how many times one run may go round the loop.
pub const ATTR_MAX_STEPS: &str = "max_steps";
/// The attribute naming the **strong** role's model: `{"provider", "model"}`.
pub const ATTR_STRONG: &str = "strong";
/// The attribute naming the **cheap** role's model: `{"provider", "model"}`.
pub const ATTR_CHEAP: &str = "cheap";
/// The attribute capping what one run may cost, in the models' priced currency
/// (TODO §10). Refused on save unless every model the agent may call has a price.
pub const ATTR_MAX_COST: &str = "max_cost";
/// The attribute capping how long one run may spend working, in seconds.
pub const ATTR_MAX_WALL_SECONDS: &str = "max_wall_seconds";
/// The attribute capping the context one request may carry, in tokens.
pub const ATTR_CONTEXT_BUDGET: &str = "context_budget";
/// The attribute capping how many images (screenshots) one run keeps in its
/// transcript. Older ones are replaced by stubs (TODO §7b).
pub const ATTR_MAX_IMAGES: &str = "max_images";
/// [`ATTR_MAX_IMAGES`] when the agent sets none.
pub const DEFAULT_MAX_IMAGES: usize = 20;
/// The attribute allowing several tool calls in one model turn. Absent is off
/// (R§12): sequential calls are easier to fingerprint.
pub const ATTR_PARALLEL_TOOL_CALLS: &str = "parallel_tool_calls";
/// The attribute listing the models a person may pick in the chat
/// (TODO-agent-skills AS10.4): `[{"provider", "model"?, "label"?}]`. When set,
/// the composer gets a [`MODEL_CONTROL`] select, and the pick answers that
/// message's turn in place of the executor's model.
pub const ATTR_PICKABLE_MODELS: &str = "pickable_models";
/// The name of the composer control [`ATTR_PICKABLE_MODELS`] adds.
pub const MODEL_CONTROL: &str = "model";
/// The [`MODEL_CONTROL`] value that keeps the agent's own model.
pub const MODEL_CONTROL_DEFAULT: &str = "";

/// One model a person may pick in the chat ([`ATTR_PICKABLE_MODELS`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickableModel {
    /// The `_fd_llm_providers` name.
    pub provider: String,
    /// The model row's name under that provider, or `None` for its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the person reads in the select; the pair when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl PickableModel {
    /// The pair it names.
    pub fn model_ref(&self) -> ModelRef {
        ModelRef::new(self.provider.trim(), self.model.as_deref())
    }

    /// The value the select sends for it: `provider/model`, or the provider
    /// alone for its default model.
    pub fn value(&self) -> String {
        let named = self.model_ref();
        match named.model {
            Some(model) => format!("{}/{model}", named.provider),
            None => named.provider,
        }
    }

    /// What the person reads for it.
    pub fn shown(&self) -> String {
        match self.label.as_deref().map(str::trim) {
            Some(label) if !label.is_empty() => label.to_owned(),
            _ => self.value(),
        }
    }
}

/// Which of an agent's models answers a step (TODO §3).
///
/// The agent's own model is the **executor**. The two roles are optional and
/// fall back to it, so an agent nobody has tuned behaves as it always did.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    /// The agent's own model: does the work.
    #[default]
    Executor,
    /// Plans, re-plans, reviews diffs and takes single-step escalations.
    Strong,
    /// Summarises, writes commit messages and explores.
    Cheap,
}

impl ModelRole {
    /// Every role, executor first.
    pub const ALL: [ModelRole; 3] = [ModelRole::Executor, ModelRole::Strong, ModelRole::Cheap];

    /// The stored spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelRole::Executor => "executor",
            ModelRole::Strong => "strong",
            ModelRole::Cheap => "cheap",
        }
    }

    /// Parse a stored spelling, strictly.
    pub fn parse(s: &str) -> sc_error::Result<ModelRole> {
        match s {
            "executor" => Ok(ModelRole::Executor),
            "strong" => Ok(ModelRole::Strong),
            "cheap" => Ok(ModelRole::Cheap),
            other => Err(sc_error::Error::invalid(format!(
                "unknown model role `{other}`; expected `executor`, `strong` or `cheap`"
            ))),
        }
    }

    /// The attribute a role's model is stored under, or `None` for the executor,
    /// whose model is the agent's own columns.
    pub fn attribute(&self) -> Option<&'static str> {
        match self {
            ModelRole::Executor => None,
            ModelRole::Strong => Some(ATTR_STRONG),
            ModelRole::Cheap => Some(ATTR_CHEAP),
        }
    }
}

impl std::fmt::Display for ModelRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A (provider, model) pair naming an `_fd_llm_models` row by name. `model`
/// empty or absent means the provider's default model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    /// The `_fd_llm_providers` name.
    pub provider: String,
    /// The model row's name under that provider, or `None` for its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ModelRef {
    /// A pair naming `model` under `provider`.
    pub fn new(provider: impl Into<String>, model: Option<&str>) -> ModelRef {
        ModelRef {
            provider: provider.into(),
            model: model
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_owned),
        }
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.model {
            Some(model) => write!(f, "{}/{model}", self.provider),
            None => write!(f, "{} (default model)", self.provider),
        }
    }
}

/// How many model calls one run makes before the loop stops it (§11.2).
///
/// An agent that will not converge — one that answers every tool result with
/// another tool call — has to be stopped by a number, because nothing else about
/// the conversation distinguishes it from one making progress. 250 is
/// mini-SWE-agent's step limit, between the 100 SWE-agent gives a SWE-bench
/// task and OpenHands' 500: a modest coding task takes several dozen
/// read-edit-check rounds, and one that is still going after 250 is looping.
/// The run's cost, time and context budgets bound a runaway before this does.
pub const DEFAULT_MAX_STEPS: u32 = 250;

/// One enabled trait: which trait, and how it is configured.
///
/// A **pair in a list**, not a map entry, because a trait may be enabled more
/// than once (§11.2): "query the `books` table" and "query the `orders` table"
/// are one trait with two configurations. Each instance derives its tool names
/// from its own configuration, and a collision between two of them is refused on
/// save by [`validate_agent`](crate::validate_agent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnabledTrait {
    /// The registered name of the trait ([`AgentTrait::name`](crate::AgentTrait::name)).
    ///
    /// Serialised as `trait` — the field is `trait_` only because `trait` is a
    /// Rust keyword, and the stored shape should not have to know that.
    #[serde(rename = "trait")]
    pub trait_: String,
    /// This instance's configuration, keyed by the trait's
    /// [`config_spec`](crate::AgentTrait::config_spec) field names.
    #[serde(default)]
    pub config: Attrs,
}

impl EnabledTrait {
    /// Enable `trait_` with no configuration set.
    pub fn new(trait_: impl Into<String>) -> EnabledTrait {
        EnabledTrait {
            trait_: trait_.into(),
            config: Attrs::new(),
        }
    }

    /// Set one configuration value, returning `self` for chaining.
    pub fn config(mut self, key: impl Into<String>, value: impl Into<Json>) -> EnabledTrait {
        self.config.insert(key.into(), value.into());
        self
    }

    /// Set the whole configuration, returning `self` for chaining.
    pub fn configuration(mut self, config: Attrs) -> EnabledTrait {
        self.config = config;
        self
    }
}

/// A configured LLM loop: a provider and model, a system prompt, and a set of
/// enabled traits.
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    /// Stable identity: the UUID of its `_fd_agents` row.
    pub id: AgentId,
    /// The unique, human-facing name — what a trigger's `run_agent` and the chat
    /// panel address it by, so renaming one breaks those references deliberately
    /// rather than silently.
    pub name: String,
    /// Human-readable description (§9 requires one on every metadata row; the
    /// empty string means "none given").
    pub description: String,
    /// The name of an `_fd_llm_providers` record.
    pub provider: String,
    /// The model to call, overriding the provider's default. `None` means the
    /// provider's own — which is the common case, and why this is not required.
    pub model: Option<String>,
    /// What the agent is told it is, before anything the conversation adds.
    pub system_prompt: String,
    /// The enabled traits, in the order the admin added them — which is the
    /// order their tools are offered to the model in.
    pub traits: Vec<EnabledTrait>,
    /// The role floor for chatting with this agent. `None` is **admin-only**,
    /// the same safe reading a trigger's takes: an agent nobody has thought about
    /// the access of is not public.
    pub min_role: Option<u8>,
    /// Sparse per-agent values (§9): [`ATTR_TEMPERATURE`], [`ATTR_MAX_TOKENS`],
    /// [`ATTR_MAX_STEPS`], the roles ([`ATTR_STRONG`], [`ATTR_CHEAP`]), the
    /// budgets ([`ATTR_MAX_COST`], [`ATTR_MAX_WALL_SECONDS`],
    /// [`ATTR_CONTEXT_BUDGET`]) and [`ATTR_PARALLEL_TOOL_CALLS`].
    pub attributes: Attrs,
}

impl Agent {
    /// A **new** agent with a fresh id: a name and a provider, and nothing else
    /// set.
    pub fn new(name: impl Into<String>, provider: impl Into<String>) -> Agent {
        Agent::with_id(AgentId::new(), name, provider)
    }

    /// Reconstruct an existing agent, which already has an id — what
    /// [`load_agent`](crate::load_agent) and an update path use.
    pub fn with_id(id: AgentId, name: impl Into<String>, provider: impl Into<String>) -> Agent {
        Agent {
            id,
            name: name.into(),
            description: String::new(),
            provider: provider.into(),
            model: None,
            system_prompt: String::new(),
            traits: Vec::new(),
            min_role: None,
            attributes: Attrs::new(),
        }
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> Agent {
        self.description = description.into();
        self
    }

    /// Override the provider's default model.
    pub fn model(mut self, model: impl Into<String>) -> Agent {
        self.model = Some(model.into());
        self
    }

    /// Set the system prompt.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Agent {
        self.system_prompt = prompt.into();
        self
    }

    /// Enable one trait, returning `self` for chaining.
    pub fn with_trait(mut self, enabled: EnabledTrait) -> Agent {
        self.traits.push(enabled);
        self
    }

    /// Set the role floor for chatting with this agent.
    pub fn min_role(mut self, role: u8) -> Agent {
        self.min_role = Some(role);
        self
    }

    /// Set one attribute, returning `self` for chaining.
    pub fn attribute(mut self, key: impl Into<String>, value: impl Into<Json>) -> Agent {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// The sampling temperature, if one is set.
    ///
    /// Absent means "do not send one", so the provider applies its own default —
    /// deliberately not resolved to a number here, because guessing which number
    /// a vendor would have chosen is how a request stops matching what the
    /// vendor's own documentation says it does.
    pub fn temperature(&self) -> Option<f64> {
        self.attributes.get(ATTR_TEMPERATURE).and_then(Json::as_f64)
    }

    /// The cap on tokens generated per answer, if one is set.
    pub fn max_tokens(&self) -> Option<u32> {
        self.attributes
            .get(ATTR_MAX_TOKENS)
            .and_then(Json::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    }

    /// How many times one run may go round the loop, defaulting to
    /// [`DEFAULT_MAX_STEPS`].
    ///
    /// Zero is not a value the loop can act on — an agent that may not call the
    /// model cannot answer — so a stored zero reads as the default rather than
    /// producing a run that ends before it starts.
    pub fn max_steps(&self) -> u32 {
        self.attributes
            .get(ATTR_MAX_STEPS)
            .and_then(Json::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_STEPS)
    }

    /// Name the model a role uses, returning `self` for chaining.
    pub fn role(mut self, role: ModelRole, model: ModelRef) -> Agent {
        match role.attribute() {
            Some(key) => {
                self.attributes.insert(
                    key.to_owned(),
                    serde_json::to_value(model).unwrap_or(Json::Null),
                );
            }
            None => {
                self.provider = model.provider;
                self.model = model.model;
            }
        }
        self
    }

    /// The model `role` is configured with, **without** falling back: `None` for
    /// a role the admin left unset. The executor is always set.
    ///
    /// A stored value of the wrong shape is an error rather than an unset role:
    /// an agent whose strong model silently became its executor would plan with
    /// the cheap model and nobody would see why.
    pub fn configured_role(&self, role: ModelRole) -> sc_error::Result<Option<ModelRef>> {
        let Some(key) = role.attribute() else {
            return Ok(Some(ModelRef::new(&self.provider, self.model.as_deref())));
        };
        match self.attributes.get(key) {
            None | Some(Json::Null) => Ok(None),
            Some(value) => {
                let parsed: ModelRef = serde_json::from_value(value.clone()).map_err(|e| {
                    sc_error::Error::invalid(format!(
                        "the `{key}` role should be {{\"provider\", \"model\"}}: {e}"
                    ))
                })?;
                if parsed.provider.trim().is_empty() {
                    // A pick-list left on "same as the agent" saves an empty
                    // provider; that is an unset role, not a broken one.
                    return Ok(None);
                }
                Ok(Some(ModelRef::new(
                    parsed.provider.trim(),
                    parsed.model.as_deref(),
                )))
            }
        }
    }

    /// The model `role` uses: its own when set, the agent's otherwise.
    pub fn model_for(&self, role: ModelRole) -> ModelRef {
        self.configured_role(role)
            .ok()
            .flatten()
            .unwrap_or_else(|| ModelRef::new(&self.provider, self.model.as_deref()))
    }

    /// The cost budget per run, if one is set.
    pub fn max_cost(&self) -> Option<f64> {
        self.attributes
            .get(ATTR_MAX_COST)
            .and_then(Json::as_f64)
            .filter(|c| c.is_finite() && *c > 0.0)
    }

    /// The wall-clock budget per run, in seconds, if one is set.
    pub fn max_wall_seconds(&self) -> Option<u64> {
        self.attributes
            .get(ATTR_MAX_WALL_SECONDS)
            .and_then(Json::as_u64)
            .filter(|s| *s > 0)
    }

    /// The context budget in tokens, if one is set explicitly.
    ///
    /// Unset means the executor model's working budget (TODO §9), which the
    /// driver knows and this record does not.
    pub fn context_budget(&self) -> Option<u64> {
        self.attributes
            .get(ATTR_CONTEXT_BUDGET)
            .and_then(Json::as_u64)
            .filter(|t| *t > 0)
    }

    /// How many images one run keeps in its transcript.
    pub fn max_images(&self) -> usize {
        self.attributes
            .get(ATTR_MAX_IMAGES)
            .and_then(Json::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(DEFAULT_MAX_IMAGES)
    }

    /// The LLM credits this agent's runs charge their caller, or `None` when it
    /// is not metered (TODO-payments P6.3, [`crate::meter`]).
    pub fn meter(&self) -> Option<crate::meter::Meter> {
        crate::meter::Meter::of(&self.attributes)
    }

    /// Whether the model may make several tool calls in one turn. Off unless the
    /// agent says so (R§12).
    pub fn parallel_tool_calls(&self) -> bool {
        self.attributes
            .get(ATTR_PARALLEL_TOOL_CALLS)
            .and_then(Json::as_bool)
            .unwrap_or(false)
    }

    /// The models a person may pick in the chat (AS10.4), in order; empty when
    /// the agent offers no choice. A stored value of the wrong shape, or an
    /// entry with no provider, is an error, as a role's is.
    pub fn pickable_models(&self) -> sc_error::Result<Vec<PickableModel>> {
        let entries = match self.attributes.get(ATTR_PICKABLE_MODELS) {
            None | Some(Json::Null) => return Ok(Vec::new()),
            Some(value) => value,
        };
        let models: Vec<PickableModel> =
            serde_json::from_value(entries.clone()).map_err(|e| {
                sc_error::Error::invalid(format!(
                    "`{ATTR_PICKABLE_MODELS}` should be a list of \
                     {{\"provider\", \"model\", \"label\"}}: {e}"
                ))
            })?;
        let mut seen = std::collections::BTreeSet::new();
        for model in &models {
            if model.provider.trim().is_empty() {
                return Err(sc_error::Error::invalid(format!(
                    "every entry of `{ATTR_PICKABLE_MODELS}` needs a provider"
                )));
            }
            if !seen.insert(model.value()) {
                return Err(sc_error::Error::invalid(format!(
                    "`{ATTR_PICKABLE_MODELS}` names {} twice",
                    model.value()
                )));
            }
        }
        Ok(models)
    }

    /// The pickable model a [`MODEL_CONTROL`] value names, or `None` for the
    /// agent's own (an empty value, or none sent).
    pub fn picked_model(&self, value: Option<&Json>) -> sc_error::Result<Option<PickableModel>> {
        let value = match value {
            None | Some(Json::Null) => return Ok(None),
            Some(Json::String(v)) if v == MODEL_CONTROL_DEFAULT => return Ok(None),
            Some(Json::String(v)) => v,
            Some(other) => {
                return Err(sc_error::Error::invalid(format!(
                    "`{MODEL_CONTROL}` should name a model, got {other}"
                )));
            }
        };
        self.pickable_models()?
            .into_iter()
            .find(|m| &m.value() == value)
            .map(Some)
            .ok_or_else(|| {
                sc_error::Error::invalid(format!(
                    "agent `{}` offers no model `{value}` to pick",
                    self.name
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_agent_is_built_from_a_provider_and_its_traits() {
        let a = Agent::new("librarian", "anthropic")
            .description("answers questions about the books table")
            .model("claude-opus-5")
            .system_prompt("You answer questions about books.")
            .with_trait(EnabledTrait::new("query_table").config("table", "books"))
            .with_trait(EnabledTrait::new("query_table").config("table", "orders"))
            .min_role(1);
        assert_eq!(a.name, "librarian");
        assert_eq!(a.model.as_deref(), Some("claude-opus-5"));
        // The same trait, twice, with different configurations (§11.2).
        assert_eq!(a.traits.len(), 2);
        assert_eq!(a.traits[0].trait_, a.traits[1].trait_);
        assert_eq!(a.traits[0].config["table"], json!("books"));
        assert_eq!(a.traits[1].config["table"], json!("orders"));
        assert_eq!(a.min_role, Some(1));
    }

    #[test]
    fn pickable_models_are_read_and_named_by_their_pair() {
        let a = Agent::new("a", "house").attribute(
            ATTR_PICKABLE_MODELS,
            json!([
                { "provider": "house", "model": "fast", "label": "Fast" },
                { "provider": "other" },
            ]),
        );
        let models = a.pickable_models().unwrap();
        assert_eq!(models[0].value(), "house/fast");
        assert_eq!(models[0].shown(), "Fast");
        assert_eq!(models[1].value(), "other");
        assert_eq!(models[1].shown(), "other");
        assert_eq!(
            a.picked_model(Some(&json!("other"))).unwrap().unwrap().provider,
            "other"
        );
        assert!(a.picked_model(Some(&json!(""))).unwrap().is_none());
        assert!(a.picked_model(None).unwrap().is_none());
        assert!(a.picked_model(Some(&json!("house/slow"))).is_err());
        let twice = Agent::new("a", "house").attribute(
            ATTR_PICKABLE_MODELS,
            json!([{ "provider": "x" }, { "provider": "x" }]),
        );
        assert!(twice.pickable_models().is_err());
        let blank = Agent::new("a", "house")
            .attribute(ATTR_PICKABLE_MODELS, json!([{ "provider": " " }]));
        assert!(blank.pickable_models().is_err());
    }

    #[test]
    fn the_sparse_attributes_read_as_their_defaults() {
        let a = Agent::new("a", "p");
        assert!(a.attributes.is_empty());
        // Absent temperature and max_tokens mean "do not send one".
        assert_eq!(a.temperature(), None);
        assert_eq!(a.max_tokens(), None);
        assert_eq!(a.max_steps(), DEFAULT_MAX_STEPS);

        let a = a
            .attribute(ATTR_TEMPERATURE, 0.2)
            .attribute(ATTR_MAX_TOKENS, 8192)
            .attribute(ATTR_MAX_STEPS, 4);
        assert_eq!(a.temperature(), Some(0.2));
        assert_eq!(a.max_tokens(), Some(8192));
        assert_eq!(a.max_steps(), 4);
    }

    #[test]
    fn a_zero_step_budget_reads_as_the_default() {
        // A run that may not call the model cannot answer, so zero is a value
        // the loop cannot act on rather than a very short leash.
        let a = Agent::new("a", "p").attribute(ATTR_MAX_STEPS, 0);
        assert_eq!(a.max_steps(), DEFAULT_MAX_STEPS);
    }

    #[test]
    fn an_enabled_trait_serialises_its_name_as_trait() {
        // The stored shape should not have to know that `trait` is a Rust
        // keyword: `_fd_agents.traits` holds `{"trait": ..., "config": ...}`.
        let enabled = EnabledTrait::new("run_trigger").config("trigger", "reindex");
        let text = serde_json::to_value(&enabled).unwrap();
        assert_eq!(
            text,
            json!({"trait": "run_trigger", "config": {"trigger": "reindex"}})
        );
        let back: EnabledTrait = serde_json::from_value(text).unwrap();
        assert_eq!(back, enabled);
    }

    #[test]
    fn a_role_falls_back_to_the_agents_own_model() {
        let a = Agent::new("a", "main").model("small");
        assert_eq!(a.configured_role(ModelRole::Strong).unwrap(), None);
        assert_eq!(
            a.model_for(ModelRole::Strong),
            ModelRef::new("main", Some("small"))
        );

        let a = a.role(ModelRole::Strong, ModelRef::new("other", Some("big")));
        assert_eq!(
            a.attributes[ATTR_STRONG],
            json!({"provider": "other", "model": "big"})
        );
        assert_eq!(
            a.model_for(ModelRole::Strong),
            ModelRef::new("other", Some("big"))
        );
        assert_eq!(
            a.model_for(ModelRole::Cheap),
            ModelRef::new("main", Some("small"))
        );
        assert_eq!(
            a.model_for(ModelRole::Executor),
            ModelRef::new("main", Some("small"))
        );

        // An empty pick-list is an unset role; a wrong shape is an error.
        let a = a.attribute(ATTR_CHEAP, json!({"provider": ""}));
        assert_eq!(a.configured_role(ModelRole::Cheap).unwrap(), None);
        let a = a.attribute(ATTR_CHEAP, json!("main"));
        assert!(a.configured_role(ModelRole::Cheap).is_err());
    }

    #[test]
    fn budgets_and_parallel_calls_read_sparsely() {
        let a = Agent::new("a", "p");
        assert_eq!(a.max_cost(), None);
        assert_eq!(a.max_wall_seconds(), None);
        assert_eq!(a.context_budget(), None);
        assert!(!a.parallel_tool_calls());
        let a = a
            .attribute(ATTR_MAX_COST, 0.5)
            .attribute(ATTR_MAX_WALL_SECONDS, 60)
            .attribute(ATTR_CONTEXT_BUDGET, 8000)
            .attribute(ATTR_PARALLEL_TOOL_CALLS, true);
        assert_eq!(a.max_cost(), Some(0.5));
        assert_eq!(a.max_wall_seconds(), Some(60));
        assert_eq!(a.context_budget(), Some(8000));
        assert!(a.parallel_tool_calls());
        for role in ModelRole::ALL {
            assert_eq!(ModelRole::parse(role.as_str()).unwrap(), role);
        }
    }

    #[test]
    fn a_fresh_agent_has_its_own_id() {
        let a = Agent::new("a", "p");
        let b = Agent::new("b", "p");
        assert_ne!(a.id, b.id);
        assert_eq!(Agent::with_id(a.id, "a", "p").id, a.id);
    }
}

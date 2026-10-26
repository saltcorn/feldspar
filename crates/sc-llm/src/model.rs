//! [`LlmModelDef`]: one model a provider serves, as a row of its own
//! (TODO §3a), and [`ConnectedModel`], what connecting one produces.
//!
//! A provider is what an admin enters once per key. A model is what differs
//! between the models that key reaches: the name sent on the wire, the prices,
//! the context window and working budget, the edit format, and the capability
//! overrides of §4. Each of those is a [`FormField`] **per backend**
//! ([`model_config_spec`]), so the admin form renders them like any other
//! settings and a save validates them field by field.
//!
//! **Every model setting is optional, and blank means the built-in default.**
//! A capability, window or budget left blank comes from
//! [`ModelCapabilities::built_in`], so an improved rule reaches every row that
//! did not set its own. A price left blank is unknown, never zero.
//!
//! The same model name under two providers is two rows, on purpose: the same
//! model bought directly and through a gateway can differ in price and in what
//! the host supports.

use std::sync::Arc;

use sc_error::{Error, Repr, Result};
use sc_types::{Attrs, BasicType, FormField, validate_attrs};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::capabilities::{
    CFG_CONTEXT_WINDOW, CFG_EDIT_FORMAT, CFG_NATIVE_APPLY_PATCH, CFG_PARALLEL_TOOL_CALLS,
    CFG_PARALLEL_TOOL_CALLS_DEFAULT, CFG_PROMPT_CACHING, CFG_REASONING_REPLAY,
    CFG_SUPPORTS_TEMPERATURE, CFG_VISION, CFG_WORKING_BUDGET, ModelCapabilities,
};
use crate::def::{
    ANTHROPIC_BACKEND, LlmProviderDefId, OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND,
    registered_backends, unknown_backend,
};
use crate::pricing::{
    CFG_PRICE_CACHE_WRITE, CFG_PRICE_CACHED_INPUT, CFG_PRICE_INPUT, CFG_PRICE_OUTPUT,
    CFG_PRICE_PER_IMAGE, CFG_PRICE_PER_MILLION_CHARACTERS, Prices,
};
use crate::provider::LlmProvider;

/// Identifies a stored model row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LlmModelDefId(pub Uuid);

impl LlmModelDefId {
    /// A fresh random id, for a row that has never been saved.
    pub fn new() -> LlmModelDefId {
        LlmModelDefId(Uuid::new_v4())
    }
}

impl Default for LlmModelDefId {
    fn default() -> LlmModelDefId {
        LlmModelDefId::new()
    }
}

/// What a model is called for (TODO-agent-skills AS3.1): a conversation, or
/// one of the three non-chat calls. Each kind has its own settings and its own
/// default model per provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    /// A chat model, for agents: [`connect_model`](crate::connect_model).
    #[default]
    Chat,
    /// Text in, vectors out: [`embed`](crate::embed).
    Embedding,
    /// A prompt in, an image out: [`generate_image`](crate::generate_image).
    Image,
    /// Text in, audio out: [`speak`](crate::speak).
    Speech,
}

impl ModelKind {
    /// Every kind, chat first.
    pub const ALL: [ModelKind; 4] = [
        ModelKind::Chat,
        ModelKind::Embedding,
        ModelKind::Image,
        ModelKind::Speech,
    ];

    /// The kind's name, as stored and as the API spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelKind::Chat => "chat",
            ModelKind::Embedding => "embedding",
            ModelKind::Image => "image",
            ModelKind::Speech => "speech",
        }
    }

    /// The kind called `name`. Blank is chat; anything else unknown is refused,
    /// naming the kinds there are.
    pub fn parse(name: &str) -> Result<ModelKind> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(ModelKind::Chat);
        }
        ModelKind::ALL
            .into_iter()
            .find(|k| k.as_str() == name)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "unknown LLM model kind `{name}`; the kinds are {}",
                    ModelKind::ALL.map(ModelKind::as_str).join(", ")
                ))
            })
    }

    /// The kind's name after its article: "an image", "a speech".
    pub fn with_article(self) -> String {
        match self {
            ModelKind::Image | ModelKind::Embedding => format!("an {}", self.as_str()),
            ModelKind::Chat | ModelKind::Speech => format!("a {}", self.as_str()),
        }
    }

    /// Whether `backend` can serve a model of this kind. Anthropic serves
    /// chat only; OpenAI and OpenAI-compatible hosts serve every kind.
    pub fn served_by(self, backend: &str) -> bool {
        self == ModelKind::Chat || backend != ANTHROPIC_BACKEND
    }
}

impl std::fmt::Display for ModelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One model a provider serves: a row of `_fd_llm_models`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmModelDef {
    /// The row's identity.
    pub id: LlmModelDefId,
    /// The provider serving it. A foreign key.
    pub provider_id: LlmProviderDefId,
    /// The vendor's model id, as sent on the wire. Unique per provider.
    pub name: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// What the model is called for. Fixed once saved: its settings and the
    /// things naming it depend on it.
    pub kind: ModelKind,
    /// Whether this is the provider's default model of its kind: for chat,
    /// the one an agent that names the provider and no model calls. At most
    /// one per provider and kind.
    pub is_default: bool,
    /// The model's settings, validated against
    /// [`model_config_spec`] for the provider's backend.
    pub config: Attrs,
    /// Sparse per-model values (§9).
    pub attributes: Attrs,
}

impl LlmModelDef {
    /// A model named `name` served by `provider`, with blank settings — which
    /// means every built-in default.
    pub fn new(provider: LlmProviderDefId, name: impl Into<String>) -> LlmModelDef {
        LlmModelDef {
            id: LlmModelDefId::new(),
            provider_id: provider,
            name: name.into(),
            description: String::new(),
            kind: ModelKind::Chat,
            is_default: false,
            config: Attrs::new(),
            attributes: Attrs::new(),
        }
    }

    /// Set the kind, returning `self` for chaining.
    pub fn kind(mut self, kind: ModelKind) -> LlmModelDef {
        self.kind = kind;
        self
    }

    /// Mark this as the provider's default, returning `self` for chaining.
    pub fn default_model(mut self) -> LlmModelDef {
        self.is_default = true;
        self
    }

    /// Set a model setting, returning `self` for chaining.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Json>) -> LlmModelDef {
        self.config.insert(key.into(), value.into());
        self
    }

    /// Set the description, returning `self` for chaining.
    pub fn description(mut self, description: impl Into<String>) -> LlmModelDef {
        self.description = description.into();
        self
    }

    /// The capabilities this model resolves to on `backend`.
    pub fn capabilities(&self, backend: &str) -> ModelCapabilities {
        ModelCapabilities::resolve(backend, &self.name, &self.config)
    }

    /// The prices set on this row.
    pub fn prices(&self) -> Prices {
        Prices::from_config(&self.config)
    }
}

/// A model ready to be called, with what the loop needs to know about it.
///
/// Returned by [`connect_model`](crate::connect_model), so the capabilities and
/// prices are resolved once, where the rows are, and the loop never looks them
/// up again.
#[derive(Clone)]
pub struct ConnectedModel {
    /// The callable model, wrapped in the call log.
    pub provider: Arc<dyn LlmProvider>,
    /// The provider's name, as an agent references it.
    pub provider_name: String,
    /// The provider's backend.
    pub backend: String,
    /// What the model can do.
    pub capabilities: ModelCapabilities,
    /// What the model costs.
    pub prices: Prices,
}

impl ConnectedModel {
    /// A model with no stored rows behind it — a scripted fake in a test, or
    /// any provider built by hand. Its capabilities are the built-in rules for
    /// an unknown backend, and its prices are unknown.
    pub fn unconfigured(provider: Arc<dyn LlmProvider>) -> ConnectedModel {
        let capabilities = ModelCapabilities::built_in("", provider.model());
        ConnectedModel {
            provider,
            provider_name: String::new(),
            backend: String::new(),
            capabilities,
            prices: Prices::default(),
        }
    }
}

impl std::fmt::Debug for ConnectedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectedModel")
            .field("provider_name", &self.provider_name)
            .field("model", &self.provider.model())
            .field("capabilities", &self.capabilities)
            .field("prices", &self.prices)
            .finish()
    }
}

/// The settings a model row of `kind` declares on `backend`.
///
/// A chat model (§3a) has the four prices, the context window and working
/// budget, the edit format, and each capability override the backend can vary.
/// The other kinds have only their prices: an embedding's per million input
/// tokens, an image's per image or by its tokens, speech's per million
/// characters. A kind the backend cannot serve is refused.
///
/// All optional. A blank setting is the built-in default, so none has a
/// default value of its own: a default here would be copied into the row and
/// stop the built-in rule reaching it.
pub fn model_config_spec(backend: &str, kind: ModelKind) -> Result<Vec<FormField>> {
    if !registered_backends().iter().any(|b| b == backend) {
        return Err(unknown_backend(backend));
    }
    if !kind.served_by(backend) {
        return Err(Error::invalid(format!(
            "LLM provider backend `{backend}` serves chat models only, not {kind} models; \
             add an OpenAI or OpenAI-compatible provider for those"
        )));
    }
    let price = |key: &str, label: &str| FormField::new(key, BasicType::Float).label(label);
    match kind {
        ModelKind::Chat => {}
        ModelKind::Embedding => {
            return Ok(vec![price(
                CFG_PRICE_INPUT,
                "Input price per million tokens",
            )]);
        }
        ModelKind::Image => {
            return Ok(vec![
                price(CFG_PRICE_PER_IMAGE, "Price per image"),
                price(CFG_PRICE_INPUT, "Text input price per million tokens"),
                price(CFG_PRICE_OUTPUT, "Image output price per million tokens"),
            ]);
        }
        ModelKind::Speech => {
            return Ok(vec![price(
                CFG_PRICE_PER_MILLION_CHARACTERS,
                "Price per million characters",
            )]);
        }
    }
    let choice = |key: &str, label: &str, options: &[&str]| {
        FormField::new(key, BasicType::Text)
            .label(label)
            .options(options.iter().map(|o| (*o).to_owned()))
    };

    let mut spec = vec![
        price(CFG_PRICE_INPUT, "Input price per million tokens"),
        price(
            CFG_PRICE_CACHED_INPUT,
            "Cached input price per million tokens",
        ),
        price(
            CFG_PRICE_CACHE_WRITE,
            "Cache write price per million tokens",
        ),
        price(CFG_PRICE_OUTPUT, "Output price per million tokens"),
        FormField::new(CFG_CONTEXT_WINDOW, BasicType::Int).label("Context window (tokens)"),
        FormField::new(CFG_WORKING_BUDGET, BasicType::Int).label("Working budget (tokens)"),
        choice(
            CFG_EDIT_FORMAT,
            "Edit format",
            &["str_replace", "apply_patch", "whole_file"],
        ),
        choice(
            CFG_PARALLEL_TOOL_CALLS,
            "Parallel tool calls",
            &["yes", "no"],
        ),
        choice(
            CFG_PARALLEL_TOOL_CALLS_DEFAULT,
            "Parallel tool calls by default",
            &["on", "off"],
        ),
    ];
    match backend {
        ANTHROPIC_BACKEND => {
            spec.push(choice(
                CFG_REASONING_REPLAY,
                "Replay thinking signatures",
                &["yes", "no"],
            ));
            spec.push(choice(
                CFG_PROMPT_CACHING,
                "Prompt caching",
                &["explicit", "none"],
            ));
        }
        OPENAI_RESPONSES_BACKEND => {
            spec.push(choice(
                CFG_NATIVE_APPLY_PATCH,
                "Native apply_patch",
                &["yes", "no"],
            ));
            spec.push(choice(
                CFG_REASONING_REPLAY,
                "Replay encrypted reasoning",
                &["yes", "no"],
            ));
            spec.push(choice(
                CFG_PROMPT_CACHING,
                "Prompt caching",
                &["automatic", "none"],
            ));
        }
        // No native `apply_patch` and no reasoning replay on Chat Completions,
        // so there is nothing to override.
        OPENAI_CHAT_BACKEND => {
            spec.push(choice(
                CFG_PROMPT_CACHING,
                "Prompt caching",
                &["automatic", "none"],
            ));
        }
        other => return Err(unknown_backend(other)),
    }
    spec.push(choice(
        crate::CFG_REASONING_EFFORT,
        "Reasoning effort",
        &crate::ReasoningEffort::ALL.map(crate::ReasoningEffort::as_str),
    ));
    spec.push(
        FormField::new(crate::CFG_EXTRA_PARAMS, BasicType::Json)
            .label("Extra request parameters (JSON object)"),
    );
    spec.push(choice(CFG_VISION, "Images in tool results", &["yes", "no"]));
    spec.push(choice(
        CFG_SUPPORTS_TEMPERATURE,
        "Accepts a temperature setting",
        &["yes", "no"],
    ));
    Ok(spec)
}

/// A model config with its blank settings removed: an empty string or a null
/// is "use the built-in default", and a row records only where it differs.
///
/// A form sends an untouched select or number input as an empty string, so
/// this runs before validation on every save.
pub fn normalise_model_config(config: &Attrs) -> Attrs {
    config
        .iter()
        .filter(|(_, v)| match v {
            Json::Null => false,
            Json::String(s) => !s.trim().is_empty(),
            _ => true,
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Check a model row's settings against `backend`'s model spec — the check a
/// save goes through, and the one a load repeats.
pub fn validate_model_config(backend: &str, model: &LlmModelDef) -> Result<()> {
    let spec = model_config_spec(backend, model.kind)?;
    let config = normalise_model_config(&model.config);
    validate_attrs(&spec, &config).map_err(|e| {
        if let Repr::Invalid(msg) = e.repr() {
            Error::invalid(format!("LLM model `{}`: {msg}", model.name))
        } else {
            e
        }
    })?;
    for key in [
        CFG_PRICE_INPUT,
        CFG_PRICE_CACHED_INPUT,
        CFG_PRICE_CACHE_WRITE,
        CFG_PRICE_OUTPUT,
        CFG_PRICE_PER_IMAGE,
        CFG_PRICE_PER_MILLION_CHARACTERS,
    ] {
        if config
            .get(key)
            .and_then(Json::as_f64)
            .is_some_and(|p| p < 0.0)
        {
            return Err(Error::invalid(format!(
                "LLM model `{}`: setting `{key}` cannot be negative",
                model.name
            )));
        }
    }
    for key in [CFG_CONTEXT_WINDOW, CFG_WORKING_BUDGET] {
        if config
            .get(key)
            .and_then(Json::as_i64)
            .is_some_and(|n| n <= 0)
        {
            return Err(Error::invalid(format!(
                "LLM model `{}`: setting `{key}` must be a positive number of tokens",
                model.name
            )));
        }
    }
    // An `extra_params` that is not an object, or sets the request's own
    // fields, is refused here rather than on the first call.
    crate::RequestDefaults::from_config(&config, &format!("LLM model `{}`", model.name))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_backend_declares_model_settings_all_optional() {
        for backend in registered_backends() {
            let spec = model_config_spec(&backend, ModelKind::Chat).unwrap();
            for key in [
                CFG_PRICE_INPUT,
                CFG_PRICE_CACHED_INPUT,
                CFG_PRICE_CACHE_WRITE,
                CFG_PRICE_OUTPUT,
                CFG_CONTEXT_WINDOW,
                CFG_WORKING_BUDGET,
                CFG_EDIT_FORMAT,
                CFG_VISION,
            ] {
                assert!(spec.iter().any(|f| f.name() == key), "{backend}: {key}");
            }
            for field in &spec {
                assert!(!field.required, "{backend}: {} is optional", field.name());
                assert!(
                    field.default.is_none(),
                    "{backend}: {} has no default",
                    field.name()
                );
            }
        }
        assert!(model_config_spec("bedrock", ModelKind::Chat).is_err());
        // Chat Completions has no native apply_patch to override.
        assert!(
            model_config_spec(OPENAI_CHAT_BACKEND, ModelKind::Chat)
                .unwrap()
                .iter()
                .all(|f| f.name() != CFG_NATIVE_APPLY_PATCH)
        );
    }

    #[test]
    fn blank_settings_validate_and_are_dropped() {
        let provider = LlmProviderDefId::new();
        let model = LlmModelDef::new(provider, "gpt-5.1")
            .with(CFG_PRICE_INPUT, "")
            .with(CFG_VISION, Json::Null)
            .with(CFG_EDIT_FORMAT, "apply_patch")
            .with(CFG_CONTEXT_WINDOW, 400_000);
        validate_model_config(OPENAI_RESPONSES_BACKEND, &model).unwrap();
        let normal = normalise_model_config(&model.config);
        assert_eq!(normal.len(), 2, "{normal:?}");
    }

    #[test]
    fn a_wrong_model_setting_is_refused_naming_the_model_and_setting() {
        let provider = LlmProviderDefId::new();
        for (key, value) in [
            (CFG_EDIT_FORMAT, json!("diff")),
            (CFG_PRICE_OUTPUT, json!(-1.0)),
            (CFG_CONTEXT_WINDOW, json!(0)),
            (CFG_PRICE_INPUT, json!("three dollars")),
            ("temperature", json!(0.5)),
            (crate::CFG_REASONING_EFFORT, json!("tiny")),
            (crate::CFG_EXTRA_PARAMS, json!({ "messages": [] })),
        ] {
            let model = LlmModelDef::new(provider, "claude-sonnet-5").with(key, value);
            let err = validate_model_config(ANTHROPIC_BACKEND, &model)
                .unwrap_err()
                .to_string();
            assert!(err.contains("claude-sonnet-5"), "{err}");
            assert!(err.contains(key), "{err}");
        }
        // Every chat backend takes a reasoning effort and extra fields.
        let model = LlmModelDef::new(provider, "glm-5.3-flash")
            .with(crate::CFG_REASONING_EFFORT, "none")
            .with(crate::CFG_EXTRA_PARAMS, json!({ "top_p": 0.9 }));
        validate_model_config(OPENAI_CHAT_BACKEND, &model).unwrap();
        // Responses-only overrides are unknown on Anthropic.
        let model =
            LlmModelDef::new(provider, "claude-sonnet-5").with(CFG_NATIVE_APPLY_PATCH, "yes");
        assert!(validate_model_config(ANTHROPIC_BACKEND, &model).is_err());
    }

    #[test]
    fn each_kind_has_its_own_prices_and_anthropic_serves_chat_only() {
        assert_eq!(ModelKind::parse("").unwrap(), ModelKind::Chat);
        assert_eq!(ModelKind::parse("speech").unwrap(), ModelKind::Speech);
        let err = ModelKind::parse("video").unwrap_err().to_string();
        assert!(err.contains("embedding, image, speech"), "{err}");

        let names = |kind| -> Vec<String> {
            model_config_spec(OPENAI_RESPONSES_BACKEND, kind)
                .unwrap()
                .iter()
                .map(|f| f.name().to_owned())
                .collect()
        };
        assert_eq!(names(ModelKind::Embedding), [CFG_PRICE_INPUT]);
        assert!(names(ModelKind::Image).contains(&CFG_PRICE_PER_IMAGE.to_owned()));
        assert_eq!(names(ModelKind::Speech), [CFG_PRICE_PER_MILLION_CHARACTERS]);
        assert!(model_config_spec(OPENAI_CHAT_BACKEND, ModelKind::Image).is_ok());

        let err = model_config_spec(ANTHROPIC_BACKEND, ModelKind::Embedding)
            .unwrap_err()
            .to_string();
        assert!(err.contains("chat models only"), "{err}");

        // A chat setting on an embedding model is unknown to it.
        let provider = LlmProviderDefId::new();
        let model = LlmModelDef::new(provider, "text-embedding-3-small")
            .kind(ModelKind::Embedding)
            .with(CFG_CONTEXT_WINDOW, 8192);
        assert!(validate_model_config(OPENAI_RESPONSES_BACKEND, &model).is_err());
        let model = LlmModelDef::new(provider, "gpt-image-1")
            .kind(ModelKind::Image)
            .with(CFG_PRICE_PER_IMAGE, -0.04);
        let err = validate_model_config(OPENAI_RESPONSES_BACKEND, &model)
            .unwrap_err()
            .to_string();
        assert!(err.contains("negative"), "{err}");
    }
}

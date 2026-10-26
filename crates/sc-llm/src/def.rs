//! [`LlmProviderDef`]: the stored *definition* of an LLM provider, and the
//! backend registry that turns one into a connected
//! [`LlmProvider`](crate::LlmProvider) — design §11.1.
//!
//! A provider holds what is entered once per key: the backend, the API key and
//! the base URL. What differs per model (prices, capabilities, the context
//! window) is a model row ([`LlmModelDef`]), and [`connect_model`] joins the
//! two.
//!
//! This follows `sc-files`' file-store backends exactly, and the parallel is
//! worth stating because it is what makes the admin UI generic: a **definition**
//! is inert data (a name, which backend, that backend's settings), a backend
//! declares its settings as [`FormField`]s, and the form that renders a file
//! store's settings renders these without knowing what an API key is.
//!
//! Two kinds of wrong, kept apart, for the same reason §14.1 keeps them apart:
//!
//! - **Structurally wrong** — a missing API key, a `base_url` that is a number,
//!   an unknown backend. The admin's typo, knowable without a network, so
//!   [`validate_provider_config`] catches it and the save is refused while they
//!   are still looking at the form.
//! - **Currently not working** — a well-formed definition with a revoked key, an
//!   endpoint that is down, a model that was retired. Not a typo, often not the
//!   admin's fault, and it can become true after a successful save. Only a real
//!   request finds it, which is what a model's *Test* is for.
//!
//! Only the first blocks a save.

use std::sync::Arc;

use sc_error::{Error, Repr, Result};
use sc_types::{Attrs, BasicType, FormField, validate_attrs};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::anthropic::Anthropic;
use crate::model::{ConnectedModel, LlmModelDef, validate_model_config};
use crate::openai::OpenAiResponses;
use crate::openai_chat::OpenAiChat;
use crate::provider::LlmProvider;

/// The backend speaking **OpenAI's Responses API** — including every
/// OpenAI-compatible endpoint, which is what [`CFG_BASE_URL`] is for.
pub const OPENAI_RESPONSES_BACKEND: &str = "openai_responses";

/// The backend speaking **Anthropic's messages API**.
pub const ANTHROPIC_BACKEND: &str = "anthropic";

/// The backend speaking **Chat Completions**: the API most cheap and
/// open-weight hosts serve (vLLM, llama.cpp, Ollama, OpenRouter, DeepSeek).
pub const OPENAI_CHAT_BACKEND: &str = "openai_chat";

/// The `base_url` setting: which endpoint to call. Blank means the backend's own
/// default.
pub const CFG_BASE_URL: &str = "base_url";

/// The `api_key` setting — declared [`secret`](FormField::secret), so it is
/// redacted wherever the record is serialised and preserved when a save submits
/// the sentinel back (§11.1).
pub const CFG_API_KEY: &str = "api_key";

/// Identifies a stored provider definition. A UUID, per §9's rule that every
/// system metadata table has a UUID primary key.
///
/// Distinct from the provider's *name*, which is what an agent references and
/// what everything else resolves through — so a provider can be renamed without
/// its row changing identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LlmProviderDefId(pub Uuid);

impl LlmProviderDefId {
    /// A fresh random id, for a definition that has never been saved.
    pub fn new() -> LlmProviderDefId {
        LlmProviderDefId(Uuid::new_v4())
    }
}

impl Default for LlmProviderDefId {
    fn default() -> LlmProviderDefId {
        LlmProviderDefId::new()
    }
}

/// The stored definition of one LLM provider (§11.1).
///
/// The fields mirror the `_fd_llm_providers` columns, following §9's
/// column-vs-attributes rule: every provider has an id, name, description,
/// backend and backend config, so each gets a column, and anything sparse goes
/// in [`attributes`](LlmProviderDef::attributes).
///
/// **No model.** A provider serves several models, and each is a row of its own
/// in `_fd_llm_models` ([`LlmModelDef`]), with its own prices and capabilities.
/// The provider's default model is the model row marked `is_default`.
///
/// **No `min_role`.** A file store has one because an application's users browse
/// it; a provider is reached only through an agent, and it is the *agent* that
/// carries who may chat with it (§11.2). Putting a floor here as well would be a
/// second authority over the same question with no way to reconcile them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmProviderDef {
    /// The row's identity.
    pub id: LlmProviderDefId,
    /// The provider's name — what an agent references, unique across providers.
    pub name: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// Which backend serves it: [`OPENAI_RESPONSES_BACKEND`],
    /// [`ANTHROPIC_BACKEND`] or [`OPENAI_CHAT_BACKEND`].
    pub backend: String,
    /// The backend's settings, validated against that backend's declared
    /// [`config_spec`](provider_config_spec) on save and on load.
    pub config: Attrs,
    /// Sparse per-provider values (§9).
    pub attributes: Attrs,
}

impl LlmProviderDef {
    /// A definition named `name` served by `backend`, with no settings and no
    /// description — the base to add settings to.
    pub fn new(name: impl Into<String>, backend: impl Into<String>) -> LlmProviderDef {
        LlmProviderDef {
            id: LlmProviderDefId::new(),
            name: name.into(),
            description: String::new(),
            backend: backend.into(),
            config: Attrs::new(),
            attributes: Attrs::new(),
        }
    }

    /// Set a backend setting, returning `self` for chaining.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.config.insert(key.into(), value.into());
        self
    }

    /// Set the description, returning `self` for chaining.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Set the id, returning `self` for chaining — for rebuilding a definition
    /// that already has a row.
    pub fn id(mut self, id: LlmProviderDefId) -> Self {
        self.id = id;
        self
    }

    /// The value of a backend setting as text, if it is set and is a string.
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(serde_json::Value::as_str)
    }
}

/// The settings the [`openai_responses`](OPENAI_RESPONSES_BACKEND) backend
/// needs.
///
/// `base_url` is optional and defaulted rather than required, which is the whole
/// design: an admin connecting OpenAI itself leaves it alone, and an admin
/// connecting a gateway, a local server or an alternative vendor types a URL.
/// Neither is a code change.
pub fn openai_config_spec() -> Vec<FormField> {
    vec![
        FormField::new(CFG_API_KEY, BasicType::Text)
            .label("API key")
            .required()
            .secret(),
        FormField::new(CFG_BASE_URL, BasicType::Text)
            .label("Base URL (blank for OpenAI)")
            .default_value(crate::openai::DEFAULT_BASE_URL),
    ]
}

/// The settings the [`anthropic`](ANTHROPIC_BACKEND) backend needs — the same
/// two, since the difference between the vendors is not in what an admin has
/// to type.
pub fn anthropic_config_spec() -> Vec<FormField> {
    vec![
        FormField::new(CFG_API_KEY, BasicType::Text)
            .label("API key")
            .required()
            .secret(),
        FormField::new(CFG_BASE_URL, BasicType::Text)
            .label("Base URL (blank for Anthropic)")
            .default_value(crate::anthropic::DEFAULT_BASE_URL),
    ]
}

/// The settings the [`openai_chat`](OPENAI_CHAT_BACKEND) backend needs.
///
/// The reverse of the other two: the key is optional, because a local host
/// (llama.cpp, Ollama, vLLM on a workstation) takes none, and the base URL is
/// required, because there is no one host this backend means.
pub fn openai_chat_config_spec() -> Vec<FormField> {
    vec![
        FormField::new(CFG_API_KEY, BasicType::Text)
            .label("API key (blank for a local host)")
            .secret(),
        FormField::new(CFG_BASE_URL, BasicType::Text)
            .label("Base URL (for example http://localhost:11434/v1)")
            .required(),
    ]
}

/// The names of every registered backend — what the admin UI lists so an admin
/// can pick one and be shown its [`provider_config_spec`].
///
/// The single place that enumerates them: a new backend is added here, to
/// [`provider_config_spec`], to [`model_config_spec`](crate::model_config_spec),
/// to [`connect_model`] and to the non-chat calls' endpoint (`endpoint.rs`).
pub fn registered_backends() -> Vec<String> {
    vec![
        ANTHROPIC_BACKEND.to_owned(),
        OPENAI_CHAT_BACKEND.to_owned(),
        OPENAI_RESPONSES_BACKEND.to_owned(),
    ]
}

/// The settings the backend registered under `name` declares.
///
/// An unknown name is a configuration error rather than a backend with no
/// settings: a provider nothing implements cannot be connected, and saying so
/// beats accepting it and failing later with no explanation.
pub fn provider_config_spec(name: &str) -> Result<Vec<FormField>> {
    match name {
        OPENAI_RESPONSES_BACKEND => Ok(openai_config_spec()),
        ANTHROPIC_BACKEND => Ok(anthropic_config_spec()),
        OPENAI_CHAT_BACKEND => Ok(openai_chat_config_spec()),
        other => Err(unknown_backend(other)),
    }
}

/// The error for a backend nothing implements, naming what there is.
pub(crate) fn unknown_backend(name: &str) -> Error {
    Error::config(format!(
        "unknown LLM provider backend `{name}`; the registered backends are {}",
        registered_backends().join(", ")
    ))
}

/// Check a definition's settings against its backend's declared spec — the check
/// a save goes through, and the one a load repeats.
pub fn validate_provider_config(def: &LlmProviderDef) -> Result<()> {
    let spec = provider_config_spec(&def.backend)?;
    validate_attrs(&spec, &def.config).map_err(|e| {
        // Rebuilt rather than wrapped, for the reason `validate_file_store_config`
        // documents: `Invalid` renders its own "invalid:" prefix, so formatting
        // the whole error into a new one would say it twice, and a `Context`
        // would show only the context and hide the setting — the part the admin
        // needs.
        if let Repr::Invalid(msg) = e.repr() {
            Error::invalid(format!(
                "LLM provider `{}` (backend `{}`): {msg}",
                def.name, def.backend
            ))
        } else {
            e
        }
    })
}

/// Turn a stored provider and one of its models into a callable model — the one
/// place a definition becomes an instance.
///
/// What comes back carries the model's **resolved capabilities and prices**
/// beside the provider, so the loop never looks them up a second time.
///
/// Building a client sends nothing, so an error here means the *configuration*
/// cannot produce a client — a missing key, an unparseable URL, a model row
/// that belongs to another provider — never that the provider is down.
/// Reachability is a request, and the admin form's *Test* is the deliberate way
/// to make one.
pub fn connect_model(provider: &LlmProviderDef, model: &LlmModelDef) -> Result<ConnectedModel> {
    validate_provider_config(provider)?;
    if model.provider_id != provider.id {
        return Err(Error::invalid(format!(
            "LLM model `{}` does not belong to provider `{}`",
            model.name, provider.name
        )));
    }
    if model.kind != crate::model::ModelKind::Chat {
        return Err(Error::invalid(format!(
            "LLM model `{}` of provider `{}` is {} model, not a chat model; an agent \
             converses with a chat model",
            model.name,
            provider.name,
            model.kind.with_article()
        )));
    }
    validate_model_config(&provider.backend, model)?;
    let name = model.name.trim();
    if name.is_empty() {
        return Err(Error::invalid(format!(
            "an LLM model of provider `{}` has no name",
            provider.name
        )));
    }

    let capabilities = model.capabilities(&provider.backend);
    let prices = model.prices();
    let api_key = provider.setting(CFG_API_KEY).unwrap_or_default();
    let base_url = provider.setting(CFG_BASE_URL).unwrap_or_default();

    // Every connected model is wrapped in the call log (§16): this is the one
    // place a stored record becomes something callable, so wrapping here is what
    // makes "every model call this server makes" true of the log — the agent
    // loop, the chat socket and the test button all arrive through this
    // function.
    let connected: Arc<dyn LlmProvider> = match provider.backend.as_str() {
        OPENAI_RESPONSES_BACKEND => {
            Arc::new(OpenAiResponses::new(base_url, api_key, name, capabilities)?)
        }
        ANTHROPIC_BACKEND => Arc::new(Anthropic::new(base_url, api_key, name, capabilities)?),
        OPENAI_CHAT_BACKEND => Arc::new(OpenAiChat::new(base_url, api_key, name, capabilities)?),
        // Unreachable while `validate_provider_config` runs first; kept so
        // adding a backend to the registry without adding it here is a clear
        // error rather than a fallthrough.
        other => {
            return Err(Error::config(format!(
                "LLM provider backend `{other}` declares settings but cannot be connected"
            )));
        }
    };
    // Outside the call log, so what the log records is the request as sent —
    // the row's reasoning effort and extra fields included.
    let defaults = crate::RequestDefaults::from_config(
        &crate::normalise_model_config(&model.config),
        &format!("LLM model `{name}`"),
    )?;
    Ok(ConnectedModel {
        provider: crate::WithRequestDefaults::wrap(
            Arc::new(crate::logging::LoggedProvider::new(
                connected,
                &provider.name,
            )),
            defaults,
        ),
        provider_name: provider.name.clone(),
        backend: provider.backend.clone(),
        capabilities,
        prices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_types::SECRET_SENTINEL;

    fn anthropic(name: &str) -> LlmProviderDef {
        LlmProviderDef::new(name, ANTHROPIC_BACKEND).with(CFG_API_KEY, "sk-x")
    }

    #[test]
    fn every_backend_declares_its_key_as_a_secret() {
        // The point of `secret` being on the declaration: this is checked once,
        // for every backend, rather than trusted per screen.
        for backend in registered_backends() {
            let spec = provider_config_spec(&backend).unwrap();
            let key = spec.iter().find(|f| f.name() == CFG_API_KEY).unwrap();
            assert!(key.secret, "{backend}'s API key must be declared secret");
            // A local Chat Completions host takes no key.
            assert_eq!(
                key.required,
                backend != OPENAI_CHAT_BACKEND,
                "{backend}'s key requirement"
            );
            // The model is a row of its own now, never a provider setting.
            assert!(spec.iter().all(|f| f.name() != "model"), "{backend}");
        }
    }

    #[test]
    fn the_registry_resolves_a_name_to_the_same_spec() {
        assert_eq!(
            provider_config_spec(OPENAI_RESPONSES_BACKEND).unwrap(),
            openai_config_spec()
        );
        assert_eq!(
            provider_config_spec(ANTHROPIC_BACKEND).unwrap(),
            anthropic_config_spec()
        );
        assert_eq!(
            provider_config_spec(OPENAI_CHAT_BACKEND).unwrap(),
            openai_chat_config_spec()
        );
        let err = provider_config_spec("bedrock").unwrap_err().to_string();
        assert!(err.contains("bedrock"), "{err}");
        assert!(
            err.contains(ANTHROPIC_BACKEND),
            "should list what there is: {err}"
        );
    }

    #[test]
    fn a_base_url_is_a_field_value_not_a_code_change() {
        // The whole of "OpenAI-compatible": an alternative endpoint is typed in.
        let spec = openai_config_spec();
        let base = spec.iter().find(|f| f.name() == CFG_BASE_URL).unwrap();
        assert!(!base.required, "an admin using OpenAI itself types nothing");
        assert_eq!(
            base.default,
            Some(serde_json::json!(crate::openai::DEFAULT_BASE_URL))
        );
        // Chat Completions has no one host, so it must be told which.
        let spec = openai_chat_config_spec();
        assert!(
            spec.iter()
                .find(|f| f.name() == CFG_BASE_URL)
                .unwrap()
                .required
        );
    }

    #[test]
    fn a_missing_key_is_refused_on_save_naming_the_provider() {
        let def = LlmProviderDef::new("main", ANTHROPIC_BACKEND);
        let err = validate_provider_config(&def).unwrap_err().to_string();
        assert!(err.contains("main"), "{err}");
        assert!(err.contains(CFG_API_KEY), "{err}");
    }

    #[test]
    fn an_unknown_setting_is_refused_rather_than_ignored() {
        // Including the old default model, which is a model row now.
        let def = anthropic("main").with("model", "claude-opus-4");
        let err = validate_provider_config(&def).unwrap_err().to_string();
        assert!(err.contains("model"), "{err}");
    }

    #[test]
    fn connecting_a_model_resolves_its_capabilities_and_prices() {
        let provider = anthropic("main");
        let model = LlmModelDef::new(provider.id, "claude-sonnet-5")
            .with(crate::CFG_PRICE_INPUT, 3.0)
            .with(crate::CFG_PRICE_OUTPUT, 15.0)
            .with(crate::CFG_VISION, "no");
        let connected = connect_model(&provider, &model).unwrap();
        assert_eq!(connected.provider.model(), "claude-sonnet-5");
        assert_eq!(connected.provider_name, "main");
        assert_eq!(connected.prices.input, Some(3.0));
        assert!(!connected.capabilities.vision, "the row's override");
        assert_eq!(connected.capabilities.context_window, 1_000_000, "the rule");
    }

    #[test]
    fn a_structurally_broken_definition_will_not_connect() {
        let broken = LlmProviderDef::new("main", ANTHROPIC_BACKEND);
        let model = LlmModelDef::new(broken.id, "claude-sonnet-5");
        assert!(connect_model(&broken, &model).is_err());

        let unknown = LlmProviderDef::new("main", "bedrock");
        let model = LlmModelDef::new(unknown.id, "x");
        assert!(connect_model(&unknown, &model).is_err());

        // A model row of another provider.
        let provider = anthropic("main");
        let other = LlmModelDef::new(LlmProviderDefId::new(), "claude-sonnet-5");
        let err = connect_model(&provider, &other).unwrap_err().to_string();
        assert!(err.contains("does not belong"), "{err}");

        // A model setting the backend does not declare.
        let model = LlmModelDef::new(provider.id, "claude-sonnet-5").with("temperature", 0.2);
        assert!(connect_model(&provider, &model).is_err());
    }

    #[test]
    fn every_backend_connects_against_an_arbitrary_base_url() {
        // Building a client sends nothing, so this asserts exactly what
        // `connect_model` promises: a well-formed definition produces a
        // callable model without reaching the network.
        for (backend, base) in [
            (OPENAI_RESPONSES_BACKEND, "http://127.0.0.1:9/v1"),
            (ANTHROPIC_BACKEND, "http://127.0.0.1:9"),
            (OPENAI_CHAT_BACKEND, "http://127.0.0.1:9/v1"),
        ] {
            let provider = LlmProviderDef::new("p", backend)
                .with(CFG_API_KEY, "sk-x")
                .with(CFG_BASE_URL, base);
            let model = LlmModelDef::new(provider.id, "some-model");
            assert_eq!(
                connect_model(&provider, &model).unwrap().provider.model(),
                "some-model",
                "{backend}"
            );
        }
        // A local Chat Completions host with no key at all.
        let local = LlmProviderDef::new("local", OPENAI_CHAT_BACKEND)
            .with(CFG_BASE_URL, "http://127.0.0.1:11434/v1");
        let model = LlmModelDef::new(local.id, "llama3.2");
        assert!(connect_model(&local, &model).is_ok());
    }

    #[test]
    fn the_sentinel_is_never_what_gets_stored() {
        // A definition whose key is the mask is a definition that cannot work,
        // and `merge_secrets` is what stops one being built. Asserted here
        // because this crate is where the two meet.
        let spec = provider_config_spec(ANTHROPIC_BACKEND).unwrap();
        let stored = LlmProviderDef::new("main", ANTHROPIC_BACKEND)
            .with(CFG_API_KEY, "sk-real")
            .config;
        let redacted = sc_types::redact_attrs(&spec, &stored);
        assert_eq!(
            redacted.get(CFG_API_KEY),
            Some(&serde_json::json!(SECRET_SENTINEL))
        );
        let merged = sc_types::merge_secrets(&spec, &stored, &redacted);
        assert_eq!(merged.get(CFG_API_KEY), Some(&serde_json::json!("sk-real")));
    }
}

//! `sc-llm` — the LLM provider seam (design §11.1).
//!
//! Layer 6. This crate knows **providers, messages, tools and streaming, and
//! nothing about Saltcorn**: no agents, no traits, no loop, no rows. Its twin
//! above it, `sc-agent`, knows agents, traits and the loop and nothing about
//! which vendor is on the other end. That separation is the reason there are two
//! crates rather than one, and it is what lets either be replaced without the
//! other noticing.
//!
//! ## What is here
//!
//! - The **vocabulary** ([`message`]): [`LlmRequest`], [`LlmMessage`],
//!   [`ToolSpec`], [`ToolCall`], [`LlmDelta`], [`Usage`]. Ours, not a provider
//!   crate's — nothing `rig-core` exposes appears in a public signature here.
//! - The **seam** ([`provider`]): the object-safe [`LlmProvider`] trait and
//!   [`LlmStream`], with [`LlmStream::collect`] as the single place the
//!   streaming and non-streaming shapes meet.
//! - Three **adapters** ([`openai`], [`anthropic`], [`openai_chat`]) over
//!   `rig-core`, sharing one translation module (`rig_bridge`).
//! - What the loop needs to know about a model: its [`ModelCapabilities`]
//!   ([`capabilities`]), its [`Prices`] ([`pricing`]) and a token estimate
//!   ([`estimate`]).
//! - The **call log** ([`logging`]): every configured provider is wrapped in a
//!   [`LoggedProvider`], so a model call reports itself — a summary line with
//!   its token cost at `info`, and the whole request and response at `trace`
//!   (§16).
//! - The **configured entities** ([`def`], [`model`], [`storage`]):
//!   [`LlmProviderDef`] and its [`LlmModelDef`]s, the backend registry,
//!   `_fd_llm_providers` and `_fd_llm_models`, and [`connect_model`] — a
//!   provider and its models are named records an admin fills in, exactly as a
//!   file store is.
//!
//! ## What is deliberately not here
//!
//! rig's `Agent`, its tool registry, its RAG and vector stores. The loop is
//! `sc-agent`'s because it persists to `_fd_runs`, runs every tool as the
//! chatting user, and streams to a browser — none of which a provider crate can
//! know about. Concretely, rig's `CompletionModel` is not object-safe (associated
//! types, `impl Future`, `Clone`), so a `Box<dyn>` chosen from stored
//! configuration needs a seam whatever crate is underneath.
//!
//! Encryption at rest for API keys is out of scope for this milestone, by
//! decision: a key sits in the primary database like every other configuration
//! value. What *is* guaranteed is that it does not leave through the API —
//! see [`sc_types::redact_attrs`] and [`FormField::secret`](sc_types::FormField::secret).

pub mod anthropic;
pub mod capabilities;
pub mod def;
pub mod estimate;
pub mod listing;
pub mod logging;
pub mod message;
pub mod model;
pub mod openai;
pub mod openai_chat;
pub mod pricing;
pub mod provider;
mod rig_bridge;
pub mod storage;

pub use capabilities::{
    CFG_CONTEXT_WINDOW, CFG_EDIT_FORMAT, CFG_NATIVE_APPLY_PATCH, CFG_PARALLEL_TOOL_CALLS,
    CFG_PARALLEL_TOOL_CALLS_DEFAULT, CFG_PROMPT_CACHING, CFG_REASONING_REPLAY, CFG_VISION,
    CFG_WORKING_BUDGET, EditFormat, MAX_BUILT_IN_WORKING_BUDGET, ModelCapabilities, PromptCaching,
    UNKNOWN_CONTEXT_WINDOW, UNKNOWN_WORKING_BUDGET,
};
pub use def::{
    ANTHROPIC_BACKEND, CFG_API_KEY, CFG_BASE_URL, LlmProviderDef, LlmProviderDefId,
    OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND, anthropic_config_spec, connect_model,
    openai_chat_config_spec, openai_config_spec, provider_config_spec, registered_backends,
    validate_provider_config,
};
pub use estimate::{TokenEstimator, estimate_tokens, image_tokens};
pub use listing::{fetch_host_models, parse_model_listing};
pub use logging::{LoggedProvider, request_summary, response_summary};
pub use message::{
    AssistantMessage, CachePlan, ImagePart, LlmDelta, LlmMessage, LlmRequest, ProviderItem,
    StopReason, ToolCall, ToolSpec, Usage,
};
pub use model::{
    ConnectedModel, LlmModelDef, LlmModelDefId, model_config_spec, normalise_model_config,
    validate_model_config,
};
pub use pricing::{
    CFG_PRICE_CACHE_WRITE, CFG_PRICE_CACHED_INPUT, CFG_PRICE_INPUT, CFG_PRICE_OUTPUT, Prices,
};
pub use provider::{DeltaStream, LlmProvider, LlmStream};
pub use storage::{
    LLM_MODELS_TABLE, LLM_PROVIDERS_TABLE, bootstrap_llm_models, bootstrap_llm_providers,
    check_model_saveable, check_provider_saveable, delete_llm_model, delete_llm_provider,
    list_llm_models, list_llm_providers, load_llm_model, load_llm_model_by_name, load_llm_provider,
    load_llm_provider_by_name, require_llm_model, require_llm_provider, save_llm_model,
    save_llm_provider,
};

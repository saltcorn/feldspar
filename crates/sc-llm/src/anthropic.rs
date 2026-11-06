//! The `anthropic` adapter: an [`LlmProvider`] over Anthropic's messages API
//! (design §11.1).
//!
//! The twin of [`openai`](crate::openai), and deliberately as thin: everything
//! that differs between the two vendors — content blocks versus output items,
//! the shape of a streamed tool call, where usage is reported — is inside rig,
//! and by the time a request or a stream reaches
//! [`rig_bridge`](crate::rig_bridge) the two are one shape.

use async_trait::async_trait;
use rig_core::client::CompletionClient;
use rig_core::completion::CompletionModel as _;
use rig_core::providers::anthropic;
use sc_error::{Error, Result};

use crate::capabilities::{ModelCapabilities, PromptCaching};
use crate::message::LlmRequest;
use crate::provider::{LlmProvider, LlmStream};
use crate::rig_bridge::{Wire, map_stream, provider_error, to_rig_request};

/// The default endpoint, used when the provider's `base_url` is left blank.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic's default cap on generated tokens.
///
/// Anthropic **requires** `max_tokens` on every request, unlike OpenAI, so a
/// request that does not set one gets this rather than a rejection the admin
/// would have to decode. A generous default, because the cap that matters is the
/// one an agent sets deliberately; this one exists so the call is well-formed.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// An Anthropic endpoint, bound to one model.
pub struct Anthropic {
    model: anthropic::completion::CompletionModel<reqwest::Client>,
    model_name: String,
    capabilities: ModelCapabilities,
}

impl Anthropic {
    /// Connect to `base_url` with `api_key`, ready to call `model`. As with
    /// [`OpenAiResponses::new`](crate::openai::OpenAiResponses::new), nothing is
    /// sent until the first request.
    pub fn new(
        base_url: &str,
        api_key: &str,
        model: impl Into<String>,
        capabilities: ModelCapabilities,
    ) -> Result<Anthropic> {
        Anthropic::with_http(
            base_url,
            api_key,
            model,
            capabilities,
            crate::http::provider_client(crate::http::PROVIDER_READ_TIMEOUT)?,
        )
    }

    /// [`new`](Self::new), streaming on `http` — a test's client with a
    /// short read timeout.
    pub(crate) fn with_http(
        base_url: &str,
        api_key: &str,
        model: impl Into<String>,
        capabilities: ModelCapabilities,
        http: reqwest::Client,
    ) -> Result<Anthropic> {
        let base_url = if base_url.trim().is_empty() {
            DEFAULT_BASE_URL
        } else {
            base_url.trim()
        };
        let client = anthropic::Client::builder()
            .http_client(http)
            .api_key(api_key)
            .base_url(base_url)
            .build()
            .map_err(|e| {
                Error::config(format!(
                    "cannot build an Anthropic client for {base_url}: {e}"
                ))
            })?;
        let model_name = model.into();
        Ok(Anthropic {
            model: client.completion_model(&model_name),
            model_name,
            capabilities,
        })
    }
}

#[async_trait]
impl LlmProvider for Anthropic {
    fn model(&self) -> &str {
        &self.model_name
    }

    async fn stream(&self, mut req: LlmRequest) -> Result<LlmStream> {
        req.max_tokens = Some(req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS));
        // rig places Anthropic's breakpoints itself — the system prompt, the
        // last tool and the last message — so a plan asking for any is a plan
        // asking for those. A breakpoint after the session header cannot be
        // placed through rig 0.41 (see `rig_bridge`).
        let cached =
            !req.cache.is_empty() && self.capabilities.prompt_caching == PromptCaching::Explicit;
        let request = to_rig_request(req, Wire::Anthropic, &self.model_name, &self.capabilities)?;
        let response = if cached {
            self.model
                .clone()
                .with_prompt_caching()
                .stream(request)
                .await
        } else {
            self.model.stream(request).await
        }
        .map_err(|e| provider_error(&e))?;
        Ok(LlmStream::new(map_stream(
            response,
            Wire::Anthropic,
            self.capabilities.reasoning_replay,
        )))
    }
}

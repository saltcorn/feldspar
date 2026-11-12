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

/// The cap on generated tokens for a model this does not recognise, and for
/// Claude 3 and older.
///
/// Anthropic **requires** `max_tokens` on every request, unlike OpenAI, so a
/// request that does not set one gets [`default_max_tokens`] rather than a
/// rejection the admin would have to decode.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// The cap for Claude 4 and later, and for Claude 3.7. Every one of them can
/// write at least this much in one answer, and thinking counts against it, so
/// a small cap cuts off a long tool call (a plan, a whole file) half-written.
/// Only the tokens generated are billed, so a higher cap costs nothing by
/// itself.
pub const MODERN_MAX_TOKENS: u32 = 32_000;

/// The cap a request to `model` gets when it sets none.
pub fn default_max_tokens(model: &str) -> u32 {
    match crate::capabilities::claude_version(&model.trim().to_ascii_lowercase()) {
        Some((major, minor)) if major >= 4 || (major == 3 && minor >= 7) => MODERN_MAX_TOKENS,
        _ => DEFAULT_MAX_TOKENS,
    }
}

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
        let base_url = if base_url.trim().is_empty() {
            DEFAULT_BASE_URL
        } else {
            base_url.trim()
        };
        let client = anthropic::Client::builder()
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

    fn output_cap(&self, requested: Option<u32>) -> Option<u32> {
        Some(requested.unwrap_or_else(|| default_max_tokens(&self.model_name)))
    }

    async fn stream(&self, mut req: LlmRequest) -> Result<LlmStream> {
        req.max_tokens = self.output_cap(req.max_tokens);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_modern_claude_may_write_a_long_answer_and_an_old_or_unknown_one_keeps_the_small_cap() {
        for model in ["claude-opus-5", "claude-opus-5-5", "claude-sonnet-4-20250514", "claude-haiku-4-5", "claude-3-7-sonnet-latest"] {
            assert_eq!(default_max_tokens(model), MODERN_MAX_TOKENS, "{model}");
        }
        for model in ["claude-3-5-haiku-latest", "claude-3-opus-20240229", "my-proxy-model"] {
            assert_eq!(default_max_tokens(model), DEFAULT_MAX_TOKENS, "{model}");
        }
    }
}

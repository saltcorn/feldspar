//! The `openai_responses` adapter: an [`LlmProvider`] over OpenAI's **Responses**
//! API (design §11.1).
//!
//! The Responses API rather than Chat Completions, and that is the whole reason
//! `rig-core` was chosen over the crate the request named: it is where OpenAI's
//! reasoning models are, and where their compatible reimplementations have
//! settled.
//!
//! **`base_url` is a field value, not a code change.** Any endpoint that speaks
//! the Responses API — a local server, a gateway, an alternative vendor — is
//! reached by typing its URL into the provider form. That is what makes
//! "OpenAI-compatible" a configuration fact rather than a list of vendors
//! somebody has to keep extending.

use async_trait::async_trait;
use rig_core::client::CompletionClient;
use rig_core::completion::CompletionModel as _;
use rig_core::providers::openai;
use sc_error::{Error, Result};

use crate::capabilities::ModelCapabilities;
use crate::message::LlmRequest;
use crate::provider::{LlmProvider, LlmStream};
use crate::rig_bridge::{Wire, map_stream, provider_error, to_rig_request};

/// The default endpoint, used when the provider's `base_url` is left blank.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// An OpenAI-compatible Responses endpoint, bound to one model.
pub struct OpenAiResponses {
    model: openai::responses_api::ResponsesCompletionModel<reqwest::Client>,
    model_name: String,
    capabilities: ModelCapabilities,
}

impl OpenAiResponses {
    /// Connect to `base_url` with `api_key`, ready to call `model`.
    ///
    /// Nothing is sent here: this builds a client, and a wrong key or an
    /// unreachable host is discovered on the first [`stream`](LlmProvider::stream).
    /// That is the same split file stores draw between a *structurally* wrong
    /// definition and an unreachable one (§14.1) — and it is why the admin form
    /// has a **Test connection** button, which is the deliberate way to find out
    /// now rather than inside a chat transcript.
    ///
    /// `capabilities` decide what the requests carry: encrypted reasoning,
    /// parallel tool calls, a cache key, images.
    pub fn new(
        base_url: &str,
        api_key: &str,
        model: impl Into<String>,
        capabilities: ModelCapabilities,
    ) -> Result<OpenAiResponses> {
        OpenAiResponses::with_http(
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
    ) -> Result<OpenAiResponses> {
        let base_url = if base_url.trim().is_empty() {
            DEFAULT_BASE_URL
        } else {
            base_url.trim()
        };
        let client = openai::Client::builder()
            .http_client(http)
            .api_key(api_key)
            .base_url(base_url)
            .build()
            .map_err(|e| {
                Error::config(format!(
                    "cannot build an OpenAI Responses client for {base_url}: {e}"
                ))
            })?;
        let model_name = model.into();
        Ok(OpenAiResponses {
            model: client.completion_model(&model_name),
            model_name,
            capabilities,
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiResponses {
    fn model(&self) -> &str {
        &self.model_name
    }

    async fn stream(&self, req: LlmRequest) -> Result<LlmStream> {
        let request = to_rig_request(req, Wire::Responses, &self.model_name, &self.capabilities)?;
        let response = self
            .model
            .stream(request)
            .await
            .map_err(|e| provider_error(&e))?;
        Ok(LlmStream::new(map_stream(
            response,
            Wire::Responses,
            self.capabilities.reasoning_replay,
        )))
    }
}

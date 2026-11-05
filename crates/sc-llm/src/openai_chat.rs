//! The `openai_chat` adapter: an [`LlmProvider`] over **Chat Completions**
//! (TODO §1).
//!
//! Chat Completions is what most cheap and open-weight hosts serve: vLLM,
//! llama.cpp, Ollama, OpenRouter, DeepSeek. So this backend's provider has no
//! default host. Its `base_url` is required and its key is optional, because a
//! local host takes none.
//!
//! What the API lacks, the harness does instead, and the capability table
//! records it: there is no native `apply_patch` and no reasoning replay, and a
//! tool result cannot carry an image, so an image follows in a user message
//! ([`rig_bridge`](crate::rig_bridge)).

use async_trait::async_trait;
use rig_core::client::CompletionClient;
use rig_core::completion::CompletionModel as _;
use rig_core::providers::openai;
use sc_error::{Error, Result};

use crate::capabilities::ModelCapabilities;
use crate::message::LlmRequest;
use crate::provider::{LlmProvider, LlmStream};
use crate::rig_bridge::{Wire, map_stream, provider_error, to_rig_request};

/// A Chat Completions endpoint, bound to one model.
pub struct OpenAiChat {
    model: openai::completion::CompletionModel<reqwest::Client>,
    model_name: String,
    capabilities: ModelCapabilities,
}

impl OpenAiChat {
    /// Connect to `base_url` with `api_key` (which may be blank), ready to call
    /// `model`. Nothing is sent until the first request.
    pub fn new(
        base_url: &str,
        api_key: &str,
        model: impl Into<String>,
        capabilities: ModelCapabilities,
    ) -> Result<OpenAiChat> {
        OpenAiChat::with_http(
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
    ) -> Result<OpenAiChat> {
        let base_url = base_url.trim();
        if base_url.is_empty() {
            return Err(Error::config(
                "a Chat Completions provider needs a base URL; there is no default host",
            ));
        }
        let client = openai::Client::builder()
            .http_client(http)
            .api_key(api_key)
            .base_url(base_url)
            .build()
            .map_err(|e| {
                Error::config(format!(
                    "cannot build a Chat Completions client for {base_url}: {e}"
                ))
            })?
            .completions_api();
        let model_name = model.into();
        Ok(OpenAiChat {
            model: client.completion_model(&model_name),
            model_name,
            capabilities,
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiChat {
    fn model(&self) -> &str {
        &self.model_name
    }

    async fn stream(&self, req: LlmRequest) -> Result<LlmStream> {
        let request = to_rig_request(req, Wire::Chat, &self.model_name, &self.capabilities)?;
        let response = self
            .model
            .stream(request)
            .await
            .map_err(|e| provider_error(&e))?;
        Ok(LlmStream::new(map_stream(response, Wire::Chat, false)))
    }
}

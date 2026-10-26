//! What the three non-chat calls share (TODO-agent-skills AS3): which URL a
//! provider's endpoint is at, one authenticated POST, the error a refusal
//! becomes, and the [`ModelCall`] each call reports for the call log and the
//! meter.
//!
//! None of them is an [`LlmProvider`](crate::LlmProvider) method: nothing
//! about them streams, calls tools or carries a conversation. Each is one
//! request to an OpenAI-shaped endpoint, which OpenAI and the OpenAI-compatible
//! hosts serve alike. Anthropic serves none of them, so a provider of that
//! backend is refused by name rather than sent a request that can only fail.

use std::time::Duration;

use sc_error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::def::{
    ANTHROPIC_BACKEND, CFG_API_KEY, CFG_BASE_URL, LlmProviderDef, OPENAI_CHAT_BACKEND,
    OPENAI_RESPONSES_BACKEND,
};
use crate::message::Usage;
use crate::model::{LlmModelDef, ModelKind};

/// One non-chat model call, as it is logged and charged: which model, what the
/// provider reported, how many things were made, and what it cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCall {
    /// What kind of call it was.
    pub kind: ModelKind,
    /// The provider's name.
    pub provider: String,
    /// The model's name, as sent on the wire.
    pub model: String,
    /// The tokens the provider reported; zero where it reports none (speech).
    pub usage: Usage,
    /// What was made: texts embedded, images generated, characters spoken.
    pub units: u64,
    /// What it cost at the model row's prices, or `None` when they are unknown.
    pub cost: Option<f64>,
    /// How long it took, in milliseconds.
    pub elapsed_ms: u64,
}

impl ModelCall {
    /// The call log's line for it (§16), at info, as a chat call has.
    pub(crate) fn log(&self) {
        let unit = match self.kind {
            ModelKind::Chat => "answers",
            ModelKind::Embedding => "texts embedded",
            ModelKind::Image => "images",
            ModelKind::Speech => "characters spoken",
        };
        let tokens = if self.usage.input_tokens + self.usage.output_tokens > 0 {
            format!(
                " · {} in / {} out tokens",
                self.usage.input_tokens, self.usage.output_tokens
            )
        } else {
            String::new()
        };
        let cost = self
            .cost
            .map_or_else(|| "cost unknown".to_owned(), |c| format!("cost {c:.6}"));
        sc_log::log_info!(
            "llm ← {}/{}: {} {unit}{tokens} · {cost} · {} ms",
            self.provider,
            self.model,
            self.units,
            self.elapsed_ms
        );
    }
}

/// A provider's OpenAI-shaped endpoint, ready for one kind of call.
pub(crate) struct Endpoint<'a> {
    provider: &'a LlmProviderDef,
    model: &'a LlmModelDef,
    base_url: String,
    api_key: String,
    client: reqwest::Client,
    started: std::time::Instant,
}

impl<'a> Endpoint<'a> {
    /// The endpoint `model` of `provider` is called at, refusing a model of
    /// another kind, one of another provider, and an Anthropic provider.
    pub(crate) fn new(
        provider: &'a LlmProviderDef,
        model: &'a LlmModelDef,
        kind: ModelKind,
        timeout: Duration,
    ) -> Result<Endpoint<'a>> {
        if model.name.trim().is_empty() {
            return Err(Error::invalid(format!(
                "{} needs {} model of LLM provider `{}`",
                what(kind),
                kind.with_article(),
                provider.name
            )));
        }
        if model.kind != kind {
            return Err(Error::invalid(format!(
                "LLM model `{}` is {} model, not {} model",
                model.name,
                model.kind.with_article(),
                kind.with_article()
            )));
        }
        if model.provider_id != provider.id {
            return Err(Error::invalid(format!(
                "LLM model `{}` does not belong to provider `{}`",
                model.name, provider.name
            )));
        }
        let base_url = match provider.backend.as_str() {
            ANTHROPIC_BACKEND => {
                return Err(Error::invalid(format!(
                    "LLM provider `{}` is an Anthropic provider, and Anthropic has no {} \
                     endpoint; use an OpenAI or OpenAI-compatible provider",
                    provider.name,
                    endpoint_name(kind)
                )));
            }
            OPENAI_RESPONSES_BACKEND => provider
                .setting(CFG_BASE_URL)
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .unwrap_or(crate::openai::DEFAULT_BASE_URL),
            OPENAI_CHAT_BACKEND => provider
                .setting(CFG_BASE_URL)
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .ok_or_else(|| {
                    Error::config(format!("LLM provider `{}` has no base URL", provider.name))
                })?,
            other => return Err(crate::def::unknown_backend(other)),
        };
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| Error::msg(format!("building the {kind} client: {e}")))?;
        Ok(Endpoint {
            provider,
            model,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key: provider
                .setting(CFG_API_KEY)
                .unwrap_or_default()
                .trim()
                .to_owned(),
            client,
            started: std::time::Instant::now(),
        })
    }

    /// The model's name, as sent on the wire.
    pub(crate) fn model(&self) -> &str {
        self.model.name.trim()
    }

    /// The error for a call that failed, naming the provider and model.
    pub(crate) fn failed(&self, doing: &str, e: &dyn std::fmt::Display) -> Error {
        Error::msg(format!(
            "LLM provider `{}` could not {doing} with `{}`: {e}",
            self.provider.name,
            self.model()
        ))
    }

    /// POST `body` to `{base_url}{path}` and return the reply's bytes and
    /// content type, or the provider's refusal with its status and (the start
    /// of) its body.
    pub(crate) async fn post(
        &self,
        path: &str,
        body: &Json,
        doing: &str,
    ) -> Result<(Vec<u8>, Option<String>)> {
        let mut request = self
            .client
            .post(format!("{}{path}", self.base_url))
            .json(body);
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request.send().await.map_err(|e| self.failed(doing, &e))?;
        self.read(response, doing).await
    }

    /// GET a URL the provider's reply named (an image a host serves by link
    /// rather than inline). Not authenticated: the link is not the API's.
    pub(crate) async fn fetch(&self, url: &str, doing: &str) -> Result<(Vec<u8>, Option<String>)> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| self.failed(doing, &e))?;
        self.read(response, doing).await
    }

    async fn read(
        &self,
        response: reqwest::Response,
        doing: &str,
    ) -> Result<(Vec<u8>, Option<String>)> {
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await.map_err(|e| self.failed(doing, &e))?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            let text: String = text.chars().take(500).collect();
            return Err(self.failed(doing, &format!("{status}: {text}")));
        }
        Ok((body.to_vec(), content_type))
    }

    /// The reply's bytes as JSON.
    pub(crate) fn json(&self, body: &[u8], doing: &str) -> Result<Json> {
        serde_json::from_slice(body)
            .map_err(|e| self.failed(doing, &format!("the reply is not JSON: {e}")))
    }

    /// The finished call's record, logged.
    pub(crate) fn finish(&self, usage: Usage, units: u64, cost: Option<f64>) -> ModelCall {
        let call = ModelCall {
            kind: self.model.kind,
            provider: self.provider.name.clone(),
            model: self.model().to_owned(),
            usage,
            units,
            cost,
            elapsed_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        call.log();
        call
    }
}

/// The usage an OpenAI reply reports, read leniently: `prompt_tokens` or
/// `input_tokens`, `output_tokens`; absent is zero.
pub(crate) fn read_usage(reply: &Json) -> Usage {
    let usage = reply.get("usage");
    let count = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.and_then(|u| u.get(*k)).and_then(Json::as_u64))
            .unwrap_or(0)
    };
    Usage {
        input_tokens: count(&["input_tokens", "prompt_tokens"]),
        output_tokens: count(&["output_tokens"]),
        ..Usage::default()
    }
}

fn what(kind: ModelKind) -> &'static str {
    match kind {
        ModelKind::Chat => "a conversation",
        ModelKind::Embedding => "an embedding",
        ModelKind::Image => "an image",
        ModelKind::Speech => "speech",
    }
}

fn endpoint_name(kind: ModelKind) -> &'static str {
    match kind {
        ModelKind::Chat => "chat",
        ModelKind::Embedding => "embedding",
        ModelKind::Image => "image generation",
        ModelKind::Speech => "speech",
    }
}

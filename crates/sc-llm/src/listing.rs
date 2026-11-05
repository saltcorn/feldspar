//! *Fetch models*: asking a provider's host which models it serves (TODO §3a).
//!
//! `GET /models` on the OpenAI-style APIs and most Chat Completions hosts, and
//! `GET /v1/models` on Anthropic's. The answer is only a list of names to
//! offer. Each one the admin picks becomes a model row with blank settings,
//! which means built-in defaults.
//!
//! A host with no listing endpoint is told apart from one that failed, so the
//! admin knows to type the name rather than to fix the key.

use std::time::Duration;

use sc_error::{Error, Result};
use serde_json::Value as Json;

use crate::def::{
    ANTHROPIC_BACKEND, CFG_BASE_URL, LlmProviderDef, OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND,
    validate_provider_config,
};

/// How long a listing may take. It is a single small request an admin is
/// waiting on.
const LISTING_TIMEOUT: Duration = Duration::from_secs(20);

/// The anthropic API version header every request to it carries.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The model names `provider`'s host lists, sorted and without duplicates.
///
/// An error that says the host has **no listing** is `NotFound`, so a caller
/// can show "type the name" rather than a failure.
pub async fn fetch_host_models(provider: &LlmProviderDef) -> Result<Vec<String>> {
    validate_provider_config(provider)?;
    let key = provider.api_key();
    let key = key.expose().as_str();
    let base = provider.setting(CFG_BASE_URL).unwrap_or_default().trim();

    let client = sc_net::client_builder()
        .timeout(LISTING_TIMEOUT)
        .build()
        .map_err(|e| Error::msg(format!("cannot build an HTTP client: {e}")))?;

    let request = match provider.backend.as_str() {
        ANTHROPIC_BACKEND => {
            let base = if base.is_empty() {
                crate::anthropic::DEFAULT_BASE_URL
            } else {
                base
            };
            client
                .get(format!(
                    "{}/v1/models?limit=1000",
                    base.trim_end_matches('/')
                ))
                .header("x-api-key", key)
                .header("anthropic-version", ANTHROPIC_VERSION)
        }
        OPENAI_RESPONSES_BACKEND | OPENAI_CHAT_BACKEND => {
            let base = if base.is_empty() {
                crate::openai::DEFAULT_BASE_URL
            } else {
                base
            };
            let request = client.get(format!("{}/models", base.trim_end_matches('/')));
            if key.is_empty() {
                request
            } else {
                request.bearer_auth(key)
            }
        }
        other => return Err(crate::def::unknown_backend(other)),
    };

    let response = request.send().await.map_err(|e| {
        Error::msg(format!(
            "could not reach LLM provider `{}` to list its models: {e}",
            provider.name
        ))
    })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
    {
        return Err(no_listing(provider));
    }
    if !status.is_success() {
        return Err(Error::msg(format!(
            "LLM provider `{}` refused to list its models ({status}): {}",
            provider.name,
            body.chars().take(500).collect::<String>()
        )));
    }
    parse_model_listing(&body).ok_or_else(|| no_listing(provider))
}

/// The names in a listing body: `{"data": [{"id": …}, …]}`, the shape both
/// OpenAI and Anthropic use. Ollama's native `{"models": [{"name": …}]}` is
/// read too. `None` when the body is neither.
pub fn parse_model_listing(body: &str) -> Option<Vec<String>> {
    let json: Json = serde_json::from_str(body).ok()?;
    let (items, key) = match json.get("data").and_then(Json::as_array) {
        Some(data) => (data, "id"),
        None => (json.get("models").and_then(Json::as_array)?, "name"),
    };
    let mut names: Vec<String> = items
        .iter()
        .filter_map(|item| item.get(key).and_then(Json::as_str))
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    Some(names)
}

fn no_listing(provider: &LlmProviderDef) -> Error {
    Error::not_found(format!(
        "LLM provider `{}` has no model listing; type the model name instead",
        provider.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings_are_read_in_both_shapes() {
        let openai =
            r#"{"object":"list","data":[{"id":"gpt-5.1"},{"id":"gpt-4.1"},{"id":"gpt-5.1"}]}"#;
        assert_eq!(
            parse_model_listing(openai),
            Some(vec!["gpt-4.1".to_owned(), "gpt-5.1".to_owned()])
        );
        let ollama = r#"{"models":[{"name":"llama3.2:latest"}]}"#;
        assert_eq!(
            parse_model_listing(ollama),
            Some(vec!["llama3.2:latest".to_owned()])
        );
        assert_eq!(parse_model_listing("<html>not here</html>"), None);
        assert_eq!(parse_model_listing(r#"{"error":"nope"}"#), None);
    }
}

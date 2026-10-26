//! Embeddings: text in, vectors out (TODO-agent-skills AS3.2, built for AS2's
//! vector fields).
//!
//! The one non-chat call a vector field needs. It is not an [`LlmProvider`]
//! method, because nothing about it streams, calls tools or carries a
//! conversation: it is one request to the provider's `/embeddings` endpoint,
//! which OpenAI and every OpenAI-compatible host (Ollama, vLLM, llama.cpp,
//! OpenRouter, …) serve with the same shape:
//!
//! ```text
//! POST {base_url}/embeddings   { "model": "…", "input": ["…", "…"] }
//! → { "data": [ { "index": 0, "embedding": [0.01, …] }, … ] }
//! ```
//!
//! Anthropic has no embedding endpoint, so a provider of that backend is
//! refused by name rather than sent a request that can only fail.
//!
//! [`LlmProvider`]: crate::LlmProvider

use std::time::Duration;

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};

use crate::def::{
    ANTHROPIC_BACKEND, CFG_API_KEY, CFG_BASE_URL, LlmProviderDef, OPENAI_CHAT_BACKEND,
    OPENAI_RESPONSES_BACKEND,
};

/// The most texts sent in one request. OpenAI takes 2,048; a smaller batch
/// keeps a slow local host's request inside the timeout.
pub const EMBED_BATCH: usize = 128;

/// How long one embedding request may take.
const EMBED_TIMEOUT: Duration = Duration::from_secs(120);

/// The provider an embedding is computed by: the one named, or the
/// installation's default when `name` is `None` or blank.
pub async fn embedding_provider(catalog: &Catalog, name: Option<&str>) -> Result<LlmProviderDef> {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => crate::storage::require_llm_provider(catalog, name).await,
        None => crate::host::default_llm_provider(catalog)
            .await?
            .ok_or_else(|| {
                Error::config(
                    "there is no LLM provider to compute embeddings with; add an OpenAI or \
                     OpenAI-compatible provider on the LLM providers screen",
                )
            }),
    }
}

/// Embed `texts` with `model` of `provider`: one vector per text, in order.
///
/// Refused for an Anthropic provider (there is no endpoint), and for a reply
/// that does not have one vector per text.
pub async fn embed(
    provider: &LlmProviderDef,
    model: &str,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    let model = model.trim();
    if model.is_empty() {
        return Err(Error::invalid(format!(
            "an embedding needs a model of LLM provider `{}`, such as text-embedding-3-small",
            provider.name
        )));
    }
    let base_url = match provider.backend.as_str() {
        ANTHROPIC_BACKEND => {
            return Err(Error::invalid(format!(
                "LLM provider `{}` is an Anthropic provider, and Anthropic has no embedding \
                 endpoint; compute embeddings with an OpenAI or OpenAI-compatible provider",
                provider.name
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
    let url = format!("{}/embeddings", base_url.trim_end_matches('/'));
    let api_key = provider.setting(CFG_API_KEY).unwrap_or_default().trim();
    let client = reqwest::Client::builder()
        .timeout(EMBED_TIMEOUT)
        .build()
        .map_err(|e| Error::msg(format!("building the embedding client: {e}")))?;
    let mut out = Vec::with_capacity(texts.len());
    for batch in texts.chunks(EMBED_BATCH) {
        let mut request = client
            .post(&url)
            .json(&json!({ "model": model, "input": batch }));
        if !api_key.is_empty() {
            request = request.bearer_auth(api_key);
        }
        let failed = |e: &dyn std::fmt::Display| {
            Error::msg(format!(
                "LLM provider `{}` could not embed with `{model}`: {e}",
                provider.name
            ))
        };
        let response = request.send().await.map_err(|e| failed(&e))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|e| failed(&e))?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            let text: String = text.chars().take(500).collect();
            return Err(failed(&format!("{status}: {text}")));
        }
        let parsed: Json = serde_json::from_slice(&body)
            .map_err(|e| failed(&format!("the reply is not JSON: {e}")))?;
        out.extend(read_embeddings(&parsed, batch.len()).map_err(|e| failed(&e))?);
    }
    Ok(out)
}

/// The vectors of an `/embeddings` reply, in input order, or why it is not one
/// with `expected` of them.
fn read_embeddings(reply: &Json, expected: usize) -> std::result::Result<Vec<Vec<f32>>, String> {
    let data = reply
        .get("data")
        .and_then(Json::as_array)
        .ok_or("the reply has no `data` array")?;
    let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
    for (position, item) in data.iter().enumerate() {
        let index = item
            .get("index")
            .and_then(Json::as_u64)
            .map_or(position, |i| i as usize);
        let embedding = item
            .get("embedding")
            .and_then(Json::as_array)
            .ok_or("an item of the reply has no `embedding` array")?
            .iter()
            .map(|x| x.as_f64().map(|x| x as f32))
            .collect::<Option<Vec<f32>>>()
            .ok_or("an embedding holds something other than numbers")?;
        indexed.push((index, embedding));
    }
    if indexed.len() != expected {
        return Err(format!(
            "the reply has {} embeddings for {expected} texts",
            indexed.len()
        ));
    }
    indexed.sort_by_key(|(i, _)| *i);
    Ok(indexed.into_iter().map(|(_, v)| v).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_read_in_input_order() {
        let reply = json!({ "data": [
            { "index": 1, "embedding": [0.0, 1.0] },
            { "index": 0, "embedding": [1.0, 0.0] },
        ]});
        assert_eq!(
            read_embeddings(&reply, 2).unwrap(),
            vec![vec![1.0, 0.0], vec![0.0, 1.0]]
        );
        assert!(
            read_embeddings(&reply, 3)
                .unwrap_err()
                .contains("2 embeddings for 3")
        );
        assert!(read_embeddings(&json!({}), 1).is_err());
    }

    #[tokio::test]
    async fn anthropic_is_refused_by_name() {
        let def = LlmProviderDef::new("claude", ANTHROPIC_BACKEND).with(CFG_API_KEY, "k");
        let err = embed(&def, "any", &["x".to_owned()])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Anthropic has no embedding endpoint"), "{err}");
    }
}

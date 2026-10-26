//! Embeddings: text in, vectors out (TODO-agent-skills AS3.2, built for AS2's
//! vector fields).
//!
//! One request to the provider's `/embeddings` endpoint (see `endpoint.rs` for
//! what the non-chat calls share), which OpenAI and every OpenAI-compatible
//! host (Ollama, vLLM, llama.cpp, OpenRouter, …) serve with the same shape:
//!
//! ```text
//! POST {base_url}/embeddings   { "model": "…", "input": ["…", "…"] }
//! → { "data": [ { "index": 0, "embedding": [0.01, …] }, … ] }
//! ```
//!
//! Anthropic has no embedding endpoint, so a provider of that backend is
//! refused by name rather than sent a request that can only fail.

use std::time::Duration;

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};

use crate::def::LlmProviderDef;
use crate::endpoint::{Endpoint, ModelCall, read_usage};
use crate::model::{LlmModelDef, ModelKind};

/// The most texts sent in one request. OpenAI takes 2,048; a smaller batch
/// keeps a slow local host's request inside the timeout.
pub const EMBED_BATCH: usize = 128;

/// How long one embedding request may take.
const EMBED_TIMEOUT: Duration = Duration::from_secs(120);

/// What [`embed`] returns: one vector per text, in order, and the call.
#[derive(Debug, Clone, PartialEq)]
pub struct Embeddings {
    /// One vector per text.
    pub vectors: Vec<Vec<f32>>,
    /// What the call (every batch of it) used and cost.
    pub call: ModelCall,
}

/// The provider a non-chat call is made with: the one named, or the
/// installation's default when `name` is `None` or blank.
pub async fn embedding_provider(catalog: &Catalog, name: Option<&str>) -> Result<LlmProviderDef> {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => crate::storage::require_llm_provider(catalog, name).await,
        None => crate::host::default_llm_provider(catalog)
            .await?
            .ok_or_else(|| {
                Error::config(
                    "there is no LLM provider to call; add an OpenAI or OpenAI-compatible \
                     provider on the LLM providers screen",
                )
            }),
    }
}

/// The provider and model of `kind` a configuration names
/// (TODO-agent-skills AS3.1): the provider named, or the installation's
/// default; and the model named, or that provider's default of `kind`.
pub async fn resolve_model(
    catalog: &Catalog,
    provider: Option<&str>,
    model: Option<&str>,
    kind: ModelKind,
) -> Result<(LlmProviderDef, LlmModelDef)> {
    let provider = embedding_provider(catalog, provider).await?;
    let model = crate::storage::require_model_of_kind(catalog, &provider, model, kind).await?;
    Ok((provider, model))
}

/// Embed `texts` with embedding model `model` of `provider`: one vector per
/// text, in order, with what the call cost at the model's input price.
///
/// Refused for a model of another kind, for an Anthropic provider (there is no
/// endpoint), and for a reply that does not have one vector per text.
pub async fn embed(
    provider: &LlmProviderDef,
    model: &LlmModelDef,
    texts: &[String],
) -> Result<Embeddings> {
    let endpoint = Endpoint::new(provider, model, ModelKind::Embedding, EMBED_TIMEOUT)?;
    let mut vectors = Vec::with_capacity(texts.len());
    let mut usage = crate::Usage::default();
    for batch in texts.chunks(EMBED_BATCH) {
        let body = json!({ "model": endpoint.model(), "input": batch });
        let (reply, _) = endpoint.post("/embeddings", &body, "embed").await?;
        let parsed = endpoint.json(&reply, "embed")?;
        vectors.extend(
            read_embeddings(&parsed, batch.len()).map_err(|e| endpoint.failed("embed", &e))?,
        );
        usage.input_tokens += read_usage(&parsed).input_tokens;
    }
    let cost = model.prices().embedding_cost(usage.input_tokens);
    let call = endpoint.finish(usage, texts.len() as u64, cost);
    Ok(Embeddings { vectors, call })
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
    use crate::def::{ANTHROPIC_BACKEND, CFG_API_KEY};

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
        let model = LlmModelDef::new(def.id, "any").kind(ModelKind::Embedding);
        let err = embed(&def, &model, &["x".to_owned()])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Anthropic has no embedding endpoint"), "{err}");
    }
}

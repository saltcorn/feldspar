//! `llm` — the configured LLM providers, used from a code body.
//!
//! A code body reaches a provider through the `db` host, as `op: "llm"`
//! requests, rather than as a surface of its own — `models`' reason: what it
//! reads is system tables (`_fd_llm_providers`, `_fd_llm_models`), it is
//! bounded by the same call budget, and it is the same seam in both languages,
//! the JavaScript and Python `llm` objects being prelude over it.
//!
//! ```json
//! { "op": "llm", "what": "generate", "provider": "OpenAI", "model": "gpt-5-mini",
//!   "system": "Answer briefly.", "messages": [{ "role": "user", "content": "Hi" }],
//!   "temperature": 0.2, "max_tokens": 400 }
//! { "op": "llm", "what": "embed", "input": ["a text", "another"] }
//! ```
//!
//! A blank `provider` is the installation's default provider, and a blank
//! `model` that provider's default model of the kind asked for — the rule an
//! agent and an `embed_from` field follow, so a body and an agent configured
//! the same way call the same model.
//!
//! `generate` answers `{ text, provider, model, usage, cost }` and `embed` one
//! vector per input. The call goes through the provider's call log like every
//! other model call. It is **not** charged to anybody's LLM credits: a code body
//! is admin-authored configuration, as an `embed_from` field is, and its calls
//! are the installation's.
//!
//! Refused on a [`viewer_only`](super::TableHost::viewer_only) host: the body
//! there is not the admin's to vouch for — an agent's sandbox runs code the
//! model wrote, and a provider reached from it would be a model call outside
//! the agent's own budget and its user's credits.

use sc_error::{Error, Result};
use sc_llm::{LlmMessage, LlmRequest, ModelKind};
use serde_json::{Value as Json, json};

use super::TableHost;

/// The most texts one `llm.embed` call takes.
pub const CODE_MAX_EMBED_INPUTS: usize = 2048;

/// One message of a `generate` request, as the guest spells it.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    /// `system`, `user` or `assistant`.
    role: String,
    /// The text.
    content: String,
}

/// One `llm` request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LlmPlan {
    /// Always `llm`; read before the plan is.
    #[allow(dead_code)]
    op: String,
    /// `generate` or `embed`.
    what: String,
    /// The provider's name; blank is the default provider.
    #[serde(default)]
    provider: Option<String>,
    /// The model's name; blank is the provider's default of the kind asked for.
    #[serde(default)]
    model: Option<String>,
    /// `generate`: the system prompt.
    #[serde(default)]
    system: Option<String>,
    /// `generate`: the conversation, oldest first.
    #[serde(default)]
    messages: Option<Vec<Message>>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    /// `embed`: the texts.
    #[serde(default)]
    input: Option<Vec<String>>,
}

/// Answer one `llm` request of `host`'s run.
pub(super) async fn answer(host: &TableHost<'_>, request: Json) -> Result<Json> {
    let plan: LlmPlan = serde_json::from_value(request).map_err(|e| {
        Error::invalid(format!(
            "this llm request is not one the server understands: {e}"
        ))
    })?;
    if host.viewer_only {
        return Err(Error::invalid(
            "`llm` is not available here: this code runs at its viewer's authority, and \
             a model call is the installation's to make (use it in a trigger, a workflow \
             step or a custom query)",
        ));
    }
    match plan.what.as_str() {
        "generate" => generate(host, plan).await,
        "embed" => embed(host, plan).await,
        other => Err(Error::invalid(format!(
            "`llm` has no `{other}`; it has generate and embed"
        ))),
    }
}

/// The request `plan` describes: a system prompt (the plan's own, then every
/// `system` message, in order) and the user and assistant turns.
fn chat_request(plan: &LlmPlan) -> Result<LlmRequest> {
    let mut system: Vec<String> = plan.system.iter().cloned().collect();
    let mut messages = Vec::new();
    for message in plan.messages.iter().flatten() {
        match message.role.as_str() {
            "system" => system.push(message.content.clone()),
            "user" => messages.push(LlmMessage::user(&message.content)),
            "assistant" => messages.push(LlmMessage::assistant(&message.content)),
            other => {
                return Err(Error::invalid(format!(
                    "an llm message's role is system, user or assistant, not `{other}`"
                )));
            }
        }
    }
    if !messages
        .iter()
        .any(|m| matches!(m, LlmMessage::User { .. }))
    {
        return Err(Error::invalid(
            "llm.generate() needs something to answer: a prompt, or messages with a user turn",
        ));
    }
    let system = system
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>();
    Ok(LlmRequest {
        system: (!system.is_empty()).then(|| system.join("\n\n")),
        messages,
        max_tokens: plan.max_tokens,
        temperature: plan.temperature,
        ..LlmRequest::default()
    })
}

async fn generate(host: &TableHost<'_>, plan: LlmPlan) -> Result<Json> {
    if plan.input.is_some() {
        return Err(Error::invalid(
            "`input` is llm.embed()'s; llm.generate() takes a prompt or messages",
        ));
    }
    let request = chat_request(&plan)?;
    let catalog = host.catalog;
    let provider = sc_llm::embedding_provider(catalog, plan.provider.as_deref()).await?;
    let model = sc_llm::require_llm_model(catalog, &provider, plan.model.as_deref()).await?;
    let connected = sc_llm::connect_model(&provider, &model)?;
    let answer = connected.provider.stream(request).await?.collect().await?;
    let usage = answer.usage;
    Ok(json!({
        "text": answer.content,
        "provider": provider.name,
        "model": connected.provider.model(),
        "usage": {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
        },
        "cost": usage.cost(&connected.prices),
    }))
}

async fn embed(host: &TableHost<'_>, plan: LlmPlan) -> Result<Json> {
    if plan.messages.is_some() || plan.system.is_some() {
        return Err(Error::invalid(
            "llm.embed() takes text, not messages or a system prompt",
        ));
    }
    let texts = plan.input.unwrap_or_default();
    if texts.is_empty() {
        return Err(Error::invalid(
            "llm.embed() needs a text, or a list of them",
        ));
    }
    if texts.len() > CODE_MAX_EMBED_INPUTS {
        return Err(Error::invalid(format!(
            "llm.embed() takes at most {CODE_MAX_EMBED_INPUTS} texts in one call, not {}",
            texts.len()
        )));
    }
    let (provider, model) = sc_llm::resolve_model(
        host.catalog,
        plan.provider.as_deref(),
        plan.model.as_deref(),
        ModelKind::Embedding,
    )
    .await?;
    let embedded = sc_llm::embed(&provider, &model, &texts).await?;
    serde_json::to_value(embedded.vectors).map_err(|e| Error::msg(format!("embed: {e}")))
}

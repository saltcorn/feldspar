//! The translation between this crate's vocabulary and `rig-core`'s — the only
//! module in the tree, besides the two thin adapters that use it, which names a
//! rig type.
//!
//! Both adapters share it because both providers reach rig through the *same*
//! generic `CompletionModel` interface: what differs between OpenAI's Responses
//! API and Anthropic's messages API is entirely inside rig, and by the time a
//! request or a stream reaches here the two are the same shape. Writing this
//! once is the point of decision 3 (streaming is the only shape) — the tool-call
//! assembly that providers do differently is assembled by rig, checked here, and
//! tested once.
//!
//! ## What this module is responsible for
//!
//! - **Merging consecutive tool results into one message.** Anthropic requires
//!   alternating roles, and rig does no merging: three tool results emitted as
//!   three user messages is a request Anthropic rejects outright. Our
//!   [`LlmMessage::ToolResult`] is one result, because that is the shape a loop
//!   produces them in, so the run of them is folded here.
//! - **Only emitting a tool call whose arguments parse.** rig assembles the
//!   partial JSON both providers stream, but it hands back whatever it
//!   assembled: a string that was never valid JSON arrives as a `Value::String`.
//!   A JSON string holding an object is repaired here (it is parsed). A
//!   fragment that never parsed is passed on **as a string**, so the loop can
//!   answer it as a malformed call the model can correct, and count it
//!   (TODO §10), rather than failing the whole answer over one call. The loop
//!   never hands a non-object to a trait.
//! - **Carrying the correlation id a provider will ask for back.** rig's tool
//!   call and tool result each have *two* identifiers: `id`, the item's own,
//!   and `call_id`, which the Responses API pairs a call with its output by and
//!   **requires** on every one it is sent. Ours has one, because one is what a
//!   loop correlates on — so [`ToolCall::id`] holds the `call_id` where the
//!   provider gave one, and both rig fields are filled from it on the way out.
//!   Leaving `call_id` unset is what made a second turn after any tool call
//!   fail against OpenAI outright.
//! - **Deriving the stop reason**, which neither provider's streaming response
//!   carries through rig 0.41. See [`StopReason`].
//! - **Mapping what differs by vendor** ([`Wire`]): the request options
//!   (`parallel_tool_calls`, `prompt_cache_key`, encrypted reasoning), the
//!   opaque items replayed with an assistant turn, where a tool result's images
//!   go, and what "input tokens" counts.
//!
//! ## What rig-core 0.41 exposes, and the gaps
//!
//! - **Responses:** `parallel_tool_calls`, `prompt_cache_key`, `store` and
//!   `include` are all fields of rig's `AdditionalParameters`, so they travel
//!   through `additional_params`. Reasoning items with an id and encrypted
//!   content are replayed by rig from an assistant turn's `Reasoning` content.
//! - **Anthropic:** caching is a switch on the model
//!   (`with_prompt_caching`), which marks the system prompt, the last tool and
//!   the last message. That covers the prefix and tail breakpoints of a
//!   [`CachePlan`](crate::CachePlan); **a breakpoint after the session header
//!   cannot be placed** through rig's generic messages, and is not sent.
//!   `disable_parallel_tool_use` has no field and goes in `tool_choice` through
//!   `additional_params`. Thinking blocks with signatures are replayed from
//!   `Reasoning` content.
//! - **Chat Completions:** `parallel_tool_calls` travels through
//!   `additional_params`. `prompt_cache_key` is not sent, because hosts other
//!   than OpenAI reject fields they do not know. A tool result cannot carry an
//!   image, so the image follows in a user message.

use std::collections::BTreeMap;

use futures::StreamExt;
use rig_core::completion::message::{
    AssistantContent, ImageMediaType, Message, MimeType as _, Reasoning, ReasoningContent, Text,
    ToolCall as RigToolCall, ToolFunction, ToolResult, ToolResultContent, UserContent,
};
use rig_core::completion::request::{CompletionRequest, GetTokenUsage, ToolDefinition};
use rig_core::one_or_many::OneOrMany;
use rig_core::streaming::{
    StreamedAssistantContent, StreamingCompletionResponse, ToolCallDeltaContent,
};
use sc_error::{Error, Result};
use serde_json::{Value as Json, json};

use crate::capabilities::ModelCapabilities;
use crate::message::{
    Attachment, ImagePart, LlmDelta, LlmMessage, LlmRequest, ProviderItem, StopReason, ToolCall,
    Usage,
};
use crate::provider::DeltaStream;

/// Which vendor API a request is shaped for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wire {
    /// OpenAI's Responses API.
    Responses,
    /// Anthropic's messages API.
    Anthropic,
    /// Chat Completions.
    Chat,
}

/// Build the rig request one [`LlmRequest`] describes, for `wire`, for a model
/// with `caps`.
///
/// Fails only when the conversation is empty: rig's `chat_history` is a
/// `OneOrMany`, so "at least one message" is its type, and a request with
/// nothing to answer is a caller's bug rather than something to invent a message
/// for.
pub(crate) fn to_rig_request(
    req: LlmRequest,
    wire: Wire,
    model: &str,
    caps: &ModelCapabilities,
) -> Result<CompletionRequest> {
    let additional_params = additional_params(&req, wire, caps);
    let messages = to_rig_messages(req.messages, wire, model, caps);
    let chat_history = OneOrMany::many(messages)
        .map_err(|_| Error::invalid("an LLM request needs at least one message"))?;

    Ok(CompletionRequest {
        model: None,
        preamble: req.system,
        chat_history,
        documents: Vec::new(),
        tools: req
            .tools
            .into_iter()
            .map(|tool| ToolDefinition {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
            })
            .collect(),
        // A reasoning model that rejects `temperature` outright (o-series,
        // gpt-5's reasoning variants, codex) gets the field dropped here
        // rather than from the caller — nothing that builds an [`LlmRequest`]
        // should need to know which models refuse it.
        temperature: req.temperature.filter(|_| caps.supports_temperature),
        max_tokens: req.max_tokens.map(u64::from),
        tool_choice: None,
        additional_params,
        output_schema: None,
        // Local observability policy, never part of the provider payload. Off:
        // switching it on would put prompts, tool arguments and model output on
        // OpenTelemetry spans, and a chat with an agent is exactly the content
        // §11 keeps inside the caller's own authority.
        record_telemetry_content: false,
    })
}

/// The vendor-specific request fields, as rig's `additional_params`.
///
/// `parallel_tool_calls` is only sent with tools: several hosts refuse the
/// field on a request that offers none.
fn additional_params(req: &LlmRequest, wire: Wire, caps: &ModelCapabilities) -> Option<Json> {
    let mut params = serde_json::Map::new();
    let parallel = if req.tools.is_empty() || !caps.parallel_tool_calls {
        None
    } else {
        req.parallel_tool_calls
    };
    match wire {
        Wire::Responses => {
            if let Some(parallel) = parallel {
                params.insert("parallel_tool_calls".to_owned(), json!(parallel));
            }
            if let Some(key) = &req.prompt_cache_key
                && caps.prompt_caching != crate::PromptCaching::None
            {
                params.insert("prompt_cache_key".to_owned(), json!(key));
            }
            if caps.reasoning_replay {
                // Stateless: nothing is kept on OpenAI's side, and the
                // reasoning comes back encrypted for the next request instead.
                params.insert("store".to_owned(), json!(false));
                params.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));
            }
        }
        Wire::Anthropic => {
            if parallel == Some(false) {
                params.insert(
                    "tool_choice".to_owned(),
                    json!({"type": "auto", "disable_parallel_tool_use": true}),
                );
            }
        }
        Wire::Chat => {
            if let Some(parallel) = parallel {
                params.insert("parallel_tool_calls".to_owned(), json!(parallel));
            }
        }
    }
    // The effort, in each wire's own words (`request_options`).
    if let Some(effort) = req.reasoning_effort {
        match wire {
            Wire::Chat => {
                params.insert("reasoning_effort".to_owned(), json!(effort.as_str()));
            }
            Wire::Responses => {
                params.insert("reasoning".to_owned(), json!({ "effort": effort.as_str() }));
            }
            Wire::Anthropic => {
                params.insert(
                    "output_config".to_owned(),
                    json!({ "effort": effort.anthropic() }),
                );
            }
        }
    }
    // Last, so the admin's escape hatch says what the request says.
    for (key, value) in &req.extra_params {
        params.insert(key.clone(), value.clone());
    }
    (!params.is_empty()).then_some(Json::Object(params))
}

/// Our messages as rig's, folding each run of consecutive tool results into one
/// user message (see the module docs for why).
pub(crate) fn to_rig_messages(
    messages: Vec<LlmMessage>,
    wire: Wire,
    model: &str,
    caps: &ModelCapabilities,
) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    // The tool results seen since the last non-tool-result message.
    let mut pending: Vec<UserContent> = Vec::new();
    // Chat Completions only: images that follow those results in a user
    // message of their own.
    let mut pending_images: Vec<UserContent> = Vec::new();

    for message in messages {
        match message {
            LlmMessage::ToolResult {
                tool_call_id,
                content,
                images,
                ..
            } => {
                let mut parts: Vec<ToolResultContent> = Vec::new();
                let mut text = content;
                if !images.is_empty() && !caps.vision {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&image_stub(images.len(), model));
                }
                if !text.is_empty() || images.is_empty() || !caps.vision {
                    parts.push(ToolResultContent::Text(Text::from(text)));
                }
                if caps.vision {
                    for image in &images {
                        match wire {
                            Wire::Responses | Wire::Anthropic => {
                                parts.push(ToolResultContent::image_base64(
                                    image.base64(),
                                    media_type(image),
                                    None,
                                ))
                            }
                            Wire::Chat => {
                                pending_images.push(UserContent::Text(Text::from(format!(
                                    "Image from tool call {tool_call_id}:"
                                ))));
                                pending_images.push(UserContent::image_base64(
                                    image.base64(),
                                    media_type(image),
                                    None,
                                ));
                            }
                        }
                    }
                    if parts.is_empty() {
                        parts.push(ToolResultContent::Text(Text::from(
                            "The image follows in the next message.",
                        )));
                    }
                }
                pending.push(UserContent::ToolResult(ToolResult {
                    // Both fields, from the one id we keep: Anthropic reads
                    // `id` and ignores `call_id`, the Responses API reads
                    // `call_id` and refuses a result without one.
                    call_id: Some(tool_call_id.clone()),
                    id: tool_call_id,
                    content: OneOrMany::many(parts).unwrap_or_else(|_| {
                        OneOrMany::one(ToolResultContent::Text(Text::from("")))
                    }),
                }));
                continue;
            }
            other => {
                pending.append(&mut pending_images);
                flush_tool_results(&mut pending, &mut out);
                out.push(match other {
                    LlmMessage::User {
                        content,
                        attachments,
                    } => Message::User {
                        content: user_content(content, &attachments, model, caps),
                    },
                    LlmMessage::Assistant {
                        content,
                        tool_calls,
                        provider_items,
                    } => {
                        let replayed = if caps.reasoning_replay {
                            replayable(provider_items, wire)
                        } else {
                            Vec::new()
                        };
                        Message::Assistant {
                            id: None,
                            content: assistant_content(replayed, content, tool_calls),
                        }
                    }
                    // Handled above; the compiler cannot see that.
                    LlmMessage::ToolResult { .. } => continue,
                });
            }
        }
    }
    pending.append(&mut pending_images);
    flush_tool_results(&mut pending, &mut out);
    out
}

/// A user message's parts: its attachments first — a document reads better
/// before the question about it — then what the person typed. An image goes to
/// a model with `vision` as an image, and to one without as a stub that names
/// it; a text document goes to every model as text.
fn user_content(
    content: String,
    attachments: &[Attachment],
    model: &str,
    caps: &ModelCapabilities,
) -> OneOrMany<UserContent> {
    let mut parts: Vec<UserContent> = Vec::new();
    for attachment in attachments {
        if !attachment.is_image() {
            parts.push(UserContent::Text(Text::from(attachment.as_prompt_text())));
        } else if caps.vision {
            parts.push(UserContent::Text(Text::from(format!(
                "Attached image `{}`:",
                attachment.name
            ))));
            let image = attachment.image();
            parts.push(UserContent::image_base64(
                image.base64(),
                media_type(&image),
                None,
            ));
        } else {
            parts.push(UserContent::Text(Text::from(format!(
                "[the attached image `{}` is omitted: the model `{model}` does not accept images]",
                attachment.name
            ))));
        }
    }
    if !content.is_empty() || parts.is_empty() {
        parts.push(UserContent::Text(Text::from(content)));
    }
    OneOrMany::many(parts).unwrap_or_else(|_| OneOrMany::one(UserContent::Text(Text::from(""))))
}

/// What replaces an image a model cannot see.
fn image_stub(count: usize, model: &str) -> String {
    let what = if count == 1 {
        "an image".to_owned()
    } else {
        format!("{count} images")
    };
    format!("[{what} omitted: the model `{model}` does not accept images]")
}

/// rig's media type for an image part, where it has one.
fn media_type(image: &ImagePart) -> Option<ImageMediaType> {
    ImageMediaType::from_mime_type(image.media_type.trim())
}

/// The provider items that `wire` accepts back, as rig reasoning content.
/// Items another vendor produced — a history that changed models — are not
/// sent: no vendor accepts another's signatures.
fn replayable(items: Vec<ProviderItem>, wire: Wire) -> Vec<AssistantContent> {
    items
        .into_iter()
        .filter_map(|item| match (wire, item) {
            (
                Wire::Responses,
                ProviderItem::EncryptedReasoning {
                    id,
                    encrypted_content,
                },
            ) => Some(Reasoning::encrypted(encrypted_content).with_id(id)),
            (
                Wire::Anthropic,
                ProviderItem::SignedThinking {
                    thinking,
                    signature,
                },
            ) => Some(Reasoning::new_with_signature(&thinking, Some(signature))),
            (Wire::Anthropic, ProviderItem::RedactedThinking { data }) => {
                Some(Reasoning::redacted(data))
            }
            _ => None,
        })
        .map(AssistantContent::Reasoning)
        .collect()
}

/// Emit the accumulated tool results as one user message, if there are any.
fn flush_tool_results(pending: &mut Vec<UserContent>, out: &mut Vec<Message>) {
    if let Ok(content) = OneOrMany::many(std::mem::take(pending)) {
        out.push(Message::User { content });
    }
}

/// An assistant turn's content: the replayed provider items first (both
/// vendors require reasoning before what it led to), then its text (dropped
/// when empty — a turn that was only tool calls must not carry an empty text
/// block, which Anthropic rejects), then its tool calls in order.
fn assistant_content(
    replayed: Vec<AssistantContent>,
    content: String,
    tool_calls: Vec<ToolCall>,
) -> OneOrMany<AssistantContent> {
    let mut items: Vec<AssistantContent> = replayed;
    if !content.is_empty() {
        items.push(AssistantContent::Text(Text::from(content)));
    }
    for call in tool_calls {
        items.push(AssistantContent::ToolCall(RigToolCall {
            // As for a tool result: one id of ours fills both of rig's. Where
            // it is not a native `fc_…` item id — an Anthropic call, or one
            // assembled from fragments — rig drops it from the Responses
            // payload and pairs the call with its output by `call_id` alone.
            call_id: Some(call.id.clone()),
            id: call.id,
            function: ToolFunction {
                name: call.name,
                // A call whose arguments never parsed goes back as an empty
                // object: every vendor requires an object here, and the tool
                // result beside it already says what was wrong.
                arguments: if call.arguments.is_object() {
                    call.arguments
                } else {
                    Json::Object(serde_json::Map::new())
                },
            },
            signature: None,
            additional_params: None,
        }));
    }
    OneOrMany::many(items)
        .unwrap_or_else(|_| OneOrMany::one(AssistantContent::Text(Text::from(""))))
}

/// The provider items in one streamed reasoning block that are worth sending
/// back: encrypted, signed or redacted content. Readable text with no signature
/// is not.
fn provider_items(reasoning: &Reasoning) -> Vec<ProviderItem> {
    reasoning
        .content
        .iter()
        .filter_map(|content| match content {
            ReasoningContent::Encrypted(data) => {
                reasoning
                    .id
                    .as_ref()
                    .map(|id| ProviderItem::EncryptedReasoning {
                        id: id.clone(),
                        encrypted_content: data.clone(),
                    })
            }
            ReasoningContent::Text {
                text,
                signature: Some(signature),
            } => Some(ProviderItem::SignedThinking {
                thinking: text.clone(),
                signature: signature.clone(),
            }),
            ReasoningContent::Redacted { data } => {
                Some(ProviderItem::RedactedThinking { data: data.clone() })
            }
            _ => None,
        })
        .collect()
}

/// Turn rig's stream into ours: [`LlmDelta`]s ending in exactly one
/// [`Stop`](LlmDelta::Stop).
///
/// The `Stop` is synthesised at the end of the stream rather than forwarded from
/// a provider event, because neither provider's streaming response carries a
/// finish reason through rig 0.41. Usage comes from the final response object
/// where the provider sent one, and is zero where it did not — which is what
/// [`Usage`] documents zero to mean.
///
/// `wire` decides what the reported input tokens mean (see [`Usage`]), and
/// `replay` whether opaque reasoning items are kept for the next request.
pub(crate) fn map_stream<R>(
    mut response: StreamingCompletionResponse<R>,
    wire: Wire,
    replay: bool,
) -> DeltaStream
where
    R: Clone + Unpin + GetTokenUsage + Send + 'static,
{
    // Tool-call fragments, keyed by rig's internal call id: a name arrives in
    // one event and the arguments across several. This is the fallback path —
    // both providers do emit a complete `ToolCall` at the end of a function-call
    // item — and it is what keeps a provider that only ever streams deltas from
    // silently dropping every tool the model asked for.
    let mut partial: BTreeMap<String, PartialToolCall> = BTreeMap::new();
    let mut emitted: Vec<String> = Vec::new();
    let mut usage = Usage::default();
    let mut saw_tool_call = false;
    // One rig event can produce several of ours (the tail produces a tool call
    // per unfinished fragment plus the `Stop`), and `poll_next` returns one at a
    // time, so what is ready but not yet handed out waits here.
    let mut queued: std::collections::VecDeque<Result<LlmDelta>> =
        std::collections::VecDeque::new();
    let mut ended = false;

    Box::pin(futures::stream::poll_fn(move |cx| {
        loop {
            if let Some(item) = queued.pop_front() {
                return std::task::Poll::Ready(Some(item));
            }
            if ended {
                return std::task::Poll::Ready(None);
            }
            match response.poll_next_unpin(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(Some(item)) => match item {
                    Err(e) => queued.push_back(Err(provider_error(&e))),
                    Ok(StreamedAssistantContent::Text(text)) => {
                        if !text.text.is_empty() {
                            queued.push_back(Ok(LlmDelta::Text(text.text)));
                        }
                    }
                    Ok(StreamedAssistantContent::ReasoningDelta { reasoning, .. }) => {
                        if !reasoning.is_empty() {
                            queued.push_back(Ok(LlmDelta::Reasoning(reasoning)));
                        }
                    }
                    Ok(StreamedAssistantContent::Reasoning(reasoning)) => {
                        let text = reasoning_text(&reasoning);
                        if !text.is_empty() {
                            queued.push_back(Ok(LlmDelta::Reasoning(text)));
                        }
                        if replay {
                            for item in provider_items(&reasoning) {
                                queued.push_back(Ok(LlmDelta::ProviderItem(item)));
                            }
                        }
                    }
                    Ok(StreamedAssistantContent::ToolCallDelta {
                        id,
                        internal_call_id,
                        content,
                    }) => {
                        let entry = partial.entry(internal_call_id).or_default();
                        if !id.is_empty() {
                            entry.id = id;
                        }
                        match content {
                            ToolCallDeltaContent::Name(name) => entry.name.push_str(&name),
                            ToolCallDeltaContent::Delta(delta) => entry.arguments.push_str(&delta),
                        }
                    }
                    Ok(StreamedAssistantContent::ToolCall {
                        tool_call,
                        internal_call_id,
                    }) => {
                        // rig has assembled it; the fragments for this call are
                        // now redundant, whatever state they are in.
                        partial.remove(&internal_call_id);
                        // Recorded as rig's *item* id, which is what a fragment
                        // is keyed by — the id our `ToolCall` keeps may be the
                        // provider's separate correlation id.
                        let item_id = tool_call.id.clone();
                        match complete_tool_call(&tool_call) {
                            Ok(call) => {
                                saw_tool_call = true;
                                emitted.push(item_id);
                                queued.push_back(Ok(LlmDelta::ToolCall(call)));
                            }
                            Err(e) => queued.push_back(Err(e)),
                        }
                    }
                    Ok(StreamedAssistantContent::Final(final_response)) => {
                        let reported = final_response.token_usage();
                        // Anthropic counts cache reads and writes apart from
                        // its input tokens; ours are always the whole prompt.
                        let input_tokens = match wire {
                            Wire::Anthropic => {
                                reported.input_tokens
                                    + reported.cached_input_tokens
                                    + reported.cache_creation_input_tokens
                            }
                            Wire::Responses | Wire::Chat => reported.input_tokens,
                        };
                        usage = Usage {
                            input_tokens,
                            output_tokens: reported.output_tokens,
                            cached_input_tokens: reported.cached_input_tokens,
                            cache_write_input_tokens: reported.cache_creation_input_tokens,
                        };
                    }
                    // A provider-native item rig does not model — a hosted web
                    // search, a code-interpreter call. Nothing here can act on
                    // one, and passing it through as text would put provider
                    // internals in the answer.
                    Ok(StreamedAssistantContent::Unknown(_)) => {}
                },
                std::task::Poll::Ready(None) => {
                    ended = true;
                    // The tail: anything rig never completed, then exactly one
                    // `Stop`.
                    for (_, fragment) in std::mem::take(&mut partial) {
                        if emitted.contains(&fragment.id) {
                            continue;
                        }
                        match fragment.finish() {
                            Ok(call) => {
                                saw_tool_call = true;
                                queued.push_back(Ok(LlmDelta::ToolCall(call)));
                            }
                            Err(e) => queued.push_back(Err(e)),
                        }
                    }
                    queued.push_back(Ok(LlmDelta::Stop {
                        reason: if saw_tool_call {
                            StopReason::ToolCalls
                        } else {
                            StopReason::EndTurn
                        },
                        usage,
                    }));
                }
            }
        }
    }))
}

/// A tool call rig streamed in fragments: the provider's id, the name, and the
/// partial JSON arguments as text.
#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl PartialToolCall {
    /// The assembled call, or an error for a call with no name.
    ///
    /// An unparseable fragment is kept as a JSON **string**, not replaced by
    /// empty arguments: running a tool with arguments the model did not ask for
    /// is a worse outcome than telling it the call was malformed, which the
    /// loop does.
    fn finish(self) -> Result<ToolCall> {
        if self.name.is_empty() {
            return Err(Error::msg(
                "the provider streamed tool-call arguments without a tool name",
            ));
        }
        let arguments = if self.arguments.trim().is_empty() {
            Json::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&self.arguments).unwrap_or(Json::String(self.arguments))
        };
        Ok(ToolCall {
            id: self.id,
            name: self.name,
            arguments,
        })
    }
}

/// One of rig's assembled tool calls as ours, repairing the shape providers
/// disagree about: arguments that arrive as a JSON *string* rather than an
/// object are parsed, and a null becomes the empty object a no-argument tool
/// declares.
///
/// The id kept is the provider's **correlation** id where there is one — the
/// Responses API's `call_id`, not the `fc_…` item id beside it — because that
/// is the one it will require back on both the call and its result. Anthropic
/// sets only `id`, and there the two are the same thing.
fn complete_tool_call(call: &RigToolCall) -> Result<ToolCall> {
    let arguments = match &call.function.arguments {
        Json::Null => Json::Object(serde_json::Map::new()),
        Json::String(text) if text.trim().is_empty() => Json::Object(serde_json::Map::new()),
        // Kept as the string it was when it does not parse — see `finish`.
        Json::String(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| call.function.arguments.clone())
        }
        other => other.clone(),
    };
    Ok(ToolCall {
        id: call.call_id.clone().unwrap_or_else(|| call.id.clone()),
        name: call.function.name.clone(),
        arguments,
    })
}

/// The readable half of a structured reasoning block. An encrypted or redacted
/// block yields nothing: there is no text in it to show.
fn reasoning_text(reasoning: &Reasoning) -> String {
    reasoning
        .content
        .iter()
        .filter_map(|content| match content {
            ReasoningContent::Text { text, .. } => Some(text.clone()),
            ReasoningContent::Summary(text) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// A rig error as ours, keeping the provider's own words.
///
/// The provider's message is the actionable part — "invalid x-api-key",
/// "model not found", "context length exceeded" — so it is preserved verbatim
/// rather than replaced by a category. That is also what the admin UI's *Test
/// connection* shows, which is the point of having it (§11.1).
pub(crate) fn provider_error(e: &rig_core::completion::CompletionError) -> Error {
    Error::msg(format!("the LLM provider failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::ModelCapabilities;
    use crate::def::{ANTHROPIC_BACKEND, OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND};
    use crate::message::ToolSpec;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: "view_app".to_owned(),
            arguments: json!({}),
        }
    }

    fn with_image(id: &str) -> LlmMessage {
        LlmMessage::ToolResult {
            tool_call_id: id.to_owned(),
            name: "view_app".to_owned(),
            content: "the page".to_owned(),
            images: vec![ImagePart::new("image/png", b"png".to_vec())],
        }
    }

    fn caps(backend: &str, model: &str) -> ModelCapabilities {
        ModelCapabilities::built_in(backend, model)
    }

    #[test]
    fn temperature_is_dropped_for_a_model_that_rejects_it() {
        let request = LlmRequest::prompt("hi").temperature(0.0);

        // gpt-5.1 is an OpenAI reasoning model: the host rejects the field
        // outright rather than ignoring it, so it must not be sent.
        let rig_request = to_rig_request(
            request.clone(),
            Wire::Responses,
            "gpt-5.1",
            &caps(OPENAI_RESPONSES_BACKEND, "gpt-5.1"),
        )
        .unwrap();
        assert_eq!(rig_request.temperature, None);

        // A model that does accept it keeps the caller's value.
        let rig_request = to_rig_request(
            request,
            Wire::Chat,
            "claude-sonnet-5",
            &caps(ANTHROPIC_BACKEND, "claude-sonnet-5"),
        )
        .unwrap();
        assert_eq!(rig_request.temperature, Some(0.0));
    }

    #[test]
    fn arguments_that_never_parsed_reach_the_loop_as_a_string_and_go_back_as_an_object() {
        let assembled = PartialToolCall {
            id: "c1".to_owned(),
            name: "edit".to_owned(),
            arguments: "{\"path\": \"a.ts\"".to_owned(),
        }
        .finish()
        .unwrap();
        assert_eq!(assembled.arguments, json!("{\"path\": \"a.ts\""));
        let parsed = PartialToolCall {
            id: "c2".to_owned(),
            name: "edit".to_owned(),
            arguments: "{\"path\": \"a.ts\"}".to_owned(),
        }
        .finish()
        .unwrap();
        assert_eq!(parsed.arguments, json!({"path": "a.ts"}));

        // Replayed in the history, the broken call is an empty object: every
        // vendor requires one, and its result already says what was wrong.
        let content = assistant_content(Vec::new(), String::new(), vec![assembled]);
        let AssistantContent::ToolCall(replayed) = content.first() else {
            panic!("a tool call");
        };
        assert_eq!(replayed.function.arguments, json!({}));
    }

    #[test]
    fn provider_items_round_trip_through_to_rig_messages_for_their_own_vendor_only() {
        let history = vec![
            LlmMessage::user("go"),
            LlmMessage::Assistant {
                content: String::new(),
                tool_calls: vec![call("c1")],
                provider_items: vec![
                    ProviderItem::EncryptedReasoning {
                        id: "rs_1".to_owned(),
                        encrypted_content: "enc".to_owned(),
                    },
                    ProviderItem::SignedThinking {
                        thinking: "hmm".to_owned(),
                        signature: "sig".to_owned(),
                    },
                ],
            },
        ];

        let reasoning_of = |messages: Vec<Message>| -> Vec<Reasoning> {
            messages
                .into_iter()
                .flat_map(|m| match m {
                    Message::Assistant { content, .. } => content
                        .into_iter()
                        .filter_map(|c| match c {
                            AssistantContent::Reasoning(r) => Some(r),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect()
        };

        // Responses: the encrypted item, with its id, and nothing of Anthropic's.
        let responses = caps(OPENAI_RESPONSES_BACKEND, "gpt-5.1");
        let sent = reasoning_of(to_rig_messages(
            history.clone(),
            Wire::Responses,
            "gpt-5.1",
            &responses,
        ));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].id.as_deref(), Some("rs_1"));
        assert_eq!(provider_items(&sent[0]), vec![history_item(&history, 0)]);

        // Anthropic: the signed thinking block.
        let anthropic = caps(ANTHROPIC_BACKEND, "claude-sonnet-5");
        let sent = reasoning_of(to_rig_messages(
            history.clone(),
            Wire::Anthropic,
            "claude-sonnet-5",
            &anthropic,
        ));
        assert_eq!(sent.len(), 1);
        assert_eq!(provider_items(&sent[0]), vec![history_item(&history, 1)]);

        // A model without replay sends none.
        let no_replay = caps(OPENAI_RESPONSES_BACKEND, "gpt-4.1");
        assert!(!no_replay.reasoning_replay);
        assert!(
            reasoning_of(to_rig_messages(
                history,
                Wire::Responses,
                "gpt-4.1",
                &no_replay
            ))
            .is_empty()
        );
    }

    fn history_item(history: &[LlmMessage], index: usize) -> ProviderItem {
        match &history[1] {
            LlmMessage::Assistant { provider_items, .. } => provider_items[index].clone(),
            _ => panic!("the assistant turn"),
        }
    }

    fn tool_result_parts(messages: &[Message]) -> Vec<ToolResultContent> {
        messages
            .iter()
            .flat_map(|m| match m {
                Message::User { content } => content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::ToolResult(r) => {
                            Some(r.content.iter().cloned().collect::<Vec<_>>())
                        }
                        _ => None,
                    })
                    .flatten()
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn an_image_goes_inside_the_tool_result_for_anthropic_and_responses() {
        for (wire, backend, model) in [
            (Wire::Anthropic, ANTHROPIC_BACKEND, "claude-sonnet-5"),
            (Wire::Responses, OPENAI_RESPONSES_BACKEND, "gpt-5.1"),
        ] {
            let caps = caps(backend, model);
            let messages = to_rig_messages(
                vec![
                    LlmMessage::assistant_with_calls("", vec![call("c1")]),
                    with_image("c1"),
                ],
                wire,
                model,
                &caps,
            );
            let parts = tool_result_parts(&messages);
            assert_eq!(parts.len(), 2, "{wire:?}: text and image");
            assert!(matches!(parts[1], ToolResultContent::Image(_)), "{wire:?}");
            assert_eq!(messages.len(), 2, "{wire:?}: no extra user message");
        }
    }

    #[test]
    fn an_image_follows_the_tool_results_in_a_user_message_for_chat_completions() {
        let caps = caps(OPENAI_CHAT_BACKEND, "gpt-5.1");
        let messages = to_rig_messages(
            vec![
                LlmMessage::assistant_with_calls("", vec![call("c1"), call("c2")]),
                with_image("c1"),
                LlmMessage::tool_result(&call("c2"), "plain"),
                LlmMessage::user("next"),
            ],
            Wire::Chat,
            "gpt-5.1",
            &caps,
        );
        // Assistant, then one user message holding both results and the
        // labelled image after them, then the next user message.
        assert_eq!(messages.len(), 3);
        let Message::User { content } = &messages[1] else {
            panic!("the tool results");
        };
        let kinds: Vec<&str> = content
            .iter()
            .map(|c| match c {
                UserContent::ToolResult(_) => "result",
                UserContent::Text(t) if t.text.contains("c1") => "label",
                UserContent::Image(_) => "image",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["result", "result", "label", "image"]);
        assert!(
            tool_result_parts(&messages)
                .iter()
                .all(|p| !matches!(p, ToolResultContent::Image(_)))
        );
    }

    #[test]
    fn a_model_without_vision_gets_a_stub_instead_of_the_image() {
        for (wire, backend) in [
            (Wire::Anthropic, ANTHROPIC_BACKEND),
            (Wire::Responses, OPENAI_RESPONSES_BACKEND),
            (Wire::Chat, OPENAI_CHAT_BACKEND),
        ] {
            let caps = caps(backend, "text-only-model");
            assert!(!caps.vision);
            let messages = to_rig_messages(
                vec![
                    LlmMessage::assistant_with_calls("", vec![call("c1")]),
                    with_image("c1"),
                ],
                wire,
                "text-only-model",
                &caps,
            );
            let parts = tool_result_parts(&messages);
            assert_eq!(parts.len(), 1, "{wire:?}");
            let text = parts[0].as_text().unwrap_or_default();
            assert!(text.starts_with("the page"), "{text}");
            assert!(
                text.contains("omitted") && text.contains("text-only-model"),
                "{text}"
            );
            assert_eq!(messages.len(), 2, "{wire:?}: no image message");
        }
    }

    #[test]
    fn a_user_messages_attachments_go_before_its_text() {
        let attachments = vec![
            Attachment::new("notes.md", "text/markdown", b"# Plan\nShip it.".to_vec()),
            Attachment::new("shot.png", "image/png", b"png".to_vec()),
        ];
        let message = || LlmMessage::user_with("What does the plan say?", attachments.clone());
        let kinds = |messages: &[Message]| -> Vec<String> {
            let Message::User { content } = &messages[0] else {
                panic!("a user message");
            };
            content
                .iter()
                .map(|c| match c {
                    UserContent::Text(t) => format!("text:{}", t.text),
                    UserContent::Image(_) => "image".to_owned(),
                    _ => "other".to_owned(),
                })
                .collect()
        };
        for (wire, backend, model) in [
            (Wire::Anthropic, ANTHROPIC_BACKEND, "claude-sonnet-4-5"),
            (Wire::Responses, OPENAI_RESPONSES_BACKEND, "gpt-5.1"),
            (Wire::Chat, OPENAI_CHAT_BACKEND, "gpt-5.1"),
        ] {
            let caps = caps(backend, model);
            assert!(caps.vision, "{model}");
            let got = kinds(&to_rig_messages(vec![message()], wire, model, &caps));
            assert_eq!(
                got,
                [
                    "text:<attachment name=\"notes.md\" type=\"text/markdown\">\n# Plan\nShip it.\n</attachment>",
                    "text:Attached image `shot.png`:",
                    "image",
                    "text:What does the plan say?",
                ],
                "{wire:?}"
            );
        }
        // Without vision, the image is named and the document still read.
        let blind = caps(OPENAI_CHAT_BACKEND, "text-only-model");
        let got = kinds(&to_rig_messages(
            vec![message()],
            Wire::Chat,
            "text-only-model",
            &blind,
        ));
        assert_eq!(got.len(), 3);
        assert!(got[0].contains("Ship it."));
        assert!(
            got[1].contains("shot.png") && got[1].contains("omitted"),
            "{}",
            got[1]
        );
    }

    #[test]
    fn request_options_map_per_wire() {
        let tools = vec![ToolSpec::new("t", "a tool", json!({"type": "object"}))];
        let mut req = LlmRequest::prompt("hi").tools(tools);
        req.parallel_tool_calls = Some(false);
        req.prompt_cache_key = Some("agent-1".to_owned());

        let responses = additional_params(
            &req,
            Wire::Responses,
            &caps(OPENAI_RESPONSES_BACKEND, "gpt-5.1"),
        )
        .unwrap();
        assert_eq!(responses["parallel_tool_calls"], json!(false));
        assert_eq!(responses["prompt_cache_key"], json!("agent-1"));
        assert_eq!(responses["store"], json!(false));
        assert_eq!(responses["include"], json!(["reasoning.encrypted_content"]));

        let anthropic = additional_params(
            &req,
            Wire::Anthropic,
            &caps(ANTHROPIC_BACKEND, "claude-sonnet-5"),
        )
        .unwrap();
        assert_eq!(
            anthropic["tool_choice"]["disable_parallel_tool_use"],
            json!(true)
        );

        let chat =
            additional_params(&req, Wire::Chat, &caps(OPENAI_CHAT_BACKEND, "llama3.2")).unwrap();
        assert_eq!(chat["parallel_tool_calls"], json!(false));
        assert!(chat.get("prompt_cache_key").is_none());

        // The effort and the passthrough, per wire; the passthrough wins.
        let mut effort = LlmRequest::prompt("hi");
        effort.reasoning_effort = Some(crate::ReasoningEffort::None);
        let chat =
            additional_params(&effort, Wire::Chat, &caps(OPENAI_CHAT_BACKEND, "glm-5")).unwrap();
        assert_eq!(chat["reasoning_effort"], json!("none"));
        let responses = additional_params(
            &effort,
            Wire::Responses,
            &caps(OPENAI_RESPONSES_BACKEND, "gpt-5.1"),
        )
        .unwrap();
        assert_eq!(responses["reasoning"], json!({ "effort": "none" }));
        let anthropic = additional_params(
            &effort,
            Wire::Anthropic,
            &caps(ANTHROPIC_BACKEND, "claude-sonnet-5"),
        )
        .unwrap();
        assert_eq!(anthropic["output_config"], json!({ "effort": "low" }));
        effort
            .extra_params
            .insert("reasoning_effort".into(), json!("high"));
        effort.extra_params.insert("top_k".into(), json!(20));
        let chat =
            additional_params(&effort, Wire::Chat, &caps(OPENAI_CHAT_BACKEND, "glm-5")).unwrap();
        assert_eq!(chat["reasoning_effort"], json!("high"));
        assert_eq!(chat["top_k"], json!(20));

        // No tools, no parallel_tool_calls; nothing to say, no params at all.
        let bare = LlmRequest::prompt("hi");
        assert_eq!(
            additional_params(&bare, Wire::Chat, &caps(OPENAI_CHAT_BACKEND, "llama3.2")),
            None
        );
    }
}

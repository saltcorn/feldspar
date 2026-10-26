//! The vocabulary of a model call: what goes in ([`LlmRequest`], [`LlmMessage`],
//! [`ToolSpec`]) and what comes back ([`LlmDelta`], [`AssistantMessage`],
//! [`Usage`]) — design §11.1.
//!
//! **These types are ours, not a provider crate's.** Everything `rig-core`
//! exposes stays behind [`LlmProvider`](crate::LlmProvider), for the reason §2
//! gives generally: a provider abstraction is precisely the kind of dependency
//! whose API churns, and `sc-agent` — which is written entirely against this
//! module — must not churn with it. The adapters in
//! [`openai`](crate::openai) and [`anthropic`](crate::anthropic) are the only
//! code in the tree that names a rig type.
//!
//! They are also **serialisable**, which is not incidental: `sc-agent` persists
//! a run's message history into `_fd_runs` after every step (§11.2), so the
//! history has to survive a round trip through JSON without losing a tool call's
//! id or arguments.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// One tool the model may call: what it is named, what it does, and what
/// arguments it takes.
///
/// `parameters` is a **JSON Schema object**, which is what every provider's tool
/// definition wants and what a trait's `tools()` produces. It is a
/// [`Json`] rather than a typed schema because it is passed through: the model
/// reads it, the provider forwards it, and nothing between here and the vendor
/// has an opinion about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The name the model calls, and the key [`ToolCall::name`] arrives under.
    pub name: String,
    /// What the tool does, in the words the model is given. This is the whole of
    /// what it has to go on when choosing, so it is prose, not a label.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: Json,
}

impl ToolSpec {
    /// A tool with the given name, description and parameter schema.
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Json) -> Self {
        ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// One call the model made: which tool, with which arguments, under which id.
///
/// The `id` is the provider's, and it is what a [`LlmMessage::ToolResult`] is
/// correlated by — so it travels through the loop unchanged and is never
/// regenerated. Two calls to one tool in a single turn are distinguished by
/// nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The provider's identifier for this call.
    ///
    /// Where a provider distinguishes an item id from a *correlation* id — the
    /// Responses API's `fc_…` and `call_id` — this is the correlation id, since
    /// that is the one it requires back on both the call and its result. One
    /// field rather than two: an id a provider does not correlate by is an id
    /// nothing in the loop has a use for.
    pub id: String,
    /// Which tool was called — a [`ToolSpec::name`] that was offered.
    pub name: String,
    /// The arguments, as complete parsed JSON.
    ///
    /// **Complete** is the contract: providers stream tool arguments as partial
    /// JSON text, and an adapter emits a `ToolCall` only once that text parses
    /// (see [`LlmDelta::ToolCall`]). A caller never has to assemble fragments,
    /// and never sees a half-formed argument object.
    pub arguments: Json,
}

/// One message in the conversation sent to the model.
///
/// A `System` variant is deliberately absent: the system prompt is
/// [`LlmRequest::system`], one per request, because that is what both providers
/// model and because a system message appearing mid-history is a bug rather than
/// a feature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum LlmMessage {
    /// What the person said.
    User {
        /// The message text.
        content: String,
    },
    /// What the model said: text, tool calls, or both. Both providers may emit
    /// a sentence and a tool call in one turn, so this is not an either/or.
    Assistant {
        /// The assistant's text, empty when the turn was only tool calls.
        content: String,
        /// The tool calls the model made, in the order it made them.
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
        /// Opaque, vendor-signed items to send back with this turn (§4's
        /// reasoning replay). Empty for every backend and model that has none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        provider_items: Vec<ProviderItem>,
    },
    /// What a tool returned, answering one [`ToolCall`] by its id.
    ///
    /// `content` is text — including for a tool that failed, whose error is its
    /// result (§11.2): an error the model can read is one it can recover from.
    ToolResult {
        /// The [`ToolCall::id`] this answers.
        tool_call_id: String,
        /// The tool's name, carried so a transcript can be rendered without
        /// walking back to find the call.
        #[serde(default)]
        name: String,
        /// What the tool produced, or the error it produced.
        content: String,
        /// Images the tool produced beside its text, such as a screenshot
        /// (§7b). An adapter sends them where its vendor accepts them, and a
        /// model without `vision` gets a stub instead.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImagePart>,
    },
}

/// One image in a tool result: its media type and its bytes.
///
/// The bytes are serialised as base64, so a run's history stays JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePart {
    /// The media type, such as `image/png` or `image/jpeg`.
    pub media_type: String,
    /// The encoded image.
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
}

impl ImagePart {
    /// An image of `media_type` holding `data`.
    pub fn new(media_type: impl Into<String>, data: impl Into<Vec<u8>>) -> ImagePart {
        ImagePart {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    /// The bytes as base64, which is how every vendor takes them.
    pub fn base64(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&self.data)
    }
}

/// Serde for bytes as a base64 string.
mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// An opaque item a vendor asks to be sent back with the assistant turn that
/// produced it (§4's reasoning replay).
///
/// **Only vendor-signed or encrypted items.** The readable reasoning text in
/// [`AssistantMessage::reasoning`] never travels back. An Anthropic thinking
/// block is the one case that carries text, because its signature covers that
/// text and Anthropic refuses the signature without it. It is still sent back
/// only as the signed block, never as reasoning the harness wrote.
///
/// The loop does not look inside these. It stores them with the run and hands
/// them back to the adapter, which sends them when the model's capabilities
/// allow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderItem {
    /// A Responses API reasoning item with its encrypted content
    /// (`store: false`).
    EncryptedReasoning {
        /// The reasoning item's id (`rs_…`).
        id: String,
        /// The encrypted payload.
        encrypted_content: String,
    },
    /// An Anthropic thinking block and its signature.
    SignedThinking {
        /// The thinking text the signature covers.
        thinking: String,
        /// The signature.
        signature: String,
    },
    /// An Anthropic redacted thinking block.
    RedactedThinking {
        /// The opaque payload.
        data: String,
    },
}

impl LlmMessage {
    /// A user message.
    pub fn user(content: impl Into<String>) -> LlmMessage {
        LlmMessage::User {
            content: content.into(),
        }
    }

    /// An assistant message that is only text.
    pub fn assistant(content: impl Into<String>) -> LlmMessage {
        LlmMessage::Assistant {
            content: content.into(),
            tool_calls: Vec::new(),
            provider_items: Vec::new(),
        }
    }

    /// An assistant message with text and tool calls, and no provider items.
    pub fn assistant_with_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> LlmMessage {
        LlmMessage::Assistant {
            content: content.into(),
            tool_calls,
            provider_items: Vec::new(),
        }
    }

    /// A tool result answering `call`.
    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> LlmMessage {
        LlmMessage::ToolResult {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            content: content.into(),
            images: Vec::new(),
        }
    }
}

/// Where a request asks the provider to cache its prompt (§4, §9).
///
/// The layout a request is built in runs from most to least stable: the system
/// prompt and tools, then the session header, then the history. A breakpoint
/// after each stable part lets the next request reuse it. Each adapter maps
/// this onto its vendor or ignores it: Anthropic marks `cache_control`, and
/// hosts that cache automatically need nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePlan {
    /// A breakpoint after the stable prefix: the system prompt and the tools.
    #[serde(default)]
    pub prefix: bool,
    /// A breakpoint after the session header, which ends with the message at
    /// this index.
    #[serde(default)]
    pub session_header: Option<usize>,
    /// A breakpoint at the tail of the history.
    #[serde(default)]
    pub tail: bool,
}

impl CachePlan {
    /// No breakpoints.
    pub fn none() -> CachePlan {
        CachePlan::default()
    }

    /// Breakpoints after the prefix, after the session header (if there is
    /// one), and at the tail.
    pub fn standard(session_header: Option<usize>) -> CachePlan {
        CachePlan {
            prefix: true,
            session_header,
            tail: true,
        }
    }

    /// Whether the plan asks for any breakpoint.
    pub fn is_empty(&self) -> bool {
        !self.prefix && self.session_header.is_none() && !self.tail
    }
}

/// One request to a model (§11.1).
///
/// Kept small. A field here is a field every adapter must answer for and every
/// caller must consider, so a vendor-specific option is added only when the
/// loop needs it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    /// The system prompt, sent once per request rather than as a message.
    pub system: Option<String>,
    /// The conversation so far, oldest first.
    pub messages: Vec<LlmMessage>,
    /// The tools the model may call. Empty means it may not call any.
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Cap on the tokens generated, if the caller sets one.
    pub max_tokens: Option<u32>,
    /// Sampling temperature, if the caller sets one.
    pub temperature: Option<f64>,
    /// Whether the model may make several tool calls in one turn. `None` sends
    /// nothing and leaves the host's default. The loop sends `false` unless the
    /// agent says otherwise (R§12), because sequential calls are easier to
    /// fingerprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Where to ask for prompt-cache breakpoints.
    #[serde(default, skip_serializing_if = "CachePlan::is_empty")]
    pub cache: CachePlan,
    /// A stable key routing requests with a shared prefix to the same cache,
    /// where the host takes one (OpenAI's `prompt_cache_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// How much the model reasons before answering. `None` sends nothing and
    /// leaves the model row's setting, then the host's default
    /// ([`request_options`](crate::request_options)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<crate::ReasoningEffort>,
    /// Vendor-specific request fields merged into the body last — a model row's
    /// `extra_params`, which win over what the adapter computed.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_params: serde_json::Map<String, serde_json::Value>,
}

impl LlmRequest {
    /// A request carrying one user message and nothing else — what a "test
    /// connection" sends and what the simplest chat turn is.
    pub fn prompt(text: impl Into<String>) -> LlmRequest {
        LlmRequest {
            messages: vec![LlmMessage::user(text)],
            ..LlmRequest::default()
        }
    }

    /// Set the system prompt, returning `self` for chaining.
    pub fn system(mut self, system: impl Into<String>) -> LlmRequest {
        self.system = Some(system.into());
        self
    }

    /// Offer the model these tools, returning `self` for chaining.
    pub fn tools(mut self, tools: impl IntoIterator<Item = ToolSpec>) -> LlmRequest {
        self.tools = tools.into_iter().collect();
        self
    }

    /// Set the token cap, returning `self` for chaining.
    pub fn max_tokens(mut self, max_tokens: u32) -> LlmRequest {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set the temperature, returning `self` for chaining.
    pub fn temperature(mut self, temperature: f64) -> LlmRequest {
        self.temperature = Some(temperature);
        self
    }
}

/// Why the model stopped.
///
/// Two variants, because two are what can be *known*. Both providers' streaming
/// responses, as `rig-core` 0.41 surfaces them, carry token usage but no finish
/// reason, so this is derived from what the stream actually produced: a turn
/// that emitted tool calls stopped to call them, and one that did not ended.
/// A `MaxTokens` variant would be a value nothing could ever produce, which is
/// worse than its absence — see §11.1's note on the deviation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn and is waiting for the user.
    EndTurn,
    /// The model stopped to call tools; the loop runs them and continues.
    ToolCalls,
}

/// The tokens a request used, as the provider reports them.
///
/// Zero throughout means "the provider did not say", which both vendors do for
/// some responses. It is not distinguished from a genuinely free call because
/// there is no such thing. What it *cost* is [`Usage::cost`], which needs the
/// model's prices.
///
/// **`input_tokens` is the whole prompt on every backend.** Anthropic reports
/// cache reads and cache writes apart from its `input_tokens`, and its adapter
/// adds them back, so the two cache counts are always parts of the input and
/// never on top of it. That is what makes the count usable for measuring a
/// context (§9) whichever vendor answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens in the prompt, cached or not.
    pub input_tokens: u64,
    /// Tokens generated.
    pub output_tokens: u64,
    /// Prompt tokens read from the provider's cache, where it reports them.
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Prompt tokens written to the provider's cache, where it reports them
    /// (Anthropic's cache creation, which has its own price).
    #[serde(default)]
    pub cache_write_input_tokens: u64,
}

impl Usage {
    /// Accumulate another call's usage into this one — what a run's total is
    /// built from across the steps of a loop.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.cache_write_input_tokens += other.cache_write_input_tokens;
    }

    /// Input plus output. Not stored, because the two halves are priced
    /// differently and a sum that hides that is a number nobody can use.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// One event from a streaming response (§11.1).
///
/// A well-formed stream is any number of `Text`/`Reasoning`/`ToolCall` events
/// followed by exactly one `Stop`. An adapter that ends without a `Stop` — a
/// connection cut mid-answer — yields an error instead, because a chat window
/// that silently stops is unfixable by the person watching it (§11.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LlmDelta {
    /// A fragment of the assistant's text, to be appended in order.
    Text(String),
    /// A fragment of the model's reasoning, where the provider exposes it.
    ///
    /// Kept separate from `Text` rather than merged into it: reasoning is not
    /// part of the answer, is not sent back in the next turn's history, and a
    /// chat that rendered it as the reply would be showing the model's notes as
    /// its conclusion.
    Reasoning(String),
    /// A complete tool call, emitted **only once its arguments parse**.
    ToolCall(ToolCall),
    /// An opaque item to send back with this turn (see [`ProviderItem`]).
    ProviderItem(ProviderItem),
    /// The end of the response.
    Stop {
        /// Why it ended.
        reason: StopReason,
        /// What it cost.
        usage: Usage,
    },
}

/// A whole response, collected from a stream (see
/// [`LlmStream::collect`](crate::LlmStream::collect)).
///
/// This is what a non-streaming caller wants and what one turn of the loop
/// appends to a run's history.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Every `Text` delta, concatenated.
    pub content: String,
    /// Every `Reasoning` delta, concatenated. Kept out of [`message`] for the
    /// reason [`LlmDelta::Reasoning`] gives.
    ///
    /// [`message`]: AssistantMessage::message
    #[serde(default)]
    pub reasoning: String,
    /// The tool calls, in the order the model asked for them — which is the
    /// order the loop must run them in (§11.2).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Opaque items to send back with this turn (see [`ProviderItem`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_items: Vec<ProviderItem>,
    /// Why the model stopped.
    pub stop_reason: Option<StopReason>,
    /// What the response cost.
    #[serde(default)]
    pub usage: Usage,
}

impl AssistantMessage {
    /// This response as the [`LlmMessage`] that goes back into the history.
    ///
    /// The readable reasoning is dropped, deliberately: it is not part of the
    /// conversation, and replaying a model's own notes to it is neither what
    /// either provider expects nor something either would accept unchanged.
    /// The opaque [`provider_items`](AssistantMessage::provider_items) are
    /// kept, because they are what a vendor asks to have back.
    pub fn message(&self) -> LlmMessage {
        LlmMessage::Assistant {
            content: self.content.clone(),
            tool_calls: self.tool_calls.clone(),
            provider_items: self.provider_items.clone(),
        }
    }

    /// Fold one delta into this message — the whole of what `collect` does per
    /// event, exposed so a *streaming* caller can accumulate the same message
    /// while it forwards deltas to a browser.
    pub fn push(&mut self, delta: LlmDelta) {
        match delta {
            LlmDelta::Text(text) => self.content.push_str(&text),
            LlmDelta::Reasoning(text) => self.reasoning.push_str(&text),
            LlmDelta::ToolCall(call) => self.tool_calls.push(call),
            LlmDelta::ProviderItem(item) => self.provider_items.push(item),
            LlmDelta::Stop { reason, usage } => {
                self.stop_reason = Some(reason);
                self.usage = usage;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_history_round_trips_through_json() {
        // `_fd_runs` stores the message history as JSON after every step
        // (§11.2), so a tool call's id and arguments have to survive the trip —
        // losing an id would break the correlation the next turn depends on.
        let history = vec![
            LlmMessage::user("how many books?"),
            LlmMessage::Assistant {
                content: "let me look".to_owned(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "query_books".to_owned(),
                    arguments: json!({"where": {"author": "Melville"}}),
                }],
                provider_items: vec![ProviderItem::EncryptedReasoning {
                    id: "rs_1".to_owned(),
                    encrypted_content: "gAAA…".to_owned(),
                }],
            },
            LlmMessage::ToolResult {
                tool_call_id: "call_1".to_owned(),
                name: "query_books".to_owned(),
                content: "3".to_owned(),
                images: vec![ImagePart::new("image/png", vec![0x89, b'P', b'N', b'G', 0])],
            },
        ];
        let text = serde_json::to_string(&history).unwrap();
        let back: Vec<LlmMessage> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, history);
    }

    #[test]
    fn an_assistant_message_accumulates_deltas_in_order() {
        let mut msg = AssistantMessage::default();
        msg.push(LlmDelta::Reasoning("thinking".to_owned()));
        msg.push(LlmDelta::Text("the ".to_owned()));
        msg.push(LlmDelta::Text("answer".to_owned()));
        msg.push(LlmDelta::ToolCall(ToolCall {
            id: "c1".to_owned(),
            name: "t".to_owned(),
            arguments: json!({}),
        }));
        msg.push(LlmDelta::Stop {
            reason: StopReason::ToolCalls,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 4,
                ..Usage::default()
            },
        });

        assert_eq!(msg.content, "the answer");
        assert_eq!(msg.reasoning, "thinking");
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.stop_reason, Some(StopReason::ToolCalls));
        assert_eq!(msg.usage.total_tokens(), 14);
    }

    #[test]
    fn an_image_is_serialised_as_base64_and_an_empty_list_not_at_all() {
        let result = LlmMessage::ToolResult {
            tool_call_id: "c".to_owned(),
            name: "view_app".to_owned(),
            content: "screenshot".to_owned(),
            images: vec![ImagePart::new("image/jpeg", b"hi".to_vec())],
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["images"][0]["data"], json!("aGk="));
        assert_eq!(value["images"][0]["media_type"], json!("image/jpeg"));

        // A history stored before images existed still reads, and a result
        // without images writes no key.
        let plain = LlmMessage::tool_result(
            &ToolCall {
                id: "c".to_owned(),
                name: "t".to_owned(),
                arguments: json!({}),
            },
            "ok",
        );
        let value = serde_json::to_value(&plain).unwrap();
        assert!(value.get("images").is_none(), "{value}");
        assert_eq!(serde_json::from_value::<LlmMessage>(value).unwrap(), plain);
    }

    #[test]
    fn provider_items_go_back_but_readable_reasoning_does_not() {
        let mut msg = AssistantMessage::default();
        msg.push(LlmDelta::Reasoning("my private notes".to_owned()));
        msg.push(LlmDelta::ProviderItem(ProviderItem::SignedThinking {
            thinking: "signed".to_owned(),
            signature: "sig".to_owned(),
        }));
        msg.push(LlmDelta::Text("done".to_owned()));
        let LlmMessage::Assistant {
            provider_items,
            content,
            ..
        } = msg.message()
        else {
            panic!("an assistant message");
        };
        assert_eq!(content, "done");
        assert_eq!(provider_items.len(), 1);
        assert!(
            !serde_json::to_string(&provider_items)
                .unwrap()
                .contains("private")
        );
    }

    #[test]
    fn a_request_without_the_new_fields_reads_and_writes_as_before() {
        let req = LlmRequest::prompt("hi");
        let value = serde_json::to_value(&req).unwrap();
        assert!(value.get("cache").is_none(), "{value}");
        assert!(value.get("parallel_tool_calls").is_none(), "{value}");
        assert_eq!(serde_json::from_value::<LlmRequest>(value).unwrap(), req);
        assert!(CachePlan::none().is_empty());
        assert!(!CachePlan::standard(None).is_empty());
    }

    #[test]
    fn reasoning_does_not_go_back_into_the_history() {
        let msg = AssistantMessage {
            content: "hello".to_owned(),
            reasoning: "the user greeted me".to_owned(),
            ..AssistantMessage::default()
        };
        assert_eq!(msg.message(), LlmMessage::assistant("hello"));
    }

    #[test]
    fn usage_accumulates_across_the_steps_of_a_loop() {
        let mut total = Usage::default();
        total.add(Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 80,
            cache_write_input_tokens: 10,
        });
        total.add(Usage {
            input_tokens: 130,
            output_tokens: 5,
            cached_input_tokens: 100,
            cache_write_input_tokens: 0,
        });
        assert_eq!(total.input_tokens, 230);
        assert_eq!(total.output_tokens, 25);
        assert_eq!(total.cached_input_tokens, 180);
        assert_eq!(total.cache_write_input_tokens, 10);
        assert_eq!(total.total_tokens(), 255);
    }

    #[test]
    fn a_tool_result_is_correlated_by_the_calls_own_id() {
        let call = ToolCall {
            id: "call_42".to_owned(),
            name: "insert_row".to_owned(),
            arguments: json!({"row": {}}),
        };
        assert_eq!(
            LlmMessage::tool_result(&call, "ok"),
            LlmMessage::ToolResult {
                tool_call_id: "call_42".to_owned(),
                name: "insert_row".to_owned(),
                content: "ok".to_owned(),
                images: Vec::new(),
            }
        );
    }
}

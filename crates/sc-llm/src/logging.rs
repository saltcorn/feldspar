//! What a model call says about itself: [`LoggedProvider`], the decorator every
//! configured provider is wrapped in.
//!
//! A model call is the one operation in this system that is slow, expensive,
//! non-deterministic and impossible to reproduce from the code — so when an
//! agent does something surprising, the only thing that explains it is what was
//! actually sent and what actually came back. That is what this prints.
//!
//! The ladder ([`sc_log::Verbosity`]):
//!
//! - **Info** — one line per finished call: which provider and model, how big
//!   the request was, why the model stopped, what it cost in tokens, and how
//!   long it took. This is the operational line — the one that answers "what is
//!   this installation spending?" and "why is the chat slow?".
//! - **Verbose** — plus a line as the call is *issued*. A model call is the
//!   longest wait in the system, and without this line a run that is waiting and
//!   a run that is wedged look identical.
//! - **Trace** — plus the **whole request and the whole response** as JSON: the
//!   system prompt, every message of the history, every tool schema offered, and
//!   the assembled answer with its reasoning, its tool calls and their arguments.
//!
//! **The request is ours, not the wire body.** What is logged is the
//! [`LlmRequest`] this workspace built — everything we decided to send, before
//! the adapter encodes it in the vendor's own shape. That is the level at which
//! the interesting mistakes live (a tool schema that is wrong, a history that
//! lost a tool result, a system prompt a trait never appended to), and it is the
//! same for every backend. The bytes on the wire are `rig`'s business.
//!
//! **The API key is not in any of this.** It lives in the adapter, from the
//! provider's stored configuration; nothing on this path has it to print.
//!
//! What *is* here is everything else: the user's messages, a tool's arguments
//! and results, whatever a trait put in the system prompt. A trace log of an
//! agent is a transcript of what the people using it typed, which is why it is
//! at trace and why the setting that turns it on says so.
//!
//! Below Info the decorator does nothing at all — it hands the call straight to
//! the provider it wraps, and no delta is copied.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sc_error::Result;
use sc_log::Verbosity;
use serde::Serialize;

use crate::message::{AssistantMessage, LlmRequest};
use crate::provider::{LlmProvider, LlmStream};

/// A provider that logs every call it forwards.
///
/// Applied by [`connect_model`](crate::connect_model), which is the one
/// place a stored provider record becomes something callable — so every model
/// call this server makes, from the agent loop, the chat socket or the
/// "test connection" button, comes through here. A test that builds an adapter
/// directly is not wrapped, which is the right way round: a fake provider has
/// nothing to say about what a real one was sent.
pub struct LoggedProvider {
    /// The provider being wrapped.
    inner: Arc<dyn LlmProvider>,
    /// How this call is named in the log: the admin's record name and the model.
    label: String,
}

impl LoggedProvider {
    /// Wrap `inner`, naming it after the provider record it was built from.
    pub fn new(inner: Arc<dyn LlmProvider>, provider_name: &str) -> LoggedProvider {
        let label = format!("{provider_name} ({})", inner.model());
        LoggedProvider { inner, label }
    }
}

#[async_trait]
impl LlmProvider for LoggedProvider {
    fn model(&self) -> &str {
        self.inner.model()
    }

    async fn stream(&self, req: LlmRequest) -> Result<LlmStream> {
        // Nothing below Info: no line, no clone, no accumulation.
        if !sc_log::enabled(Verbosity::Info) {
            return self.inner.stream(req).await;
        }

        sc_log::log_verbose!("llm → {}: {}", self.label, request_summary(&req));
        if sc_log::enabled(Verbosity::Trace) {
            sc_log::log_trace!(
                "llm request to {}:\n{}",
                self.label,
                as_json(&without_images(&req))
            );
        }

        let summary = request_summary(&req);
        let started = Instant::now();
        let stream = match self.inner.stream(req).await {
            Ok(stream) => stream,
            // A call that never started still cost the caller its wait, and the
            // reason ("connection refused", "invalid API key") is the whole
            // diagnosis. It is logged at Warning rather than Error because the
            // error is also *returned* — it reaches the chat window and the run
            // row — so this is the copy that says which call it was.
            Err(e) => {
                sc_log::log_warn!("llm ✗ {}: could not start the call: {e}", self.label);
                return Err(e);
            }
        };
        Ok(LlmStream::new(Box::pin(logged_stream(
            stream,
            CallLog {
                label: self.label.clone(),
                summary,
                started,
                answer: AssistantMessage::default(),
            },
        ))))
    }
}

/// What one in-flight call has seen so far.
struct CallLog {
    /// The provider and model, as the log names them.
    label: String,
    /// What the request was, kept for the completion line so one line explains
    /// both halves of the call.
    summary: String,
    /// When the call was issued.
    started: Instant,
    /// The answer as it assembles — the same reassembly
    /// [`LlmStream::collect`](crate::LlmStream::collect) does, kept here so the
    /// log does not depend on what the caller chose to do with the stream.
    answer: AssistantMessage,
}

impl CallLog {
    /// The call ended cleanly.
    fn finished(&self) {
        sc_log::log_info!(
            "llm ← {}: {} · {}",
            self.label,
            self.summary,
            response_summary(&self.answer, self.started.elapsed())
        );
        if sc_log::enabled(Verbosity::Trace) {
            sc_log::log_trace!(
                "llm response from {}:\n{}",
                self.label,
                as_json(&self.answer)
            );
        }
    }

    /// The stream ended in an error — a connection cut mid-answer, a provider
    /// error partway through.
    fn failed(&self, error: &sc_error::Error) {
        sc_log::log_warn!(
            "llm ✗ {}: failed after {} — {error}",
            self.label,
            sc_log::human_duration(self.started.elapsed())
        );
        if sc_log::enabled(Verbosity::Trace) {
            sc_log::log_trace!(
                "llm partial response from {}:\n{}",
                self.label,
                as_json(&self.answer)
            );
        }
    }
}

/// Forward every delta unchanged, accumulating the answer, and log when the
/// stream ends.
///
/// Written as an `unfold` rather than a `Stream` impl because the state is the
/// point: the deltas the caller sees are exactly the ones the provider yielded,
/// in the same order, and the only thing that happens on the way past is a
/// clone into the accumulator.
fn logged_stream(inner: LlmStream, log: CallLog) -> crate::provider::DeltaStream {
    Box::pin(futures::stream::unfold(
        Some((inner, log)),
        |state| async move {
            let (mut inner, mut log) = state?;
            match inner.next().await {
                Some(Ok(delta)) => {
                    log.answer.push(delta.clone());
                    Some((Ok(delta), Some((inner, log))))
                }
                // An `Err` item ends the response (see `DeltaStream`), so the
                // error is yielded and the stream stops here.
                Some(Err(e)) => {
                    log.failed(&e);
                    Some((Err(e), None))
                }
                None => {
                    log.finished();
                    None
                }
            }
        },
    ))
}

/// What is in a request, in one clause: the shape of it, not its contents.
///
/// The contents are the trace dump; this is what fits on the line an operator
/// reads a hundred of. Sizes are in characters because that is what the caller
/// can act on — a system prompt that has quietly grown to 40 000 characters is
/// the finding.
pub fn request_summary(req: &LlmRequest) -> String {
    let mut parts = vec![format!("{} messages", req.messages.len())];
    if !req.tools.is_empty() {
        parts.push(format!("{} tools", req.tools.len()));
    }
    if let Some(system) = &req.system {
        parts.push(format!("system {} chars", system.chars().count()));
    }
    if let Some(max_tokens) = req.max_tokens {
        parts.push(format!("max_tokens {max_tokens}"));
    }
    parts.join(", ")
}

/// What came back, in one clause: why it stopped, what it produced, what it
/// cost, and how long it took.
pub fn response_summary(answer: &AssistantMessage, elapsed: Duration) -> String {
    let mut parts = Vec::new();
    match answer.stop_reason {
        Some(reason) => parts.push(format!("stopped {reason:?}")),
        // A stream that ended without a `Stop` is a provider bug or a cut
        // connection; saying so beats printing nothing where the reason goes.
        None => parts.push("no stop reason".to_owned()),
    }
    parts.push(format!("{} chars", answer.content.chars().count()));
    if !answer.reasoning.is_empty() {
        parts.push(format!(
            "{} reasoning chars",
            answer.reasoning.chars().count()
        ));
    }
    if !answer.tool_calls.is_empty() {
        let names: Vec<&str> = answer
            .tool_calls
            .iter()
            .map(|call| call.name.as_str())
            .collect();
        parts.push(format!(
            "{} tool calls [{}]",
            answer.tool_calls.len(),
            names.join(", ")
        ));
    }
    let usage = &answer.usage;
    if usage.input_tokens > 0 || usage.output_tokens > 0 {
        let cached = if usage.cached_input_tokens > 0 {
            format!(" ({} cached)", usage.cached_input_tokens)
        } else {
            String::new()
        };
        parts.push(format!(
            "{} in{cached} / {} out tokens",
            usage.input_tokens, usage.output_tokens
        ));
    }
    parts.push(format!("in {}", sc_log::human_duration(elapsed)));
    parts.join(", ")
}

/// Pretty JSON for a trace dump, or a note saying why there is none.
///
/// Never an error and never a panic: a logging path that could fail the call it
/// is describing would be worse than no log at all.
/// The request as JSON with every image's bytes replaced by their size: a
/// screenshot is never logged, even at trace (TODO §7b).
fn without_images(req: &crate::message::LlmRequest) -> serde_json::Value {
    let mut value = serde_json::to_value(req).unwrap_or(serde_json::Value::Null);
    if let Some(messages) = value.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for message in messages {
            // A user message's attachments are not logged either: an image is
            // a screenshot's equal, and a document is the person's file.
            for key in ["images", "attachments"] {
                let Some(images) = message.get_mut(key).and_then(|i| i.as_array_mut()) else {
                    continue;
                };
                for image in images {
                    if let Some(data) = image.get_mut("data") {
                        let chars = data.as_str().map_or(0, str::len);
                        *data = serde_json::Value::String(format!(
                            "‹{chars} base64 characters not logged›"
                        ));
                    }
                }
            }
        }
    }
    value
}

fn as_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|e| format!("‹could not be serialised for the log: {e}›"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_logged_request_carries_no_image_bytes() {
        let call = ToolCall {
            id: "c".to_owned(),
            name: "view_app".to_owned(),
            arguments: json!({}),
        };
        let mut result = LlmMessage::tool_result(&call, "shot");
        if let LlmMessage::ToolResult { images, .. } = &mut result {
            images.push(crate::message::ImagePart::new("image/jpeg", vec![7u8; 300]));
        }
        let req = crate::message::LlmRequest {
            messages: vec![LlmMessage::user("look"), result],
            ..Default::default()
        };
        let logged = as_json(&without_images(&req));
        let encoded = crate::message::ImagePart::new("image/jpeg", vec![7u8; 300]).base64();
        assert!(!logged.contains(&encoded[..40]), "{logged}");
        assert!(
            logged.contains("‹400 base64 characters not logged›"),
            "{logged}"
        );
        assert!(logged.contains("image/jpeg"));
    }
    use crate::message::{LlmDelta, LlmMessage, StopReason, ToolCall, ToolSpec, Usage};

    /// A provider that yields a scripted stream, so the decorator can be tested
    /// without a network.
    struct Scripted {
        deltas: Vec<Result<LlmDelta>>,
    }

    #[async_trait]
    impl LlmProvider for Scripted {
        fn model(&self) -> &str {
            "gpt-5.1"
        }

        async fn stream(&self, _req: LlmRequest) -> Result<LlmStream> {
            let deltas: Vec<Result<LlmDelta>> = self
                .deltas
                .iter()
                .map(|d| match d {
                    Ok(delta) => Ok(delta.clone()),
                    Err(e) => Err(sc_error::Error::msg(e.to_string())),
                })
                .collect();
            Ok(LlmStream::new(Box::pin(futures::stream::iter(deltas))))
        }
    }

    fn scripted_deltas() -> Vec<Result<LlmDelta>> {
        vec![
            Ok(LlmDelta::Text("Looking".to_owned())),
            Ok(LlmDelta::Text(" it up.".to_owned())),
            Ok(LlmDelta::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "query_books".to_owned(),
                arguments: json!({ "limit": 5 }),
            })),
            Ok(LlmDelta::Stop {
                reason: StopReason::ToolCalls,
                usage: Usage {
                    input_tokens: 1200,
                    output_tokens: 300,
                    cached_input_tokens: 1024,
                    cache_write_input_tokens: 0,
                },
            }),
        ]
    }

    fn request() -> LlmRequest {
        LlmRequest::prompt("how many books are there?")
            .system("You are a librarian.")
            .tools([ToolSpec::new(
                "query_books",
                "Query the books table",
                json!({ "type": "object" }),
            )])
            .max_tokens(4096)
    }

    /// The decorator's first duty: the caller sees exactly the stream the
    /// provider produced. Driven at trace, where the decorator does the most.
    #[tokio::test]
    async fn logging_a_call_does_not_change_what_the_caller_receives() {
        let previous = sc_log::verbosity();
        sc_log::set_verbosity(Verbosity::Trace);

        let provider = LoggedProvider::new(
            Arc::new(Scripted {
                deltas: scripted_deltas(),
            }),
            "openai",
        );
        let answer = provider
            .stream(request())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();

        sc_log::set_verbosity(previous);
        assert_eq!(answer.content, "Looking it up.");
        assert_eq!(answer.tool_calls.len(), 1);
        assert_eq!(answer.tool_calls[0].arguments, json!({ "limit": 5 }));
        assert_eq!(answer.stop_reason, Some(StopReason::ToolCalls));
        assert_eq!(answer.usage.input_tokens, 1200);
        assert_eq!(provider.model(), "gpt-5.1");
    }

    /// An error mid-stream reaches the caller unchanged — the log is not
    /// allowed to swallow it, and the partial answer is not allowed to become
    /// the answer.
    #[tokio::test]
    async fn an_error_mid_stream_still_reaches_the_caller() {
        let previous = sc_log::verbosity();
        sc_log::set_verbosity(Verbosity::Trace);

        let provider = LoggedProvider::new(
            Arc::new(Scripted {
                deltas: vec![
                    Ok(LlmDelta::Text("half an ans".to_owned())),
                    Err(sc_error::Error::msg("connection reset")),
                ],
            }),
            "openai",
        );
        let mut stream = provider.stream(request()).await.unwrap();
        assert!(matches!(stream.next().await, Some(Ok(LlmDelta::Text(_)))));
        let err = stream.next().await.unwrap().unwrap_err();
        assert!(err.to_string().contains("connection reset"), "{err}");
        // The error ended the response.
        assert!(stream.next().await.is_none());

        sc_log::set_verbosity(previous);
    }

    /// Below Info the decorator is a pass-through, which is what makes leaving
    /// it in front of every provider free.
    #[tokio::test]
    async fn a_quiet_server_still_gets_its_answer() {
        let previous = sc_log::verbosity();
        sc_log::set_verbosity(Verbosity::Error);

        let provider = LoggedProvider::new(
            Arc::new(Scripted {
                deltas: scripted_deltas(),
            }),
            "openai",
        );
        let answer = provider
            .stream(request())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();

        sc_log::set_verbosity(previous);
        assert_eq!(answer.content, "Looking it up.");
    }

    #[test]
    fn the_request_line_says_what_was_sent() {
        assert_eq!(
            request_summary(&request()),
            "1 messages, 1 tools, system 20 chars, max_tokens 4096"
        );
        assert_eq!(
            request_summary(&LlmRequest {
                messages: vec![LlmMessage::user("hi"), LlmMessage::assistant("hello")],
                ..LlmRequest::default()
            }),
            "2 messages"
        );
    }

    #[test]
    fn the_response_line_says_what_came_back_and_what_it_cost() {
        let mut answer = AssistantMessage::default();
        for delta in scripted_deltas() {
            answer.push(delta.unwrap());
        }
        assert_eq!(
            response_summary(&answer, Duration::from_millis(2_340)),
            "stopped ToolCalls, 14 chars, 1 tool calls [query_books], \
             1200 in (1024 cached) / 300 out tokens, in 2.3s"
        );
    }

    /// A stream that stopped without saying why is a fact worth printing, not a
    /// blank in the middle of the line.
    #[test]
    fn a_response_with_no_stop_reason_says_so() {
        let line = response_summary(&AssistantMessage::default(), Duration::from_millis(12));
        assert!(line.starts_with("no stop reason"), "{line}");
        assert!(line.ends_with("in 12ms"), "{line}");
    }

    /// The whole point of the trace dump: the messages and the tool schemas are
    /// in it, in full.
    #[test]
    fn the_trace_dump_is_the_whole_request() {
        let dump = as_json(&request());
        assert!(dump.contains("how many books are there?"), "{dump}");
        assert!(dump.contains("You are a librarian."), "{dump}");
        assert!(dump.contains("query_books"), "{dump}");
    }
}

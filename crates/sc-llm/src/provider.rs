//! [`LlmProvider`]: one configured model, ready to be called, and [`LlmStream`],
//! the one shape a response comes back in (design §11.1).
//!
//! ## Why the trait exists at all
//!
//! Which provider runs is decided at runtime, from a row in `_fd_llm_providers`
//! that an admin filled in. That needs a `Box<dyn _>`, and rig's
//! `CompletionModel` cannot be one: it has associated types, returns
//! `impl Future`, and requires `Clone`. So a seam is needed whatever crate is
//! underneath — this is that seam, and is not much more than it.
//!
//! ## Why streaming is the only shape
//!
//! A non-streaming call is a stream collected to the end ([`LlmStream::collect`]);
//! the reverse is not true, and the chat interface needs deltas from the first
//! turn. One path also means the tool-call assembly that providers each do
//! differently is written and tested once, rather than once per shape.

use std::pin::Pin;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use sc_error::Result;

use crate::message::{AssistantMessage, LlmDelta, LlmRequest};

/// The stream of deltas a provider yields, boxed so it is nameable across the
/// object-safe [`LlmProvider::stream`].
///
/// An item that is an `Err` ends the response: a provider error, a malformed
/// body or a connection cut mid-answer arrives here rather than as a silent end
/// of stream, because "the model said nothing" and "the call failed" must not
/// look alike to a chat window (§11.4).
pub type DeltaStream = Pin<Box<dyn Stream<Item = Result<LlmDelta>> + Send>>;

/// One configured model, ready to be called (§11.1).
///
/// Object-safe by construction: no generics, no associated types, `&self`
/// throughout. What implements it is an adapter over a provider crate
/// ([`openai_responses`](crate::openai), [`anthropic`](crate::anthropic)), and
/// what holds it is an `Arc<dyn LlmProvider>` built from stored configuration by
/// [`connect_model`](crate::connect_model).
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Which model this will call — the resolved one, after any per-agent
    /// override, so a transcript can record what actually answered.
    fn model(&self) -> &str;

    /// Send one request and return its stream of deltas.
    ///
    /// An `Err` here is a failure to *start*: a refused connection, a rejected
    /// key. Once a stream exists, everything that goes wrong arrives as an `Err`
    /// item inside it.
    async fn stream(&self, req: LlmRequest) -> Result<LlmStream>;

    /// The most tokens an answer may generate when the request asks for
    /// `requested`: what the backend actually sends. `None` when no cap is sent
    /// and the model stops where it likes. An answer that used exactly this
    /// many was cut off, which the agent loop tells the model.
    fn output_cap(&self, requested: Option<u32>) -> Option<u32> {
        requested
    }
}

/// A response in progress: deltas until a
/// [`Stop`](crate::LlmDelta::Stop), then the end of the stream.
///
/// A thin wrapper over [`DeltaStream`] rather than a bare `Pin<Box<dyn Stream>>`
/// so [`collect`](LlmStream::collect) has somewhere to live — the one place the
/// streaming and non-streaming shapes meet.
pub struct LlmStream {
    inner: DeltaStream,
}

impl LlmStream {
    /// Wrap a delta stream. Adapters build one of these; nothing else needs to.
    pub fn new(inner: DeltaStream) -> LlmStream {
        LlmStream { inner }
    }

    /// A stream of exactly these deltas — the shape a test or a scripted fake
    /// provider produces.
    pub fn from_deltas(deltas: impl IntoIterator<Item = LlmDelta>) -> LlmStream {
        let items: Vec<Result<LlmDelta>> = deltas.into_iter().map(Ok).collect();
        LlmStream::new(Box::pin(futures::stream::iter(items)))
    }

    /// The next delta, or `None` at the end of the response.
    pub async fn next(&mut self) -> Option<Result<LlmDelta>> {
        self.inner.next().await
    }

    /// Drain the whole stream into one assistant message.
    ///
    /// This is what makes "streaming is the only shape" affordable: `run_agent`
    /// (§11.5) and every test that does not care about deltas call this, and get
    /// exactly what a non-streaming API would have returned, assembled by the
    /// same code the chat path uses.
    ///
    /// The **first** error wins and ends the collection. A partially received
    /// answer followed by a failure is a failure: returning the fragment as
    /// though it were the response is how a truncated stream becomes a wrong
    /// answer nobody notices.
    pub async fn collect(mut self) -> Result<AssistantMessage> {
        let mut message = AssistantMessage::default();
        while let Some(delta) = self.next().await {
            message.push(delta?);
        }
        Ok(message)
    }
}

impl Stream for LlmStream {
    type Item = Result<LlmDelta>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{StopReason, ToolCall, Usage};
    use serde_json::json;

    fn scripted() -> Vec<LlmDelta> {
        vec![
            LlmDelta::Reasoning("weighing it up".to_owned()),
            LlmDelta::Text("Looking".to_owned()),
            LlmDelta::Text(" it up.".to_owned()),
            LlmDelta::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: "query_books".to_owned(),
                arguments: json!({"limit": 5}),
            }),
            LlmDelta::Stop {
                reason: StopReason::ToolCalls,
                usage: Usage {
                    input_tokens: 42,
                    output_tokens: 7,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
            },
        ]
    }

    #[tokio::test]
    async fn collect_reassembles_exactly_what_stream_emitted() {
        let msg = LlmStream::from_deltas(scripted()).collect().await.unwrap();
        assert_eq!(msg.content, "Looking it up.");
        assert_eq!(msg.reasoning, "weighing it up");
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].arguments, json!({"limit": 5}));
        assert_eq!(msg.stop_reason, Some(StopReason::ToolCalls));
        assert_eq!(msg.usage.input_tokens, 42);
    }

    #[tokio::test]
    async fn an_error_mid_stream_fails_the_collection_rather_than_truncating_it() {
        let items: Vec<Result<LlmDelta>> = vec![
            Ok(LlmDelta::Text("half an ans".to_owned())),
            Err(sc_error::Error::msg("connection reset")),
        ];
        let stream = LlmStream::new(Box::pin(futures::stream::iter(items)));
        let err = stream.collect().await.unwrap_err();
        assert!(err.to_string().contains("connection reset"), "{err}");
    }

    #[tokio::test]
    async fn a_stream_can_be_consumed_delta_by_delta() {
        // The chat path's shape: forward each delta, accumulate as you go.
        let mut stream = LlmStream::from_deltas(scripted());
        let mut seen = 0;
        let mut msg = AssistantMessage::default();
        while let Some(delta) = stream.next().await {
            msg.push(delta.unwrap());
            seen += 1;
        }
        assert_eq!(seen, 5);
        assert_eq!(msg.content, "Looking it up.");
    }
}

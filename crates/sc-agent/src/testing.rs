//! [`FakeProvider`]: a model that says what it was told to say.
//!
//! Behind the `testing` **feature**, not `#[cfg(test)]`, and deliberately
//! (decision 7): `sc-core-traits` tests its traits through a whole run, and
//! `sc-server` tests the chat socket through one, so a fake that existed only
//! inside this crate's own test build is one neither could reach. It is a
//! first-class part of the crate.
//!
//! **No test in this tree may need an API key or spend a token.** This is what
//! makes that possible — every path through the loop, every trait and the socket
//! itself are exercised against a script rather than a vendor.
//!
//! ```
//! # use sc_agent::testing::{FakeProvider, Reply};
//! let provider = FakeProvider::new([
//!     Reply::calls("query_books", serde_json::json!({"limit": 5})),
//!     Reply::says("There are five."),
//! ]);
//! ```
//!
//! ## Scripts that depend on the role and the mode (TODO §13)
//!
//! A **role** is answered by its own provider: [`FakeModels`] hands one out per
//! role. A **mode** shows in what the request offers, so a provider can keep a
//! script per [`Match`] beside its main one: a request in `plan` mode offers the
//! planning tools, and a compaction's summary request offers none and carries
//! the summary prompt.
//!
//! ```
//! # use sc_agent::testing::{FakeProvider, Match, Reply};
//! let provider = FakeProvider::new([Reply::says("done")])
//!     .when(Match::Offers("save_plan".into()), [Reply::says("planned")])
//!     .when(Match::Summary, [Reply::says("## Goal\n…")]);
//! ```
//!
//! ## Asserting what was sent
//!
//! [`FakeProvider::requests`] is every request. The assertions below it read
//! the layout the loop promises: [`assert_stable_prefix`] (system prompt, tools
//! and session header byte-identical across a session),
//! [`session_header`](FakeProvider::session_header), and
//! [`elided_results`](FakeProvider::elided_results).
//!
//! [`assert_stable_prefix`]: FakeProvider::assert_stable_prefix

use std::sync::{Arc, Mutex};

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_llm::{
    ConnectedModel, LlmDelta, LlmMessage, LlmProvider, LlmRequest, LlmStream, Prices, StopReason,
    ToolCall, Usage, estimate_tokens,
};
use serde_json::Value as Json;

use crate::agent::{Agent, ModelRole};
use crate::driver::ProviderConnector;

/// One scripted answer.
///
/// Named `Reply` rather than `Turn` to stay out of the way of
/// [`Turn`](crate::Turn), which is the thing a trait changes before a request
/// goes out — a different object at a different moment.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// The model says this and ends its turn. Text arrives as several deltas so a
    /// caller that is testing streaming sees more than one.
    Says(String),
    /// The model calls these tools, in this order, saying `preamble` first.
    Calls {
        /// What it says before calling — often empty, which is what both vendors
        /// most often do.
        preamble: String,
        /// The calls, with ids generated in order.
        calls: Vec<(String, Json)>,
    },
    /// The provider fails: a rejected key, a rate limit, a cut connection.
    ///
    /// A first-class scripted outcome rather than something a test has to
    /// simulate, because "the provider refused" is a path the loop and the chat
    /// window both have to handle and neither is exercised by a happy script.
    Fails(String),
}

impl Reply {
    /// A turn that answers with text.
    pub fn says(text: impl Into<String>) -> Reply {
        Reply::Says(text.into())
    }

    /// A turn that calls one tool.
    pub fn calls(tool: impl Into<String>, arguments: Json) -> Reply {
        Reply::Calls {
            preamble: String::new(),
            calls: vec![(tool.into(), arguments)],
        }
    }

    /// A turn that calls several tools at once — the case whose *order* the loop
    /// must preserve.
    pub fn calls_many(calls: impl IntoIterator<Item = (String, Json)>) -> Reply {
        Reply::Calls {
            preamble: String::new(),
            calls: calls.into_iter().collect(),
        }
    }

    /// A turn that fails.
    pub fn fails(error: impl Into<String>) -> Reply {
        Reply::Fails(error.into())
    }

    /// Say something before calling the tools.
    pub fn with_preamble(mut self, text: impl Into<String>) -> Reply {
        if let Reply::Calls { preamble, .. } = &mut self {
            *preamble = text.into();
        }
        self
    }
}

/// Which requests a script kept beside the main one answers (see
/// [`FakeProvider::when`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Match {
    /// A request offering a tool of this name — how a mode is told apart.
    Offers(String),
    /// A request whose system prompt contains this text.
    SystemContains(String),
    /// A request with this text in any message.
    Mentions(String),
    /// A compaction's summary request.
    Summary,
}

impl Match {
    /// Whether `req` is one this matches.
    pub fn matches(&self, req: &LlmRequest) -> bool {
        match self {
            Match::Offers(tool) => req.tools.iter().any(|t| &t.name == tool),
            Match::SystemContains(text) => req
                .system
                .as_deref()
                .is_some_and(|s| s.contains(text.as_str())),
            Match::Mentions(text) => req
                .messages
                .iter()
                .any(|m| content(m).contains(text.as_str())),
            Match::Summary => {
                req.tools.is_empty()
                    && req.system.as_deref() == Some(crate::context::SUMMARY_PROMPT)
            }
        }
    }
}

/// A message's text.
fn content(message: &LlmMessage) -> &str {
    match message {
        LlmMessage::User { content, .. }
        | LlmMessage::Assistant { content, .. }
        | LlmMessage::ToolResult { content, .. } => content,
    }
}

/// A provider that replays a script, and records what it was asked.
///
/// Interior-mutable and `Send + Sync`, because [`LlmProvider::stream`] takes
/// `&self` — which it does because the real adapters hold a connection pool.
pub struct FakeProvider {
    model: String,
    script: Mutex<std::collections::VecDeque<Reply>>,
    /// Scripts for particular requests, checked in order before `script`.
    matched: Mutex<Vec<(Match, std::collections::VecDeque<Reply>)>>,
    /// Report each request's estimated size as its input tokens, rather than a
    /// fixed ten.
    count_input: bool,
    seen: Mutex<Vec<LlmRequest>>,
}

impl FakeProvider {
    /// A provider that will answer with these turns, in order.
    pub fn new(script: impl IntoIterator<Item = Reply>) -> FakeProvider {
        FakeProvider {
            model: "fake-model".to_owned(),
            script: Mutex::new(script.into_iter().collect()),
            matched: Mutex::new(Vec::new()),
            count_input: false,
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Answer requests that `matches` from `script` first, and the rest from
    /// the main script. A matched script that has run out falls through to the
    /// main one.
    pub fn when(self, matches: Match, script: impl IntoIterator<Item = Reply>) -> FakeProvider {
        lock(&self.matched).push((matches, script.into_iter().collect()));
        self
    }

    /// Report each request's size, as [`estimate_tokens`] counts it, as its
    /// `input_tokens` — so the context budget is measured as it would be
    /// against a real provider.
    pub fn counting_input(mut self) -> FakeProvider {
        self.count_input = true;
        self
    }

    /// A provider that answers **every** call the same way — for testing a loop
    /// that has to be stopped by its step budget rather than by the model.
    pub fn repeating(turn: Reply, times: usize) -> FakeProvider {
        FakeProvider::new(std::iter::repeat_n(turn, times))
    }

    /// Report a different model name, for a test that asserts what answered.
    pub fn as_model(mut self, model: impl Into<String>) -> FakeProvider {
        self.model = model.into();
        self
    }

    /// Every request it was sent, in order — which is how a test asserts what the
    /// agent actually offered the model: the system prompt a trait appended to,
    /// the tools, the history.
    pub fn requests(&self) -> Vec<LlmRequest> {
        lock(&self.seen).clone()
    }

    /// The last request it was sent.
    pub fn last_request(&self) -> Option<LlmRequest> {
        self.requests().pop()
    }

    /// The requests that `matches`, in order.
    pub fn requests_matching(&self, matches: &Match) -> Vec<LlmRequest> {
        self.requests()
            .into_iter()
            .filter(|r| matches.matches(r))
            .collect()
    }

    /// Assert that every request of the loop's own — not a summary — carried
    /// the same prefix: the system prompt, the tools, and the session header,
    /// byte for byte. What a prompt cache needs (TODO §9).
    pub fn assert_stable_prefix(&self) {
        let requests: Vec<LlmRequest> = self
            .requests()
            .into_iter()
            .filter(|r| !Match::Summary.matches(r))
            .collect();
        let prefix = |r: &LlmRequest| {
            serde_json::to_string(&(
                &r.system,
                &r.tools,
                r.cache.session_header.map(|i| &r.messages[..=i]),
            ))
            .unwrap_or_default()
        };
        for (n, pair) in requests.windows(2).enumerate() {
            assert_eq!(
                prefix(&pair[0]),
                prefix(&pair[1]),
                "requests {} and {} differ before the history",
                n + 1,
                n + 2
            );
        }
    }

    /// The session header request `n` carried, if it had one.
    pub fn session_header(&self, n: usize) -> Option<String> {
        let requests = self.requests();
        let request = requests.get(n)?;
        let index = request.cache.session_header?;
        Some(content(request.messages.get(index)?).to_owned())
    }

    /// How many tool results in `request` were cleared to a stub.
    pub fn elided_results(request: &LlmRequest) -> usize {
        request
            .messages
            .iter()
            .filter(|m| matches!(m, LlmMessage::ToolResult { content, .. } if content.starts_with("[elided")))
            .count()
    }

    /// How many turns of the script are left unused.
    ///
    /// A test that scripted three turns and used two has learned something: the
    /// loop stopped earlier than it meant to.
    pub fn remaining(&self) -> usize {
        lock(&self.script).len()
            + lock(&self.matched)
                .iter()
                .map(|(_, s)| s.len())
                .sum::<usize>()
    }
}

#[async_trait::async_trait]
impl LlmProvider for FakeProvider {
    fn model(&self) -> &str {
        &self.model
    }

    async fn stream(&self, req: LlmRequest) -> Result<LlmStream> {
        let usage = if self.count_input {
            Usage {
                input_tokens: estimate_tokens(&req, ""),
                ..usage()
            }
        } else {
            usage()
        };
        let matched = lock(&self.matched)
            .iter_mut()
            .find(|(m, script)| !script.is_empty() && m.matches(&req))
            .and_then(|(_, script)| script.pop_front());
        lock(&self.seen).push(req);
        let reply = matched
            .or_else(|| lock(&self.script).pop_front())
            // A script that ran out is the test's bug, not the loop's, and saying
            // so beats an empty answer the loop would treat as a real one.
            .ok_or_else(|| Error::msg("the fake provider's script ran out of turns"))?;

        let deltas = match reply {
            Reply::Fails(error) => {
                // Fails where a real provider most often does: after the request
                // was accepted, as an error inside the stream.
                let items: Vec<Result<LlmDelta>> = vec![Err(Error::msg(error))];
                return Ok(LlmStream::new(Box::pin(futures::stream::iter(items))));
            }
            Reply::Says(text) => {
                let mut deltas: Vec<LlmDelta> = split(&text)
                    .into_iter()
                    .map(|part| LlmDelta::Text(part.to_owned()))
                    .collect();
                deltas.push(LlmDelta::Stop {
                    reason: StopReason::EndTurn,
                    usage,
                });
                deltas
            }
            Reply::Calls { preamble, calls } => {
                let mut deltas: Vec<LlmDelta> = Vec::new();
                if !preamble.is_empty() {
                    deltas.push(LlmDelta::Text(preamble));
                }
                for (i, (name, arguments)) in calls.into_iter().enumerate() {
                    deltas.push(LlmDelta::ToolCall(ToolCall {
                        id: format!("fake_call_{}", i + 1),
                        name,
                        arguments,
                    }));
                }
                deltas.push(LlmDelta::Stop {
                    reason: StopReason::ToolCalls,
                    usage,
                });
                deltas
            }
        };
        Ok(LlmStream::from_deltas(deltas))
    }
}

/// A [`ProviderConnector`] that hands out scripted providers **by role** (TODO
/// §13), so a test can assert which role answered each step.
///
/// A role with no script of its own gets the executor's, which is the fallback
/// an agent with the role unset gets anyway. Every agent that asks gets the same
/// providers, so a self-delegated child shares its parent's scripts.
#[derive(Default)]
pub struct FakeModels {
    models: Mutex<Vec<(ModelRole, ConnectedModel)>>,
}

impl FakeModels {
    /// No models yet.
    pub fn new() -> FakeModels {
        FakeModels::default()
    }

    /// `role` is answered by `provider`, with unknown prices.
    pub fn role(self, role: ModelRole, provider: Arc<FakeProvider>) -> FakeModels {
        self.priced(role, provider, Prices::default())
    }

    /// `role` is answered by `provider`, at `prices`.
    pub fn priced(
        self,
        role: ModelRole,
        provider: Arc<FakeProvider>,
        prices: Prices,
    ) -> FakeModels {
        let mut model = ConnectedModel::unconfigured(provider as Arc<dyn LlmProvider>);
        model.prices = prices;
        lock(&self.models).push((role, model));
        self
    }

    /// The model for `role`, falling back to the executor's.
    pub fn model(&self, role: ModelRole) -> Result<ConnectedModel> {
        let models = lock(&self.models);
        models
            .iter()
            .find(|(r, _)| *r == role)
            .or_else(|| models.iter().find(|(r, _)| *r == ModelRole::Executor))
            .map(|(_, m)| m.clone())
            .ok_or_else(|| Error::config(format!("no scripted model for the `{role}` role")))
    }
}

#[async_trait::async_trait]
impl ProviderConnector for FakeModels {
    async fn connect(
        &self,
        _catalog: &Catalog,
        _agent: &Agent,
        role: ModelRole,
    ) -> Result<ConnectedModel> {
        self.model(role)
    }
}

/// A lock that survives a poisoned mutex.
///
/// A test that panicked while holding one has already failed; refusing to answer
/// the *next* test because of it would turn one failure into a cascade nobody can
/// read.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Text as more than one delta, so a streaming caller sees a stream.
fn split(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mid = text.len() / 2;
    // Never split inside a character.
    let mid = (mid..=text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    if mid == 0 || mid == text.len() {
        vec![text]
    } else {
        vec![&text[..mid], &text[mid..]]
    }
}

/// Plausible, non-zero usage — so a test asserting that a run accumulates it has
/// something to accumulate.
fn usage() -> Usage {
    Usage {
        input_tokens: 10,
        output_tokens: 5,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_scripted_turn_streams_text_and_stops() {
        let provider = FakeProvider::new([Reply::says("There are five.")]);
        let answer = provider
            .stream(LlmRequest::prompt("how many?"))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(answer.content, "There are five.");
        assert_eq!(answer.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(answer.usage.total_tokens(), 15);
        assert_eq!(provider.remaining(), 0);
        assert_eq!(provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_scripted_tool_call_arrives_whole_and_in_order() {
        let provider = FakeProvider::new([Reply::calls_many([
            ("read".to_owned(), json!({"path": "a"})),
            ("write".to_owned(), json!({"path": "b"})),
        ])
        .with_preamble("Doing both.")]);
        let answer = provider
            .stream(LlmRequest::prompt("go"))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(answer.content, "Doing both.");
        assert_eq!(answer.stop_reason, Some(StopReason::ToolCalls));
        let names: Vec<&str> = answer.tool_calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["read", "write"]);
        assert_ne!(answer.tool_calls[0].id, answer.tool_calls[1].id);
    }

    #[tokio::test]
    async fn a_scripted_failure_arrives_inside_the_stream() {
        let provider = FakeProvider::new([Reply::fails("401 invalid x-api-key")]);
        let err = provider
            .stream(LlmRequest::prompt("hi"))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }

    #[tokio::test]
    async fn a_matched_script_answers_its_requests_first() {
        use sc_llm::ToolSpec;
        let provider = FakeProvider::new([Reply::says("main")])
            .when(Match::Offers("save_plan".into()), [Reply::says("planned")])
            .counting_input();
        let plan = LlmRequest::prompt("plan it").tools([ToolSpec::new(
            "save_plan",
            "Save the plan",
            json!({"type": "object"}),
        )]);
        let answer = provider
            .stream(plan)
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(answer.content, "planned");
        assert!(answer.usage.input_tokens > 10, "{:?}", answer.usage);
        // Its script used up, a matching request falls through to the main one.
        let again = LlmRequest::prompt("again").tools([ToolSpec::new(
            "save_plan",
            "Save the plan",
            json!({"type": "object"}),
        )]);
        let answer = provider
            .stream(again)
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(answer.content, "main");
        assert_eq!(provider.remaining(), 0);
        assert_eq!(
            provider
                .requests_matching(&Match::Offers("save_plan".into()))
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn an_exhausted_script_says_so() {
        let provider = FakeProvider::new([]);
        let err = match provider.stream(LlmRequest::prompt("hi")).await {
            Err(e) => e,
            Ok(_) => panic!("an exhausted script must not answer"),
        };
        assert!(err.to_string().contains("script"), "{err}");
    }
}

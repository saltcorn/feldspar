//! The context a request carries: its layout, its budget, and how it is kept
//! within that budget by clearing and compacting (TODO §9).
//!
//! ## The layout
//!
//! A request runs from most to least stable, so that each part can be cached
//! behind the one before it:
//!
//! 1. **The stable prefix:** the agent's prompt, each trait's static
//!    contribution and the tools in name order. The driver builds it.
//! 2. **The session header:** a first user-side message that traits build
//!    **once per session** through [`AgentTrait::session_header`]. It is stored
//!    here with the run, so neither step 2 nor a resume rebuilds it.
//! 3. **The history**, append-only.
//!
//! ## The stored transcript stays whole
//!
//! Nothing here edits [`AgentLoop`](crate::AgentLoop)'s messages. Clearing and
//! compacting are **overlays** kept beside them: a stub per cleared tool result,
//! keyed by the result's index, and `(up_to_index, summary)` records. The request
//! is built from the transcript plus the overlays, while the chat and the admin
//! read the transcript itself, with a marker where each compaction happened.
//!
//! ## The budget
//!
//! A request is measured as the `input_tokens` the provider reported for the
//! previous one, plus a calibrated estimate of what has changed since
//! ([`ContextState::measure`]). At [`COMPACT_PERCENT`] of the budget the loop
//! compacts before the next model call:
//!
//! - **Pass 1** replaces every old tool result with the stub its trait writes
//!   through [`AgentTrait::elide`], all in one batch, so the cache breaks once
//!   rather than on every step.
//! - **Pass 2**, only when pass 1 left the request at or above
//!   [`SUMMARY_PERCENT`], has the cheap role summarise everything before the
//!   last K turns ([`ATTR_KEEP_TURNS`]).
//!
//! A request still over the whole budget after that ends the run as
//! [`Budget::Context`](crate::Budget::Context).
//!
//! [`AgentTrait::session_header`]: crate::AgentTrait::session_header
//! [`AgentTrait::elide`]: crate::AgentTrait::elide

use std::collections::BTreeMap;

use sc_llm::{CachePlan, LlmMessage, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::agent::Agent;

/// The agent attribute naming how many of the latest turns compaction leaves
/// untouched. A turn is one assistant message and the tool results answering it.
pub const ATTR_KEEP_TURNS: &str = "keep_turns";
/// Turns left untouched when the agent does not say.
pub const DEFAULT_KEEP_TURNS: usize = 3;
/// The share of the context budget, in percent, at which the loop compacts:
/// late enough that a long task keeps its working context (200k tokens of the
/// largest built-in budget), early enough to leave room for the next result.
pub const COMPACT_PERCENT: u64 = 80;
/// The share of the context budget, in percent, that clearing must get below
/// for the summary to be skipped.
pub const SUMMARY_PERCENT: u64 = 50;
/// The context budget for a model that says nothing about its own: the
/// working budget the capability rules give a model they do not recognise.
pub const FALLBACK_CONTEXT_BUDGET: u64 = sc_llm::UNKNOWN_WORKING_BUDGET;

/// How far calibration may move the estimate, as [`sc_llm::TokenEstimator`]
/// bounds it.
const MIN_FACTOR: f64 = 0.25;
const MAX_FACTOR: f64 = 4.0;

/// The most characters of one message the summariser is shown. Pass 1 has
/// usually stubbed the long ones already.
const SUMMARY_SOURCE_CHARS: usize = 4_000;

/// What opens the message a summary is sent as.
pub const SUMMARY_HEADING: &str = "[Summary of the conversation before this point. The full \
                                   transcript is kept, but only this summary is in your context.]";

/// The system prompt the cheap role summarises under. The sections are fixed so
/// that a summary of a summary keeps the same shape.
pub const SUMMARY_PROMPT: &str = "You compact the context of a software agent. You are given \
the start of its conversation, and possibly an earlier summary. Write a summary that lets the \
agent carry on without the original. Use exactly these Markdown sections, in this order, and \
write `none` under a section with nothing in it:\n\n\
## Goal\nWhat the person asked for, in their terms, with every constraint they gave.\n\n\
## Decisions\nWhat was decided and why, including approaches ruled out.\n\n\
## Files changed\nEach file created, edited or deleted, with one line on what changed.\n\n\
## Failing checks\nChecks, tests or errors still failing, with the exact message.\n\n\
## Next step\nWhat the agent was about to do.\n\n\
Be concrete: names, paths, identifiers and numbers. Do not invent anything that is not in the \
conversation.";

/// One compaction, as the run stores it and the chat marks it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// The model call it was made before, counting from 1.
    pub step: u32,
    /// The length of the transcript when it happened: where the chat draws its
    /// marker.
    pub at: usize,
    /// How many tool results pass 1 cleared.
    pub elided: usize,
    /// The measured size of the request before, in tokens.
    pub before_tokens: u64,
    /// The measured size after.
    pub after_tokens: u64,
    /// The first transcript index the request still carries after a summary:
    /// everything before it is replaced by `summary`. `None` when pass 1 was
    /// enough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_to_index: Option<usize>,
    /// The summary pass 2 wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// The last request's size: what was estimated and what the provider reported.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Measured {
    /// The raw estimate, before calibration.
    raw: u64,
    /// The `input_tokens` the provider reported.
    reported: u64,
}

/// What the loop should do about the context before the next model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextVerdict {
    /// Send the request as it is.
    Fits,
    /// Compact first.
    Compact,
    /// Compaction has been tried for this call and the request is still over
    /// the budget: the run ends.
    Over,
}

/// One old tool result that pass 1 may clear, as a trait's
/// [`elide`](crate::AgentTrait::elide) hook sees it.
#[derive(Debug, Clone, Copy)]
pub struct Elidable<'a> {
    /// Its index in the transcript.
    pub index: usize,
    /// The call it answers.
    pub call: &'a ToolCall,
    /// What the tool returned.
    pub content: &'a str,
    /// How many images came with it.
    pub images: usize,
    /// The whole transcript, so a trait can see what came after — a later read
    /// of the same file, an edit that made this read stale.
    pub transcript: &'a [LlmMessage],
    /// The owning trait's per-run state, where it has any.
    pub state: Option<&'a Json>,
}

impl Elidable<'_> {
    /// The stub every trait gets unless it writes its own:
    /// `[elided: N characters of <tool> output]`.
    pub fn default_stub(&self) -> String {
        let chars = self.content.chars().count();
        match self.images {
            0 => format!("[elided: {chars} characters of {} output]", self.call.name),
            1 => format!(
                "[elided: {chars} characters and 1 image of {} output]",
                self.call.name
            ),
            n => format!(
                "[elided: {chars} characters and {n} images of {} output]",
                self.call.name
            ),
        }
    }
}

/// The context overlays and accounting one run keeps (TODO §9). Part of
/// [`AgentLoop`](crate::AgentLoop), so it is saved and resumed with the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextState {
    /// The session header: `None` until the traits have been asked, and the
    /// empty string when they had nothing to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    header: Option<String>,
    /// Pass 1's stubs, by transcript index.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    elided: BTreeMap<usize, String>,
    /// Every compaction, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    compactions: Vec<Compaction>,
    /// The last request's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    measured: Option<Measured>,
    /// Reported tokens per estimated token.
    #[serde(default = "one")]
    factor: f64,
    /// Turns compaction leaves untouched.
    #[serde(default = "default_keep_turns")]
    keep_turns: usize,
    /// Whether a compaction happened since the last model call, for that call's
    /// ledger entry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pending: bool,
}

fn one() -> f64 {
    1.0
}

fn default_keep_turns() -> usize {
    DEFAULT_KEEP_TURNS
}

impl Default for ContextState {
    fn default() -> Self {
        ContextState {
            header: None,
            elided: BTreeMap::new(),
            compactions: Vec::new(),
            measured: None,
            factor: 1.0,
            keep_turns: DEFAULT_KEEP_TURNS,
            pending: false,
        }
    }
}

impl ContextState {
    /// The context state for a new run of `agent`.
    pub fn for_agent(agent: &Agent) -> ContextState {
        ContextState {
            keep_turns: agent
                .attributes
                .get(ATTR_KEEP_TURNS)
                .and_then(Json::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n > 0)
                .unwrap_or(DEFAULT_KEEP_TURNS),
            ..ContextState::default()
        }
    }

    /// The session header, once built.
    pub fn header(&self) -> Option<&str> {
        self.header.as_deref()
    }

    /// Store the session header. Called once per session.
    pub fn set_header(&mut self, header: String) {
        self.header = Some(header);
    }

    /// Every compaction, oldest first.
    pub fn compactions(&self) -> &[Compaction] {
        &self.compactions
    }

    /// Pass 1's stubs, by transcript index.
    pub fn elided(&self) -> &BTreeMap<usize, String> {
        &self.elided
    }

    /// Turns compaction leaves untouched.
    pub fn keep_turns(&self) -> usize {
        self.keep_turns
    }

    /// The first transcript index the request carries: after the latest
    /// summary, or the start.
    fn start(&self) -> usize {
        self.compactions
            .iter()
            .rev()
            .find_map(|c| c.up_to_index)
            .unwrap_or(0)
    }

    /// The latest summary, with the index it runs up to.
    fn summary(&self) -> Option<(usize, &str)> {
        self.compactions
            .iter()
            .rev()
            .find_map(|c| Some((c.up_to_index?, c.summary.as_deref()?)))
    }

    /// The messages a request carries, from `transcript` and the overlays, and
    /// the cache plan for that layout.
    ///
    /// `summary` stands in for the latest summary, so a compaction can be
    /// measured before it is recorded.
    pub fn request_messages(
        &self,
        transcript: &[LlmMessage],
        summary: Option<(usize, &str)>,
    ) -> (Vec<LlmMessage>, CachePlan) {
        let mut messages = Vec::new();
        let header = self.header.as_deref().filter(|h| !h.trim().is_empty());
        if let Some(header) = header {
            messages.push(LlmMessage::user(header));
        }
        let summary = summary.or_else(|| self.summary());
        let start = match summary {
            Some((up_to, text)) => {
                messages.push(LlmMessage::user(format!("{SUMMARY_HEADING}\n\n{text}")));
                up_to.min(transcript.len())
            }
            None => 0,
        };
        for (index, message) in transcript.iter().enumerate().skip(start) {
            messages.push(match (message, self.elided.get(&index)) {
                (
                    LlmMessage::ToolResult {
                        tool_call_id, name, ..
                    },
                    Some(stub),
                ) => LlmMessage::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                    content: stub.clone(),
                    images: Vec::new(),
                },
                _ => message.clone(),
            });
        }
        let plan = CachePlan::standard(header.map(|_| 0));
        (messages, plan)
    }

    /// The transcript index of the oldest turn compaction leaves untouched:
    /// the `keep_turns`-th assistant message from the end, or `None` when there
    /// are no more turns than that.
    ///
    /// Always an assistant message, so a cut here never separates a tool call
    /// from its result: results follow their call.
    fn recent_cut(&self, transcript: &[LlmMessage]) -> Option<usize> {
        let mut turns = transcript
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, m)| matches!(m, LlmMessage::Assistant { .. }))
            .map(|(index, _)| index);
        let cut = turns.nth(self.keep_turns.max(1) - 1)?;
        // Only when there is an older turn to compact.
        turns.next().map(|_| cut)
    }

    /// The old tool results pass 1 may clear: those before the kept turns, not
    /// already summarised away and not already cleared, each with its call.
    ///
    /// **Images go first** (TODO §7b): a result carrying images is old as soon
    /// as a later result carries one, kept turns or not, because a screenshot
    /// is the most expensive thing in the context and the latest one is the
    /// one that shows the page as it is.
    pub fn elidable<'a>(
        &self,
        transcript: &'a [LlmMessage],
    ) -> Vec<(usize, &'a ToolCall, &'a str, usize)> {
        let last_image = transcript.iter().rposition(
            |m| matches!(m, LlmMessage::ToolResult { images, .. } if !images.is_empty()),
        );
        let cut = match (self.recent_cut(transcript), last_image) {
            (Some(cut), _) => cut,
            (None, Some(_)) => 0,
            (None, None) => return Vec::new(),
        };
        let start = self.start();
        let mut calls: BTreeMap<&str, &ToolCall> = BTreeMap::new();
        let mut out = Vec::new();
        for (index, message) in transcript.iter().enumerate() {
            let old = index < cut
                || matches!(message, LlmMessage::ToolResult { images, .. }
                    if !images.is_empty() && last_image.is_some_and(|last| index < last));
            if !old && !matches!(message, LlmMessage::Assistant { .. }) {
                continue;
            }
            match message {
                LlmMessage::Assistant { tool_calls, .. } => {
                    for call in tool_calls {
                        calls.insert(call.id.as_str(), call);
                    }
                }
                LlmMessage::ToolResult {
                    tool_call_id,
                    content,
                    images,
                    ..
                } if index >= start && !self.elided.contains_key(&index) => {
                    if let Some(call) = calls.get(tool_call_id.as_str()) {
                        out.push((index, *call, content.as_str(), images.len()));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Record pass 1's stubs, in one batch. A stub no shorter than what it
    /// replaces is dropped. Returns how many were kept.
    pub fn apply_elisions(
        &mut self,
        transcript: &[LlmMessage],
        stubs: Vec<(usize, String)>,
    ) -> usize {
        let mut kept = 0;
        for (index, stub) in stubs {
            let Some(LlmMessage::ToolResult {
                content, images, ..
            }) = transcript.get(index)
            else {
                continue;
            };
            if images.is_empty() && stub.len() >= content.len() {
                continue;
            }
            self.elided.insert(index, stub);
            kept += 1;
        }
        kept
    }

    /// Where pass 2 would cut: the kept turns' start, when that is past the
    /// latest summary.
    pub fn summary_cut(&self, transcript: &[LlmMessage]) -> Option<usize> {
        self.recent_cut(transcript)
            .filter(|cut| *cut > self.start())
    }

    /// What the cheap role is asked to summarise, up to `cut`: the latest
    /// summary, then every message since it, as plain text with the stubs
    /// applied.
    pub fn summary_source(&self, transcript: &[LlmMessage], cut: usize) -> String {
        let mut out = String::new();
        if let Some((_, summary)) = self.summary() {
            out.push_str("# Earlier summary\n\n");
            out.push_str(summary);
            out.push_str("\n\n# The conversation since\n\n");
        } else {
            out.push_str("# The conversation\n\n");
        }
        for (index, message) in transcript.iter().enumerate().take(cut).skip(self.start()) {
            match message {
                LlmMessage::User {
                    content,
                    attachments,
                } => {
                    out.push_str("## User\n\n");
                    if !attachments.is_empty() {
                        let names: Vec<&str> =
                            attachments.iter().map(|a| a.name.as_str()).collect();
                        out.push_str(&format!("(attached: {})\n\n", names.join(", ")));
                    }
                    out.push_str(&clip(content));
                }
                LlmMessage::Assistant {
                    content,
                    tool_calls,
                    ..
                } => {
                    out.push_str("## Assistant\n\n");
                    if !content.is_empty() {
                        out.push_str(&clip(content));
                        out.push('\n');
                    }
                    for call in tool_calls {
                        out.push_str(&format!(
                            "- calls `{}` with {}\n",
                            call.name,
                            clip(&call.arguments.to_string())
                        ));
                    }
                }
                LlmMessage::ToolResult { name, content, .. } => {
                    out.push_str(&format!("## Result of `{name}`\n\n"));
                    out.push_str(&clip(self.elided.get(&index).unwrap_or(content)));
                }
            }
            out.push_str("\n\n");
        }
        out
    }

    /// Record a compaction.
    pub fn record(&mut self, compaction: Compaction) {
        self.compactions.push(compaction);
        self.pending = true;
    }

    /// Whether a compaction was already made before model call `step`.
    pub fn compacted_before(&self, step: u32) -> bool {
        self.compactions.last().is_some_and(|c| c.step == step)
    }

    /// Take the flag saying a compaction happened since the last model call.
    pub fn take_pending(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }

    /// The measured size of a request whose raw estimate is `raw`: what the
    /// provider reported for the last request, plus the calibrated estimate of
    /// the difference. Before any report, the calibrated estimate alone.
    pub fn measure(&self, raw: u64) -> u64 {
        match self.measured {
            Some(Measured {
                raw: last,
                reported,
            }) => {
                let delta = (raw as f64 - last as f64) * self.factor;
                (reported as f64 + delta).max(0.0).ceil() as u64
            }
            None => (raw as f64 * self.factor).ceil() as u64,
        }
    }

    /// Calibrate against a request whose raw estimate was `raw` and for which
    /// the provider reported `reported` input tokens. A zero on either side
    /// means nothing is known, and changes nothing.
    pub fn calibrate(&mut self, raw: u64, reported: u64) {
        if raw == 0 || reported == 0 {
            return;
        }
        self.factor = (reported as f64 / raw as f64).clamp(MIN_FACTOR, MAX_FACTOR);
        self.measured = Some(Measured { raw, reported });
    }

    /// What to do before model call `step`, whose request measures `used`
    /// against `budget`.
    pub fn verdict(&self, used: u64, budget: u64, step: u32) -> ContextVerdict {
        if used.saturating_mul(100) < budget.saturating_mul(COMPACT_PERCENT) {
            ContextVerdict::Fits
        } else if !self.compacted_before(step) {
            ContextVerdict::Compact
        } else if used >= budget {
            ContextVerdict::Over
        } else {
            ContextVerdict::Fits
        }
    }
}

/// `text`, cut to what the summariser is shown.
fn clip(text: &str) -> String {
    match text.char_indices().nth(SUMMARY_SOURCE_CHARS) {
        Some((cut, _)) => format!(
            "{}… [{} more characters]",
            &text[..cut],
            text[cut..].chars().count()
        ),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: json!({"path": id}),
        }
    }

    /// A user message, then `rounds` turns of one call and a long result each.
    fn transcript(rounds: usize) -> Vec<LlmMessage> {
        let mut messages = vec![LlmMessage::user("fix the bug")];
        for n in 0..rounds {
            let c = call(&format!("c{n}"), "read_file");
            messages.push(LlmMessage::assistant_with_calls("", vec![c.clone()]));
            messages.push(LlmMessage::tool_result(&c, "x".repeat(1000)));
        }
        messages
    }

    #[test]
    fn the_request_is_the_header_then_the_history() {
        let mut cx = ContextState::default();
        let t = transcript(1);
        let (messages, plan) = cx.request_messages(&t, None);
        assert_eq!(messages, t);
        assert_eq!(plan, CachePlan::standard(None));

        cx.set_header("# AGENTS.md".to_owned());
        let (messages, plan) = cx.request_messages(&t, None);
        assert_eq!(messages[0], LlmMessage::user("# AGENTS.md"));
        assert_eq!(&messages[1..], &t[..]);
        assert_eq!(plan.session_header, Some(0));

        // A header that came back empty was built, and adds no message.
        cx.set_header(String::new());
        assert_eq!(cx.request_messages(&t, None).0, t);
        assert_eq!(cx.header(), Some(""));
    }

    #[test]
    fn the_kept_turns_start_at_an_assistant_message() {
        let cx = ContextState::default();
        // Three turns or fewer: nothing is old.
        assert_eq!(cx.recent_cut(&transcript(3)), None);
        // Five turns: user, then turns at 1, 3, 5, 7, 9. The last three start at 5.
        let t = transcript(5);
        assert_eq!(cx.recent_cut(&t), Some(5));
        let old: Vec<usize> = cx.elidable(&t).iter().map(|e| e.0).collect();
        assert_eq!(old, vec![2, 4]);
    }

    #[test]
    fn an_older_screenshot_is_elidable_even_in_the_kept_turns() {
        let cx = ContextState::default();
        let mut t = transcript(1);
        for n in 0..2 {
            let c = call(&format!("s{n}"), "view_app");
            t.push(LlmMessage::assistant_with_calls("", vec![c.clone()]));
            let mut result = LlmMessage::tool_result(&c, "shot");
            if let LlmMessage::ToolResult { images, .. } = &mut result {
                images.push(sc_llm::ImagePart::new("image/jpeg", vec![0xff, 0xd8]));
            }
            t.push(result);
        }
        // Three turns, all kept; the first screenshot is old, the latest is not,
        // and the text result is not.
        assert_eq!(cx.recent_cut(&t), None);
        let old: Vec<(usize, usize)> = cx.elidable(&t).iter().map(|e| (e.0, e.3)).collect();
        assert_eq!(old, vec![(4, 1)]);
    }

    #[test]
    fn clearing_keeps_every_call_and_its_result_and_the_transcript_whole() {
        let mut cx = ContextState::default();
        let t = transcript(5);
        let stubs = cx
            .elidable(&t)
            .into_iter()
            .map(|(i, c, content, images)| {
                let e = Elidable {
                    index: i,
                    call: c,
                    content,
                    images,
                    transcript: &t,
                    state: None,
                };
                (i, e.default_stub())
            })
            .collect();
        assert_eq!(cx.apply_elisions(&t, stubs), 2);
        let (messages, _) = cx.request_messages(&t, None);
        assert_eq!(messages.len(), t.len());
        let LlmMessage::ToolResult { content, .. } = &messages[2] else {
            panic!("a result stays a result");
        };
        assert_eq!(content, "[elided: 1000 characters of read_file output]");
        // The kept turns are whole.
        assert_eq!(messages[6], t[6]);
        // Nothing is offered twice.
        assert!(cx.elidable(&t).is_empty());
    }

    #[test]
    fn a_summary_replaces_everything_before_the_kept_turns() {
        let mut cx = ContextState::default();
        cx.set_header("header".to_owned());
        let t = transcript(5);
        let cut = cx.summary_cut(&t).unwrap();
        assert_eq!(cut, 5);
        let source = cx.summary_source(&t, cut);
        assert!(source.contains("fix the bug") && source.contains("read_file"));
        cx.record(Compaction {
            step: 6,
            at: t.len(),
            elided: 0,
            before_tokens: 100,
            after_tokens: 50,
            up_to_index: Some(cut),
            summary: Some("## Goal\nfix the bug".to_owned()),
        });
        let (messages, plan) = cx.request_messages(&t, None);
        assert_eq!(messages[0], LlmMessage::user("header"));
        let LlmMessage::User { content, .. } = &messages[1] else {
            panic!("the summary is a user message");
        };
        assert!(content.starts_with(SUMMARY_HEADING) && content.ends_with("fix the bug"));
        assert_eq!(&messages[2..], &t[5..]);
        assert_eq!(plan.session_header, Some(0));
        assert!(cx.take_pending() && !cx.take_pending());
        // Nothing new to summarise until more turns arrive.
        assert_eq!(cx.summary_cut(&t), None);
        assert!(cx.elidable(&t).is_empty());
    }

    #[test]
    fn measuring_adds_the_calibrated_difference_to_the_last_report() {
        let mut cx = ContextState::default();
        assert_eq!(cx.measure(1000), 1000);
        cx.calibrate(1000, 1500);
        // 1500 reported, plus 200 raw tokens more at 1.5 each.
        assert_eq!(cx.measure(1200), 1800);
        // A smaller request after clearing measures smaller.
        assert_eq!(cx.measure(600), 900);
        // A provider that reports nothing changes nothing.
        cx.calibrate(1000, 0);
        assert_eq!(cx.measure(1200), 1800);
    }

    #[test]
    fn the_verdict_compacts_at_four_fifths_and_ends_the_run_only_after_trying() {
        let mut cx = ContextState::default();
        assert_eq!(cx.verdict(799, 1000, 3), ContextVerdict::Fits);
        assert_eq!(cx.verdict(800, 1000, 3), ContextVerdict::Compact);
        // The largest built-in budget compacts at 200k tokens.
        assert_eq!(cx.verdict(199_999, 250_000, 3), ContextVerdict::Fits);
        assert_eq!(cx.verdict(200_000, 250_000, 3), ContextVerdict::Compact);
        cx.record(Compaction {
            step: 3,
            at: 0,
            elided: 0,
            before_tokens: 1200,
            after_tokens: 1100,
            up_to_index: None,
            summary: None,
        });
        assert_eq!(cx.verdict(800, 1000, 3), ContextVerdict::Fits);
        assert_eq!(cx.verdict(1100, 1000, 3), ContextVerdict::Over);
        assert_eq!(cx.verdict(1100, 1000, 4), ContextVerdict::Compact);
    }

    #[test]
    fn the_state_round_trips_through_json() {
        let mut cx = ContextState::default();
        cx.set_header("h".to_owned());
        cx.elided.insert(4, "stub".to_owned());
        cx.calibrate(10, 20);
        let back: ContextState =
            serde_json::from_value(serde_json::to_value(&cx).unwrap()).unwrap();
        assert_eq!(back, cx);
        let empty: ContextState = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, ContextState::default());
    }
}

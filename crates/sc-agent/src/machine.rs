//! The loop, as a **steppable machine** rather than an `async fn` (§11.2,
//! decision 8).
//!
//! [`AgentLoop`] owns every *decision* the loop makes — what to send, when to run
//! tools, when to stop — and performs no IO at all. A driver advances it by
//! asking [`next_step`](AgentLoop::next_step) what to do and feeding the result
//! back through [`model_answered`](AgentLoop::model_answered) or
//! [`tool_results`](AgentLoop::tool_results):
//!
//! ```text
//! next_step() ──▶ CallModel { messages }  ──▶ model_answered(assistant)
//!             ──▶ CallTools { calls }     ──▶ tool_results(outcomes)
//!             ──▶ Done(conclusion)
//! ```
//!
//! ## Why this shape
//!
//! Because the whole state is a value, it is `Serialize + Deserialize`, and *the
//! state is what `_fd_runs` stores*. A run persisted after every step is
//! therefore a run that resumes: load it, ask for the next step, carry on. An
//! `async fn` holding the same state on its stack could do the work but not the
//! resuming, and §10.3's durable engine would then need a second mechanism to do
//! what this one already does. The shape is borrowed from `rig-agent`'s sans-IO
//! `AgentRun`; the crate is not (§11.1 records why).
//!
//! It also makes the loop testable without a provider, a database or a runtime:
//! every test in this module is synchronous, and the ones that need a model use
//! the [`FakeProvider`](crate::testing::FakeProvider) through the driver.
//!
//! ## What it deliberately does not decide
//!
//! The system prompt and the tool list are **not** in the state. They are rebuilt
//! from the agent's definition on every turn, because an agent is editable while
//! its runs exist: a conversation resumed after a trait was added must get the
//! new tool, not the one that was current when it started.

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use sc_llm::{AssistantMessage, CachePlan, LlmMessage, ToolCall, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::agent::{Agent, DEFAULT_MAX_STEPS, ModelRole};
use crate::context::{Compaction, ContextState, ContextVerdict, FALLBACK_CONTEXT_BUDGET};
use crate::control::{ControlLimits, LoopControl, RoundCall, Signal, Verdict, fingerprint};
use crate::interact::{DisplayRef, UserRequest};
use crate::ledger::{Ledger, LedgerStep};

/// What the driver must do next to advance an [`AgentLoop`].
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Send these messages to the model and feed the answer back through
    /// [`model_answered`](AgentLoop::model_answered).
    CallModel {
        /// What the request carries after the system prompt and the tools: the
        /// session header, then the history with its clearing and compaction
        /// applied (TODO §9). The system prompt and the tools are the driver's
        /// to add — they come from the agent, not the run.
        messages: Vec<LlmMessage>,
        /// The cache breakpoints for that layout.
        cache: CachePlan,
        /// Which model call this will be, counting from 1. What a trait's
        /// [`on_turn`](crate::AgentTrait::on_turn) is told, and what the step
        /// budget is spent from.
        step: u32,
        /// Whether the escalation ladder hands this one call to the **strong**
        /// role, whatever role the run is answered by otherwise (TODO §10).
        escalated: bool,
    },
    /// Run these tool calls **in this order** and feed the outcomes back through
    /// [`tool_results`](AgentLoop::tool_results).
    ///
    /// Sequentially, because a trait that writes a row and a trait that reads one
    /// have an order between them that only the model knows (§11.2). Running them
    /// concurrently is carried past this milestone precisely because deciding
    /// which are independent is a separate question.
    CallTools {
        /// The calls, in the order the model asked for them.
        calls: Vec<ToolCall>,
    },
    /// The run is over; nothing more will be asked of the driver.
    Done(Conclusion),
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "conclusion", rename_all = "snake_case")]
pub enum Conclusion {
    /// The model finished its turn: this is its answer, and the conversation can
    /// be continued with another [`push_user`](AgentLoop::push_user).
    Answered {
        /// The final assistant text. Empty is a real answer — a model that ends a
        /// turn having only called tools has said nothing, and pretending
        /// otherwise would put words in its mouth.
        answer: String,
    },
    /// The step budget ran out with the model still asking for tools.
    ///
    /// Not an error: the transcript is intact and readable, and the number that
    /// stopped it is the admin's own ([`max_steps`](crate::Agent::max_steps)).
    MaxSteps,
    /// Someone pressed stop.
    Aborted,
    /// A budget other than the step budget ran out (TODO §10).
    ///
    /// Not an error, for the reason [`MaxSteps`](Conclusion::MaxSteps) is not:
    /// the transcript is intact and the limit is the admin's.
    OverBudget {
        /// Which budget.
        budget: Budget,
    },
    /// Loop control stopped the run: the model kept repeating itself after a
    /// warning and an escalation, or kept making malformed calls (TODO §10).
    ///
    /// Not an error either — the transcript is intact and can be continued —
    /// but for a run nobody is watching it is a failure to report, which is
    /// what `run_agent` does.
    Stuck {
        /// What the detectors saw, for the admin.
        reason: String,
    },
    /// A tool asked the person something, and the run waits for the answer
    /// (TODO-agent-skills AS6.1). Continued by an answer naming the request, or
    /// by an ordinary message, which leaves the question unanswered.
    AwaitingUser {
        /// What was asked.
        request: UserRequest,
    },
}

/// Which budget ended a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Budget {
    /// `max_cost`: what the run and its children have spent.
    Cost,
    /// `max_wall_seconds`: the time the run has spent working.
    WallTime,
    /// `context_budget`: the size of the context the last request carried.
    Context,
    /// The caller's LLM credits (TODO-payments P6.3): their balance was at or
    /// below zero, or below the agent's minimum to start, before a model call.
    Credits,
}

impl Budget {
    /// The stored spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Budget::Cost => "cost",
            Budget::WallTime => "wall_time",
            Budget::Context => "context",
            Budget::Credits => "credits",
        }
    }
}

impl std::fmt::Display for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The budgets a run is kept within, besides its steps. Each is optional:
/// absent is unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Budgets {
    /// The most the run (with its children) may cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost: Option<f64>,
    /// The most time the run may spend working, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_ms: Option<u64>,
    /// The most input tokens one request may carry. Unset means the executor
    /// model's working budget, which the driver knows and the run does not.
    ///
    /// The loop compacts at 75% of it, and ends the run only when a request is
    /// still over it after compacting (TODO §9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// The most images the transcript keeps; older ones become stubs (TODO
    /// §7b). Unset is [`DEFAULT_MAX_IMAGES`](crate::agent::DEFAULT_MAX_IMAGES).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_images: Option<usize>,
}

impl Budgets {
    /// The budgets `agent` sets.
    pub fn of(agent: &Agent) -> Budgets {
        Budgets {
            max_cost: agent.max_cost(),
            max_wall_ms: agent.max_wall_seconds().map(|s| s.saturating_mul(1000)),
            context_tokens: agent.context_budget(),
            max_images: Some(agent.max_images()),
        }
    }
}

/// What an image dropped from a run's transcript is replaced by.
pub const IMAGE_STUB: &str = "[screenshot removed: the run keeps only its latest ones]";

/// What the driver knows about a model call that the answer does not carry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StepMeta {
    /// The role whose model answered.
    pub role: ModelRole,
    /// That model's name.
    pub model: String,
    /// What the call cost, or `None` when the model has no price.
    pub cost: Option<f64>,
    /// How long the call took.
    pub elapsed: std::time::Duration,
    /// The raw token estimate of the request that was sent, which the reported
    /// `input_tokens` calibrate (TODO §9). `None` when nobody estimated.
    pub request_tokens: Option<u64>,
}

impl Conclusion {
    /// The final answer, for a run that produced one.
    pub fn answer(&self) -> Option<&str> {
        match self {
            Conclusion::Answered { answer } => Some(answer),
            Conclusion::MaxSteps
            | Conclusion::Aborted
            | Conclusion::OverBudget { .. }
            | Conclusion::Stuck { .. }
            | Conclusion::AwaitingUser { .. } => None,
        }
    }
}

/// One tool call's outcome, on its way back into the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    /// The call this answers. Carried whole rather than as an id, so the loop can
    /// check that what came back is what it asked for.
    pub call: ToolCall,
    /// What the model is told, whether the tool succeeded or failed.
    pub content: String,
    /// Whether this is a failure — for the transcript, which renders a failed
    /// tool differently, and for nothing else: the model sees only `content`.
    pub is_error: bool,
    /// Whether the call itself was malformed: an unknown tool, or arguments
    /// that did not parse or did not match the schema. Counted by the
    /// malformed-call cap.
    pub malformed: bool,
    /// The call's fingerprint as its trait computes it, or `None` for the
    /// default: the tool and its canonical arguments.
    pub fingerprint: Option<String>,
    /// The signals the call's trait raised.
    pub signals: Vec<Signal>,
    /// Images the result carries beside its text (TODO §7b).
    pub images: Vec<sc_llm::ImagePart>,
    /// What the call showed the person, stored (AS6.3). Never in a request.
    pub display: Vec<DisplayRef>,
    /// What the call asked the person, which ends the turn (AS6.1).
    pub awaiting: Option<UserRequest>,
}

impl ToolOutcome {
    /// A tool that returned `value`.
    ///
    /// A JSON string is passed through unquoted; anything else is serialised
    /// compactly. Quoting a plain string would spend tokens on punctuation and
    /// make the model read escaping as content.
    pub fn ok(call: ToolCall, value: &Json) -> ToolOutcome {
        let content = match value {
            Json::String(s) => s.clone(),
            other => other.to_string(),
        };
        ToolOutcome {
            call,
            content,
            is_error: false,
            malformed: false,
            fingerprint: None,
            signals: Vec::new(),
            images: Vec::new(),
            display: Vec::new(),
            awaiting: None,
        }
    }

    /// A tool that failed, whose error becomes its result (§11.2).
    ///
    /// The prefix is there because the model has no other signal: a tool result
    /// is a string, and "no table named `bookz`" reads as data unless something
    /// says it is a failure. An error the model can read is one it can recover
    /// from — which is the whole reason a failing tool does not end the run.
    pub fn failed(call: ToolCall, error: impl std::fmt::Display) -> ToolOutcome {
        ToolOutcome {
            call,
            content: format!("error: {error}"),
            is_error: true,
            malformed: false,
            fingerprint: None,
            signals: Vec::new(),
            images: Vec::new(),
            display: Vec::new(),
            awaiting: None,
        }
    }

    /// A call the harness refused before any trait saw it: an unknown tool, or
    /// arguments that did not parse or did not match the schema.
    pub fn malformed(call: ToolCall, error: impl std::fmt::Display) -> ToolOutcome {
        ToolOutcome {
            malformed: true,
            ..ToolOutcome::failed(call, error)
        }
    }

    /// Set the fingerprint, returning `self` for chaining.
    pub fn with_fingerprint(mut self, fingerprint: String) -> ToolOutcome {
        self.fingerprint = Some(fingerprint);
        self
    }

    /// Attach the images the trait produced, returning `self` for chaining.
    pub fn with_images(mut self, images: Vec<sc_llm::ImagePart>) -> ToolOutcome {
        self.images.extend(images);
        self
    }

    /// Attach the references to what the call showed the person, returning
    /// `self` for chaining.
    pub fn with_display(mut self, display: Vec<DisplayRef>) -> ToolOutcome {
        self.display.extend(display);
        self
    }

    /// Attach what the call asked the person, returning `self` for chaining.
    pub fn with_awaiting(mut self, request: Option<UserRequest>) -> ToolOutcome {
        self.awaiting = request;
        self
    }

    /// Add the signals the trait raised, returning `self` for chaining.
    pub fn with_signals(mut self, signals: Vec<Signal>) -> ToolOutcome {
        self.signals.extend(signals);
        self
    }
}

/// Which side the machine is waiting on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    /// The model is next.
    Model,
    /// These tool calls are next.
    Tools { calls: Vec<ToolCall> },
    /// Nothing is next.
    Done { conclusion: Conclusion },
}

/// The conversation, the budget, and which side is next: everything one run is.
///
/// `Serialize + Deserialize`, and that is not incidental — this value *is*
/// `_fd_runs.context`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentLoop {
    /// The conversation so far, oldest first.
    messages: Vec<LlmMessage>,
    /// Model calls made so far.
    step: u32,
    /// The budget.
    max_steps: u32,
    /// Every call's usage, accumulated.
    usage: Usage,
    /// The budgets besides the step budget.
    #[serde(default)]
    budgets: Budgets,
    /// What each step cost and who answered it.
    #[serde(default)]
    ledger: Ledger,
    /// Per-run state, one JSON value per enabled trait instance, keyed by
    /// [`trait_state_key`]. What a trait keeps across its tool calls — a plan, the
    /// hashes of the files it has read — lives here, so it is saved with the run
    /// and restored on resume.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    trait_state: BTreeMap<String, Json>,
    /// The doom-loop detectors, the malformed-call count and the escalation
    /// ladder.
    #[serde(default)]
    control: LoopControl,
    /// The session header, clearing and compaction overlays, and the context
    /// accounting (TODO §9). The transcript in `messages` stays whole.
    #[serde(default)]
    context: ContextState,
    /// What each tool call showed the person, by tool call id (AS6.3): kept
    /// beside the transcript, so it never reaches a request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    display: BTreeMap<String, Vec<DisplayRef>>,
    /// The composer controls sent with the message that started this turn
    /// (AS6.4).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    controls: BTreeMap<String, Json>,
    /// Which side is next.
    phase: Phase,
}

/// The key one enabled trait instance's state is kept under: its position in
/// the agent's trait list and its name. The name is part of it so that a list
/// reordered by the admin hands no trait another's state.
pub fn trait_state_key(index: usize, trait_name: &str) -> String {
    format!("{index}:{trait_name}")
}

/// The key what a final answer showed the person is kept under in
/// [`AgentLoop::display`], beside the tool calls' ids: `answer:` and the
/// answer's position in the transcript (TODO-agent-skills AS11.2).
pub fn answer_display_key(message_index: usize) -> String {
    format!("answer:{message_index}")
}

impl Default for AgentLoop {
    fn default() -> Self {
        AgentLoop::new(DEFAULT_MAX_STEPS)
    }
}

impl AgentLoop {
    /// An empty conversation with a step budget.
    ///
    /// A budget of zero would make a run that can never call the model, so it is
    /// raised to one: the caller who wrote zero gets a run that answers rather
    /// than one that reports a conclusion nobody can act on.
    /// [`Agent::max_steps`](crate::Agent::max_steps) has already made the same
    /// correction; this is the second gate, for a caller that did not come
    /// through an agent.
    pub fn new(max_steps: u32) -> AgentLoop {
        AgentLoop {
            messages: Vec::new(),
            step: 0,
            max_steps: max_steps.max(1),
            usage: Usage::default(),
            budgets: Budgets::default(),
            ledger: Ledger::default(),
            trait_state: BTreeMap::new(),
            control: LoopControl::default(),
            context: ContextState::default(),
            display: BTreeMap::new(),
            controls: BTreeMap::new(),
            phase: Phase::Model,
        }
    }

    /// An empty conversation within `agent`'s budgets and loop-control limits.
    pub fn for_agent(agent: &Agent) -> AgentLoop {
        let mut state = AgentLoop::new(agent.max_steps())
            .with_budgets(Budgets::of(agent))
            .with_control_limits(ControlLimits::of(agent));
        state.context = ContextState::for_agent(agent);
        state
    }

    /// Set the step budget, returning `self` for chaining. Raised to one, as
    /// [`new`](AgentLoop::new) raises it.
    pub fn with_max_steps(mut self, max_steps: u32) -> AgentLoop {
        self.max_steps = max_steps.max(1);
        self
    }

    /// Set the loop-control thresholds, returning `self` for chaining.
    pub fn with_control_limits(mut self, limits: ControlLimits) -> AgentLoop {
        self.control = LoopControl::new(limits);
        self
    }

    /// Set the budgets besides the step budget, returning `self` for chaining.
    pub fn with_budgets(mut self, budgets: Budgets) -> AgentLoop {
        self.budgets = budgets;
        self
    }

    /// Add what the person said, starting or continuing the conversation.
    ///
    /// Refused mid-turn: a message that arrived while a tool was running would
    /// have to be spliced into a history the model is already answering, and
    /// silently appending it there is how a question gets answered before it was
    /// asked. A chat client holds the message until the turn ends.
    pub fn push_user(&mut self, text: impl Into<String>) -> Result<()> {
        self.push_user_with(text, BTreeMap::new())
    }

    /// [`push_user`](AgentLoop::push_user), with the composer controls the
    /// message was sent with (AS6.4): what [`Turn::controls`](crate::Turn::controls)
    /// holds for every step of the turn it starts.
    pub fn push_user_with(
        &mut self,
        text: impl Into<String>,
        controls: BTreeMap<String, Json>,
    ) -> Result<()> {
        // A turn boundary is the start of the run or the end of a turn. Anywhere
        // else the history is half-built — an assistant message awaiting its tool
        // results, or a question already on its way to the model.
        let at_boundary = self.messages.is_empty() || matches!(self.phase, Phase::Done { .. });
        if !at_boundary {
            return Err(Error::invalid(
                "a message cannot be added while the agent is mid-turn; \
                 wait for the turn to finish",
            ));
        }
        self.messages.push(LlmMessage::user(text));
        self.controls = controls;
        // The person has weighed in, so whatever was repeating starts again.
        self.control.reset();
        self.phase = Phase::Model;
        Ok(())
    }

    /// What the driver must do next.
    ///
    /// **Idempotent**: asking twice without answering gives the same instruction
    /// twice, which is what makes a resumed run resumable — a process that died
    /// between being told to call the model and doing it comes back and is told
    /// again.
    pub fn next_step(&mut self) -> Step {
        match &self.phase {
            Phase::Done { conclusion } => Step::Done(conclusion.clone()),
            Phase::Tools { calls } => Step::CallTools {
                calls: calls.clone(),
            },
            Phase::Model => {
                let over = if self.step >= self.max_steps {
                    Some(Conclusion::MaxSteps)
                } else {
                    self.spent_budget()
                        .map(|budget| Conclusion::OverBudget { budget })
                };
                if let Some(conclusion) = over {
                    self.phase = Phase::Done {
                        conclusion: conclusion.clone(),
                    };
                    return Step::Done(conclusion);
                }
                let (messages, cache) = self.context.request_messages(&self.messages, None);
                Step::CallModel {
                    messages,
                    cache,
                    step: self.step + 1,
                    escalated: self.control.escalating(),
                }
            }
        }
    }

    /// The first budget that has run out, if any. Checked before every model
    /// call, so a run is never stopped halfway through its tools.
    ///
    /// The context budget is not here: it is measured on the request the driver
    /// builds, and compacting comes before stopping ([`context_verdict`]).
    ///
    /// [`context_verdict`]: AgentLoop::context_verdict
    fn spent_budget(&self) -> Option<Budget> {
        if let Some(max) = self.budgets.max_cost
            && let Some(spent) = self.ledger.total().cost
            && spent >= max
        {
            return Some(Budget::Cost);
        }
        if let Some(max) = self.budgets.max_wall_ms
            && self.ledger.working_ms() >= max
        {
            return Some(Budget::WallTime);
        }
        None
    }

    /// Feed back what the model said, as the executor with nothing known about
    /// its cost or time.
    pub fn model_answered(&mut self, answer: AssistantMessage) -> Result<()> {
        self.model_answered_with(answer, StepMeta::default())
    }

    /// Feed back what the model said, and record the step in the ledger.
    ///
    /// The reasoning is not appended: it is not part of the conversation, and
    /// [`AssistantMessage::message`] is where that decision lives.
    pub fn model_answered_with(&mut self, answer: AssistantMessage, meta: StepMeta) -> Result<()> {
        if !matches!(self.phase, Phase::Model) {
            return Err(Error::msg(
                "the agent loop was given a model answer it did not ask for",
            ));
        }
        self.step += 1;
        self.control.escalation_taken();
        self.usage.add(answer.usage);
        if let Some(raw) = meta.request_tokens {
            self.context.calibrate(raw, answer.usage.input_tokens);
        }
        self.ledger.record_step(LedgerStep {
            step: self.step,
            role: meta.role,
            model: meta.model,
            usage: answer.usage,
            cost: meta.cost,
            elapsed_ms: u64::try_from(meta.elapsed.as_millis()).unwrap_or(u64::MAX),
            signals: Vec::new(),
            compacted: self.context.take_pending(),
            kind: sc_llm::ModelKind::Chat,
        });
        self.messages.push(answer.message());
        self.phase = if answer.tool_calls.is_empty() {
            Phase::Done {
                conclusion: Conclusion::Answered {
                    answer: answer.content,
                },
            }
        } else {
            Phase::Tools {
                calls: answer.tool_calls,
            }
        };
        Ok(())
    }

    /// End the turn just started with `text` as its answer, without a model
    /// call: what a trait answers a composer button with (TODO-agent-skills
    /// AS10.3's `run_js_code` button). The answer is an assistant message, so
    /// the model reads it as said on a later turn; no step is counted.
    pub fn answered_without_model(&mut self, text: impl Into<String>) -> Result<()> {
        if !matches!(self.phase, Phase::Model)
            || !matches!(self.messages.last(), Some(LlmMessage::User { .. }))
        {
            return Err(Error::msg(
                "a turn can be answered without the model only right after the person spoke",
            ));
        }
        let text = text.into();
        self.messages.push(LlmMessage::assistant(text.clone()));
        self.phase = Phase::Done {
            conclusion: Conclusion::Answered { answer: text },
        };
        Ok(())
    }

    /// Feed back what the tools returned.
    ///
    /// Every call must be answered, in the order it was asked: a result missing
    /// or out of place would leave a tool call with no matching result in the
    /// history, which both providers reject outright — better to fail here, where
    /// the driver's own mistake is visible, than at the vendor.
    pub fn tool_results(&mut self, outcomes: Vec<ToolOutcome>) -> Result<()> {
        let Phase::Tools { calls } = &self.phase else {
            return Err(Error::msg(
                "the agent loop was given tool results it did not ask for",
            ));
        };
        if outcomes.len() != calls.len()
            || outcomes
                .iter()
                .zip(calls)
                .any(|(outcome, call)| outcome.call.id != call.id)
        {
            return Err(Error::msg(format!(
                "the agent loop asked for {} tool results and got {}, or got them out of order",
                calls.len(),
                outcomes.len()
            )));
        }
        let text = match self.messages.last() {
            Some(LlmMessage::Assistant { content, .. }) => content.clone(),
            _ => String::new(),
        };
        let round: Vec<RoundCall> = outcomes
            .iter()
            .map(|o| RoundCall {
                tool: o.call.name.clone(),
                fingerprint: o
                    .fingerprint
                    .clone()
                    .unwrap_or_else(|| fingerprint(&o.call.name, &o.call.arguments)),
                malformed: o.malformed,
                signals: o.signals.clone(),
            })
            .collect();
        let verdict = self.control.observe_round(&text, &round);

        // The signals go on the step that asked for these calls.
        if let Some(step) = self.ledger.last_step_mut() {
            step.signals.extend(
                round
                    .iter()
                    .flat_map(|c| &c.signals)
                    .map(|s| s.as_str().to_owned()),
            );
        }

        let last = outcomes.len().saturating_sub(1);
        let mut awaiting = None;
        for (i, mut outcome) in outcomes.into_iter().enumerate() {
            if !outcome.display.is_empty() {
                self.display
                    .entry(outcome.call.id.clone())
                    .or_default()
                    .append(&mut outcome.display);
            }
            // The first question asked this round is the one waited on.
            if awaiting.is_none() {
                awaiting = outcome.awaiting.take();
            }
            // On the result, not the system prompt: the cached prefix survives.
            if i == last
                && let Verdict::Continue { note: Some(note) } = &verdict
            {
                outcome.content.push_str("\n\n");
                outcome.content.push_str(note);
            }
            let mut message = LlmMessage::tool_result(&outcome.call, outcome.content);
            if let LlmMessage::ToolResult { images, .. } = &mut message {
                *images = outcome.images;
            }
            self.messages.push(message);
        }
        self.cap_images();
        // A question waits for the person whatever loop control thought: they
        // are the one who can say what happens next.
        if let Some(request) = awaiting {
            self.phase = Phase::Done {
                conclusion: Conclusion::AwaitingUser { request },
            };
            return Ok(());
        }
        self.phase = match verdict {
            // Every call has its result first, so a person can continue the
            // conversation from here.
            Verdict::Stuck { reason } => Phase::Done {
                conclusion: Conclusion::Stuck { reason },
            },
            Verdict::Continue { .. } => Phase::Model,
        };
        Ok(())
    }

    /// Keep at most the budget's number of images in the transcript, dropping
    /// the oldest and saying so where each was (TODO §7b). The stored
    /// transcript is what changes: a run is not a screenshot archive.
    fn cap_images(&mut self) {
        let cap = self
            .budgets
            .max_images
            .unwrap_or(crate::agent::DEFAULT_MAX_IMAGES);
        let mut total: usize = self
            .messages
            .iter()
            .map(|m| match m {
                LlmMessage::ToolResult { images, .. } => images.len(),
                _ => 0,
            })
            .sum();
        for message in &mut self.messages {
            if total <= cap {
                break;
            }
            if let LlmMessage::ToolResult {
                images, content, ..
            } = message
                && !images.is_empty()
            {
                let drop = images.len().min(total - cap);
                images.drain(..drop);
                total -= drop;
                content.push('\n');
                content.push_str(IMAGE_STUB);
            }
        }
    }

    /// The question the run is waiting on, when it is waiting on one.
    pub fn pending_request(&self) -> Option<&UserRequest> {
        match &self.phase {
            Phase::Done {
                conclusion: Conclusion::AwaitingUser { request },
            } => Some(request),
            _ => None,
        }
    }

    /// The question `id` names, when it is the one the run is waiting on — or
    /// the refusal of an answer to anything else: a question answered, passed
    /// over by an ordinary message, or never asked (AS6.2).
    pub fn awaited(&self, id: &str) -> Result<&UserRequest> {
        match self.pending_request() {
            Some(request) if request.id == id => Ok(request),
            _ => Err(Error::invalid(
                "that question is no longer waiting for an answer; it was answered, or the \
                 conversation moved on",
            )),
        }
    }

    /// What each tool call showed the person, by tool call id.
    pub fn display(&self) -> &BTreeMap<String, Vec<DisplayRef>> {
        &self.display
    }

    /// Attach `refs` to the last message, the final answer, under
    /// [`answer_display_key`] (AS11.2).
    pub fn show_with_answer(&mut self, refs: Vec<DisplayRef>) {
        if refs.is_empty() {
            return;
        }
        let index = self.messages.len().saturating_sub(1);
        self.display
            .entry(answer_display_key(index))
            .or_default()
            .extend(refs);
    }

    /// The composer controls sent with the message that started this turn.
    pub fn controls(&self) -> &BTreeMap<String, Json> {
        &self.controls
    }

    /// Set the composer controls of the turn just started — for the first
    /// message, which a run is created with.
    pub fn set_controls(&mut self, controls: BTreeMap<String, Json>) {
        self.controls = controls;
    }

    /// Stop the run where it is.
    ///
    /// The transcript is left exactly as it stands, because the history is what
    /// the chat panel shows afterwards and truncating it would hide what the
    /// agent had already done. A run already finished is not re-concluded.
    pub fn abort(&mut self) {
        if !matches!(self.phase, Phase::Done { .. }) {
            self.phase = Phase::Done {
                conclusion: Conclusion::Aborted,
            };
        }
    }

    /// Whether the session header has yet to be built: the driver asks the
    /// traits once, before the first model call.
    pub fn needs_session_header(&self) -> bool {
        self.context.header().is_none()
    }

    /// Store the session header the traits built. Empty when none said
    /// anything, which still counts as built.
    pub fn set_session_header(&mut self, header: String) {
        self.context.set_header(header);
    }

    /// The context overlays and accounting.
    pub fn context(&self) -> &ContextState {
        &self.context
    }

    /// The context budget, in tokens: the agent's own, or `working_budget` —
    /// the executor model's — when it sets none.
    pub fn context_budget(&self, working_budget: u64) -> u64 {
        self.budgets
            .context_tokens
            .or((working_budget > 0).then_some(working_budget))
            .unwrap_or(FALLBACK_CONTEXT_BUDGET)
    }

    /// The measured size of the next request, from its raw estimate.
    pub fn measure_context(&self, raw: u64) -> u64 {
        self.context.measure(raw)
    }

    /// What to do about a next request measuring `used` tokens, against
    /// `budget`: send it, compact first, or — when compacting for this call has
    /// already been tried — end the run.
    pub fn context_verdict(&self, used: u64, budget: u64) -> ContextVerdict {
        self.context.verdict(used, budget, self.step + 1)
    }

    /// The request messages there would be with `summary` standing in for the
    /// latest summary, so a compaction can be measured before it is recorded.
    pub fn messages_with_summary(&self, summary: Option<(usize, &str)>) -> Vec<LlmMessage> {
        self.context.request_messages(&self.messages, summary).0
    }

    /// Record pass 1's stubs in one batch, returning how many were kept.
    pub fn apply_elisions(&mut self, stubs: Vec<(usize, String)>) -> usize {
        self.context.apply_elisions(&self.messages, stubs)
    }

    /// Record a compaction made before the next model call.
    pub fn record_compaction(
        &mut self,
        elided: usize,
        before_tokens: u64,
        after_tokens: u64,
        summary: Option<(usize, String)>,
    ) -> &Compaction {
        let (up_to_index, summary) = match summary {
            Some((cut, text)) => (Some(cut), Some(text)),
            None => (None, None),
        };
        self.context.record(Compaction {
            step: self.step + 1,
            at: self.messages.len(),
            elided,
            before_tokens,
            after_tokens,
            up_to_index,
            summary,
        });
        &self.context.compactions()[self.context.compactions().len() - 1]
    }

    /// End the run because its context is over budget even after compacting.
    pub fn context_over_budget(&mut self) {
        self.over(Budget::Context);
    }

    /// End the run before its next model call because the caller is out of
    /// LLM credits (TODO-payments P6.3). The driver decides that — the
    /// balance is in the database, not in the run — and the run ends as any
    /// other budget ends it: not an error, the transcript intact, and a later
    /// message (after a top-up) continues it.
    pub fn out_of_credits(&mut self) {
        self.over(Budget::Credits);
    }

    /// Conclude before the next model call, over `budget`. Only between turns'
    /// halves: a run waiting on its tools finishes them first.
    fn over(&mut self, budget: Budget) {
        if matches!(self.phase, Phase::Model) {
            self.phase = Phase::Done {
                conclusion: Conclusion::OverBudget { budget },
            };
        }
    }

    /// The conversation so far.
    pub fn messages(&self) -> &[LlmMessage] {
        &self.messages
    }

    /// Every model call's usage, accumulated.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// How many model calls have been made.
    pub fn step(&self) -> u32 {
        self.step
    }

    /// The budget.
    pub fn max_steps(&self) -> u32 {
        self.max_steps
    }

    /// The budgets besides the step budget.
    pub fn budgets(&self) -> Budgets {
        self.budgets
    }

    /// The loop-control state: the detectors and the ladder.
    pub fn control(&self) -> &LoopControl {
        &self.control
    }

    /// What each step cost and who answered it.
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The ledger, for the driver to add working time and children to.
    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }

    /// One trait instance's state (see [`trait_state_key`]), `null` until the
    /// trait writes something.
    pub fn trait_state(&self, key: &str) -> Option<&Json> {
        self.trait_state.get(key)
    }

    /// Every trait instance's state, by [`trait_state_key`], in key order: how
    /// something reading a run from outside it finds a trait's state without
    /// the agent it ran.
    pub fn trait_states(&self) -> impl Iterator<Item = (&str, &Json)> {
        self.trait_state.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// One trait instance's state, for writing. Created as `null`.
    pub fn trait_state_mut(&mut self, key: &str) -> &mut Json {
        self.trait_state.entry(key.to_owned()).or_insert(Json::Null)
    }

    /// Whether the run is over.
    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done { .. })
    }

    /// How it ended, if it has.
    pub fn conclusion(&self) -> Option<&Conclusion> {
        match &self.phase {
            Phase::Done { conclusion } => Some(conclusion),
            Phase::Model | Phase::Tools { .. } => None,
        }
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
            arguments: json!({}),
        }
    }

    fn text_answer(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: text.to_owned(),
            ..AssistantMessage::default()
        }
    }

    fn tool_answer(calls: Vec<ToolCall>) -> AssistantMessage {
        AssistantMessage {
            tool_calls: calls,
            ..AssistantMessage::default()
        }
    }

    #[test]
    fn a_single_turn_asks_the_model_once_and_ends() {
        let mut run = AgentLoop::new(20);
        run.push_user("hello").unwrap();

        let Step::CallModel { messages, step, .. } = run.next_step() else {
            panic!("the first step is a model call");
        };
        assert_eq!(step, 1);
        assert_eq!(messages, vec![LlmMessage::user("hello")]);

        run.model_answered(text_answer("hi")).unwrap();
        assert!(run.is_done());
        assert_eq!(
            run.next_step(),
            Step::Done(Conclusion::Answered {
                answer: "hi".to_owned()
            })
        );
        assert_eq!(run.step(), 1);
    }

    #[test]
    fn a_turn_can_be_answered_without_the_model() {
        let mut run = AgentLoop::new(5);
        run.push_user("Press").unwrap();
        run.answered_without_model("Pressed.").unwrap();
        assert_eq!(
            run.next_step(),
            Step::Done(Conclusion::Answered {
                answer: "Pressed.".to_owned()
            })
        );
        assert_eq!(run.step(), 0, "no model call is counted");
        assert!(matches!(
            run.messages().last(),
            Some(LlmMessage::Assistant { .. })
        ));
        // Only straight after the person spoke.
        assert!(run.answered_without_model("again").is_err());
        run.push_user("Next").unwrap();
    }

    #[test]
    fn a_tool_call_is_run_and_the_conversation_continues() {
        let mut run = AgentLoop::new(20);
        run.push_user("how many books?").unwrap();
        run.next_step();
        run.model_answered(tool_answer(vec![call("c1", "query_books")]))
            .unwrap();

        assert_eq!(
            run.next_step(),
            Step::CallTools {
                calls: vec![call("c1", "query_books")]
            }
        );
        run.tool_results(vec![ToolOutcome::ok(call("c1", "query_books"), &json!(3))])
            .unwrap();

        // Back to the model, with the call and its result in the history.
        let Step::CallModel { messages, step, .. } = run.next_step() else {
            panic!("after tools, the model runs again");
        };
        assert_eq!(step, 2);
        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[2], LlmMessage::ToolResult { .. }));

        run.model_answered(text_answer("three")).unwrap();
        assert_eq!(run.conclusion().unwrap().answer(), Some("three"));
    }

    #[test]
    fn two_calls_in_one_turn_are_answered_in_order() {
        let mut run = AgentLoop::new(20);
        run.push_user("do both").unwrap();
        run.next_step();
        run.model_answered(tool_answer(vec![call("c1", "read"), call("c2", "write")]))
            .unwrap();

        let Step::CallTools { calls } = run.next_step() else {
            panic!("two calls are still one tools step");
        };
        assert_eq!(calls.len(), 2);

        run.tool_results(vec![
            ToolOutcome::ok(call("c1", "read"), &json!("a")),
            ToolOutcome::ok(call("c2", "write"), &json!("b")),
        ])
        .unwrap();

        // The results are in the history in the order the model asked.
        let ids: Vec<&str> = run
            .messages()
            .iter()
            .filter_map(|m| match m {
                LlmMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["c1", "c2"]);
    }

    #[test]
    fn results_that_do_not_match_the_calls_are_refused() {
        let mut run = AgentLoop::new(20);
        run.push_user("go").unwrap();
        run.next_step();
        run.model_answered(tool_answer(vec![call("c1", "read"), call("c2", "write")]))
            .unwrap();
        run.next_step();

        // One result for two calls: a history with an unanswered tool call is one
        // both vendors reject, so it is refused here instead.
        let err = run
            .tool_results(vec![ToolOutcome::ok(call("c1", "read"), &json!("a"))])
            .unwrap_err();
        assert!(err.to_string().contains('2'), "{err}");

        // Out of order is refused too.
        let err = run
            .tool_results(vec![
                ToolOutcome::ok(call("c2", "write"), &json!("b")),
                ToolOutcome::ok(call("c1", "read"), &json!("a")),
            ])
            .unwrap_err();
        assert!(err.to_string().contains("order"), "{err}");
    }

    #[test]
    fn a_failed_tool_becomes_a_result_the_model_can_read() {
        let mut run = AgentLoop::new(20);
        run.push_user("go").unwrap();
        run.next_step();
        run.model_answered(tool_answer(vec![call("c1", "query_bookz")]))
            .unwrap();
        run.next_step();
        run.tool_results(vec![ToolOutcome::failed(
            call("c1", "query_bookz"),
            "no table named `bookz`",
        )])
        .unwrap();

        // The loop is still running, and the error is in the transcript.
        assert!(!run.is_done());
        let LlmMessage::ToolResult { content, .. } = run.messages().last().unwrap() else {
            panic!("the last message is the tool result");
        };
        assert_eq!(content, "error: no table named `bookz`");
    }

    #[test]
    fn a_model_that_never_stops_asking_is_stopped_by_the_budget() {
        let mut run = AgentLoop::new(2);
        run.push_user("loop forever").unwrap();
        for step in 1..=2 {
            let Step::CallModel { step: n, .. } = run.next_step() else {
                panic!("step {step} should be a model call");
            };
            assert_eq!(n, step);
            run.model_answered(tool_answer(vec![call(&format!("c{step}"), "read")]))
                .unwrap();
            run.next_step();
            run.tool_results(vec![ToolOutcome::ok(
                call(&format!("c{step}"), "read"),
                &json!("more"),
            )])
            .unwrap();
        }
        assert_eq!(run.next_step(), Step::Done(Conclusion::MaxSteps));
        assert!(run.is_done());
        // The work that was done is still readable — the budget stops the loop,
        // it does not erase the transcript.
        assert_eq!(run.messages().len(), 5);
    }

    #[test]
    fn a_budget_of_zero_is_raised_rather_than_producing_a_run_that_cannot_answer() {
        let mut run = AgentLoop::new(0);
        assert_eq!(run.max_steps(), 1);
        run.push_user("hi").unwrap();
        assert!(matches!(run.next_step(), Step::CallModel { .. }));
    }

    #[test]
    fn asking_twice_without_answering_gives_the_same_instruction() {
        // What resumption depends on: a process that died between being told to
        // act and acting comes back and is told the same thing.
        let mut run = AgentLoop::new(20);
        run.push_user("hello").unwrap();
        assert_eq!(run.next_step(), run.next_step());

        run.model_answered(tool_answer(vec![call("c1", "read")]))
            .unwrap();
        assert_eq!(run.next_step(), run.next_step());
    }

    #[test]
    fn the_whole_state_round_trips_through_json() {
        // `_fd_runs.context` is exactly this value, so a run that cannot survive
        // serialisation is a run that cannot resume.
        let mut run = AgentLoop::new(5);
        run.push_user("how many books?").unwrap();
        run.next_step();
        run.model_answered(AssistantMessage {
            content: "checking".to_owned(),
            tool_calls: vec![call("c1", "query_books")],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 4,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
            ..AssistantMessage::default()
        })
        .unwrap();

        let text = serde_json::to_string(&run).unwrap();
        let mut back: AgentLoop = serde_json::from_str(&text).unwrap();
        assert_eq!(back, run);
        assert_eq!(back.usage().total_tokens(), 14);
        // And it resumes where it left off: the tools it was about to run.
        assert_eq!(
            back.next_step(),
            Step::CallTools {
                calls: vec![call("c1", "query_books")]
            }
        );
    }

    fn answer_costing(cost: f64, input: u64) -> (AssistantMessage, StepMeta) {
        (
            AssistantMessage {
                tool_calls: vec![call("c", "read")],
                usage: Usage {
                    input_tokens: input,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
                ..AssistantMessage::default()
            },
            StepMeta {
                cost: Some(cost),
                ..StepMeta::default()
            },
        )
    }

    fn one_tool_round(run: &mut AgentLoop, cost: f64, input: u64) {
        let Step::CallModel { .. } = run.next_step() else {
            panic!("expected a model call");
        };
        let (answer, meta) = answer_costing(cost, input);
        run.model_answered_with(answer, meta).unwrap();
        run.next_step();
        run.tool_results(vec![ToolOutcome::ok(call("c", "read"), &json!("x"))])
            .unwrap();
    }

    #[test]
    fn a_run_keeps_only_its_latest_images() {
        let mut run = AgentLoop::new(20).with_budgets(Budgets {
            max_images: Some(2),
            ..Budgets::default()
        });
        run.push_user("go").unwrap();
        for n in 0..3u8 {
            let Step::CallModel { .. } = run.next_step() else {
                panic!("expected a model call");
            };
            let (answer, meta) = answer_costing(0.0, 10);
            run.model_answered_with(answer, meta).unwrap();
            run.next_step();
            let image = sc_llm::ImagePart::new("image/jpeg", vec![0xff, 0xd8, n]);
            run.tool_results(vec![
                ToolOutcome::ok(call("c", "view_app"), &json!("shot")).with_images(vec![image]),
            ])
            .unwrap();
        }
        let results: Vec<(bool, Vec<u8>)> = run
            .messages()
            .iter()
            .filter_map(|m| match m {
                LlmMessage::ToolResult {
                    content, images, ..
                } => Some((
                    content.contains(IMAGE_STUB),
                    images.iter().map(|i| i.data[2]).collect(),
                )),
                _ => None,
            })
            .collect();
        // The oldest image is gone and its result says so; the latest two stay.
        assert_eq!(
            results,
            vec![(true, vec![]), (false, vec![1]), (false, vec![2])]
        );
    }

    #[test]
    fn a_cost_budget_ends_the_run_before_the_next_model_call() {
        let mut run = AgentLoop::new(20).with_budgets(Budgets {
            max_cost: Some(1.0),
            ..Budgets::default()
        });
        run.push_user("go").unwrap();
        one_tool_round(&mut run, 0.6, 10);
        one_tool_round(&mut run, 0.6, 10);
        assert_eq!(
            run.next_step(),
            Step::Done(Conclusion::OverBudget {
                budget: Budget::Cost
            })
        );
        assert_eq!(run.ledger().steps().len(), 2);
    }

    #[test]
    fn wall_time_and_context_budgets_end_the_run() {
        let mut run = AgentLoop::new(20).with_budgets(Budgets {
            max_wall_ms: Some(1000),
            ..Budgets::default()
        });
        run.push_user("go").unwrap();
        one_tool_round(&mut run, 0.0, 10);
        assert!(matches!(run.next_step(), Step::CallModel { .. }));
        run.ledger_mut()
            .add_working(std::time::Duration::from_millis(1000));
        assert_eq!(
            run.next_step(),
            Step::Done(Conclusion::OverBudget {
                budget: Budget::WallTime
            })
        );

        // The context budget is the driver's to measure: the loop decides.
        let mut run = AgentLoop::new(20).with_budgets(Budgets {
            context_tokens: Some(500),
            ..Budgets::default()
        });
        assert_eq!(run.context_budget(100_000), 500);
        assert_eq!(AgentLoop::new(20).context_budget(8_000), 8_000);
        run.push_user("go").unwrap();
        one_tool_round(&mut run, 0.0, 400);
        assert_eq!(run.context_verdict(600, 500), ContextVerdict::Compact);
        run.record_compaction(0, 600, 600, None);
        assert_eq!(run.context_verdict(600, 500), ContextVerdict::Over);
        run.context_over_budget();
        let Step::Done(conclusion) = run.next_step() else {
            panic!("the context budget ends the run");
        };
        assert_eq!(
            serde_json::to_value(&conclusion).unwrap(),
            json!({"conclusion": "over_budget", "budget": "context"})
        );
    }

    #[test]
    fn trait_state_round_trips_with_the_run() {
        let mut run = AgentLoop::new(20);
        *run.trait_state_mut(&trait_state_key(0, "coding")) = json!({"plan": [1, 2]});
        let back: AgentLoop = serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
        assert_eq!(
            back.trait_state(&trait_state_key(0, "coding")),
            Some(&json!({"plan": [1, 2]}))
        );
        assert_eq!(back.trait_state(&trait_state_key(1, "coding")), None);
    }

    #[test]
    fn a_finished_run_continues_when_the_person_says_something_else() {
        let mut run = AgentLoop::new(20);
        run.push_user("hello").unwrap();
        run.next_step();
        run.model_answered(text_answer("hi")).unwrap();
        assert!(run.is_done());

        run.push_user("and again").unwrap();
        assert!(!run.is_done());
        let Step::CallModel { messages, step, .. } = run.next_step() else {
            panic!("the second turn is a model call");
        };
        // The budget is the run's, not the turn's, and the history is kept.
        assert_eq!(step, 2);
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn a_message_is_refused_while_tools_are_running() {
        let mut run = AgentLoop::new(20);
        run.push_user("go").unwrap();
        run.next_step();
        run.model_answered(tool_answer(vec![call("c1", "read")]))
            .unwrap();
        let err = run.push_user("actually, stop").unwrap_err();
        assert!(err.to_string().contains("mid-turn"), "{err}");
    }

    #[test]
    fn aborting_ends_the_run_and_keeps_the_transcript() {
        let mut run = AgentLoop::new(20);
        run.push_user("go").unwrap();
        run.next_step();
        run.model_answered(tool_answer(vec![call("c1", "read")]))
            .unwrap();
        run.abort();
        assert_eq!(run.next_step(), Step::Done(Conclusion::Aborted));
        assert_eq!(run.messages().len(), 2);

        // A run that already answered is not re-concluded by a late stop.
        let mut answered = AgentLoop::new(20);
        answered.push_user("hi").unwrap();
        answered.next_step();
        answered.model_answered(text_answer("hello")).unwrap();
        answered.abort();
        assert_eq!(answered.conclusion().unwrap().answer(), Some("hello"));
    }

    #[test]
    fn an_answer_the_loop_did_not_ask_for_is_an_internal_error() {
        let mut run = AgentLoop::new(20);
        run.push_user("go").unwrap();
        run.next_step();
        run.model_answered(tool_answer(vec![call("c1", "read")]))
            .unwrap();
        // Now it wants tools, not another model answer.
        assert!(run.model_answered(text_answer("hi")).is_err());
    }
}

//! The [`Run`]: one execution of an agent, as the thing that is stored (§11.4).
//!
//! A run is *not* an agent's private bookkeeping. It is the chat transcript the
//! panel reloads, the record a triggered run leaves behind, and — once §10.3
//! lands — the same row a durable workflow suspends into. That is why
//! [`RunKind`] exists now, with a `workflow` variant nothing produces yet: the
//! shape is decided once, here, rather than discovered later by a second
//! mechanism growing alongside this one.
//!
//! The state that makes a run resumable is [`AgentLoop`], serialised into
//! [`context`](Run::context). Everything else on the row is about the run rather
//! than in it: who it was for, what it is of, whether it is still going.

use chrono::{DateTime, Utc};
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::agent::ModelRole;
use crate::agent_trait::RunCaller;
use crate::machine::{AgentLoop, Conclusion};

/// The run attribute holding the run's [`RunMode`]. Sparse: absent is
/// [`RunMode::Act`].
pub const ATTR_MODE: &str = "mode";
/// The run attribute holding the [`ModelRole`] whose model answers the run's
/// steps. Sparse: absent is [`ModelRole::Executor`].
pub const ATTR_ROLE: &str = "role";

/// What a run is doing, which decides the tools its traits offer (TODO §5).
///
/// A run attribute rather than an agent setting, because one agent runs in
/// several: a planner run starts one child run of the *same* agent per feature,
/// in `act`. Traits that do not care about modes offer the same tools in all
/// three.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Plan the work: read-only tools plus planning.
    Plan,
    /// Do the work — every tool the grants allow. Today's behaviour, and the
    /// default.
    #[default]
    Act,
    /// Look around: read-only tools only.
    Explore,
}

impl RunMode {
    /// Every mode.
    pub const ALL: [RunMode; 3] = [RunMode::Plan, RunMode::Act, RunMode::Explore];

    /// The stored spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunMode::Plan => "plan",
            RunMode::Act => "act",
            RunMode::Explore => "explore",
        }
    }

    /// The role whose model answers a run in this mode, unless something says
    /// otherwise (TODO §5): `plan` is the strong role's, `explore` the cheap
    /// role's, and `act` the executor's.
    pub fn role(&self) -> ModelRole {
        match self {
            RunMode::Plan => ModelRole::Strong,
            RunMode::Act => ModelRole::Executor,
            RunMode::Explore => ModelRole::Cheap,
        }
    }

    /// Parse a stored spelling, strictly.
    pub fn parse(s: &str) -> Result<RunMode> {
        match s {
            "plan" => Ok(RunMode::Plan),
            "act" => Ok(RunMode::Act),
            "explore" => Ok(RunMode::Explore),
            other => Err(Error::invalid(format!(
                "unknown run mode `{other}`; expected `plan`, `act` or `explore`"
            ))),
        }
    }
}

impl std::fmt::Display for RunMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identifies a run: the UUID primary key of its `_fd_runs` row (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(pub Uuid);

impl RunId {
    /// Mint an id for a new run.
    pub fn new() -> RunId {
        RunId(Uuid::new_v4())
    }
}

impl Default for RunId {
    fn default() -> Self {
        RunId::new()
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What kind of thing is running.
///
/// One table for both, because a chat session and a durable workflow run are the
/// same problem — a context that is written after every step and read back to
/// carry on — and giving them two tables would mean writing the resume path
/// twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    /// An agent run: a chat turn, or a trigger that started one.
    Agent,
    /// A workflow run. Nothing produces one yet (§10.3).
    Workflow,
}

impl RunKind {
    /// The stored spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunKind::Agent => "agent",
            RunKind::Workflow => "workflow",
        }
    }

    /// Parse a stored spelling. An unknown one is an error rather than a default:
    /// a run whose kind is not understood must not be read as an agent's and
    /// resumed by the wrong engine.
    pub fn parse(s: &str) -> Result<RunKind> {
        match s {
            "agent" => Ok(RunKind::Agent),
            "workflow" => Ok(RunKind::Workflow),
            other => Err(Error::invalid(format!(
                "unknown run kind `{other}`; expected `agent` or `workflow`"
            ))),
        }
    }
}

impl std::fmt::Display for RunKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a run has got to.
///
/// `Failed` is separate from a [`Conclusion`] because the two are different kinds
/// of ending: a run that hit its step budget *concluded* — the model was asked,
/// answered, and the number stopped it — whereas a run that failed never got an
/// answer at all. Collapsing them would make "the provider refused the key" and
/// "the agent worked for twenty steps" the same row to anyone reading the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Still going, or waiting for its next step.
    Running,
    /// **Suspended**: a workflow run waiting for a time to come round or for a
    /// person to answer (§10.3). Live, but not runnable until
    /// [`wake_at`](Run::wake_at) arrives — or, for a run waiting on a human,
    /// until somebody resumes it.
    ///
    /// A state of its own rather than `Running` with something null, because the
    /// difference is what the queue's runnable set is defined by and what an
    /// admin looking at a stuck run needs to see: "waiting for approval since
    /// Tuesday" and "running" are not the same report.
    Waiting,
    /// Finished: the model answered, or the budget stopped it.
    Done,
    /// Something outside the conversation went wrong — see [`Run::error`].
    Failed,
    /// Someone pressed stop.
    Aborted,
}

impl RunState {
    /// The stored spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Waiting => "waiting",
            RunState::Done => "done",
            RunState::Failed => "failed",
            RunState::Aborted => "aborted",
        }
    }

    /// Parse a stored spelling, strictly.
    pub fn parse(s: &str) -> Result<RunState> {
        match s {
            "running" => Ok(RunState::Running),
            "waiting" => Ok(RunState::Waiting),
            "done" => Ok(RunState::Done),
            "failed" => Ok(RunState::Failed),
            "aborted" => Ok(RunState::Aborted),
            other => Err(Error::invalid(format!(
                "unknown run state `{other}`; expected \
                 `running`, `waiting`, `done`, `failed` or `aborted`"
            ))),
        }
    }

    /// Whether a run in this state has **not finished** — it is running, or
    /// waiting for a time or a person.
    ///
    /// Not the same question as "is it runnable *now*", which the queue asks
    /// with a clock in hand (live *and* `wake_at` past): a run waiting on a
    /// human is live for a week and runnable at no point in it.
    pub fn is_live(&self) -> bool {
        matches!(self, RunState::Running | RunState::Waiting)
    }
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One execution of an agent: what it is of, who it is for, and where it got to.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Stable identity: the UUID of its `_fd_runs` row.
    pub id: RunId,
    /// Agent or workflow.
    pub kind: RunKind,
    /// What is running: the agent's **name**, not its id, so a run stays readable
    /// after the agent it was of has been deleted. A transcript that becomes an
    /// orphaned UUID is a transcript nobody can interpret.
    pub subject: String,
    /// A human-readable line for a run list (§9). Empty means "none given"; the
    /// chat panel falls back to the first message.
    pub description: String,
    /// Where it got to.
    pub state: RunState,
    /// Why it failed, for a run that did. `None` in every other state.
    pub error: Option<String>,
    /// The resumable state: an [`AgentLoop`] for an agent run.
    pub context: Json,
    /// The user the run is on behalf of, or `None` for one a trigger started
    /// (decision 5). This is the authority every tool in the run executed with,
    /// recorded so an audit can answer "as whom?".
    pub user: Option<Uuid>,
    /// The version of its subject this run is **pinned** to: the workflow
    /// version it started on (§10.3, decision 2), or `None` for an agent run.
    ///
    /// A run loads that version for its whole life, which is what "a suspended
    /// run finishes on its own version of the workflow" means: the workflow may
    /// be edited twice while the run waits, and the run is not retro-fitted to
    /// steps it never started.
    pub subject_version: Option<u32>,
    /// When this run next wants the engine: now for one that is runnable, a
    /// retry's deadline, a `Wait`'s end — and `None` for a run waiting on a
    /// **person**, which no clock will make runnable.
    pub wake_at: Option<DateTime<Utc>>,
    /// Until when a node has claimed this run (§10.3, decision 5). A lease in the
    /// past, or none at all, means nothing is working on it — which is how a
    /// crashed node's runs are recovered without anybody having to detect the
    /// crash.
    pub lease_until: Option<DateTime<Utc>>,
    /// Who holds that lease: a node identifier, for the operator reading a run
    /// that is not moving. Never authority — the lease is.
    pub claimed_by: Option<String>,
    /// Sparse per-run values (§9).
    pub attributes: Attrs,
    /// When the run was created.
    pub created_at: DateTime<Utc>,
    /// When it was last written — which is after every step, so this is how far
    /// behind a resumed run is.
    pub updated_at: DateTime<Utc>,
}

impl Run {
    /// A new agent run for `agent`, on behalf of `caller`, holding `state`.
    ///
    /// The caller is a parameter with no default, deliberately: decision 5 says
    /// the difference between a chat turn's authority and a trigger's is set at
    /// exactly one place, and this is that place.
    pub fn new(agent: impl Into<String>, caller: &RunCaller, state: &AgentLoop) -> Run {
        let now = Utc::now();
        Run {
            id: RunId::new(),
            kind: RunKind::Agent,
            subject: agent.into(),
            description: String::new(),
            state: RunState::Running,
            error: None,
            context: serde_json::to_value(state).unwrap_or(Json::Null),
            user: caller.user.as_ref().map(|u| u.id),
            // An agent run is pinned to nothing, is always runnable, and is
            // never leased: the loop is driven by whoever started it, not by the
            // workflow queue.
            subject_version: None,
            wake_at: None,
            lease_until: None,
            claimed_by: None,
            attributes: Attrs::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// The loop state this run carries.
    ///
    /// Strict: a context that does not parse is an error naming the run, never an
    /// empty conversation silently substituted. Resuming a run as though it had
    /// never said anything is worse than refusing to resume it.
    pub fn agent_loop(&self) -> Result<AgentLoop> {
        if self.kind != RunKind::Agent {
            return Err(Error::invalid(format!(
                "run {} is a {} run, not an agent run",
                self.id, self.kind
            )));
        }
        serde_json::from_value(self.context.clone()).map_err(|e| {
            Error::invalid(format!(
                "run {}: its stored context is unreadable: {e}",
                self.id
            ))
        })
    }

    /// Write `state` into the run and move its state on to match — what the
    /// driver calls after **every** step (§11.2).
    ///
    /// The two are set together because they cannot be allowed to disagree: a
    /// context that says the model answered and a state that says `running` is a
    /// row that would be resumed forever.
    pub fn record(&mut self, state: &AgentLoop) {
        self.context = serde_json::to_value(state).unwrap_or(Json::Null);
        self.state = match state.conclusion() {
            None => RunState::Running,
            Some(Conclusion::Aborted) => RunState::Aborted,
            // A budget that ran out concluded the run: the transcript is intact
            // and the limit was the admin's, so it is not a failure. A stuck run
            // is the same: nothing outside the conversation went wrong, and
            // `conclusion` says why it stopped.
            Some(
                Conclusion::Answered { .. }
                | Conclusion::MaxSteps
                | Conclusion::OverBudget { .. }
                | Conclusion::Stuck { .. },
            ) => RunState::Done,
            // Waiting on a person (TODO-agent-skills AS6.1): live, and no clock
            // makes it runnable — `wake_at` stays empty.
            Some(Conclusion::AwaitingUser { .. }) => RunState::Waiting,
        };
        self.updated_at = Utc::now();
    }

    /// How the run's loop concluded, read from its stored context: `None` for
    /// a run still going, a workflow run, or one whose context is unreadable.
    ///
    /// What a run list shows beside `done`, so a run that answered and one that
    /// was stopped as stuck do not read the same.
    pub fn conclusion(&self) -> Option<Conclusion> {
        if self.kind != RunKind::Agent {
            return None;
        }
        // Read from the one field rather than the whole loop, because a run
        // list does this for dozens of transcripts.
        let phase = self.context.get("phase")?;
        if phase.get("phase")?.as_str()? != "done" {
            return None;
        }
        serde_json::from_value(phase.get("conclusion")?.clone()).ok()
    }

    /// Mark the run failed, with the reason a reader will see.
    ///
    /// The context is left exactly as it was, because what the agent had already
    /// done is the most useful thing to show next to why it stopped (§11.4).
    pub fn fail(&mut self, error: impl std::fmt::Display) {
        self.state = RunState::Failed;
        self.error = Some(error.to_string());
        self.updated_at = Utc::now();
    }

    /// Set the description, returning `self` for chaining.
    pub fn description(mut self, description: impl Into<String>) -> Run {
        self.description = description.into();
        self
    }

    /// The run's mode. Strict, like the context: an unreadable mode is an
    /// error rather than `act`, which would hand a read-only run its edit tools.
    pub fn mode(&self) -> Result<RunMode> {
        match self.attributes.get(ATTR_MODE) {
            None | Some(Json::Null) => Ok(RunMode::Act),
            Some(Json::String(s)) => {
                RunMode::parse(s).map_err(|e| Error::invalid(format!("run {}: {e}", self.id)))
            }
            Some(other) => Err(Error::invalid(format!(
                "run {}: its `{ATTR_MODE}` should be a string, got {other}",
                self.id
            ))),
        }
    }

    /// The role whose model answers this run's steps.
    pub fn role(&self) -> Result<ModelRole> {
        match self.attributes.get(ATTR_ROLE) {
            None | Some(Json::Null) => Ok(ModelRole::Executor),
            Some(Json::String(s)) => {
                ModelRole::parse(s).map_err(|e| Error::invalid(format!("run {}: {e}", self.id)))
            }
            Some(other) => Err(Error::invalid(format!(
                "run {}: its `{ATTR_ROLE}` should be a string, got {other}",
                self.id
            ))),
        }
    }

    /// Set the mode, returning `self` for chaining. `act` is stored as absent.
    pub fn with_mode(mut self, mode: RunMode) -> Run {
        if mode == RunMode::Act {
            self.attributes.remove(ATTR_MODE);
        } else {
            self.attributes
                .insert(ATTR_MODE.to_owned(), mode.as_str().into());
        }
        self
    }

    /// Set the role, returning `self` for chaining. The executor is stored as
    /// absent.
    pub fn with_role(mut self, role: ModelRole) -> Run {
        if role == ModelRole::Executor {
            self.attributes.remove(ATTR_ROLE);
        } else {
            self.attributes
                .insert(ATTR_ROLE.to_owned(), role.as_str().into());
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_auth::User;

    fn answered_loop() -> AgentLoop {
        let mut state = AgentLoop::new(20);
        state.push_user("hi").unwrap();
        state.next_step();
        state
            .model_answered(sc_llm::AssistantMessage {
                content: "hello".to_owned(),
                ..sc_llm::AssistantMessage::default()
            })
            .unwrap();
        state
    }

    #[test]
    fn a_chat_run_records_the_user_it_is_for() {
        let user = User::new(Uuid::new_v4(), 40).unwrap();
        let id = user.id;
        let run = Run::new("librarian", &RunCaller::user(user), &AgentLoop::new(20));
        assert_eq!(run.kind, RunKind::Agent);
        assert_eq!(run.subject, "librarian");
        assert_eq!(run.user, Some(id));
        assert_eq!(run.state, RunState::Running);
        assert!(run.state.is_live());
    }

    #[test]
    fn a_trigger_started_run_has_no_user() {
        let run = Run::new("nightly", &RunCaller::system(), &AgentLoop::new(20));
        assert_eq!(run.user, None);
    }

    #[test]
    fn recording_a_finished_loop_moves_the_run_on() {
        let mut run = Run::new("a", &RunCaller::system(), &AgentLoop::new(20));
        let state = answered_loop();
        run.record(&state);
        assert_eq!(run.state, RunState::Done);
        // And the context is the loop, byte for byte the state it can resume from.
        assert_eq!(run.agent_loop().unwrap(), state);
    }

    #[test]
    fn a_failure_keeps_the_transcript_and_says_why() {
        let mut run = Run::new("a", &RunCaller::system(), &AgentLoop::new(20));
        let state = answered_loop();
        run.record(&state);
        run.fail("the provider rejected the API key");
        assert_eq!(run.state, RunState::Failed);
        assert!(run.error.as_deref().unwrap_or_default().contains("API key"));
        // What the agent had already done is still there to show.
        assert_eq!(run.agent_loop().unwrap().messages().len(), 2);
    }

    #[test]
    fn an_unreadable_context_is_refused_rather_than_read_as_empty() {
        let mut run = Run::new("a", &RunCaller::system(), &AgentLoop::new(20));
        run.context = Json::String("not a loop".to_owned());
        let err = run.agent_loop().unwrap_err();
        assert!(err.to_string().contains("unreadable"), "{err}");
    }

    #[test]
    fn a_run_carries_its_mode_and_role_as_sparse_attributes() {
        let run = Run::new("a", &RunCaller::system(), &AgentLoop::new(20));
        assert_eq!(run.mode().unwrap(), RunMode::Act);
        assert_eq!(run.role().unwrap(), ModelRole::Executor);
        assert!(run.attributes.is_empty());

        let run = run.with_mode(RunMode::Plan).with_role(ModelRole::Strong);
        assert_eq!(run.mode().unwrap(), RunMode::Plan);
        assert_eq!(run.role().unwrap(), ModelRole::Strong);
        assert_eq!(run.attributes[ATTR_MODE], "plan");

        let mut broken = run.clone();
        broken
            .attributes
            .insert(ATTR_MODE.to_owned(), "dream".into());
        assert!(broken.mode().is_err());
        for mode in RunMode::ALL {
            assert_eq!(RunMode::parse(mode.as_str()).unwrap(), mode);
        }
    }

    #[test]
    fn the_stored_spellings_round_trip_and_refuse_what_they_do_not_know() {
        for kind in [RunKind::Agent, RunKind::Workflow] {
            assert_eq!(RunKind::parse(kind.as_str()).unwrap(), kind);
        }
        assert!(RunKind::parse("copilot").is_err());
        for state in [
            RunState::Running,
            RunState::Waiting,
            RunState::Done,
            RunState::Failed,
            RunState::Aborted,
        ] {
            assert_eq!(RunState::parse(state.as_str()).unwrap(), state);
        }
        assert!(RunState::parse("paused").is_err());
        // Waiting is live — the run has not finished — and nothing else is.
        assert!(RunState::Waiting.is_live() && RunState::Running.is_live());
        assert!(!RunState::Done.is_live() && !RunState::Failed.is_live());
    }
}

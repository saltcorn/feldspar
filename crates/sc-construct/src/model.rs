//! A construction, its phases and its tasks, as the store reads and writes them
//! and the admin API answers them.

use chrono::{DateTime, Utc};
use sc_error::{Error, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where a construction is. The runner (AC3) moves it between these; the
/// specification and the phases are edited in `Draft` and `Planned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstructionStatus {
    Draft,
    Planned,
    Running,
    Stopped,
    Done,
    Failed,
}

/// The kind of artefact a task makes, which decides the agent that runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Tables, fields, queries: run by the construction's own agent.
    DataModel,
    /// Code: delegated to the application's builder agent.
    Feature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    Running,
    Done,
    Failed,
    Skipped,
}

/// A phase's status. Not stored: it follows from the phase's tasks
/// ([`phase_status`]), so the two cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    /// No tasks yet: they are planned when the phase is reached.
    Unplanned,
    Planned,
    Running,
    Failed,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Must,
    Should,
    Could,
}

/// One thing a phase must deliver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Requirement {
    pub text: String,
    pub priority: Priority,
}

/// The construction of one application.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Construction {
    pub id: Uuid,
    pub application: Uuid,
    pub specification: String,
    pub status: ConstructionStatus,
    /// The agent that plans and builds the data model, by name.
    pub agent: Option<String>,
    /// Who last started it: the runner works as this admin.
    pub started_by: Option<Uuid>,
    /// The run that planned the phases last: its latest `phases` run's.
    pub plan_run: Option<Uuid>,
    /// Why its latest run stopped or failed, for the admin to read: the
    /// newest error logged for it ([`crate::current_error`]).
    pub error: Option<String>,
    /// The logged error `error` is, when it is one.
    pub error_entry: Option<Uuid>,
    /// The phase a run stops after, or `None` to run them all.
    pub until_phase: Option<Uuid>,
    /// The run only plans the tasks of `until_phase` and stops, so the admin
    /// reviews them before they run.
    pub plan_only: bool,
    /// What the run that plans `until_phase`'s tasks is told besides the
    /// phase: the plan it replaces and the admin's note on it.
    pub plan_note: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Phase {
    pub id: Uuid,
    pub construction: Uuid,
    pub position: f64,
    pub title: String,
    pub description: String,
    pub requirements: Vec<Requirement>,
}

/// A phase with its tasks, in order. Its status is not part of [`Phase`]
/// because it is not stored: it comes from the tasks ([`Self::status`]), so
/// only code that read them has one.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseWithTasks {
    pub phase: Phase,
    pub tasks: Vec<Task>,
}

impl PhaseWithTasks {
    /// The phase's status, from its tasks ([`phase_status`]).
    pub fn status(&self) -> PhaseStatus {
        phase_status(&self.tasks)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Task {
    pub id: Uuid,
    pub phase: Uuid,
    pub position: f64,
    pub kind: TaskKind,
    pub title: String,
    pub brief: String,
    pub depends_on: Vec<Uuid>,
    pub status: TaskStatus,
    /// The run of each attempt, oldest first.
    pub runs: Vec<Uuid>,
    /// The closing summary of the last attempt.
    pub summary: Option<String>,
    /// The tables and custom queries this task made, e.g.
    /// `{"kind": "table", "name": "contacts"}` or `{"kind": "query", "name": "countContacts"}`.
    /// Taken from what the tools reported during the task's runs.
    pub created: Vec<serde_json::Value>,
    /// The feedback it was planned for (AC9), or `None` for a task of the
    /// phase's own plan.
    pub feedback: Option<Uuid>,
}

impl Task {
    /// A new task to do.
    pub fn new(phase: Uuid, position: f64, kind: TaskKind, title: &str, brief: &str) -> Task {
        Task {
            id: Uuid::new_v4(),
            phase,
            position,
            kind,
            title: title.to_owned(),
            brief: brief.to_owned(),
            depends_on: Vec::new(),
            status: TaskStatus::Todo,
            runs: Vec::new(),
            summary: None,
            created: Vec::new(),
            feedback: None,
        }
    }
}

/// Whether the admin chose a mockup (TODO-app-constructor AC7). One of a
/// construction's mockups at most is `Chosen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MockupStatus {
    Proposed,
    Chosen,
}

/// One screen of a mockup: a self-contained HTML page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockupScreen {
    /// What the screen is, e.g. "Orders list"; also its file's name.
    pub name: String,
    pub html: String,
}

/// One proposed look of the application, as screens. The agent proposes a
/// few at a time, a **round**; a revision is the next round.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mockup {
    pub id: Uuid,
    pub construction: Uuid,
    /// Which proposal it came with: 1 for the first, one more per revision.
    pub round: i64,
    pub position: f64,
    pub title: String,
    /// What sets it apart from the others, in a sentence.
    pub rationale: String,
    pub screens: Vec<MockupScreen>,
    pub status: MockupStatus,
    /// What the admin said about it, for the next round.
    pub feedback: Option<String>,
}

/// What the admin said about one mockup, for the next round. Blank text
/// clears it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockupFeedback {
    pub mockup: Uuid,
    pub text: String,
}

/// A question the construction agent asked about the specification, before
/// the mockups are drawn and the phases planned (TODO-app-constructor AC8),
/// with the admin's answer once given.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Question {
    pub id: Uuid,
    pub construction: Uuid,
    pub position: f64,
    pub text: String,
    /// `None` until answered; a question may be left open.
    pub answer: Option<String>,
}

/// The admin's answer to one question. Blank text clears it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionAnswer {
    pub question: Uuid,
    pub text: String,
}

/// Feedback on what a phase built (TODO-app-constructor AC9). Once the admin
/// approves it, the construction agent turns it into requirements, kept here,
/// and follow-up tasks after the phase's own, which name it
/// ([`Task::feedback`]); its status then follows from those tasks, as a
/// phase's does.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Feedback {
    pub id: Uuid,
    pub phase: Uuid,
    pub position: f64,
    pub title: String,
    pub description: String,
    /// What the feedback asks of the phase, as the agent wrote it; empty until
    /// it is planned.
    pub requirements: Vec<Requirement>,
    /// The run that planned it last, from its construction runs; `None`
    /// until the admin approves it.
    pub run: Option<Uuid>,
}

/// A phase's status, from its tasks: none is unplanned, any running is
/// running, any failed is failed, all done or skipped is done, and anything
/// else (some still to do) is planned.
pub fn phase_status(tasks: &[Task]) -> PhaseStatus {
    if tasks.is_empty() {
        PhaseStatus::Unplanned
    } else if tasks.iter().any(|t| t.status == TaskStatus::Running) {
        PhaseStatus::Running
    } else if tasks.iter().any(|t| t.status == TaskStatus::Failed) {
        PhaseStatus::Failed
    } else if tasks
        .iter()
        .all(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Skipped))
    {
        PhaseStatus::Done
    } else {
        PhaseStatus::Planned
    }
}

/// The stored spelling of an enum: its serde name.
pub(crate) fn spelling<T: Serialize>(value: T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// An enum from its stored spelling, or an error naming the column.
pub(crate) fn parse<T: DeserializeOwned>(column: &str, raw: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(raw.to_owned()))
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid {column}")))
}

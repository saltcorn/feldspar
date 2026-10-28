//! What a checklist, a step, a proposal and a run are (docs/CHECKLISTS.md §1).

use chrono::{DateTime, Utc};
use sc_catalog::outbox::Sent;
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::{Value as Json, json};
use uuid::Uuid;

/// `attributes.stop_on_failure`: whether a failing step ends the run (default
/// true).
pub const ATTR_STOP_ON_FAILURE: &str = "stop_on_failure";
/// `attributes.timeout`: how many seconds one step may take (default
/// [`DEFAULT_STEP_TIMEOUT_SECS`]).
pub const ATTR_TIMEOUT: &str = "timeout";
/// `attributes.allow_on_production`: a mutating checklist may run on an
/// environment marked `production`. Only a human may set it.
pub const ATTR_ALLOW_ON_PRODUCTION: &str = "allow_on_production";
/// `attributes.tags`: the instance's tags, as on every other entity.
pub const ATTR_TAGS: &str = "tags";
/// `false` turns off the clean-page check after every `browser.open` (TODO
/// C2.4). On by default.
pub const ATTR_IMPLICIT_CLEAN: &str = "implicit_clean";

/// How long a step may take when the checklist does not say.
pub const DEFAULT_STEP_TIMEOUT_SECS: u64 = 60;

/// Who wrote something: a person, or an agent's run.
///
/// Stored as `human:<user id>` or `agent:<run id>`. An agent's run is what
/// decides which unaccepted steps it may still edit (§4 rule 2): the steps
/// *this* run wrote, not the steps some agent wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    /// A person, by user id.
    Human(Uuid),
    /// An agent run, by its id (an internal run's id, or `mcp:<token id>` for
    /// a session of the MCP server, which has no run of its own).
    Agent(String),
}

impl Author {
    /// The stored spelling.
    pub fn to_text(&self) -> String {
        match self {
            Author::Human(id) => format!("human:{id}"),
            Author::Agent(run) => format!("agent:{run}"),
        }
    }

    /// Read the stored spelling back.
    pub fn parse(text: &str) -> Result<Author> {
        if let Some(id) = text.strip_prefix("human:") {
            let id = id
                .parse()
                .map_err(|_| Error::invalid(format!("`{text}` names no user")))?;
            return Ok(Author::Human(id));
        }
        if let Some(run) = text.strip_prefix("agent:").filter(|r| !r.is_empty()) {
            return Ok(Author::Agent(run.to_owned()));
        }
        Err(Error::invalid(format!(
            "`{text}` is not an author; it should be `human:<user id>` or `agent:<run id>`"
        )))
    }

    /// Whether an agent wrote it.
    pub fn is_agent(&self) -> bool {
        matches!(self, Author::Agent(_))
    }
}

/// A checklist: an ordered list of steps run as one user (§1).
#[derive(Debug, Clone, PartialEq)]
pub struct Checklist {
    /// Its `_fd_checklists` row.
    pub id: Uuid,
    /// Unique across the instance; what the CLI and the agent tools name it by.
    pub name: String,
    /// What it checks, for a person.
    pub description: String,
    /// The application it checks, or `None` for a data-only checklist.
    pub application: Option<Uuid>,
    /// The user every step runs as, or `None` for an anonymous visitor.
    pub run_as: Option<Uuid>,
    /// Whether it runs with the others. Only a human changes this.
    pub enabled: bool,
    /// Whether any current step writes data — derived from the step types
    /// whenever the steps change, never typed by anyone.
    pub mutates: bool,
    /// Sparse settings: [`ATTR_STOP_ON_FAILURE`], [`ATTR_TIMEOUT`],
    /// [`ATTR_ALLOW_ON_PRODUCTION`], [`ATTR_TAGS`], [`ATTR_IMPLICIT_CLEAN`].
    pub attributes: Attrs,
}

impl Checklist {
    /// A new, enabled, empty checklist.
    pub fn new(name: impl Into<String>) -> Checklist {
        Checklist {
            id: Uuid::new_v4(),
            name: name.into(),
            description: String::new(),
            application: None,
            run_as: None,
            enabled: true,
            mutates: false,
            attributes: Attrs::new(),
        }
    }

    /// Whether a failing step ends the run.
    pub fn stop_on_failure(&self) -> bool {
        self.attributes
            .get(ATTR_STOP_ON_FAILURE)
            .and_then(Json::as_bool)
            .unwrap_or(true)
    }

    /// Whether it may run on a production environment although it mutates.
    pub fn allow_on_production(&self) -> bool {
        self.attributes
            .get(ATTR_ALLOW_ON_PRODUCTION)
            .and_then(Json::as_bool)
            .unwrap_or(false)
    }

    /// Whether every `browser.open` also checks the page is clean: no console
    /// errors, no uncaught exceptions, no failed requests.
    pub fn implicit_clean(&self) -> bool {
        self.attributes
            .get(ATTR_IMPLICIT_CLEAN)
            .and_then(Json::as_bool)
            .unwrap_or(true)
    }

    /// How long one step may take.
    pub fn step_timeout(&self) -> std::time::Duration {
        let secs = self
            .attributes
            .get(ATTR_TIMEOUT)
            .and_then(Json::as_u64)
            .filter(|s| *s > 0)
            .unwrap_or(DEFAULT_STEP_TIMEOUT_SECS);
        std::time::Duration::from_secs(secs)
    }

    /// The checklist as the admin API and the agent tools show it.
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id.to_string(),
            "name": self.name,
            "description": self.description,
            "application": self.application.map(|a| a.to_string()),
            "run_as": self.run_as.map(|u| u.to_string()),
            "enabled": self.enabled,
            "mutates": self.mutates,
            "attributes": Json::Object(self.attributes.clone()),
        })
    }
}

/// What a caller submits for one step: the parts a person or an agent writes.
#[derive(Debug, Clone, PartialEq)]
pub struct StepDraft {
    /// The registered step type, namespaced (`data.insert`).
    pub type_: String,
    /// Its parameters, validated against the type's `config_spec` on save.
    pub params: Attrs,
    /// What a person reads in the list.
    pub label: String,
    /// The variable the step's value is captured into (`"as"`), if any.
    pub capture: Option<String>,
}

impl StepDraft {
    /// A step of `type_` with no parameters.
    pub fn new(type_: impl Into<String>) -> StepDraft {
        StepDraft {
            type_: type_.into(),
            params: Attrs::new(),
            label: String::new(),
            capture: None,
        }
    }

    /// Set one parameter.
    pub fn param(mut self, key: &str, value: impl Into<Json>) -> StepDraft {
        self.params.insert(key.to_owned(), value.into());
        self
    }

    /// Set the label.
    pub fn label(mut self, label: impl Into<String>) -> StepDraft {
        self.label = label.into();
        self
    }

    /// Capture the step's value as `name`.
    pub fn capture(mut self, name: impl Into<String>) -> StepDraft {
        self.capture = Some(name.into());
        self
    }

    /// Read one from the JSON the admin API and the tools take:
    /// `{type, params, label, as}`.
    pub fn from_json(value: &Json) -> Result<StepDraft> {
        let obj = value
            .as_object()
            .ok_or_else(|| Error::invalid("a step should be an object: {type, params, label, as}"))?;
        for key in obj.keys() {
            if !["type", "params", "label", "as"].contains(&key.as_str()) {
                return Err(Error::invalid(format!(
                    "a step has no `{key}`; it takes type, params, label and as"
                )));
            }
        }
        let type_ = obj
            .get("type")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Error::invalid("a step needs its `type`, e.g. `data.expect_rows`"))?;
        let params = match obj.get("params") {
            None | Some(Json::Null) => Attrs::new(),
            Some(Json::Object(map)) => map.clone(),
            Some(other) => {
                return Err(Error::invalid(format!(
                    "a step's `params` should be an object, got {other}"
                )));
            }
        };
        let text = |key: &str| -> Result<Option<String>> {
            match obj.get(key) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::String(s)) if s.trim().is_empty() => Ok(None),
                Some(Json::String(s)) => Ok(Some(s.trim().to_owned())),
                Some(other) => Err(Error::invalid(format!(
                    "a step's `{key}` should be a string, got {other}"
                ))),
            }
        };
        Ok(StepDraft {
            type_: type_.to_owned(),
            params,
            label: text("label")?.unwrap_or_default(),
            capture: text("as")?,
        })
    }

    /// The draft as JSON, in [`from_json`](StepDraft::from_json)'s shape.
    pub fn to_json(&self) -> Json {
        json!({
            "type": self.type_,
            "params": Json::Object(self.params.clone()),
            "label": self.label,
            "as": self.capture,
        })
    }
}

/// One version of one step (§1). Never updated in place, except for the two
/// bookkeeping columns: [`accepted`](Step::accepted) when a person accepts it
/// and [`superseded_by`](Step::superseded_by) when a newer version replaces it.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// This version's id — what a run records as the version it ran.
    pub id: Uuid,
    /// The checklist it belongs to.
    pub checklist: Uuid,
    /// Where it runs, ascending. A float, so a step can be put between two
    /// others without renumbering (which would be an update in place).
    pub position: f64,
    /// What it is.
    pub draft: StepDraft,
    /// Who wrote this version.
    pub author: Author,
    /// Whether a person has seen it (§4). A person's own steps are accepted.
    pub accepted: bool,
    /// The version that replaced this one, or `None` while it is current.
    pub superseded_by: Option<Uuid>,
    /// Whether this row is a **tombstone**: the version a deletion wrote. A
    /// tombstone is never current; the step it replaced points at it.
    pub deleted: bool,
    /// When this version was written.
    pub created_at: DateTime<Utc>,
}

impl Step {
    /// Whether this is the version that runs.
    pub fn is_current(&self) -> bool {
        self.superseded_by.is_none() && !self.deleted
    }

    /// The step as the admin API and the agent tools show it.
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id.to_string(),
            "checklist": self.checklist.to_string(),
            "position": self.position,
            "type": self.draft.type_,
            "params": Json::Object(self.draft.params.clone()),
            "label": self.draft.label,
            "as": self.draft.capture,
            "author": self.author.to_text(),
            "accepted": self.accepted,
            "superseded_by": self.superseded_by.map(|s| s.to_string()),
            "deleted": self.deleted,
            "created_at": self.created_at.to_rfc3339(),
        })
    }
}

/// A change an agent asked for and a person has not yet decided on (§4 rule 3).
#[derive(Debug, Clone, PartialEq)]
pub enum ProposalChange {
    /// Replace step `step` with `draft`.
    Edit { step: Uuid, draft: StepDraft },
    /// Put `draft` before step `before`.
    InsertBefore { before: Uuid, draft: StepDraft },
    /// Run the current steps in this order.
    Reorder { order: Vec<Uuid> },
    /// Run the checklist as this user (`None` is an anonymous visitor).
    RunAs { user: Option<Uuid> },
}

impl ProposalChange {
    /// The stored `kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            ProposalChange::Edit { .. } => "edit",
            ProposalChange::InsertBefore { .. } => "insert_before",
            ProposalChange::Reorder { .. } => "reorder",
            ProposalChange::RunAs { .. } => "run_as",
        }
    }

    /// The step it is about, if it is about one.
    pub fn step(&self) -> Option<Uuid> {
        match self {
            ProposalChange::Edit { step, .. } => Some(*step),
            ProposalChange::InsertBefore { before, .. } => Some(*before),
            _ => None,
        }
    }

    /// The stored `change` column.
    pub fn to_json(&self) -> Json {
        match self {
            ProposalChange::Edit { draft, .. } | ProposalChange::InsertBefore { draft, .. } => {
                json!({ "step": draft.to_json() })
            }
            ProposalChange::Reorder { order } => {
                json!({ "order": order.iter().map(Uuid::to_string).collect::<Vec<_>>() })
            }
            ProposalChange::RunAs { user } => json!({ "run_as": user.map(|u| u.to_string()) }),
        }
    }

    /// Rebuild one from its stored `kind`, `step` and `change` columns.
    pub fn from_parts(kind: &str, step: Option<Uuid>, change: &Json) -> Result<ProposalChange> {
        let need_step = || {
            step.ok_or_else(|| Error::invalid(format!("a `{kind}` proposal names its step")))
        };
        let draft = || {
            StepDraft::from_json(change.get("step").unwrap_or(&Json::Null))
        };
        match kind {
            "edit" => Ok(ProposalChange::Edit {
                step: need_step()?,
                draft: draft()?,
            }),
            "insert_before" => Ok(ProposalChange::InsertBefore {
                before: need_step()?,
                draft: draft()?,
            }),
            "reorder" => Ok(ProposalChange::Reorder {
                order: parse_ids(change.get("order").unwrap_or(&Json::Null), "order")?,
            }),
            "run_as" => Ok(ProposalChange::RunAs {
                user: match change.get("run_as") {
                    None | Some(Json::Null) => None,
                    Some(Json::String(s)) => Some(
                        s.parse()
                            .map_err(|_| Error::invalid(format!("`{s}` is not a user id")))?,
                    ),
                    Some(other) => {
                        return Err(Error::invalid(format!(
                            "`run_as` should be a user id, got {other}"
                        )));
                    }
                },
            }),
            other => Err(Error::invalid(format!("`{other}` is not a kind of proposal"))),
        }
    }
}

/// A list of ids, as JSON strings.
pub fn parse_ids(value: &Json, what: &str) -> Result<Vec<Uuid>> {
    let items = value
        .as_array()
        .ok_or_else(|| Error::invalid(format!("`{what}` should be a list of step ids")))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Error::invalid(format!("`{item}` in `{what}` is not a step id")))
        })
        .collect()
}

/// A stored proposal.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    /// Its row.
    pub id: Uuid,
    /// The checklist it would change.
    pub checklist: Uuid,
    /// What it would change.
    pub change: ProposalChange,
    /// Why the agent asks for it — shown to the person deciding.
    pub reason: String,
    /// Who asked.
    pub author: Author,
    /// When.
    pub created_at: DateTime<Utc>,
}

impl Proposal {
    /// The proposal as the admin API and the agent tools show it.
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id.to_string(),
            "checklist": self.checklist.to_string(),
            "kind": self.change.kind(),
            "step": self.change.step().map(|s| s.to_string()),
            "change": self.change.to_json(),
            "reason": self.reason,
            "author": self.author.to_text(),
            "created_at": self.created_at.to_rfc3339(),
        })
    }
}

/// How one step went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It did what it does and every assertion held.
    Pass,
    /// An assertion did not hold — the check found something.
    Fail,
    /// The step could not be carried out: a table that is gone, a parameter
    /// that does not render. The check is broken rather than red.
    Error,
    /// It was not run: the host lacks what it needs, it is a manual step, or an
    /// earlier step ended the run.
    Skipped,
}

impl Outcome {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Error => "error",
            Outcome::Skipped => "skipped",
        }
    }

    /// Read the stored spelling back.
    pub fn parse(text: &str) -> Result<Outcome> {
        Ok(match text {
            "pass" => Outcome::Pass,
            "fail" => Outcome::Fail,
            "error" => Outcome::Error,
            "skipped" => Outcome::Skipped,
            other => return Err(Error::invalid(format!("`{other}` is not a step outcome"))),
        })
    }
}

/// Why a skipped step was skipped — which decides whether it makes the run
/// [`Incomplete`](RunStatus::Incomplete).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The host cannot run it (no browser, no application router). The run
    /// is incomplete: it never goes green by not running.
    Unavailable,
    /// A `core.manual` step: an instruction for a person, listed but never
    /// automated. Does not make the run incomplete — it would otherwise never
    /// pass.
    Manual,
    /// An earlier step ended the run.
    NotReached,
}

impl SkipReason {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Unavailable => "unavailable",
            SkipReason::Manual => "manual",
            SkipReason::NotReached => "not_reached",
        }
    }

    fn parse(text: &str) -> Option<SkipReason> {
        match text {
            "unavailable" => Some(SkipReason::Unavailable),
            "manual" => Some(SkipReason::Manual),
            "not_reached" => Some(SkipReason::NotReached),
            _ => None,
        }
    }
}

/// How one step of a run went, as the run row records it.
#[derive(Debug, Clone, PartialEq)]
pub struct StepResult {
    /// The step version that ran.
    pub step: Uuid,
    /// Its type and label, kept so a run reads the same after the step changes.
    pub type_: String,
    /// Its label.
    pub label: String,
    /// How it went.
    pub outcome: Outcome,
    /// Why it was skipped, for a skipped one.
    pub skip: Option<SkipReason>,
    /// What happened, in a sentence.
    pub message: String,
    /// How long it took.
    pub duration_ms: u64,
    /// The value it captured, if it has an `as`.
    pub captured: Option<Json>,
    /// Console errors seen during the step (browser steps, C2).
    pub console_errors: Vec<String>,
    /// Failed requests seen during the step (browser steps, C2).
    pub failed_requests: Vec<String>,
    /// The screenshot kept for the step — taken when a browser step failed, or
    /// by `browser.screenshot` — as a path in the screenshot store
    /// ([`crate::screenshots`]).
    pub screenshot: Option<String>,
}

impl StepResult {
    /// The stored and served JSON.
    pub fn to_json(&self) -> Json {
        json!({
            "step": self.step.to_string(),
            "type": self.type_,
            "label": self.label,
            "outcome": self.outcome.as_str(),
            "skip": self.skip.map(SkipReason::as_str),
            "message": self.message,
            "duration_ms": self.duration_ms,
            "captured": self.captured,
            "console_errors": self.console_errors,
            "failed_requests": self.failed_requests,
            "screenshot": self.screenshot,
        })
    }

    /// Read a stored one back.
    pub fn from_json(value: &Json) -> Result<StepResult> {
        let text = |key: &str| value.get(key).and_then(Json::as_str).unwrap_or("").to_owned();
        let list = |key: &str| -> Vec<String> {
            value
                .get(key)
                .and_then(Json::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                .unwrap_or_default()
        };
        Ok(StepResult {
            step: text("step")
                .parse()
                .map_err(|_| Error::invalid("a stored step result names no step"))?,
            type_: text("type"),
            label: text("label"),
            outcome: Outcome::parse(&text("outcome"))?,
            skip: value.get("skip").and_then(Json::as_str).and_then(SkipReason::parse),
            message: text("message"),
            duration_ms: value.get("duration_ms").and_then(Json::as_u64).unwrap_or(0),
            captured: value.get("captured").filter(|v| !v.is_null()).cloned(),
            console_errors: list("console_errors"),
            failed_requests: list("failed_requests"),
            screenshot: value
                .get("screenshot")
                .and_then(Json::as_str)
                .map(str::to_owned),
        })
    }
}

/// How a whole run went (§1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// Every step that ran passed, and none that should have run was skipped.
    Passed,
    /// A step's assertion did not hold.
    Failed,
    /// A step could not be carried out, or the run could not start (refused on
    /// production, a `run_as` user that is gone).
    Error,
    /// Nothing failed, but a step the host cannot run was skipped: **never**
    /// reported as passed.
    Incomplete,
}

impl RunStatus {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Passed => "passed",
            RunStatus::Failed => "failed",
            RunStatus::Error => "error",
            RunStatus::Incomplete => "incomplete",
        }
    }

    /// Read the stored spelling back.
    pub fn parse(text: &str) -> Result<RunStatus> {
        Ok(match text {
            "passed" => RunStatus::Passed,
            "failed" => RunStatus::Failed,
            "error" => RunStatus::Error,
            "incomplete" => RunStatus::Incomplete,
            other => return Err(Error::invalid(format!("`{other}` is not a run status"))),
        })
    }

    /// The status of a run whose steps went like this.
    pub fn of(results: &[StepResult]) -> RunStatus {
        if results.iter().any(|r| r.outcome == Outcome::Fail) {
            RunStatus::Failed
        } else if results.iter().any(|r| r.outcome == Outcome::Error) {
            RunStatus::Error
        } else if results
            .iter()
            .any(|r| r.outcome == Outcome::Skipped && r.skip == Some(SkipReason::Unavailable))
        {
            RunStatus::Incomplete
        } else {
            RunStatus::Passed
        }
    }
}

/// One run of one checklist.
#[derive(Debug, Clone, PartialEq)]
pub struct ChecklistRun {
    /// Its `_fd_checklist_runs` row; also `{{ run.id }}` inside the run.
    pub id: Uuid,
    /// The checklist.
    pub checklist: Uuid,
    /// The checklist's name when it ran.
    pub checklist_name: String,
    /// What started it: `manual`, `cli`, `agent:<run id>`, `build`, `schedule`.
    pub trigger: String,
    /// What it ran against: `live`, or a preview label (C2).
    pub target: String,
    /// How it went.
    pub status: RunStatus,
    /// Who it ran as.
    pub run_as: Option<Uuid>,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// When it finished.
    pub finished_at: DateTime<Utc>,
    /// Each step, in order — including the ones not reached.
    pub steps: Vec<StepResult>,
    /// What the clean-up of the rows `data.insert` wrote could not delete.
    pub cleanup_errors: Vec<String>,
    /// Why the run could not start, or anything else worth saying about it.
    pub message: Option<String>,
    /// What the run would have sent — e-mail, webhooks, payment calls — and
    /// kept instead (C4.1), in order.
    pub outbox: Vec<Sent>,
}

impl ChecklistRun {
    /// The step versions it ran.
    pub fn step_versions(&self) -> Vec<Uuid> {
        self.steps.iter().map(|s| s.step).collect()
    }

    /// The run as the admin API and the agent tools show it.
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id.to_string(),
            "checklist": self.checklist.to_string(),
            "checklist_name": self.checklist_name,
            "trigger": self.trigger,
            "target": self.target,
            "status": self.status.as_str(),
            "run_as": self.run_as.map(|u| u.to_string()),
            "started_at": self.started_at.to_rfc3339(),
            "finished_at": self.finished_at.to_rfc3339(),
            "steps": self.steps.iter().map(StepResult::to_json).collect::<Vec<_>>(),
            "step_versions": self.step_versions().iter().map(Uuid::to_string).collect::<Vec<_>>(),
            "cleanup_errors": self.cleanup_errors,
            "message": self.message,
            "outbox": self.outbox.iter().map(Sent::to_json).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(outcome: Outcome, skip: Option<SkipReason>) -> StepResult {
        StepResult {
            step: Uuid::new_v4(),
            type_: "core.wait".into(),
            label: String::new(),
            outcome,
            skip,
            message: String::new(),
            duration_ms: 0,
            captured: None,
            console_errors: vec![],
            failed_requests: vec![],
            screenshot: None,
        }
    }

    #[test]
    fn a_run_is_never_green_by_not_running() {
        use Outcome::*;
        assert_eq!(RunStatus::of(&[result(Pass, None)]), RunStatus::Passed);
        assert_eq!(
            RunStatus::of(&[result(Pass, None), result(Skipped, Some(SkipReason::Unavailable))]),
            RunStatus::Incomplete
        );
        // A manual step is listed, not counted against the run.
        assert_eq!(
            RunStatus::of(&[result(Pass, None), result(Skipped, Some(SkipReason::Manual))]),
            RunStatus::Passed
        );
        assert_eq!(
            RunStatus::of(&[
                result(Fail, None),
                result(Skipped, Some(SkipReason::NotReached))
            ]),
            RunStatus::Failed
        );
        assert_eq!(
            RunStatus::of(&[result(Error, None), result(Skipped, Some(SkipReason::Unavailable))]),
            RunStatus::Error
        );
    }

    #[test]
    fn an_author_round_trips() {
        let human = Author::Human(Uuid::new_v4());
        assert_eq!(Author::parse(&human.to_text()).unwrap(), human);
        let agent = Author::Agent("mcp:abc".into());
        assert_eq!(Author::parse(&agent.to_text()).unwrap(), agent);
        assert!(Author::parse("robot:1").is_err());
        assert!(Author::parse("agent:").is_err());
    }

    #[test]
    fn a_proposal_change_round_trips_through_its_columns() {
        let step = Uuid::new_v4();
        let change = ProposalChange::Edit {
            step,
            draft: StepDraft::new("core.wait").param("ms", 5).label("pause"),
        };
        let back = ProposalChange::from_parts(change.kind(), change.step(), &change.to_json());
        assert_eq!(back.unwrap(), change);
        let reorder = ProposalChange::Reorder {
            order: vec![step, Uuid::new_v4()],
        };
        let back = ProposalChange::from_parts("reorder", None, &reorder.to_json()).unwrap();
        assert_eq!(back, reorder);
    }
}

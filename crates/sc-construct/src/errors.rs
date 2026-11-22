//! The construction's error log (TODO-app-constructor AC10).
//!
//! Every error the construction meets is logged here, with what failed, the
//! agent run, and the construction run it happened in
//! ([`crate::construction_runs`]), so the Activity tab can show them all,
//! newest first. The construction's current error is the newest one logged
//! for its latest construction run ([`current_error`]): a new run has none
//! until it fails, which is what clears it.

use chrono::{DateTime, Utc};
use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Expr, OrderBy, Select, Source, Statement, Value};
use sc_types::BasicType;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::construction_runs::latest_construction_run;
use crate::model::{parse, spelling};
use crate::store::{insert_statement, opt_uuid, req_text, req_uuid, rows};

/// The errors a construction met, one row each.
pub const ERRORS_TABLE: &str = "_fd_construction_errors";

/// The most errors read back: the newest, which are the ones that matter.
pub const MAX_ERRORS_SHOWN: u64 = 100;

pub(crate) fn error_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", BasicType::Uuid)
            .required()
            .primary_key(),
        crate::store::key("construction", crate::store::CONSTRUCTIONS_TABLE).required(),
        DataField::plain("at", BasicType::Timestamp).required(),
        DataField::plain("source", BasicType::Text).required(),
        // No keys on what failed: the log outlives the phases, tasks,
        // feedback and runs it names.
        DataField::plain("phase", BasicType::Uuid),
        DataField::plain("task", BasicType::Uuid),
        // The feedback being planned, for a `feedback` error.
        DataField::plain("feedback", BasicType::Uuid),
        DataField::plain("run", BasicType::Uuid),
        DataField::plain("message", BasicType::Text).required(),
        // The construction run it happened in (AC10.5); none on errors
        // logged before those were recorded.
        crate::store::key(
            "construction_run",
            crate::construction_runs::CONSTRUCTION_RUNS_TABLE,
        ),
    ]
}

/// What failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorSource {
    /// Planning the phases.
    Phases,
    /// Planning one phase's tasks.
    Tasks,
    /// Running one task.
    Task,
    /// Proposing mockups.
    Mockups,
    /// Asking clarifying questions.
    Questions,
    /// Planning a feedback.
    Feedback,
    /// The runner itself: what is not one step's, such as tasks that wait on
    /// each other, or an admin who is no longer one.
    Runner,
}

/// An error to log: what failed, where, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub source: ErrorSource,
    pub phase: Option<Uuid>,
    pub task: Option<Uuid>,
    pub feedback: Option<Uuid>,
    pub run: Option<Uuid>,
    /// The construction run it happened in.
    pub construction_run: Option<Uuid>,
    pub message: String,
}

impl Failure {
    /// A failure of `source` with `message`, not tied to anything yet.
    pub fn new(source: ErrorSource, message: impl Into<String>) -> Self {
        Self {
            source,
            phase: None,
            task: None,
            feedback: None,
            run: None,
            construction_run: None,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn phase(mut self, phase: Uuid) -> Self {
        self.phase = Some(phase);
        self
    }

    #[must_use]
    pub fn task(mut self, task: Uuid) -> Self {
        self.task = Some(task);
        self
    }

    #[must_use]
    pub fn feedback(mut self, feedback: Uuid) -> Self {
        self.feedback = Some(feedback);
        self
    }

    #[must_use]
    pub fn run(mut self, run: Option<Uuid>) -> Self {
        self.run = run;
        self
    }

    /// In construction run `id`, whose current error it then is.
    #[must_use]
    pub fn during(mut self, id: Option<Uuid>) -> Self {
        self.construction_run = id;
        self
    }
}

/// An error the construction met, as logged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConstructionError {
    pub id: Uuid,
    pub construction: Uuid,
    pub at: DateTime<Utc>,
    pub source: ErrorSource,
    pub phase: Option<Uuid>,
    pub task: Option<Uuid>,
    /// The feedback being planned, for a `feedback` error.
    pub feedback: Option<Uuid>,
    pub run: Option<Uuid>,
    pub construction_run: Option<Uuid>,
    pub message: String,
}

/// Set the construction `id`'s columns as `assignments` say, and log
/// `failure` when there is one, in one transaction.
pub(crate) async fn update_logging(
    catalog: &Catalog,
    id: Uuid,
    assignments: Vec<Assignment>,
    failure: Option<&Failure>,
) -> Result<()> {
    let extra = failure
        .map(|f| vec![Statement::from(error_insert(id, f))])
        .unwrap_or_default();
    crate::store::update_construction_with(catalog, id, assignments, extra).await
}

fn error_insert(construction: Uuid, failure: &Failure) -> sc_query::Insert {
    let uuid = |id: Option<Uuid>| id.map_or(Value::Null, Value::Uuid);
    insert_statement(
        ERRORS_TABLE,
        &[
            "id",
            "construction",
            "at",
            "source",
            "phase",
            "task",
            "feedback",
            "run",
            "message",
            "construction_run",
        ],
        vec![
            Value::Uuid(Uuid::new_v4()),
            Value::Uuid(construction),
            Value::Timestamp(Utc::now()),
            Value::Text(spelling(failure.source)),
            uuid(failure.phase),
            uuid(failure.task),
            uuid(failure.feedback),
            uuid(failure.run),
            Value::Text(failure.message.clone()),
            uuid(failure.construction_run),
        ],
    )
}

/// The errors `construction` met, newest first, at most
/// [`MAX_ERRORS_SHOWN`].
pub async fn errors_of(catalog: &Catalog, construction: Uuid) -> Result<Vec<ConstructionError>> {
    // Should not happen: the server bootstraps the table at startup.
    if !matches!(catalog.get(ERRORS_TABLE), Ok(Some(_))) {
        return Ok(Vec::new());
    }
    let mut select = Select::from(Source::table(ERRORS_TABLE))
        .filter(Expr::col("construction").eq(Expr::lit(construction)))
        .limit(MAX_ERRORS_SHOWN);
    select.order = vec![OrderBy::desc(Expr::col("at"))];
    rows(catalog, select)
        .await?
        .iter()
        .map(error_from_row)
        .collect()
}

/// The construction's current error: the newest one logged for its latest
/// construction run, if that run has one.
pub async fn current_error(
    catalog: &Catalog,
    construction: Uuid,
) -> Result<Option<ConstructionError>> {
    let Some(latest) = latest_construction_run(catalog, construction).await? else {
        return Ok(None);
    };
    if !matches!(catalog.get(ERRORS_TABLE), Ok(Some(_))) {
        return Ok(None);
    }
    let mut select = Select::from(Source::table(ERRORS_TABLE))
        .filter(Expr::col("construction_run").eq(Expr::lit(latest.id)))
        .limit(1);
    select.order = vec![OrderBy::desc(Expr::col("at"))];
    let Some(row) = rows(catalog, select).await?.into_iter().next() else {
        return Ok(None);
    };
    error_from_row(&row).map(Some)
}

fn error_from_row(row: &Row) -> Result<ConstructionError> {
    let t = ERRORS_TABLE;
    Ok(ConstructionError {
        id: req_uuid(row, t, "id")?,
        construction: req_uuid(row, t, "construction")?,
        at: match row.get("at") {
            Some(Value::Timestamp(at)) => *at,
            _ => return Err(Error::invalid(format!("{t}.at should be a timestamp"))),
        },
        source: parse("construction error source", &req_text(row, t, "source")?)?,
        phase: opt_uuid(row, t, "phase")?,
        task: opt_uuid(row, t, "task")?,
        feedback: opt_uuid(row, t, "feedback")?,
        run: opt_uuid(row, t, "run")?,
        construction_run: opt_uuid(row, t, "construction_run")?,
        message: req_text(row, t, "message")?,
    })
}

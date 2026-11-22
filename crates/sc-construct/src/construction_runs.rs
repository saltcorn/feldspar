//! The runs a construction starts (TODO-app-constructor AC10.5): one row per
//! run of the construction agent (planning the phases, mockups, questions, a
//! feedback) and per session of the runner (`build`), newest last.
//!
//! What the construction did follows from them, so it is not stored twice:
//! its current error is the error logged for its latest run, if that run has
//! one ([`crate::errors`]), and starting a run is what clears it; the run that
//! planned the phases last is its latest `phases` run's; a feedback is
//! approved once a run planning it has launched, and an error logged in a
//! `feedback` run is that feedback's.

use chrono::{DateTime, Utc};
use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Expr, Insert, OrderBy, Select, Source, Statement, UnOp, Update, Value};
use sc_types::BasicType;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{parse, spelling};
use crate::store::{insert_statement, opt_uuid, req_text, req_uuid, rows, run};

/// The runs a construction started, one row each.
pub const CONSTRUCTION_RUNS_TABLE: &str = "_fd_construction_runs";

pub(crate) fn construction_run_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", BasicType::Uuid)
            .required()
            .primary_key(),
        crate::store::key("construction", crate::store::CONSTRUCTIONS_TABLE).required(),
        DataField::plain("purpose", BasicType::Text).required(),
        // The agent run, once launched; none for a runner session, whose
        // tasks list their own runs. No key: agent runs can be deleted on
        // their own (the Agents screen) while this stays recorded.
        DataField::plain("run", BasicType::Uuid),
        // The feedback a `feedback` run plans. No key: the feedback can be
        // deleted (declined) while its run stays recorded.
        DataField::plain("feedback", BasicType::Uuid),
        DataField::plain("started_at", BasicType::Timestamp).required(),
    ]
}

/// What a construction run is for.
///
/// Tasks have no purpose here: a task keeps its own runs
/// (`_fd_construction_tasks.runs`), and they happen inside a
/// [`Build`](Self::Build) session. Moving them here is a TODO ("Later").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPurpose {
    /// Planning the phases.
    Phases,
    /// Proposing mockups.
    Mockups,
    /// Asking clarifying questions.
    Questions,
    /// Planning a feedback.
    Feedback,
    /// A session of the runner: planning phases' tasks and running them.
    Build,
}

/// A run the construction started.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConstructionRun {
    pub id: Uuid,
    pub construction: Uuid,
    pub purpose: RunPurpose,
    pub run: Option<Uuid>,
    pub feedback: Option<Uuid>,
    pub started_at: DateTime<Utc>,
}

/// The statement that records a new run of `construction` for `purpose`,
/// with the id it gets.
pub(crate) fn construction_run_insert(
    construction: Uuid,
    purpose: RunPurpose,
    feedback: Option<Uuid>,
) -> (Uuid, Insert) {
    let id = Uuid::new_v4();
    let insert = insert_statement(
        CONSTRUCTION_RUNS_TABLE,
        &[
            "id",
            "construction",
            "purpose",
            "run",
            "feedback",
            "started_at",
        ],
        vec![
            Value::Uuid(id),
            Value::Uuid(construction),
            Value::Text(spelling(purpose)),
            Value::Null,
            feedback.map_or(Value::Null, Value::Uuid),
            Value::Timestamp(Utc::now()),
        ],
    );
    (id, insert)
}

/// Insert a run of `construction` for `purpose`, starting now. Being the latest,
/// it leaves the construction with no current error. Answers the row's id.
pub async fn insert_construction_run(
    catalog: &Catalog,
    construction: Uuid,
    purpose: RunPurpose,
    feedback: Option<Uuid>,
) -> Result<Uuid> {
    let (id, insert) = construction_run_insert(construction, purpose, feedback);
    run(catalog, Statement::from(insert)).await?;
    Ok(id)
}

/// Record the agent run that construction run `id` launched.
pub async fn set_agent_run(catalog: &Catalog, id: Uuid, agent_run: Uuid) -> Result<()> {
    let update = Update::new(
        CONSTRUCTION_RUNS_TABLE,
        vec![Assignment::new("run", Expr::Lit(Value::Uuid(agent_run)))],
    )
    .filter(Expr::col("id").eq(Expr::lit(id)));
    run(catalog, Statement::from(update)).await
}

/// The construction's newest run, or `None` if it has none yet.
pub async fn latest_construction_run(
    catalog: &Catalog,
    construction: Uuid,
) -> Result<Option<ConstructionRun>> {
    newest(
        catalog,
        Expr::col("construction").eq(Expr::lit(construction)),
    )
    .await
}

/// The runs `construction` started for `purpose`, newest first.
pub async fn construction_runs_for(
    catalog: &Catalog,
    construction: Uuid,
    purpose: RunPurpose,
) -> Result<Vec<ConstructionRun>> {
    let filter = Expr::col("construction")
        .eq(Expr::lit(construction))
        .and(Expr::col("purpose").eq(Expr::lit(spelling(purpose))));
    select_runs(catalog, filter, None).await
}

/// The agent run that `construction`'s latest launched run for `purpose`
/// started: the newest of them with an agent run, read alone.
pub async fn latest_agent_run(
    catalog: &Catalog,
    construction: Uuid,
    purpose: RunPurpose,
) -> Result<Option<Uuid>> {
    let filter = Expr::col("construction")
        .eq(Expr::lit(construction))
        .and(Expr::col("purpose").eq(Expr::lit(spelling(purpose))))
        .and(Expr::unary(UnOp::IsNotNull, Expr::col("run")));
    Ok(newest(catalog, filter).await?.and_then(|r| r.run))
}

/// The runner session the running `construction` is in: its latest `build`
/// run, or one started now for a construction that has none recorded (left
/// running by a version before this table).
pub async fn current_build(catalog: &Catalog, construction: Uuid) -> Result<Uuid> {
    let build = Expr::col("construction")
        .eq(Expr::lit(construction))
        .and(Expr::col("purpose").eq(Expr::lit(spelling(RunPurpose::Build))));
    match newest(catalog, build).await? {
        Some(session) => Ok(session.id),
        None => insert_construction_run(catalog, construction, RunPurpose::Build, None).await,
    }
}

async fn newest(catalog: &Catalog, filter: Expr) -> Result<Option<ConstructionRun>> {
    Ok(select_runs(catalog, filter, Some(1))
        .await?
        .into_iter()
        .next())
}

/// The runs `filter` picks, newest first, at most `limit`.
async fn select_runs(
    catalog: &Catalog,
    filter: Expr,
    limit: Option<u64>,
) -> Result<Vec<ConstructionRun>> {
    // Should not happen: the server bootstraps the table at startup.
    if !matches!(catalog.get(CONSTRUCTION_RUNS_TABLE), Ok(Some(_))) {
        return Ok(Vec::new());
    }
    let mut select = Select::from(Source::table(CONSTRUCTION_RUNS_TABLE)).filter(filter);
    if let Some(limit) = limit {
        select = select.limit(limit);
    }
    select.order = vec![OrderBy::desc(Expr::col("started_at"))];
    rows(catalog, select)
        .await?
        .iter()
        .map(construction_run_from_row)
        .collect()
}

fn construction_run_from_row(row: &Row) -> Result<ConstructionRun> {
    let t = CONSTRUCTION_RUNS_TABLE;
    Ok(ConstructionRun {
        id: req_uuid(row, t, "id")?,
        construction: req_uuid(row, t, "construction")?,
        purpose: parse("construction run purpose", &req_text(row, t, "purpose")?)?,
        run: opt_uuid(row, t, "run")?,
        feedback: opt_uuid(row, t, "feedback")?,
        started_at: match row.get("started_at") {
            Some(Value::Timestamp(at)) => *at,
            _ => {
                return Err(Error::invalid(format!(
                    "{t}.started_at should be a timestamp"
                )));
            }
        },
    })
}

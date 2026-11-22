//! The construction tables (TODO-app-constructor AC1.1): their schema,
//! bootstrap, and the row mapping.
//!
//! - `_fd_constructions` — one row per application.
//! - `_fd_construction_phases` — a construction's phases, in `position` order.
//!   No status column: a phase's status follows from its tasks.
//! - `_fd_construction_tasks` — a phase's tasks, in `position` order.
//! - `_fd_construction_mockups` — a construction's mockups ([`crate::mockups`]).
//! - `_fd_construction_questions` — its clarifying questions ([`crate::questions`]).
//! - `_fd_construction_feedback` — the feedback on its phases
//!   ([`crate::feedback`]).
//! - `_fd_construction_errors` — the errors it met ([`crate::errors`]).
//! - `_fd_construction_runs` — the runs it started
//!   ([`crate::construction_runs`]).
//!
//! The tables have no foreign keys (the schema layer renders no `ON DELETE`),
//! so [`delete_construction`] removes the tasks and phases itself, and the
//! server calls it when it deletes the application. Reading is strict, as for
//! `_fd_checklists`: a column of the wrong shape is an error naming it.

use std::collections::HashSet;

use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId};
use sc_db::{Row, Transaction};
use sc_error::{Error, Result};
use sc_query::{
    Assignment, Delete, Expr, InSet, Insert, OrderBy, Projection, Select, Source, Statement, Value,
};
use sc_types::BasicType;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::construction_runs::{RunPurpose, construction_run_insert, latest_agent_run};
use crate::errors::{Failure, update_logging};
use crate::model::{
    Construction, ConstructionStatus, Phase, PhaseWithTasks, Requirement, Task, parse, spelling,
};

/// One row per application.
pub const CONSTRUCTIONS_TABLE: &str = "_fd_constructions";
/// A construction's phases.
pub const PHASES_TABLE: &str = "_fd_construction_phases";
/// A phase's tasks.
pub const TASKS_TABLE: &str = "_fd_construction_tasks";

/// A uuid column holding the id of a row of `table`, as a foreign key. Only
/// for a link whose rows are always deleted before the row they point at: the
/// key has no `ON DELETE`, so it refuses to delete a row still pointed at.
pub(crate) fn key(name: &str, table: &str) -> DataField {
    DataField {
        kind: DataFieldKind::Key {
            target_table: table.to_owned(),
            target_field: FieldId("id".to_owned()),
            summary_field: None,
        },
        ..DataField::plain(name, BasicType::Uuid)
    }
}

fn construction_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", BasicType::Uuid)
            .required()
            .primary_key(),
        DataField::plain("application", BasicType::Uuid)
            .required()
            .unique(),
        DataField::plain("specification", BasicType::Text).required(),
        DataField::plain("status", BasicType::Text).required(),
        DataField::plain("agent", BasicType::Text),
        DataField::plain("started_by", BasicType::Uuid),
        // The phase a run ends after (AC3).
        DataField::plain("until_phase", BasicType::Uuid),
        // A run that only plans `until_phase`'s tasks, and what it is told.
        DataField::plain("plan_only", BasicType::Bool),
        DataField::plain("plan_note", BasicType::Text),
        sc_catalog::updated_at_field(),
    ]
}

fn phase_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", BasicType::Uuid)
            .required()
            .primary_key(),
        key("construction", CONSTRUCTIONS_TABLE).required(),
        DataField::plain("position", BasicType::Float).required(),
        DataField::plain("title", BasicType::Text).required(),
        DataField::plain("description", BasicType::Text).required(),
        DataField::plain("requirements", BasicType::Json).required(),
    ]
}

fn task_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", BasicType::Uuid)
            .required()
            .primary_key(),
        key("phase", PHASES_TABLE).required(),
        DataField::plain("position", BasicType::Float).required(),
        DataField::plain("kind", BasicType::Text).required(),
        DataField::plain("title", BasicType::Text).required(),
        DataField::plain("brief", BasicType::Text).required(),
        DataField::plain("depends_on", BasicType::Json).required(),
        DataField::plain("status", BasicType::Text).required(),
        DataField::plain("runs", BasicType::Json).required(),
        DataField::plain("summary", BasicType::Text),
        DataField::plain("created", BasicType::Json).required(),
        // The feedback a follow-up task was planned for (AC9).
        key("feedback", crate::feedback::FEEDBACK_TABLE),
    ]
}

/// Ensure the construction tables exist. Idempotent; called at startup.
pub async fn bootstrap_constructions(catalog: &Catalog) -> Result<()> {
    catalog
        .bootstrap_table(CONSTRUCTIONS_TABLE, &construction_fields())
        .await?;
    catalog
        .bootstrap_table(PHASES_TABLE, &phase_fields())
        .await?;
    // Before the tasks, which point at it.
    catalog
        .bootstrap_table(
            crate::feedback::FEEDBACK_TABLE,
            &crate::feedback::feedback_fields(),
        )
        .await?;
    catalog.bootstrap_table(TASKS_TABLE, &task_fields()).await?;
    catalog
        .bootstrap_table(
            crate::mockups::MOCKUPS_TABLE,
            &crate::mockups::mockup_fields(),
        )
        .await?;
    catalog
        .bootstrap_table(
            crate::questions::QUESTIONS_TABLE,
            &crate::questions::question_fields(),
        )
        .await?;
    // Before the errors, which point at it.
    catalog
        .bootstrap_table(
            crate::construction_runs::CONSTRUCTION_RUNS_TABLE,
            &crate::construction_runs::construction_run_fields(),
        )
        .await?;
    catalog
        .bootstrap_table(crate::errors::ERRORS_TABLE, &crate::errors::error_fields())
        .await?;
    Ok(())
}

/// Whether the tables are there — a process that never bootstrapped them
/// answers "no construction" rather than an error.
pub fn bootstrapped(catalog: &Catalog) -> bool {
    matches!(catalog.get(CONSTRUCTIONS_TABLE), Ok(Some(_)))
}

// --- constructions ------------------------------------------------------------

/// Start the construction of `application`, as a draft with no specification.
/// An application has one construction at most.
pub async fn create_construction(
    catalog: &Catalog,
    application: Uuid,
    agent: Option<&str>,
) -> Result<Construction> {
    let exists = || Error::invalid("this application already has a construction".to_owned());
    if load_construction(catalog, application).await?.is_some() {
        return Err(exists());
    }
    let construction = Construction {
        id: Uuid::new_v4(),
        application,
        specification: String::new(),
        status: ConstructionStatus::Draft,
        agent: agent.map(str::to_owned),
        started_by: None,
        plan_run: None,
        error: None,
        error_entry: None,
        until_phase: None,
        plan_only: false,
        plan_note: None,
        updated_at: None,
    };
    let inserted = insert(
        catalog,
        CONSTRUCTIONS_TABLE,
        &[
            "id",
            "application",
            "specification",
            "status",
            "agent",
            "started_by",
            sc_catalog::COL_UPDATED_AT,
        ],
        vec![
            Value::Uuid(construction.id),
            Value::Uuid(application),
            Value::Text(String::new()),
            Value::Text(spelling(construction.status)),
            opt_text_value(agent),
            Value::Null,
            sc_catalog::updated_now(),
        ],
    )
    .await;
    // The check above and the insert are two statements: a second create in
    // between is stopped by the unique `application` column. Its error is the
    // database's, so it is told the same sentence as the check's.
    if let Err(e) = inserted {
        return Err(match load_construction(catalog, application).await? {
            Some(_) => exists(),
            None => e,
        });
    }
    require_construction(catalog, application).await
}

/// The construction of `application`, if it has one.
pub async fn load_construction(
    catalog: &Catalog,
    application: Uuid,
) -> Result<Option<Construction>> {
    if !bootstrapped(catalog) {
        return Ok(None);
    }
    let select = Select::from(Source::table(CONSTRUCTIONS_TABLE))
        .filter(Expr::col("application").eq(Expr::lit(application)));
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(with_runs(catalog, construction_from_row(row)?).await?)),
        None => Ok(None),
    }
}

/// `construction` with what its runs say: its current error
/// ([`crate::current_error`]) and the run that planned its phases last.
async fn with_runs(catalog: &Catalog, mut construction: Construction) -> Result<Construction> {
    if let Some(entry) = crate::errors::current_error(catalog, construction.id).await? {
        construction.error = Some(entry.message);
        construction.error_entry = Some(entry.id);
    }
    construction.plan_run = latest_agent_run(catalog, construction.id, RunPurpose::Phases).await?;
    Ok(construction)
}

/// [`load_construction`], or a not-found error.
pub async fn require_construction(catalog: &Catalog, application: Uuid) -> Result<Construction> {
    load_construction(catalog, application)
        .await?
        .ok_or_else(|| Error::not_found("this application has no construction".to_owned()))
}

/// Every construction.
pub async fn list_constructions(catalog: &Catalog) -> Result<Vec<Construction>> {
    if !bootstrapped(catalog) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for row in rows(catalog, Select::from(Source::table(CONSTRUCTIONS_TABLE))).await? {
        out.push(with_runs(catalog, construction_from_row(&row)?).await?);
    }
    Ok(out)
}

/// Replace the specification. Only while nothing runs: a running construction
/// is building from the one it has.
pub async fn save_specification(
    catalog: &Catalog,
    application: Uuid,
    specification: &str,
) -> Result<Construction> {
    let construction = require_construction(catalog, application).await?;
    if construction.status == ConstructionStatus::Running {
        return Err(Error::invalid(
            "the construction is running; stop it before changing its specification".to_owned(),
        ));
    }
    update_construction_with(
        catalog,
        construction.id,
        vec![Assignment::new(
            "specification",
            Expr::Lit(Value::Text(specification.to_owned())),
        )],
        Vec::new(),
    )
    .await?;
    require_construction(catalog, application).await
}

/// Log why a run of the construction agent for `application` failed
/// (planning the phases, mockups, questions or feedback), leaving the status
/// as it is: a failed plan changes nothing else. It is the construction's
/// current error while `failure`'s construction run is its latest.
pub async fn set_plan_error(
    catalog: &Catalog,
    application: Uuid,
    failure: &Failure,
) -> Result<Construction> {
    let construction = require_construction(catalog, application).await?;
    update_logging(catalog, construction.id, Vec::new(), Some(failure)).await?;
    require_construction(catalog, application).await
}

/// Mark the construction of `application` as running, for `user`, up to
/// `until_phase` (all phases when `None`), starting a runner session
/// ([`RunPurpose::Build`]), which has no error yet. Only the rows: the runner
/// that does the work is started by the server.
pub async fn mark_running(
    catalog: &Catalog,
    application: Uuid,
    user: Uuid,
    until_phase: Option<Uuid>,
) -> Result<Construction> {
    let construction = require_construction(catalog, application).await?;
    let (_, session) = construction_run_insert(construction.id, RunPurpose::Build, None);
    update_construction_with(
        catalog,
        construction.id,
        vec![
            Assignment::new(
                "status",
                Expr::Lit(Value::Text(spelling(ConstructionStatus::Running))),
            ),
            Assignment::new("started_by", Expr::Lit(Value::Uuid(user))),
            Assignment::new(
                "until_phase",
                Expr::Lit(until_phase.map_or(Value::Null, Value::Uuid)),
            ),
            Assignment::new("plan_only", Expr::Lit(Value::Bool(false))),
            Assignment::new("plan_note", Expr::Lit(Value::Null)),
        ],
        vec![Statement::from(session)],
    )
    .await?;
    require_construction(catalog, application).await
}

/// Mark the construction of `application` as running for `user` to plan the
/// tasks of `phase` only, the planning run told `note` besides the phase.
/// The runner stops once they are planned, leaving them for the admin to
/// review before they run. A runner session, as [`mark_running`]'s.
pub async fn mark_planning(
    catalog: &Catalog,
    application: Uuid,
    user: Uuid,
    phase: Uuid,
    note: Option<&str>,
) -> Result<Construction> {
    let construction = require_construction(catalog, application).await?;
    let (_, session) = construction_run_insert(construction.id, RunPurpose::Build, None);
    update_construction_with(
        catalog,
        construction.id,
        vec![
            Assignment::new(
                "status",
                Expr::Lit(Value::Text(spelling(ConstructionStatus::Running))),
            ),
            Assignment::new("started_by", Expr::Lit(Value::Uuid(user))),
            Assignment::new("until_phase", Expr::Lit(Value::Uuid(phase))),
            Assignment::new("plan_only", Expr::Lit(Value::Bool(true))),
            Assignment::new("plan_note", Expr::Lit(opt_text_value(note))),
        ],
        vec![Statement::from(session)],
    )
    .await?;
    require_construction(catalog, application).await
}

/// Set the status of the construction of `application`, logging why when it
/// stopped or failed. A reason is the current error while its construction
/// run is the latest; `None` logs nothing and clears nothing, as starting a
/// run is what clears it.
pub async fn set_construction_status(
    catalog: &Catalog,
    application: Uuid,
    status: ConstructionStatus,
    failure: Option<&Failure>,
) -> Result<Construction> {
    let construction = require_construction(catalog, application).await?;
    update_logging(
        catalog,
        construction.id,
        vec![Assignment::new(
            "status",
            Expr::Lit(Value::Text(spelling(status))),
        )],
        failure,
    )
    .await?;
    require_construction(catalog, application).await
}

/// The constructions whose status is `running`: what the runner resumes at
/// startup.
pub async fn running_constructions(catalog: &Catalog) -> Result<Vec<Construction>> {
    Ok(list_constructions(catalog)
        .await?
        .into_iter()
        .filter(|c| c.status == ConstructionStatus::Running)
        .collect())
}

/// Delete the construction of `application` with its phases, tasks, mockups,
/// questions, feedback, errors and runs, answering what it was. `None` when it had none.
///
/// All in one transaction: a failure deletes nothing. The phases are locked
/// first, so a task being added to one of them is either deleted with it or
/// refused ([`insert_task`]).
pub async fn delete_construction(
    catalog: &Catalog,
    application: Uuid,
) -> Result<Option<Construction>> {
    if !bootstrapped(catalog) {
        return Ok(None);
    }
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| {
            Box::pin(async move {
                let select = Select::from(Source::table(CONSTRUCTIONS_TABLE))
                    .filter(Expr::col("application").eq(Expr::lit(application)))
                    .for_update();
                let Some(construction) = tx_rows(tx, select)
                    .await?
                    .first()
                    .map(construction_from_row)
                    .transpose()?
                else {
                    return Ok(None);
                };
                let phases: Vec<Expr> = lock_phases(tx, construction.id)
                    .await?
                    .iter()
                    .map(|p| Expr::lit(p.id))
                    .collect();
                if !phases.is_empty() {
                    for table in [TASKS_TABLE, crate::feedback::FEEDBACK_TABLE] {
                        let delete = Delete::from(table).filter(Expr::In {
                            e: Box::new(Expr::col("phase")),
                            set: InSet::List(phases.clone()),
                        });
                        tx_run(tx, &Statement::from(delete)).await?;
                    }
                }
                for (table, column) in [
                    (crate::mockups::MOCKUPS_TABLE, "construction"),
                    (crate::questions::QUESTIONS_TABLE, "construction"),
                    (crate::errors::ERRORS_TABLE, "construction"),
                    (
                        crate::construction_runs::CONSTRUCTION_RUNS_TABLE,
                        "construction",
                    ),
                    (PHASES_TABLE, "construction"),
                    (CONSTRUCTIONS_TABLE, "id"),
                ] {
                    let delete = Delete::from(table)
                        .filter(Expr::col(column).eq(Expr::lit(construction.id)));
                    tx_run(tx, &Statement::from(delete)).await?;
                }
                Ok(Some(construction))
            })
        },
    )
    .await
}

/// Set the construction `id`'s columns as `assignments` say and run `extra`,
/// in one transaction.
pub(crate) async fn update_construction_with(
    catalog: &Catalog,
    id: Uuid,
    mut assignments: Vec<Assignment>,
    extra: Vec<Statement>,
) -> Result<()> {
    assignments.push(Assignment::new(
        sc_catalog::COL_UPDATED_AT,
        Expr::Lit(sc_catalog::updated_now()),
    ));
    let update = Statement::from(
        sc_query::Update::new(CONSTRUCTIONS_TABLE, assignments)
            .filter(Expr::col("id").eq(Expr::lit(id))),
    );
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| {
            Box::pin(async move {
                tx_run(tx, &update).await?;
                for statement in &extra {
                    tx_run(tx, statement).await?;
                }
                Ok(())
            })
        },
    )
    .await
}

// --- phases -------------------------------------------------------------------

/// Add `phase` to its construction.
pub async fn insert_phase(catalog: &Catalog, phase: &Phase) -> Result<()> {
    run(catalog, Statement::from(phase_insert(phase)?)).await
}

/// The statement that adds `phase`.
pub(crate) fn phase_insert(phase: &Phase) -> Result<Insert> {
    Ok(insert_statement(
        PHASES_TABLE,
        &[
            "id",
            "construction",
            "position",
            "title",
            "description",
            "requirements",
        ],
        vec![
            Value::Uuid(phase.id),
            Value::Uuid(phase.construction),
            Value::Float(phase.position),
            Value::Text(phase.title.clone()),
            Value::Text(phase.description.clone()),
            Value::Json(to_json(&phase.requirements)?),
        ],
    ))
}

/// A construction's phases, in order, each with its tasks: the one loader of
/// a phase's status ([`PhaseWithTasks::status`]).
pub async fn phases_with_tasks(
    catalog: &Catalog,
    construction: Uuid,
) -> Result<Vec<PhaseWithTasks>> {
    let mut out = Vec::new();
    for phase in phases_of(catalog, construction).await? {
        let tasks = tasks_of_phase(catalog, phase.id).await?;
        out.push(PhaseWithTasks { phase, tasks });
    }
    Ok(out)
}

/// A construction's phases, in order.
pub async fn phases_of(catalog: &Catalog, construction: Uuid) -> Result<Vec<Phase>> {
    rows(catalog, phases_select(construction))
        .await?
        .iter()
        .map(phase_from_row)
        .collect()
}

/// `construction`'s phases, in order, read in `tx` and locked until it ends:
/// a task for one of them waits for `tx` ([`insert_task`]).
pub(crate) async fn lock_phases(
    tx: &mut dyn Transaction,
    construction: Uuid,
) -> Result<Vec<Phase>> {
    tx_rows(tx, phases_select(construction).for_update())
        .await?
        .iter()
        .map(phase_from_row)
        .collect()
}

/// Which of `phases` have at least one task, read in `tx`.
pub(crate) async fn phase_ids_with_tasks(
    tx: &mut dyn Transaction,
    phases: &[Uuid],
) -> Result<HashSet<Uuid>> {
    if phases.is_empty() {
        return Ok(HashSet::new());
    }
    let select = Select::from(Source::table(TASKS_TABLE))
        .columns(vec![Projection::expr(Expr::col("phase"))])
        .filter(Expr::In {
            e: Box::new(Expr::col("phase")),
            set: InSet::List(phases.iter().map(|id| Expr::lit(*id)).collect()),
        });
    tx_rows(tx, select)
        .await?
        .iter()
        .map(|row| req_uuid(row, TASKS_TABLE, "phase"))
        .collect()
}

/// `phase`'s tasks, in order, read in `tx`.
pub(crate) async fn tasks_in(tx: &mut dyn Transaction, phase: Uuid) -> Result<Vec<Task>> {
    tx_rows(tx, tasks_select(phase))
        .await?
        .iter()
        .map(task_from_row)
        .collect()
}

fn tasks_select(phase: Uuid) -> Select {
    let mut select =
        Select::from(Source::table(TASKS_TABLE)).filter(Expr::col("phase").eq(Expr::lit(phase)));
    select.order = vec![OrderBy::asc(Expr::col("position"))];
    select
}

fn phases_select(construction: Uuid) -> Select {
    let mut select = Select::from(Source::table(PHASES_TABLE))
        .filter(Expr::col("construction").eq(Expr::lit(construction)));
    select.order = vec![OrderBy::asc(Expr::col("position"))];
    select
}

/// Delete a phase that has no tasks, answering whether it existed. One with
/// tasks is refused: its tasks record what was built for it.
///
/// The phase is locked before its tasks are counted, so a task being added
/// meanwhile is either counted or refused ([`insert_task`]).
pub async fn delete_phase(catalog: &Catalog, id: Uuid) -> Result<bool> {
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| {
            Box::pin(async move {
                let select = Select::from(Source::table(PHASES_TABLE))
                    .filter(Expr::col("id").eq(Expr::lit(id)))
                    .for_update();
                if tx_rows(tx, select).await?.is_empty() {
                    return Ok(false);
                }
                if !phase_ids_with_tasks(tx, &[id]).await?.is_empty() {
                    return Err(Error::invalid(
                        "this phase has tasks, so it cannot be deleted".to_owned(),
                    ));
                }
                let delete = Delete::from(PHASES_TABLE).filter(Expr::col("id").eq(Expr::lit(id)));
                tx_run(tx, &Statement::from(delete)).await?;
                Ok(true)
            })
        },
    )
    .await
}

// --- tasks --------------------------------------------------------------------

/// Add `task` to its phase. Refused when the phase does not exist.
pub async fn insert_task(catalog: &Catalog, task: &Task) -> Result<()> {
    let insert = task_insert(task)?;
    // The phase is locked first: a save of the phases that would delete it
    // either sees this task or has deleted the phase, and the insert is
    // refused rather than left without one (`plan::save_phases`).
    let phase = task.phase;
    sc_db::with_tx(
        catalog.primary(crate::INTERNAL_TABLES).as_ref(),
        move |tx| {
            Box::pin(async move {
                let select = Select::from(Source::table(PHASES_TABLE))
                    .filter(Expr::col("id").eq(Expr::lit(phase)))
                    .for_update();
                if tx_rows(tx, select).await?.is_empty() {
                    return Err(Error::invalid(format!(
                        "phase {phase} does not exist; it may have been deleted from the plan"
                    )));
                }
                tx_run(tx, &Statement::from(insert)).await
            })
        },
    )
    .await
}

/// The `INSERT` of `task`, for a caller that writes it in its own transaction
/// with its phase locked, as [`insert_task`] does.
pub fn task_insert(task: &Task) -> Result<Insert> {
    Ok(insert_statement(
        TASKS_TABLE,
        &[
            "id",
            "phase",
            "position",
            "kind",
            "title",
            "brief",
            "depends_on",
            "status",
            "runs",
            "summary",
            "created",
            "feedback",
        ],
        vec![
            Value::Uuid(task.id),
            Value::Uuid(task.phase),
            Value::Float(task.position),
            Value::Text(spelling(task.kind)),
            Value::Text(task.title.clone()),
            Value::Text(task.brief.clone()),
            Value::Json(to_json(&task.depends_on)?),
            Value::Text(spelling(task.status)),
            Value::Json(to_json(&task.runs)?),
            opt_text_value(task.summary.as_deref()),
            Value::Json(Json::Array(task.created.clone())),
            task.feedback.map_or(Value::Null, Value::Uuid),
        ],
    ))
}

/// Write what the runner changes on a task: its status, its runs, its summary
/// and what it created.
pub async fn update_task(catalog: &Catalog, task: &Task) -> Result<()> {
    let update = sc_query::Update::new(
        TASKS_TABLE,
        vec![
            Assignment::new("status", Expr::Lit(Value::Text(spelling(task.status)))),
            Assignment::new("runs", Expr::Lit(Value::Json(to_json(&task.runs)?))),
            Assignment::new(
                "summary",
                Expr::Lit(opt_text_value(task.summary.as_deref())),
            ),
            Assignment::new(
                "created",
                Expr::Lit(Value::Json(Json::Array(task.created.clone()))),
            ),
        ],
    )
    .filter(Expr::col("id").eq(Expr::lit(task.id)));
    run(catalog, Statement::from(update)).await
}

/// The task with `id`, if there is one.
pub async fn load_task(catalog: &Catalog, id: Uuid) -> Result<Option<Task>> {
    let select = Select::from(Source::table(TASKS_TABLE)).filter(Expr::col("id").eq(Expr::lit(id)));
    rows(catalog, select)
        .await?
        .first()
        .map(task_from_row)
        .transpose()
}

/// A phase's tasks, in order.
pub async fn tasks_of_phase(catalog: &Catalog, phase: Uuid) -> Result<Vec<Task>> {
    rows(catalog, tasks_select(phase))
        .await?
        .iter()
        .map(task_from_row)
        .collect()
}

// --- rows ---------------------------------------------------------------------

fn construction_from_row(row: &Row) -> Result<Construction> {
    let t = CONSTRUCTIONS_TABLE;
    Ok(Construction {
        id: req_uuid(row, t, "id")?,
        application: req_uuid(row, t, "application")?,
        specification: req_text(row, t, "specification")?,
        status: parse("construction status", &req_text(row, t, "status")?)?,
        agent: opt_text(row, t, "agent")?,
        started_by: opt_uuid(row, t, "started_by")?,
        // From its runs ([`with_runs`]).
        plan_run: None,
        error: None,
        error_entry: None,
        until_phase: opt_uuid(row, t, "until_phase")?,
        // Absent (NULL) on rows written before the column: an ordinary run.
        plan_only: opt_bool(row, t, "plan_only")?.unwrap_or(false),
        plan_note: opt_text(row, t, "plan_note")?,
        updated_at: match row.get(sc_catalog::COL_UPDATED_AT) {
            Some(Value::Timestamp(at)) => Some(*at),
            Some(Value::Null) | None => None,
            other => return Err(bad(t, sc_catalog::COL_UPDATED_AT, "a timestamp", other)),
        },
    })
}

fn phase_from_row(row: &Row) -> Result<Phase> {
    let t = PHASES_TABLE;
    Ok(Phase {
        id: req_uuid(row, t, "id")?,
        construction: req_uuid(row, t, "construction")?,
        position: req_float(row, t, "position")?,
        title: req_text(row, t, "title")?,
        description: req_text(row, t, "description")?,
        requirements: from_json::<Vec<Requirement>>(row, t, "requirements")?,
    })
}

fn task_from_row(row: &Row) -> Result<Task> {
    let t = TASKS_TABLE;
    Ok(Task {
        id: req_uuid(row, t, "id")?,
        phase: req_uuid(row, t, "phase")?,
        position: req_float(row, t, "position")?,
        kind: parse("task kind", &req_text(row, t, "kind")?)?,
        title: req_text(row, t, "title")?,
        brief: req_text(row, t, "brief")?,
        depends_on: from_json(row, t, "depends_on")?,
        status: parse("task status", &req_text(row, t, "status")?)?,
        runs: from_json(row, t, "runs")?,
        summary: opt_text(row, t, "summary")?,
        created: from_json(row, t, "created")?,
        // Absent (NULL) on tasks written before the column: the phase's own.
        feedback: opt_uuid(row, t, "feedback")?,
    })
}

// --- helpers ------------------------------------------------------------------

pub(crate) fn opt_text_value(text: Option<&str>) -> Value {
    text.map_or(Value::Null, |t| Value::Text(t.to_owned()))
}

pub(crate) fn to_json<T: serde::Serialize>(value: &T) -> Result<Json> {
    serde_json::to_value(value).map_err(|e| Error::serde(e.to_string()))
}

pub(crate) fn from_json<T: serde::de::DeserializeOwned>(
    row: &Row,
    table: &str,
    column: &str,
) -> Result<T> {
    match row.get(column) {
        Some(Value::Json(value)) => serde_json::from_value(value.clone())
            .map_err(|e| Error::invalid(format!("{table}.{column} is malformed: {e}"))),
        other => Err(bad(table, column, "json", other)),
    }
}

pub(crate) fn req_uuid(row: &Row, table: &str, column: &str) -> Result<Uuid> {
    match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        other => Err(bad(table, column, "a uuid", other)),
    }
}

pub(crate) fn opt_uuid(row: &Row, table: &str, column: &str) -> Result<Option<Uuid>> {
    match row.get(column) {
        Some(Value::Uuid(u)) => Ok(Some(*u)),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad(table, column, "a uuid", other)),
    }
}

pub(crate) fn req_text(row: &Row, table: &str, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad(table, column, "text", other)),
    }
}

pub(crate) fn opt_text(row: &Row, table: &str, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(Value::Text(t)) if t.is_empty() => Ok(None),
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad(table, column, "text", other)),
    }
}

fn opt_bool(row: &Row, table: &str, column: &str) -> Result<Option<bool>> {
    match row.get(column) {
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad(table, column, "a boolean", other)),
    }
}

pub(crate) fn req_float(row: &Row, table: &str, column: &str) -> Result<f64> {
    match row.get(column) {
        Some(Value::Float(f)) => Ok(*f),
        #[allow(clippy::cast_precision_loss)]
        Some(Value::Int(i)) => Ok(*i as f64),
        other => Err(bad(table, column, "a number", other)),
    }
}

fn bad(table: &str, column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{table}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("{table} row has no `{column}` column")),
    }
}

async fn insert(
    catalog: &Catalog,
    table: &str,
    columns: &[&str],
    values: Vec<Value>,
) -> Result<()> {
    run(
        catalog,
        Statement::from(insert_statement(table, columns, values)),
    )
    .await
}

pub(crate) fn insert_statement(table: &str, columns: &[&str], values: Vec<Value>) -> Insert {
    Insert::row(
        table,
        columns.iter().map(|c| (*c).to_owned()).collect(),
        values.into_iter().map(Expr::Lit).collect(),
    )
}

pub(crate) async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run `statement` in `tx`.
pub(crate) async fn tx_run(tx: &mut dyn Transaction, statement: &Statement) -> Result<()> {
    tx.query(statement).await?.try_collect().await?;
    Ok(())
}

pub(crate) async fn tx_rows(tx: &mut dyn Transaction, select: Select) -> Result<Vec<Row>> {
    tx.query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

pub(crate) async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

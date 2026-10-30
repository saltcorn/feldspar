//! The checklist tables (docs/CHECKLISTS.md §1, TODO C1.1): their schema,
//! bootstrap, and the row mapping.
//!
//! - `_fd_checklists` — one row per checklist; its row *is* its definition.
//! - `_fd_checklist_steps` — every **version** of every step. A step is never
//!   updated in place: an edit inserts a new row and points the old one at it
//!   (`superseded_by`), and a deletion points it at a **tombstone** (a row with
//!   `deleted`), so the history of every check is kept and a deletion can be
//!   undone from it. The only columns ever updated are that pointer and
//!   `accepted`.
//! - `_fd_checklist_runs` — one row per run, with the step versions it ran
//!   and each step's result.
//! - `_fd_checklist_proposals` — an agent's change to an existing step that a
//!   person has not yet accepted or rejected (§4 rule 3).
//!
//! Reading is strict, as for `_fd_agents`: a column of the wrong shape is an
//! error naming it.

use chrono::{DateTime, Utc};
use sc_catalog::outbox::Sent;
use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, OrderBy, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::model::{
    Author, Checklist, ChecklistRun, Proposal, ProposalChange, RunStatus, Step, StepDraft,
    StepResult,
};

/// The checklists.
pub const CHECKLISTS_TABLE: &str = "_fd_checklists";
/// Every version of every step.
pub const STEPS_TABLE: &str = "_fd_checklist_steps";
/// One row per run.
pub const RUNS_TABLE: &str = "_fd_checklist_runs";
/// Pending proposals.
pub const PROPOSALS_TABLE: &str = "_fd_checklist_proposals";

fn uuid() -> TypeRef {
    TypeRef::Basic(BasicType::Uuid)
}
fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}
fn json() -> TypeRef {
    TypeRef::Basic(BasicType::Json)
}
fn boolean() -> TypeRef {
    TypeRef::Basic(BasicType::Bool)
}
fn timestamp() -> TypeRef {
    TypeRef::Basic(BasicType::Timestamp)
}

fn checklist_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", uuid()).required().primary_key(),
        DataField::plain("name", text()).required().unique(),
        DataField::plain("description", text()),
        DataField::plain("application", uuid()),
        DataField::plain("run_as", uuid()),
        DataField::plain("enabled", boolean()).required(),
        DataField::plain("mutates", boolean()).required(),
        DataField::plain("attributes", json()).required(),
        sc_catalog::updated_at_field(),
    ]
}

fn step_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", uuid()).required().primary_key(),
        DataField::plain("checklist", uuid()).required(),
        DataField::plain("position", TypeRef::Basic(BasicType::Float)).required(),
        DataField::plain("type", text()).required(),
        DataField::plain("params", json()).required(),
        DataField::plain("label", text()),
        // The step's `as`: the variable its value is captured into.
        DataField::plain("capture", text()),
        DataField::plain("author", text()).required(),
        DataField::plain("accepted", boolean()).required(),
        DataField::plain("superseded_by", uuid()),
        DataField::plain("deleted", boolean()).required(),
        DataField::plain("created_at", timestamp()).required(),
    ]
}

fn run_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", uuid()).required().primary_key(),
        DataField::plain("checklist", uuid()).required(),
        DataField::plain("checklist_name", text()).required(),
        DataField::plain("trigger", text()).required(),
        DataField::plain("target", text()).required(),
        DataField::plain("status", text()).required(),
        DataField::plain("run_as", uuid()),
        DataField::plain("started_at", timestamp()).required(),
        DataField::plain("finished_at", timestamp()).required(),
        DataField::plain("step_versions", json()).required(),
        DataField::plain("steps", json()).required(),
        DataField::plain("cleanup_errors", json()).required(),
        DataField::plain("message", text()),
        // What the run kept instead of sending (C4.1). NULL reads as nothing.
        DataField::plain("outbox", json()),
    ]
}

fn proposal_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", uuid()).required().primary_key(),
        DataField::plain("checklist", uuid()).required(),
        DataField::plain("kind", text()).required(),
        DataField::plain("step", uuid()),
        DataField::plain("change", json()).required(),
        DataField::plain("reason", text()).required(),
        DataField::plain("author", text()).required(),
        DataField::plain("created_at", timestamp()).required(),
    ]
}

/// Ensure the four tables exist. Idempotent; called at startup.
pub async fn bootstrap_checklists(catalog: &Catalog) -> Result<()> {
    catalog
        .bootstrap_table(CHECKLISTS_TABLE, &checklist_fields())
        .await?;
    catalog.bootstrap_table(STEPS_TABLE, &step_fields()).await?;
    catalog.bootstrap_table(RUNS_TABLE, &run_fields()).await?;
    catalog
        .bootstrap_table(PROPOSALS_TABLE, &proposal_fields())
        .await?;
    Ok(())
}

/// Whether the tables are there — a process that never bootstrapped them (a
/// unit test, a tool) answers "no checklists" rather than an error.
pub fn bootstrapped(catalog: &Catalog) -> bool {
    matches!(catalog.get(CHECKLISTS_TABLE), Ok(Some(_)))
}

// --- checklists ---------------------------------------------------------------

/// Insert `checklist`, or update it in place when its row exists.
pub(crate) async fn write_checklist(catalog: &Catalog, checklist: &Checklist) -> Result<()> {
    let columns = [
        "id",
        "name",
        "description",
        "application",
        "run_as",
        "enabled",
        "mutates",
        "attributes",
        sc_catalog::COL_UPDATED_AT,
    ];
    let values = vec![
        Value::Uuid(checklist.id),
        Value::Text(checklist.name.trim().to_owned()),
        Value::Text(checklist.description.clone()),
        opt_uuid(checklist.application),
        opt_uuid(checklist.run_as),
        Value::Bool(checklist.enabled),
        Value::Bool(checklist.mutates),
        Value::Json(Json::Object(checklist.attributes.clone())),
        sc_catalog::updated_now(),
    ];
    if load_checklist(catalog, checklist.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            .skip(1)
            .map(|(col, value)| Assignment::new(*col, Expr::Lit(value)))
            .collect();
        let update = sc_query::Update::new(CHECKLISTS_TABLE, assignments)
            .filter(Expr::col("id").eq(Expr::lit(checklist.id)));
        run(catalog, Statement::from(update)).await
    } else {
        insert(catalog, CHECKLISTS_TABLE, &columns, values).await
    }
}

/// The checklist with this id.
pub async fn load_checklist(catalog: &Catalog, id: Uuid) -> Result<Option<Checklist>> {
    first(
        catalog,
        CHECKLISTS_TABLE,
        Expr::col("id").eq(Expr::lit(id)),
        checklist_from_row,
    )
    .await
}

/// The checklist called `name`.
pub async fn load_checklist_by_name(catalog: &Catalog, name: &str) -> Result<Option<Checklist>> {
    first(
        catalog,
        CHECKLISTS_TABLE,
        Expr::col("name").eq(Expr::lit(name.trim())),
        checklist_from_row,
    )
    .await
}

/// The checklist with this id, or a not-found naming it.
pub async fn require_checklist(catalog: &Catalog, id: Uuid) -> Result<Checklist> {
    load_checklist(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no checklist with id {id}")))
}

/// The checklist named by `key` — its name, or its id.
pub async fn find_checklist(catalog: &Catalog, key: &str) -> Result<Checklist> {
    if let Some(found) = load_checklist_by_name(catalog, key).await? {
        return Ok(found);
    }
    if let Ok(id) = key.trim().parse::<Uuid>()
        && let Some(found) = load_checklist(catalog, id).await?
    {
        return Ok(found);
    }
    Err(Error::not_found(format!("no checklist called `{key}`")))
}

/// Which checklists to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecklistScope {
    /// Every checklist.
    All,
    /// The checklists of one application.
    Application(Uuid),
    /// The checklists with no application.
    DataOnly,
}

/// The checklists in `scope`, by name.
pub async fn list_checklists(catalog: &Catalog, scope: ChecklistScope) -> Result<Vec<Checklist>> {
    if !bootstrapped(catalog) {
        return Ok(Vec::new());
    }
    let mut out: Vec<Checklist> = rows(catalog, Select::from(Source::table(CHECKLISTS_TABLE)))
        .await?
        .iter()
        .map(checklist_from_row)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|c| match scope {
            ChecklistScope::All => true,
            ChecklistScope::Application(app) => c.application == Some(app),
            ChecklistScope::DataOnly => c.application.is_none(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Delete a checklist, its steps (every version) and its proposals. Its runs
/// are kept: what happened does not stop having happened.
pub(crate) async fn delete_checklist_rows(catalog: &Catalog, id: Uuid) -> Result<()> {
    for (table, column) in [
        (PROPOSALS_TABLE, "checklist"),
        (STEPS_TABLE, "checklist"),
        (CHECKLISTS_TABLE, "id"),
    ] {
        let delete = Delete::from(table).filter(Expr::col(column).eq(Expr::lit(id)));
        run(catalog, Statement::from(delete)).await?;
    }
    Ok(())
}

/// Record whether the checklist's current steps write data.
pub(crate) async fn set_mutates(catalog: &Catalog, id: Uuid, mutates: bool) -> Result<()> {
    let update = sc_query::Update::new(
        CHECKLISTS_TABLE,
        vec![Assignment::new("mutates", Expr::lit(mutates))],
    )
    .filter(Expr::col("id").eq(Expr::lit(id)));
    run(catalog, Statement::from(update)).await
}

fn checklist_from_row(row: &Row) -> Result<Checklist> {
    Ok(Checklist {
        id: req_uuid(row, CHECKLISTS_TABLE, "id")?,
        name: req_text(row, CHECKLISTS_TABLE, "name")?,
        description: opt_text(row, CHECKLISTS_TABLE, "description")?.unwrap_or_default(),
        application: opt_uuid_col(row, CHECKLISTS_TABLE, "application")?,
        run_as: opt_uuid_col(row, CHECKLISTS_TABLE, "run_as")?,
        enabled: req_bool(row, CHECKLISTS_TABLE, "enabled")?,
        mutates: req_bool(row, CHECKLISTS_TABLE, "mutates")?,
        attributes: req_object(row, CHECKLISTS_TABLE, "attributes")?,
    })
}

// --- steps --------------------------------------------------------------------

/// Write one step version.
pub(crate) async fn insert_step(catalog: &Catalog, step: &Step) -> Result<()> {
    insert(
        catalog,
        STEPS_TABLE,
        &[
            "id",
            "checklist",
            "position",
            "type",
            "params",
            "label",
            "capture",
            "author",
            "accepted",
            "superseded_by",
            "deleted",
            "created_at",
        ],
        vec![
            Value::Uuid(step.id),
            Value::Uuid(step.checklist),
            Value::Float(step.position),
            Value::Text(step.draft.type_.clone()),
            Value::Json(Json::Object(step.draft.params.clone())),
            Value::Text(step.draft.label.clone()),
            step.draft.capture.clone().map_or(Value::Null, Value::Text),
            Value::Text(step.author.to_text()),
            Value::Bool(step.accepted),
            opt_uuid(step.superseded_by),
            Value::Bool(step.deleted),
            Value::Timestamp(step.created_at),
        ],
    )
    .await
}

/// Point `old` at the version that replaces it.
pub(crate) async fn supersede(catalog: &Catalog, old: Uuid, new: Uuid) -> Result<()> {
    let update = sc_query::Update::new(
        STEPS_TABLE,
        vec![Assignment::new("superseded_by", Expr::lit(new))],
    )
    .filter(Expr::col("id").eq(Expr::lit(old)));
    run(catalog, Statement::from(update)).await
}

/// Mark a step version seen by a person.
pub(crate) async fn set_accepted(catalog: &Catalog, id: Uuid) -> Result<()> {
    let update = sc_query::Update::new(
        STEPS_TABLE,
        vec![Assignment::new("accepted", Expr::lit(true))],
    )
    .filter(Expr::col("id").eq(Expr::lit(id)));
    run(catalog, Statement::from(update)).await
}

/// One step version.
pub async fn load_step(catalog: &Catalog, id: Uuid) -> Result<Option<Step>> {
    first(
        catalog,
        STEPS_TABLE,
        Expr::col("id").eq(Expr::lit(id)),
        step_from_row,
    )
    .await
}

/// One step version, or a not-found.
pub async fn require_step(catalog: &Catalog, id: Uuid) -> Result<Step> {
    load_step(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no checklist step with id {id}")))
}

/// The checklist's **current** steps, in the order they run.
pub async fn current_steps(catalog: &Catalog, checklist: Uuid) -> Result<Vec<Step>> {
    Ok(step_history(catalog, checklist)
        .await?
        .into_iter()
        .filter(Step::is_current)
        .collect())
}

/// Every version of every step of the checklist, tombstones included, by
/// position and then by when each was written.
pub async fn step_history(catalog: &Catalog, checklist: Uuid) -> Result<Vec<Step>> {
    let select = Select::from(Source::table(STEPS_TABLE))
        .filter(Expr::col("checklist").eq(Expr::lit(checklist)));
    let mut steps: Vec<Step> = rows(catalog, select)
        .await?
        .iter()
        .map(step_from_row)
        .collect::<Result<_>>()?;
    steps.sort_by(|a, b| {
        a.position
            .total_cmp(&b.position)
            .then(a.created_at.cmp(&b.created_at))
    });
    Ok(steps)
}

fn step_from_row(row: &Row) -> Result<Step> {
    let t = STEPS_TABLE;
    Ok(Step {
        id: req_uuid(row, t, "id")?,
        checklist: req_uuid(row, t, "checklist")?,
        position: match row.get("position") {
            Some(Value::Float(f)) => *f,
            Some(Value::Int(i)) => *i as f64,
            other => return Err(bad(t, "position", "a number", other)),
        },
        draft: StepDraft {
            type_: req_text(row, t, "type")?,
            params: req_object(row, t, "params")?,
            label: opt_text(row, t, "label")?.unwrap_or_default(),
            capture: opt_text(row, t, "capture")?,
        },
        author: Author::parse(&req_text(row, t, "author")?)?,
        accepted: req_bool(row, t, "accepted")?,
        superseded_by: opt_uuid_col(row, t, "superseded_by")?,
        deleted: req_bool(row, t, "deleted")?,
        created_at: req_time(row, t, "created_at")?,
    })
}

// --- proposals ----------------------------------------------------------------

/// Store a proposal.
pub(crate) async fn insert_proposal(catalog: &Catalog, proposal: &Proposal) -> Result<()> {
    insert(
        catalog,
        PROPOSALS_TABLE,
        &[
            "id",
            "checklist",
            "kind",
            "step",
            "change",
            "reason",
            "author",
            "created_at",
        ],
        vec![
            Value::Uuid(proposal.id),
            Value::Uuid(proposal.checklist),
            Value::Text(proposal.change.kind().to_owned()),
            opt_uuid(proposal.change.step()),
            Value::Json(proposal.change.to_json()),
            Value::Text(proposal.reason.clone()),
            Value::Text(proposal.author.to_text()),
            Value::Timestamp(proposal.created_at),
        ],
    )
    .await
}

/// One proposal.
pub async fn load_proposal(catalog: &Catalog, id: Uuid) -> Result<Option<Proposal>> {
    first(
        catalog,
        PROPOSALS_TABLE,
        Expr::col("id").eq(Expr::lit(id)),
        proposal_from_row,
    )
    .await
}

/// The checklist's pending proposals, oldest first.
pub async fn proposals_of(catalog: &Catalog, checklist: Uuid) -> Result<Vec<Proposal>> {
    let mut select = Select::from(Source::table(PROPOSALS_TABLE))
        .filter(Expr::col("checklist").eq(Expr::lit(checklist)));
    select.order = vec![OrderBy::asc(Expr::col("created_at"))];
    rows(catalog, select)
        .await?
        .iter()
        .map(proposal_from_row)
        .collect()
}

/// Discard a proposal.
pub(crate) async fn delete_proposal(catalog: &Catalog, id: Uuid) -> Result<()> {
    let delete = Delete::from(PROPOSALS_TABLE).filter(Expr::col("id").eq(Expr::lit(id)));
    run(catalog, Statement::from(delete)).await
}

fn proposal_from_row(row: &Row) -> Result<Proposal> {
    let t = PROPOSALS_TABLE;
    let change = match row.get("change") {
        Some(Value::Json(j)) => j.clone(),
        other => return Err(bad(t, "change", "json", other)),
    };
    Ok(Proposal {
        id: req_uuid(row, t, "id")?,
        checklist: req_uuid(row, t, "checklist")?,
        change: ProposalChange::from_parts(
            &req_text(row, t, "kind")?,
            opt_uuid_col(row, t, "step")?,
            &change,
        )?,
        reason: req_text(row, t, "reason")?,
        author: Author::parse(&req_text(row, t, "author")?)?,
        created_at: req_time(row, t, "created_at")?,
    })
}

// --- runs ---------------------------------------------------------------------

/// Record a finished run.
pub(crate) async fn insert_run(catalog: &Catalog, run: &ChecklistRun) -> Result<()> {
    insert(
        catalog,
        RUNS_TABLE,
        &[
            "id",
            "checklist",
            "checklist_name",
            "trigger",
            "target",
            "status",
            "run_as",
            "started_at",
            "finished_at",
            "step_versions",
            "steps",
            "cleanup_errors",
            "message",
            "outbox",
        ],
        vec![
            Value::Uuid(run.id),
            Value::Uuid(run.checklist),
            Value::Text(run.checklist_name.clone()),
            Value::Text(run.trigger.clone()),
            Value::Text(run.target.clone()),
            Value::Text(run.status.as_str().to_owned()),
            opt_uuid(run.run_as),
            Value::Timestamp(run.started_at),
            Value::Timestamp(run.finished_at),
            Value::Json(Json::Array(
                run.step_versions()
                    .iter()
                    .map(|id| Json::String(id.to_string()))
                    .collect(),
            )),
            Value::Json(Json::Array(
                run.steps.iter().map(StepResult::to_json).collect(),
            )),
            Value::Json(Json::from(run.cleanup_errors.clone())),
            run.message.clone().map_or(Value::Null, Value::Text),
            Value::Json(Json::Array(run.outbox.iter().map(Sent::to_json).collect())),
        ],
    )
    .await
}

/// One run.
pub async fn load_run(catalog: &Catalog, id: Uuid) -> Result<Option<ChecklistRun>> {
    first(
        catalog,
        RUNS_TABLE,
        Expr::col("id").eq(Expr::lit(id)),
        run_from_row,
    )
    .await
}

/// The checklist's runs, newest first, at most `limit`.
pub async fn runs_of(catalog: &Catalog, checklist: Uuid, limit: u64) -> Result<Vec<ChecklistRun>> {
    let mut select = Select::from(Source::table(RUNS_TABLE))
        .filter(Expr::col("checklist").eq(Expr::lit(checklist)))
        .limit(limit);
    select.order = vec![OrderBy::desc(Expr::col("started_at"))];
    rows(catalog, select)
        .await?
        .iter()
        .map(run_from_row)
        .collect()
}

/// The checklist's newest run.
pub async fn last_run(catalog: &Catalog, checklist: Uuid) -> Result<Option<ChecklistRun>> {
    Ok(runs_of(catalog, checklist, 1).await?.into_iter().next())
}

fn run_from_row(row: &Row) -> Result<ChecklistRun> {
    let t = RUNS_TABLE;
    let steps = match row.get("steps") {
        Some(Value::Json(Json::Array(items))) => items
            .iter()
            .map(StepResult::from_json)
            .collect::<Result<_>>()?,
        other => return Err(bad(t, "steps", "a json array", other)),
    };
    let cleanup_errors = match row.get("cleanup_errors") {
        Some(Value::Json(Json::Array(items))) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        other => return Err(bad(t, "cleanup_errors", "a json array", other)),
    };
    let outbox = match row.get("outbox") {
        Some(Value::Json(Json::Array(items))) => items.iter().filter_map(Sent::from_json).collect(),
        Some(Value::Null) | None => Vec::new(),
        other => return Err(bad(t, "outbox", "a json array", other)),
    };
    Ok(ChecklistRun {
        id: req_uuid(row, t, "id")?,
        checklist: req_uuid(row, t, "checklist")?,
        checklist_name: req_text(row, t, "checklist_name")?,
        trigger: req_text(row, t, "trigger")?,
        target: req_text(row, t, "target")?,
        status: RunStatus::parse(&req_text(row, t, "status")?)?,
        run_as: opt_uuid_col(row, t, "run_as")?,
        started_at: req_time(row, t, "started_at")?,
        finished_at: req_time(row, t, "finished_at")?,
        steps,
        cleanup_errors,
        message: opt_text(row, t, "message")?,
        outbox,
    })
}

// --- helpers ------------------------------------------------------------------

fn opt_uuid(id: Option<Uuid>) -> Value {
    id.map_or(Value::Null, Value::Uuid)
}

fn req_uuid(row: &Row, table: &str, column: &str) -> Result<Uuid> {
    match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        other => Err(bad(table, column, "a uuid", other)),
    }
}

fn opt_uuid_col(row: &Row, table: &str, column: &str) -> Result<Option<Uuid>> {
    match row.get(column) {
        Some(Value::Uuid(u)) => Ok(Some(*u)),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad(table, column, "a uuid", other)),
    }
}

fn req_text(row: &Row, table: &str, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad(table, column, "text", other)),
    }
}

fn opt_text(row: &Row, table: &str, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(Value::Text(t)) if t.is_empty() => Ok(None),
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad(table, column, "text", other)),
    }
}

fn req_bool(row: &Row, table: &str, column: &str) -> Result<bool> {
    match row.get(column) {
        Some(Value::Bool(b)) => Ok(*b),
        other => Err(bad(table, column, "a boolean", other)),
    }
}

fn req_time(row: &Row, table: &str, column: &str) -> Result<DateTime<Utc>> {
    match row.get(column) {
        Some(Value::Timestamp(t)) => Ok(*t),
        other => Err(bad(table, column, "a timestamp", other)),
    }
}

fn req_object(row: &Row, table: &str, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        other => Err(bad(table, column, "a json object", other)),
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
    let insert = Insert::row(
        table,
        columns.iter().map(|c| (*c).to_owned()).collect(),
        values.into_iter().map(Expr::Lit).collect(),
    );
    run(catalog, Statement::from(insert)).await
}

async fn first<T>(
    catalog: &Catalog,
    table: &str,
    filter: Expr,
    read: fn(&Row) -> Result<T>,
) -> Result<Option<T>> {
    if catalog.get(table)?.is_none() {
        return Ok(None);
    }
    let select = Select::from(Source::table(table)).filter(filter);
    rows(catalog, select).await?.first().map(read).transpose()
}

async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

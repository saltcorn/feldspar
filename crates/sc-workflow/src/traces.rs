//! `_fd_run_traces`: what happened, step by step (design §9, §10.3).
//!
//! §9 named this table with the agent milestone and left it uncreated; it is
//! created here, because the workflow engine is the thing that has per-step
//! timing worth keeping. One row per **completed attempt** of one step: when it
//! started, when it finished, which attempt it was, how it came out, and the
//! context *after* it.
//!
//! **Only when the workflow's `trace` flag is on.** A trace row carries a copy of
//! the whole run context, so tracing a busy workflow is a real cost and an admin
//! chooses to pay it. What it buys is the run-detail timeline: the context after
//! every step, with the change from the step before highlighted, beside the same
//! graph the workflow was drawn on.
//!
//! The row is written **in the same batch as the run's advance** (decision 6), so
//! a trace that exists is a trace of a step that actually committed. A run is
//! never observed with a trace row for a step it did not take, or missing one for
//! a step it did.

use chrono::{DateTime, Utc};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Delete, Expr, Insert, OrderBy, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

/// Name of the run-traces table in the primary database.
pub const TRACES_TABLE: &str = "_fd_run_traces";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The run this is a step of — `_fd_runs.id`, by value.
pub const COL_RUN: &str = "run";
/// Which step of the run this is: 1, 2, 3 … in the order they were taken.
pub const COL_SEQ: &str = "seq";
/// The step's name, as the workflow version spells it.
pub const COL_STEP: &str = "step";
/// When the attempt started.
pub const COL_STARTED_AT: &str = "started_at";
/// When it finished.
pub const COL_FINISHED_AT: &str = "finished_at";
/// Which attempt of this step it was: 1 for the first, 2 after one retry.
pub const COL_ATTEMPT: &str = "attempt";
/// How it came out: `ok` | `error` | `suspended`.
pub const COL_OUTCOME: &str = "outcome";
/// The failure, for an attempt that had one.
pub const COL_ERROR: &str = "error";
/// The run context **after** the step.
pub const COL_CONTEXT: &str = "context";
/// The sparse per-trace values column (§9).
pub const COL_ATTRIBUTES: &str = "attributes";

/// How one attempt of one step came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceOutcome {
    /// The step did what it does and the run moved on.
    Ok,
    /// It failed; whether the run failed with it is the error policy's answer,
    /// and the next trace row says which.
    Error,
    /// It suspended the run — a `Wait`, a `UserForm`, or a retry's backoff.
    Suspended,
}

impl TraceOutcome {
    /// The stored spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            TraceOutcome::Ok => "ok",
            TraceOutcome::Error => "error",
            TraceOutcome::Suspended => "suspended",
        }
    }

    /// Parse a stored spelling, strictly: an outcome nobody recognises must not
    /// be read as success.
    pub fn parse(s: &str) -> Result<TraceOutcome> {
        match s {
            "ok" => Ok(TraceOutcome::Ok),
            "error" => Ok(TraceOutcome::Error),
            "suspended" => Ok(TraceOutcome::Suspended),
            other => Err(Error::invalid(format!(
                "unknown trace outcome `{other}`; expected `ok`, `error` or `suspended`"
            ))),
        }
    }
}

impl std::fmt::Display for TraceOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One step of one run, as it is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct RunTrace {
    /// The row's own id.
    pub id: Uuid,
    /// The run.
    pub run: Uuid,
    /// Its position in the run, from 1.
    pub seq: u32,
    /// The step's name.
    pub step: String,
    /// When the attempt started.
    pub started_at: DateTime<Utc>,
    /// When it finished.
    pub finished_at: DateTime<Utc>,
    /// Which attempt it was, from 1.
    pub attempt: u32,
    /// How it came out.
    pub outcome: TraceOutcome,
    /// Why it failed, for one that did.
    pub error: Option<String>,
    /// The run context after the step.
    pub context: Attrs,
    /// Sparse per-trace values (§9).
    pub attributes: Attrs,
}

impl RunTrace {
    /// A trace row for one attempt of `step`.
    pub fn new(
        run: Uuid,
        seq: u32,
        step: impl Into<String>,
        started_at: DateTime<Utc>,
        outcome: TraceOutcome,
    ) -> RunTrace {
        RunTrace {
            id: Uuid::new_v4(),
            run,
            seq,
            step: step.into(),
            started_at,
            finished_at: Utc::now(),
            attempt: 1,
            outcome,
            error: None,
            context: Attrs::new(),
            attributes: Attrs::new(),
        }
    }

    /// Record which attempt this was.
    pub fn attempt(mut self, attempt: u32) -> RunTrace {
        self.attempt = attempt;
        self
    }

    /// Record when it finished.
    pub fn finished_at(mut self, at: DateTime<Utc>) -> RunTrace {
        self.finished_at = at;
        self
    }

    /// Record the context after the step.
    pub fn context(mut self, context: Attrs) -> RunTrace {
        self.context = context;
        self
    }

    /// Record why it failed.
    pub fn error(mut self, error: impl std::fmt::Display) -> RunTrace {
        self.error = Some(error.to_string());
        self
    }

    /// How long the attempt took, in milliseconds — what the timeline draws a
    /// bar from. Never negative: a clock that went backwards reads as zero
    /// rather than as a step that finished before it started.
    pub fn duration_ms(&self) -> i64 {
        (self.finished_at - self.started_at)
            .num_milliseconds()
            .max(0)
    }
}

/// The fields of the `_fd_run_traces` table, in declaration order.
fn trace_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let int = || TypeRef::Basic(BasicType::Int);
    let ts = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        // By value, not a foreign key — the same rule `_fd_runs.user_id`
        // follows: a trace is evidence, and deleting the run it belongs to is
        // `delete_run_traces`' job rather than a cascade nobody declared.
        DataField::plain(COL_RUN, uuid()).required(),
        DataField::plain(COL_SEQ, int()).required(),
        DataField::plain(COL_STEP, text()).required(),
        DataField::plain(COL_STARTED_AT, ts()).required(),
        DataField::plain(COL_FINISHED_AT, ts()).required(),
        DataField::plain(COL_ATTEMPT, int()).required(),
        DataField::plain(COL_OUTCOME, text()).required(),
        DataField::plain(COL_ERROR, text()),
        DataField::plain(COL_CONTEXT, json()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure `_fd_run_traces` exists, creating it if absent, and return it.
pub async fn bootstrap_run_traces(catalog: &Catalog) -> Result<Table> {
    catalog.bootstrap_table(TRACES_TABLE, &trace_fields()).await
}

/// Write one trace row.
///
/// Insert-only: a trace is what happened, and nothing that happened later
/// changes it. There is no update path here at all, which is why a trace row
/// cannot drift away from the advance it was written with.
pub async fn save_run_trace(catalog: &Catalog, trace: &RunTrace) -> Result<()> {
    exec(catalog, Statement::from(trace_insert(trace))).await
}

/// The `INSERT` that writes one trace row.
///
/// Public because the driver commits a trace row **in the same transaction** as
/// the run's advance (decision 6), and a transaction takes statements rather
/// than a catalog. [`save_run_trace`] is a caller of it like any other, so there
/// is one place the row is built.
pub fn trace_insert(trace: &RunTrace) -> Insert {
    Insert::row(
        TRACES_TABLE,
        trace_columns(),
        trace_values(trace).into_iter().map(Expr::Lit).collect(),
    )
}

/// Every trace row of one run, in the order the steps were taken.
pub async fn list_run_traces(catalog: &Catalog, run: Uuid) -> Result<Vec<RunTrace>> {
    let mut select =
        Select::from(Source::table(TRACES_TABLE)).filter(Expr::col(COL_RUN).eq(Expr::lit(run)));
    select.order = vec![OrderBy::asc(Expr::col(COL_SEQ))];
    rows(catalog, select)
        .await?
        .iter()
        .map(trace_from_row)
        .collect()
}

/// Delete every trace row of one run, returning how many — what deleting the run
/// does.
pub async fn delete_run_traces(catalog: &Catalog, run: Uuid) -> Result<usize> {
    let existing = list_run_traces(catalog, run).await?.len();
    if existing == 0 {
        return Ok(0);
    }
    let delete = Delete::from(TRACES_TABLE).filter(Expr::col(COL_RUN).eq(Expr::lit(run)));
    exec(catalog, Statement::from(delete)).await?;
    Ok(existing)
}

/// The row's columns, in the order [`trace_values`] produces them.
fn trace_columns() -> Vec<String> {
    [
        COL_ID,
        COL_RUN,
        COL_SEQ,
        COL_STEP,
        COL_STARTED_AT,
        COL_FINISHED_AT,
        COL_ATTEMPT,
        COL_OUTCOME,
        COL_ERROR,
        COL_CONTEXT,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// A trace serialised to its row's values, in [`trace_columns`] order.
fn trace_values(trace: &RunTrace) -> Vec<Value> {
    vec![
        Value::Uuid(trace.id),
        Value::Uuid(trace.run),
        Value::Int(i64::from(trace.seq)),
        Value::Text(trace.step.clone()),
        Value::Timestamp(trace.started_at),
        Value::Timestamp(trace.finished_at),
        Value::Int(i64::from(trace.attempt)),
        Value::Text(trace.outcome.as_str().to_owned()),
        match &trace.error {
            Some(error) => Value::Text(error.clone()),
            None => Value::Null,
        },
        Value::Json(Json::Object(trace.context.clone())),
        Value::Json(Json::Object(trace.attributes.clone())),
    ]
}

/// Rebuild a [`RunTrace`] from its row, strictly.
fn trace_from_row(row: &Row) -> Result<RunTrace> {
    Ok(RunTrace {
        id: uuid(row, COL_ID)?,
        run: uuid(row, COL_RUN)?,
        seq: count(row, COL_SEQ)?,
        step: text(row, COL_STEP)?,
        started_at: timestamp(row, COL_STARTED_AT)?,
        finished_at: timestamp(row, COL_FINISHED_AT)?,
        attempt: count(row, COL_ATTEMPT)?,
        outcome: TraceOutcome::parse(&text(row, COL_OUTCOME)?)?,
        error: match row.get(COL_ERROR) {
            Some(Value::Text(t)) if t.is_empty() => None,
            Some(Value::Text(t)) => Some(t.clone()),
            Some(Value::Null) | None => None,
            other => return Err(bad_column(COL_ERROR, "text", other)),
        },
        context: object(row, COL_CONTEXT)?,
        attributes: object(row, COL_ATTRIBUTES)?,
    })
}

fn uuid(row: &Row, column: &str) -> Result<Uuid> {
    match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        other => Err(bad_column(column, "a uuid", other)),
    }
}

fn count(row: &Row, column: &str) -> Result<u32> {
    match row.get(column) {
        Some(Value::Int(i)) => u32::try_from(*i).map_err(|_| {
            Error::invalid(format!(
                "{TRACES_TABLE}.{column} should be a positive whole number, got {i}"
            ))
        }),
        other => Err(bad_column(column, "a whole number", other)),
    }
}

fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

fn timestamp(row: &Row, column: &str) -> Result<DateTime<Utc>> {
    match row.get(column) {
        Some(Value::Timestamp(t)) => Ok(*t),
        other => Err(bad_column(column, "a timestamp", other)),
    }
}

fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{TRACES_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{TRACES_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

async fn exec(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_records_the_attempt_as_well_as_the_step() {
        let fields = trace_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();
        assert!(by_name(COL_ID).primary_key);
        for column in [
            COL_RUN,
            COL_SEQ,
            COL_STEP,
            COL_STARTED_AT,
            COL_FINISHED_AT,
            COL_ATTEMPT,
            COL_OUTCOME,
            COL_CONTEXT,
        ] {
            assert!(by_name(column).required, "{column} should be required");
        }
        // The error is the one honest null: most attempts have none.
        assert!(!by_name(COL_ERROR).required);
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        let trace = RunTrace::new(Uuid::new_v4(), 1, "fetch", Utc::now(), TraceOutcome::Ok);
        assert_eq!(trace_columns().len(), trace_values(&trace).len());
        let declared: Vec<String> = trace_fields().iter().map(|f| f.base.name.clone()).collect();
        assert_eq!(trace_columns(), declared);
    }

    #[test]
    fn the_outcome_spellings_round_trip_and_refuse_what_they_do_not_know() {
        for outcome in [
            TraceOutcome::Ok,
            TraceOutcome::Error,
            TraceOutcome::Suspended,
        ] {
            assert_eq!(TraceOutcome::parse(outcome.as_str()).unwrap(), outcome);
        }
        let err = TraceOutcome::parse("fine").unwrap_err();
        assert!(err.to_string().contains("unknown trace outcome"), "{err}");
    }

    #[test]
    fn a_duration_is_never_negative() {
        let now = Utc::now();
        let trace = RunTrace::new(Uuid::new_v4(), 1, "s", now, TraceOutcome::Ok)
            .finished_at(now + chrono::Duration::milliseconds(250));
        assert_eq!(trace.duration_ms(), 250);
        // A clock that went backwards is a clock problem, not a step that
        // finished before it started.
        let backwards = RunTrace::new(Uuid::new_v4(), 1, "s", now, TraceOutcome::Ok)
            .finished_at(now - chrono::Duration::seconds(1));
        assert_eq!(backwards.duration_ms(), 0);
    }
}

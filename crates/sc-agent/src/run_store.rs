//! The `_fd_runs` table: its schema, bootstrap, and the [`Run`] ⇄ row mapping
//! (design §9, §11.4).
//!
//! Written after **every** step, not at the end of a run. That is what makes a
//! reloaded chat panel show a conversation that is still going, what lets a
//! resumed process pick a run up where it stopped, and what §10.3's durable
//! engine will need with no second mechanism — the same table, discriminated by
//! [`RunKind`].
//!
//! One deliberate departure from §9's "every metadata table has a `name`": a run
//! has no name. It has a [`subject`](Run::subject) — what it is a run *of* — and
//! inventing a name column that always equalled it would be residue on every row.
//! §9's rule is about definitions an admin names and addresses; a run is neither
//! named nor addressed by anything but its id.

use chrono::{DateTime, Utc};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, OrderBy, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;

use crate::run::{Run, RunId, RunKind, RunState};

/// Name of the runs table in the primary database.
pub const RUNS_TABLE: &str = "_fd_runs";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// `agent` or `workflow` — which engine this run belongs to.
pub const COL_KIND: &str = "kind";
/// What is running: the agent's name.
pub const COL_SUBJECT: &str = "subject";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// Where the run got to: `running` | `waiting` | `done` | `failed` | `aborted`.
pub const COL_STATE: &str = "state";
/// Why a failed run failed, NULL otherwise.
pub const COL_ERROR: &str = "error";
/// The resumable state — an `AgentLoop` for an agent run.
pub const COL_CONTEXT: &str = "context";
/// The user the run is on behalf of, NULL for one a trigger started.
pub const COL_USER: &str = "user_id";
/// The workflow version this run is pinned to, NULL for an agent run (§10.3).
pub const COL_SUBJECT_VERSION: &str = "subject_version";
/// When this run next wants the engine, NULL for one waiting on a person.
pub const COL_WAKE_AT: &str = "wake_at";
/// Until when a node has claimed this run, NULL for one nothing is working on.
pub const COL_LEASE_UNTIL: &str = "lease_until";
/// Which node holds that lease, NULL for one nothing is working on.
pub const COL_CLAIMED_BY: &str = "claimed_by";
/// The sparse per-run values column (§9).
pub const COL_ATTRIBUTES: &str = "attributes";
/// When the run was created.
pub const COL_CREATED_AT: &str = "created_at";
/// When it was last written — after every step.
pub const COL_UPDATED_AT: &str = "updated_at";

/// The fields of the `_fd_runs` table, in declaration order.
fn run_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let int = || TypeRef::Basic(BasicType::Int);
    let ts = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_KIND, text()).required(),
        DataField::plain(COL_SUBJECT, text()).required(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_STATE, text()).required(),
        DataField::plain(COL_ERROR, text()),
        DataField::plain(COL_CONTEXT, json()).required(),
        // Nullable, and *not* a foreign key: a run is a record of what happened,
        // and deleting the user who chatted must not delete the evidence or be
        // blocked by it.
        DataField::plain(COL_USER, uuid()),
        // The workflow engine's four (§10.3), every one of them nullable and
        // meaningless for an agent run: what version this run is pinned to, when
        // it next wants the engine, and the lease that says a node is working on
        // it. They reach a database that already has runs in it through the
        // additive bootstrap, exactly as `_fd_triggers.last_run_at` did.
        DataField::plain(COL_SUBJECT_VERSION, int()),
        DataField::plain(COL_WAKE_AT, ts()),
        DataField::plain(COL_LEASE_UNTIL, ts()),
        DataField::plain(COL_CLAIMED_BY, text()),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
        DataField::plain(COL_CREATED_AT, ts()).required(),
        DataField::plain(COL_UPDATED_AT, ts()).required(),
    ]
}

/// Ensure the `_fd_runs` table exists, creating it if absent, and return it.
pub async fn bootstrap_runs(catalog: &Catalog) -> Result<Table> {
    catalog.bootstrap_table(RUNS_TABLE, &run_fields()).await
}

/// Save a run: insert its row, or update it in place if one with its [`RunId`]
/// already exists.
///
/// No validation, deliberately: a run is not a definition an admin wrote, it is
/// what happened. Refusing to store a step because the state looks odd would lose
/// the only record of the thing that went wrong.
pub async fn save_run(catalog: &Catalog, run: &Run) -> Result<()> {
    note_wakeup(catalog, run);
    if load_run(catalog, run.id).await?.is_some() {
        exec(catalog, Statement::from(run_update(run))).await
    } else {
        exec(catalog, Statement::from(run_insert(run))).await
    }
}

/// Tell the catalog that this run wants the engine, so the engine's queue does
/// not have to discover it by polling ([`sc_catalog::RunWakeups`]).
///
/// Called by everything that writes a run row — [`save_run`] here, and the
/// workflow driver, which writes the row itself inside the step's transaction.
/// **Before** the write, deliberately: a note that is early costs one query that
/// finds nothing, a note that is lost costs a run that never wakes, and a write
/// that fails leaves the row as it was, which the cache is still allowed to be
/// early about.
///
/// A run waiting on a person has no `wake_at` and is not noted: no clock will
/// make it runnable, and the resume that will is a write of its own.
pub fn note_wakeup(catalog: &Catalog, run: &Run) {
    if run.kind == RunKind::Workflow
        && let Some(at) = run.wake_at
    {
        catalog.run_wakeups().note(at);
    }
}

/// The `INSERT` that first writes `run`'s row.
///
/// Public, with [`run_update`], because the workflow engine commits a run's
/// advance **and** its trace row in one transaction (§10.3, decision 6), and a
/// transaction takes statements rather than a catalog. Two writers of the same
/// row would be two chances for a column to be forgotten by one of them, so
/// there is one place the row's statements are built and [`save_run`] is a
/// caller of it like any other.
pub fn run_insert(run: &Run) -> Insert {
    Insert::row(
        RUNS_TABLE,
        run_columns(),
        run_values(run).into_iter().map(Expr::Lit).collect(),
    )
}

/// The `UPDATE` that writes `run` back to its row: every column except the id
/// and the creation instant, neither of which a later step may rewrite.
pub fn run_update(run: &Run) -> sc_query::Update {
    let assignments = run_columns()
        .iter()
        .zip(run_values(run))
        .filter(|(col, _)| *col != COL_ID && *col != COL_CREATED_AT)
        .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
        .collect();
    sc_query::Update::new(RUNS_TABLE, assignments).filter(Expr::col(COL_ID).eq(Expr::lit(run.id.0)))
}

/// Load the run with this id, if it exists.
pub async fn load_run(catalog: &Catalog, id: RunId) -> Result<Option<Run>> {
    let select =
        Select::from(Source::table(RUNS_TABLE)).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(run_from_row(row)?)),
        None => Ok(None),
    }
}

/// The run with this id, or a not-found error naming it.
pub async fn require_run(catalog: &Catalog, id: RunId) -> Result<Run> {
    load_run(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no run with id {id}")))
}

/// Every run of `subject`, newest first — the chat panel's history.
pub async fn list_runs(catalog: &Catalog, subject: &str) -> Result<Vec<Run>> {
    let mut select = Select::from(Source::table(RUNS_TABLE))
        .filter(Expr::col(COL_SUBJECT).eq(Expr::lit(subject)));
    select.order = vec![OrderBy::desc(Expr::col(COL_CREATED_AT))];
    rows(catalog, select)
        .await?
        .iter()
        .map(run_from_row)
        .collect()
}

/// The runs of `subject` on behalf of `user`, newest first — or, for `None`,
/// the runs of it that record no user (anonymous and triggered ones alike, which
/// a caller tells apart by the run's attributes).
///
/// The user is in the query rather than filtered afterwards, because an
/// application's chat history is one person's runs of an agent many people
/// use, and reading everybody's transcripts to show one person theirs would be
/// the expensive half of the answer.
pub async fn list_runs_for(
    catalog: &Catalog,
    subject: &str,
    user: Option<uuid::Uuid>,
) -> Result<Vec<Run>> {
    let by_user = match user {
        Some(user) => Expr::col(COL_USER).eq(Expr::lit(user)),
        None => Expr::unary(sc_query::UnOp::IsNull, Expr::col(COL_USER)),
    };
    let mut select = Select::from(Source::table(RUNS_TABLE))
        .filter(Expr::col(COL_SUBJECT).eq(Expr::lit(subject)).and(by_user));
    select.order = vec![OrderBy::desc(Expr::col(COL_CREATED_AT))];
    rows(catalog, select)
        .await?
        .iter()
        .map(run_from_row)
        .collect()
}

/// The live runs whose [`ATTR_PARENT_RUN`](crate::ATTR_PARENT_RUN) is `parent`,
/// oldest first.
pub async fn list_live_children(catalog: &Catalog, parent: RunId) -> Result<Vec<Run>> {
    let mut select = Select::from(Source::table(RUNS_TABLE))
        .filter(Expr::col(COL_STATE).eq(Expr::lit(RunState::Running.as_str())));
    select.order = vec![OrderBy::asc(Expr::col(COL_CREATED_AT))];
    let parent = parent.to_string();
    Ok(rows(catalog, select)
        .await?
        .iter()
        .map(run_from_row)
        .collect::<Result<Vec<Run>>>()?
        .into_iter()
        .filter(|run| {
            run.attributes
                .get(crate::ATTR_PARENT_RUN)
                .and_then(Json::as_str)
                == Some(parent.as_str())
        })
        .collect())
}

/// Stop `run` where it stands, and every live run it delegated to, however
/// deep, and write them all.
///
/// A child is driven inside its parent's tool call, so stopping the parent's
/// drive stops the child's too — and without this the child's row would say
/// `running` for ever. Transcripts are left as they are.
pub async fn abort_run(catalog: &Catalog, run: &mut Run) -> Result<()> {
    let mut state = run.agent_loop()?;
    state.abort();
    run.record(&state);
    save_run(catalog, run).await?;
    let mut pending = vec![run.id];
    while let Some(parent) = pending.pop() {
        for mut child in list_live_children(catalog, parent).await? {
            let mut state = child.agent_loop()?;
            state.abort();
            child.record(&state);
            save_run(catalog, &child).await?;
            pending.push(child.id);
        }
    }
    Ok(())
}

/// Delete a run, returning whether one was there to delete.
pub async fn delete_run(catalog: &Catalog, id: RunId) -> Result<bool> {
    if load_run(catalog, id).await?.is_none() {
        return Ok(false);
    }
    let delete = Delete::from(RUNS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    exec(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// The row's columns, in the order [`run_values`] produces them.
fn run_columns() -> Vec<String> {
    [
        COL_ID,
        COL_KIND,
        COL_SUBJECT,
        COL_DESCRIPTION,
        COL_STATE,
        COL_ERROR,
        COL_CONTEXT,
        COL_USER,
        COL_SUBJECT_VERSION,
        COL_WAKE_AT,
        COL_LEASE_UNTIL,
        COL_CLAIMED_BY,
        COL_ATTRIBUTES,
        COL_CREATED_AT,
        COL_UPDATED_AT,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The run serialised to its row's values, in [`run_columns`] order.
fn run_values(run: &Run) -> Vec<Value> {
    vec![
        Value::Uuid(run.id.0),
        Value::Text(run.kind.as_str().to_owned()),
        Value::Text(run.subject.clone()),
        Value::Text(run.description.clone()),
        Value::Text(run.state.as_str().to_owned()),
        match &run.error {
            Some(error) => Value::Text(error.clone()),
            None => Value::Null,
        },
        Value::Json(run.context.clone()),
        match run.user {
            Some(user) => Value::Uuid(user),
            None => Value::Null,
        },
        match run.subject_version {
            Some(version) => Value::Int(i64::from(version)),
            None => Value::Null,
        },
        timestamp_or_null(run.wake_at),
        timestamp_or_null(run.lease_until),
        match &run.claimed_by {
            Some(node) => Value::Text(node.clone()),
            None => Value::Null,
        },
        Value::Json(Json::Object(run.attributes.clone())),
        Value::Timestamp(run.created_at),
        Value::Timestamp(run.updated_at),
    ]
}

/// Rebuild a [`Run`] from its `_fd_runs` row.
///
/// Strict, for the reason `_fd_agents`' reader is: a run read as something other
/// than what was stored would be resumed as something other than what was
/// running.
///
/// Public because the workflow engine's queue (§10.3, decision 5) selects the
/// runnable set with a predicate of its own and must read the rows back the same
/// way `load_run` does — a second reader would be a second chance to interpret a
/// stored state differently from the one that wrote it.
pub fn run_from_row(row: &Row) -> Result<Run> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => RunId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    Ok(Run {
        id,
        kind: RunKind::parse(&text(row, COL_KIND)?)
            .map_err(|e| Error::invalid(format!("run {id}: {e}")))?,
        subject: text(row, COL_SUBJECT)?,
        description: optional_text(row, COL_DESCRIPTION)?.unwrap_or_default(),
        state: RunState::parse(&text(row, COL_STATE)?)
            .map_err(|e| Error::invalid(format!("run {id}: {e}")))?,
        error: optional_text(row, COL_ERROR)?,
        context: match row.get(COL_CONTEXT) {
            Some(Value::Json(json)) => json.clone(),
            other => return Err(bad_column(COL_CONTEXT, "json", other)),
        },
        user: match row.get(COL_USER) {
            Some(Value::Uuid(u)) => Some(*u),
            Some(Value::Null) | None => None,
            other => return Err(bad_column(COL_USER, "a uuid", other)),
        },
        subject_version: match row.get(COL_SUBJECT_VERSION) {
            Some(Value::Int(i)) => Some(u32::try_from(*i).map_err(|_| {
                Error::invalid(format!(
                    "{RUNS_TABLE}.{COL_SUBJECT_VERSION} should be a version number, got {i}"
                ))
            })?),
            Some(Value::Null) | None => None,
            other => return Err(bad_column(COL_SUBJECT_VERSION, "a version number", other)),
        },
        wake_at: optional_timestamp(row, COL_WAKE_AT)?,
        lease_until: optional_timestamp(row, COL_LEASE_UNTIL)?,
        claimed_by: optional_text(row, COL_CLAIMED_BY)?,
        attributes: object(row, COL_ATTRIBUTES)?,
        created_at: timestamp(row, COL_CREATED_AT)?,
        updated_at: timestamp(row, COL_UPDATED_AT)?,
    })
}

fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

fn optional_text(row: &Row, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(Value::Text(t)) if t.is_empty() => Ok(None),
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad_column(column, "text", other)),
    }
}

fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{RUNS_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

/// A nullable timestamp column. A column the additive bootstrap has not added
/// yet reads as absent, which is the same "nothing to say" a NULL is.
fn optional_timestamp(row: &Row, column: &str) -> Result<Option<DateTime<Utc>>> {
    match row.get(column) {
        Some(Value::Timestamp(t)) => Ok(Some(*t)),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad_column(column, "a timestamp", other)),
    }
}

/// An optional instant as a value.
fn timestamp_or_null(at: Option<DateTime<Utc>>) -> Value {
    match at {
        Some(at) => Value::Timestamp(at),
        None => Value::Null,
    }
}

fn timestamp(row: &Row, column: &str) -> Result<DateTime<Utc>> {
    match row.get(column) {
        Some(Value::Timestamp(t)) => Ok(*t),
        other => Err(bad_column(column, "a timestamp", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{RUNS_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

async fn exec(catalog: &Catalog, statement: Statement) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_trait::RunCaller;
    use crate::machine::AgentLoop;

    #[test]
    fn schema_carries_the_kind_that_discriminates_the_two_engines() {
        let fields = run_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();
        assert!(by_name(COL_ID).primary_key);
        assert!(by_name(COL_KIND).required);
        assert!(by_name(COL_SUBJECT).required);
        assert!(by_name(COL_CONTEXT).required);
        assert!(by_name(COL_STATE).required);
        // Both instants are required: a run that cannot say when it last moved
        // is one nothing can decide is stuck.
        assert!(by_name(COL_CREATED_AT).required && by_name(COL_UPDATED_AT).required);
        // The user and the error are honest nulls, and so is every one of the
        // engine's four: an agent run is pinned to no version, wants the engine
        // at no time and is leased by nobody.
        assert!(!by_name(COL_USER).required && !by_name(COL_ERROR).required);
        for column in [
            COL_SUBJECT_VERSION,
            COL_WAKE_AT,
            COL_LEASE_UNTIL,
            COL_CLAIMED_BY,
        ] {
            assert!(!by_name(column).required, "{column} should be nullable");
        }
        // A run has no name; see the module docs.
        assert!(fields.iter().all(|f| f.base.name != "name"));
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        let run = Run::new("a", &RunCaller::system(), &AgentLoop::new(20));
        assert_eq!(run_columns().len(), run_values(&run).len());
        let declared: Vec<String> = run_fields().iter().map(|f| f.base.name.clone()).collect();
        assert_eq!(run_columns(), declared);
    }

    #[test]
    fn a_system_run_stores_a_null_user_and_a_live_run_a_null_error() {
        let run = Run::new("a", &RunCaller::system(), &AgentLoop::new(20));
        let columns = run_columns();
        let values = run_values(&run);
        let at = |column: &str| {
            columns
                .iter()
                .position(|c| c == column)
                .map(|i| values[i].clone())
                .unwrap()
        };
        assert_eq!(at(COL_ERROR), Value::Null);
        assert_eq!(at(COL_USER), Value::Null);
        assert_eq!(at(COL_STATE), Value::Text("running".to_owned()));
        // An agent run leaves every one of the engine's columns null.
        for column in [
            COL_SUBJECT_VERSION,
            COL_WAKE_AT,
            COL_LEASE_UNTIL,
            COL_CLAIMED_BY,
        ] {
            assert_eq!(at(column), Value::Null, "{column}");
        }
    }
}

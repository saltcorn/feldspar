//! `_fd_workflow_versions`: the rows a workflow's history is made of (design §9,
//! §10.3).
//!
//! **Versions are rows, and the table is append-only.** Saving an edited workflow
//! mints `version + 1`; nothing ever updates a row, and nothing deletes one while
//! its workflow exists. That is not bookkeeping for its own sake — it is what
//! makes the sentence in GOALS implementable: *a suspended run finishes on its
//! own version of the workflow*. A run records the version it started on and
//! loads that one for the rest of its life, so the workflow can be edited twice
//! while a run waits for an approval and the run still finishes as the steps it
//! started with.
//!
//! Steps in the trigger's `attributes` would have been smaller and would have
//! made that sentence unimplementable, because there would be nothing to pin to.
//!
//! Two departures from §9's "every metadata table has a `name`", both of them the
//! same departure `_fd_runs` makes: a version has no name and no description of
//! its own identity. It is addressed by `(workflow, version)`, which is what the
//! `UNIQUE` is on, and the name an admin knows it by is the **trigger's** —
//! inventing one here would be residue on every row.
//!
//! Reading is strict, as `_fd_triggers`' reader is: a version that does not parse
//! is an error naming the workflow and the version, never a workflow silently
//! read as fewer steps than it has. A run advanced on a half-read program would
//! do a subset of what the admin drew.

use chrono::{DateTime, Utc};
use sc_action::TriggerId;
use sc_catalog::{Catalog, ConstraintKind, DataField, SchemaStep, Table, TableConstraint};
use sc_db::{Row, SchemaChange};
use sc_error::{Error, Result};
use sc_query::{Delete, Expr, Insert, OrderBy, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::workflow::Workflow;

/// Name of the workflow-versions table in the primary database.
pub const VERSIONS_TABLE: &str = "_fd_workflow_versions";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The trigger whose body this is a version of.
pub const COL_WORKFLOW: &str = "workflow";
/// Which version this is: 1 for the first, `max + 1` for each save after.
pub const COL_VERSION: &str = "version";
/// What the admin said about this version when they saved it — a commit message,
/// in effect, and the one thing a version history is unreadable without.
pub const COL_DESCRIPTION: &str = "description";
/// The workflow itself, as the JSON document [`Workflow`] serialises to.
pub const COL_STEPS: &str = "steps";
/// The sparse per-version values column (§9).
pub const COL_ATTRIBUTES: &str = "attributes";
/// When this version was minted.
pub const COL_CREATED_AT: &str = "created_at";
/// Who minted it, or NULL for one nobody was signed in for (a restore, a
/// migration, an agent acting on its own).
pub const COL_CREATED_BY: &str = "created_by";

/// The fields of the `_fd_workflow_versions` table, in declaration order.
fn version_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let int = || TypeRef::Basic(BasicType::Int);
    let ts = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        // Not a foreign key, for the reason `_fd_runs.user_id` is not one: the
        // schema layer renders no `ON DELETE` action, so a key here would make
        // deleting a workflow trigger impossible rather than tidy. Deleting the
        // trigger deletes its versions, and `delete_workflow_versions` is what
        // does it.
        DataField::plain(COL_WORKFLOW, uuid()).required(),
        DataField::plain(COL_VERSION, int()).required(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_STEPS, json()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
        DataField::plain(COL_CREATED_AT, ts()).required(),
        DataField::plain(COL_CREATED_BY, uuid()),
    ]
}

/// The jointly-unique key `(workflow, version)`: two rows claiming to be version
/// 3 of one workflow is not a state anything can resolve, and the database is the
/// authority because two admins saving at once cannot see each other's
/// transaction.
///
/// [`save_workflow`] reads the current maximum and inserts the next, which is
/// correct for one writer and a race for two — so the constraint is what turns
/// that race into one save succeeding and the other being told to look again,
/// rather than into a version history with a fork in it.
fn version_key() -> ConstraintKind {
    ConstraintKind::Unique {
        fields: vec![COL_WORKFLOW.to_owned(), COL_VERSION.to_owned()],
    }
}

/// Ensure `_fd_workflow_versions` exists, with its jointly-unique key, and return
/// it.
///
/// Idempotent, like every other bootstrap: an existing table is reconciled
/// additively, and the key is added only when introspection says it is not
/// already there.
pub async fn bootstrap_workflow_versions(catalog: &Catalog) -> Result<Table> {
    let table = catalog
        .bootstrap_table(VERSIONS_TABLE, &version_fields())
        .await?;
    let key = version_key();
    if table.constraints.iter().any(|c| c.kind == key) {
        return Ok(table);
    }
    let name = TableConstraint::derived_name(VERSIONS_TABLE, &key, "");
    catalog
        .apply_schema_batch(&[SchemaStep::Change(SchemaChange::AddUniqueConstraint {
            table: VERSIONS_TABLE.to_owned(),
            name,
            columns: vec![COL_WORKFLOW.to_owned(), COL_VERSION.to_owned()],
        })])
        .await?;
    catalog.reload().await?;
    catalog.require(VERSIONS_TABLE)
}

/// Mint the next version of `workflow`, returning it with its `version` set.
///
/// **Append-only**: this reads the current maximum and inserts `max + 1`. It
/// never updates a row, so the version a suspended run is pinned to is still
/// there to be loaded, and `revertWorkflow` is a new version whose steps are an
/// old one's rather than a rewrite of history.
///
/// The `version` on the workflow passed in is **ignored** — the store decides
/// what version this is, because it is the only thing that can see the other
/// saves.
pub async fn save_workflow(
    catalog: &Catalog,
    workflow: &Workflow,
    description: &str,
    by: Option<Uuid>,
) -> Result<Workflow> {
    let next = match max_version(catalog, workflow.id).await? {
        Some(current) => current + 1,
        None => 1,
    };
    let mut minted = workflow.clone();
    minted.version = next;

    let insert = Insert::row(
        VERSIONS_TABLE,
        version_columns(),
        version_values(&minted, description, by)
            .into_iter()
            .map(Expr::Lit)
            .collect(),
    );
    exec(catalog, Statement::from(insert)).await?;
    sc_action::touch_trigger(catalog, workflow.id).await?;
    Ok(minted)
}

/// The highest version number stored for `workflow`, or `None` when it has none
/// yet.
pub async fn max_version(catalog: &Catalog, workflow: TriggerId) -> Result<Option<u32>> {
    Ok(list_workflow_versions(catalog, workflow)
        .await?
        .first()
        .map(|v| v.version))
}

/// One version of a workflow, and what is known about the saving of it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowVersion {
    /// The version row's own id.
    pub id: Uuid,
    /// Which version this is.
    pub version: u32,
    /// What the admin said when they saved it.
    pub description: String,
    /// When it was minted.
    pub created_at: DateTime<Utc>,
    /// Who minted it, when anybody was signed in.
    pub created_by: Option<Uuid>,
    /// The workflow itself.
    pub workflow: Workflow,
}

/// Every stored version of `workflow`, **newest first** — the history the admin
/// screen lists and reverts from.
pub async fn list_workflow_versions(
    catalog: &Catalog,
    workflow: TriggerId,
) -> Result<Vec<WorkflowVersion>> {
    let mut select = Select::from(Source::table(VERSIONS_TABLE))
        .filter(Expr::col(COL_WORKFLOW).eq(Expr::lit(workflow.0)));
    select.order = vec![OrderBy::desc(Expr::col(COL_VERSION))];
    rows(catalog, select)
        .await?
        .iter()
        .map(version_from_row)
        .collect()
}

/// One stored version, or `None` when that workflow has no such version.
pub async fn load_workflow_version(
    catalog: &Catalog,
    workflow: TriggerId,
    version: u32,
) -> Result<Option<Workflow>> {
    let select = Select::from(Source::table(VERSIONS_TABLE)).filter(
        Expr::col(COL_WORKFLOW)
            .eq(Expr::lit(workflow.0))
            .and(Expr::col(COL_VERSION).eq(Expr::lit(i64::from(version)))),
    );
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(version_from_row(row)?.workflow)),
        None => Ok(None),
    }
}

/// The version a **new run** is pinned to: the newest one there is.
///
/// `None` means the workflow has never been saved, which is a real state — a
/// trigger created with a workflow body before anybody drew anything — and the
/// caller says so by name rather than starting a run of nothing.
pub async fn current_workflow(catalog: &Catalog, workflow: TriggerId) -> Result<Option<Workflow>> {
    Ok(list_workflow_versions(catalog, workflow)
        .await?
        .into_iter()
        .next()
        .map(|v| v.workflow))
}

/// The newest version of `workflow`, or an error naming the trigger it belongs
/// to — what starting a run asks for.
pub async fn require_current_workflow(
    catalog: &Catalog,
    workflow: TriggerId,
    name: &str,
) -> Result<Workflow> {
    current_workflow(catalog, workflow).await?.ok_or_else(|| {
        Error::not_found(format!(
            "trigger `{name}` is a workflow, but no version of it has been saved yet"
        ))
    })
}

/// The version `version` of `workflow`, or an error naming both — what a
/// **resumed** run asks for, and where the pinning becomes visible: a version
/// that has gone missing is reported, never silently replaced by the current one.
pub async fn require_workflow_version(
    catalog: &Catalog,
    workflow: TriggerId,
    version: u32,
) -> Result<Workflow> {
    load_workflow_version(catalog, workflow, version)
        .await?
        .ok_or_else(|| {
            Error::not_found(format!(
                "workflow {workflow} has no version {version}; a run pinned to it \
                 cannot be advanced"
            ))
        })
}

/// Delete every version of `workflow`, returning how many were deleted — what
/// deleting the trigger does.
///
/// The one operation that removes a row here, and it exists because the
/// alternative is orphaned history nobody can reach: append-only is a rule about
/// *editing* a workflow, not a promise that a deleted trigger leaves rows behind
/// forever.
pub async fn delete_workflow_versions(catalog: &Catalog, workflow: TriggerId) -> Result<usize> {
    let existing = list_workflow_versions(catalog, workflow).await?.len();
    if existing == 0 {
        return Ok(0);
    }
    let delete =
        Delete::from(VERSIONS_TABLE).filter(Expr::col(COL_WORKFLOW).eq(Expr::lit(workflow.0)));
    exec(catalog, Statement::from(delete)).await?;
    Ok(existing)
}

/// The row's columns, in the order [`version_values`] produces them.
fn version_columns() -> Vec<String> {
    [
        COL_ID,
        COL_WORKFLOW,
        COL_VERSION,
        COL_DESCRIPTION,
        COL_STEPS,
        COL_ATTRIBUTES,
        COL_CREATED_AT,
        COL_CREATED_BY,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// A version serialised to its row's values, in [`version_columns`] order.
fn version_values(workflow: &Workflow, description: &str, by: Option<Uuid>) -> Vec<Value> {
    vec![
        Value::Uuid(Uuid::new_v4()),
        Value::Uuid(workflow.id.0),
        Value::Int(i64::from(workflow.version)),
        Value::Text(description.trim().to_owned()),
        // The document is the workflow *whole* — id and version included — and
        // the columns beside it are what the database needs to find it by. The
        // reader checks the two agree rather than trusting either alone.
        Value::Json(serde_json::to_value(workflow).unwrap_or(Json::Null)),
        Value::Json(Json::Object(Attrs::new())),
        Value::Timestamp(Utc::now()),
        match by {
            Some(user) => Value::Uuid(user),
            None => Value::Null,
        },
    ]
}

/// Rebuild a [`WorkflowVersion`] from its row, strictly.
fn version_from_row(row: &Row) -> Result<WorkflowVersion> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => *u,
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let workflow_id = match row.get(COL_WORKFLOW) {
        Some(Value::Uuid(u)) => TriggerId(*u),
        other => return Err(bad_column(COL_WORKFLOW, "a uuid", other)),
    };
    let version = match row.get(COL_VERSION) {
        Some(Value::Int(i)) => u32::try_from(*i).map_err(|_| {
            Error::invalid(format!(
                "{VERSIONS_TABLE}.{COL_VERSION} should be a version number, got {i}"
            ))
        })?,
        other => return Err(bad_column(COL_VERSION, "a version number", other)),
    };
    let document = match row.get(COL_STEPS) {
        Some(Value::Json(json)) => json.clone(),
        other => return Err(bad_column(COL_STEPS, "json", other)),
    };
    let workflow: Workflow = serde_json::from_value(document).map_err(|e| {
        Error::invalid(format!(
            "workflow {workflow_id} version {version}: its stored steps are unreadable: {e}"
        ))
    })?;
    // The columns and the document have to agree. They can only disagree if
    // somebody wrote the row by hand, and the honest report of that is which
    // workflow and which version — because loading it under the row's identity
    // would run one workflow's steps under another's name.
    if workflow.id != workflow_id || workflow.version != version {
        return Err(Error::invalid(format!(
            "workflow {workflow_id} version {version}: its stored steps say they are \
             workflow {} version {}",
            workflow.id, workflow.version
        )));
    }
    Ok(WorkflowVersion {
        id,
        version,
        description: optional_text(row, COL_DESCRIPTION)?.unwrap_or_default(),
        created_at: match row.get(COL_CREATED_AT) {
            Some(Value::Timestamp(t)) => *t,
            other => return Err(bad_column(COL_CREATED_AT, "a timestamp", other)),
        },
        created_by: match row.get(COL_CREATED_BY) {
            Some(Value::Uuid(u)) => Some(*u),
            Some(Value::Null) | None => None,
            other => return Err(bad_column(COL_CREATED_BY, "a uuid", other)),
        },
        workflow,
    })
}

fn optional_text(row: &Row, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(Value::Text(t)) if t.trim().is_empty() => Ok(None),
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad_column(column, "text", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{VERSIONS_TABLE}.{column} should be {expected}, got {}",
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

    #[test]
    fn the_schema_is_addressed_by_workflow_and_version() {
        let fields = version_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();
        assert!(by_name(COL_ID).primary_key && by_name(COL_ID).required);
        // What a version *is*: a workflow, a number and a document.
        assert!(by_name(COL_WORKFLOW).required);
        assert!(by_name(COL_VERSION).required);
        assert!(by_name(COL_STEPS).required);
        // Who saved it is an honest null: a restore has no signed-in user.
        assert!(!by_name(COL_CREATED_BY).required);
        // Addressed by (workflow, version) — see the module docs for why there is
        // no `name`.
        assert!(fields.iter().all(|f| f.base.name != "name"));
        assert_eq!(
            version_key(),
            ConstraintKind::Unique {
                fields: vec![COL_WORKFLOW.to_owned(), COL_VERSION.to_owned()]
            }
        );
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        let workflow = Workflow::empty(TriggerId::new());
        assert_eq!(
            version_columns().len(),
            version_values(&workflow, "first", None).len()
        );
        let declared: Vec<String> = version_fields()
            .iter()
            .map(|f| f.base.name.clone())
            .collect();
        assert_eq!(version_columns(), declared);
    }

    #[test]
    fn the_stored_document_is_the_whole_workflow() {
        let workflow = Workflow::empty(TriggerId::new());
        let values = version_values(&workflow, "first", None);
        let Value::Json(document) = &values[4] else {
            panic!("the steps column holds json");
        };
        // Not "the steps": the whole thing, so what is read back is what was
        // saved and there is no field that lives only in a column.
        let back: Workflow = serde_json::from_value(document.clone()).unwrap();
        assert_eq!(back, workflow);
    }
}

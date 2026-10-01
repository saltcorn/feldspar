//! Workspaces (analytics TODO A1.12, A1.21): the seven kinds of the goals
//! document, and the `_fd_workspaces` table that keeps each one's state.
//!
//! A workspace is a name, a kind and a **state** — JSON owned by the kind's
//! screen, restored when the workspace is opened again. This crate does not
//! look inside the state: an explorer's (A2) is its drop zones, a model fit's
//! (A3) is the model open, and neither is the other's business. The Dataset
//! editor is not a workspace: datasets are listed beside the workspaces and
//! each opens in the editor on its own.
//!
//! The store keeps a workspace of any kind. Whether a kind can be created yet
//! is [`WorkspaceKind::check_available`]'s question, which the API asks, so
//! there is never a workspace in the list that opens onto nothing.

use chrono::{DateTime, Utc};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, Delete, Expr, Insert, OrderBy, Select, Source, Statement, Update, Value,
};
use sc_types::{BasicType, TypeRef};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

/// Name of the workspaces table in the primary database.
pub const WORKSPACES_TABLE: &str = "_fd_workspaces";

/// What a workspace is for (the goals document's "Workspaces").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    /// Plots, summary tables and tests from drop zones.
    DataExplorer,
    /// Tiles of panels, cross-filtered.
    Dashboard,
    /// Creating, fitting and inspecting models.
    ModelFit,
    /// Code, text and output cells.
    Notebook,
    /// A printable document of panels.
    Report,
    /// Layers of datasets on a base map.
    Map,
    /// Using a fitted model: the profiler, scenarios and scoring.
    Simulation,
}

impl WorkspaceKind {
    /// Every kind, in the order the create dialog lists them.
    pub const ALL: [WorkspaceKind; 7] = [
        WorkspaceKind::DataExplorer,
        WorkspaceKind::ModelFit,
        WorkspaceKind::Report,
        WorkspaceKind::Map,
        WorkspaceKind::Dashboard,
        WorkspaceKind::Simulation,
        WorkspaceKind::Notebook,
    ];

    /// The kind's name as it is stored and sent.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceKind::DataExplorer => "data_explorer",
            WorkspaceKind::Dashboard => "dashboard",
            WorkspaceKind::ModelFit => "model_fit",
            WorkspaceKind::Notebook => "notebook",
            WorkspaceKind::Report => "report",
            WorkspaceKind::Map => "map",
            WorkspaceKind::Simulation => "simulation",
        }
    }

    /// The kind called `name`.
    pub fn parse(name: &str) -> Result<WorkspaceKind> {
        WorkspaceKind::ALL
            .into_iter()
            .find(|k| k.as_str() == name)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "`{name}` is not a kind of workspace; the kinds are {}",
                    WorkspaceKind::ALL
                        .iter()
                        .map(|k| format!("`{}`", k.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// Its name for a person.
    pub fn label(self) -> &'static str {
        match self {
            WorkspaceKind::DataExplorer => "Data explorer",
            WorkspaceKind::Dashboard => "Dashboard",
            WorkspaceKind::ModelFit => "Model fit",
            WorkspaceKind::Notebook => "Notebook",
            WorkspaceKind::Report => "Report",
            WorkspaceKind::Map => "Map",
            WorkspaceKind::Simulation => "Simulation",
        }
    }

    /// The milestone that brings it, when it is not here yet (`None` when it
    /// is): the Analytics UI plan's A2–A9. The notebook is not scheduled.
    pub fn arrives_in(self) -> Option<&'static str> {
        match self {
            WorkspaceKind::DataExplorer => Some("A2"),
            WorkspaceKind::ModelFit => Some("A3"),
            WorkspaceKind::Report => Some("A4"),
            WorkspaceKind::Map => Some("A5"),
            WorkspaceKind::Dashboard => Some("A6"),
            WorkspaceKind::Simulation => Some("A7"),
            WorkspaceKind::Notebook => Some("a later milestone"),
        }
    }

    /// Whether a workspace of this kind can be created and opened.
    pub fn is_available(self) -> bool {
        self.arrives_in().is_none()
    }

    /// Refuse, naming the milestone, a kind that is not here yet.
    pub fn check_available(self) -> Result<()> {
        match self.arrives_in() {
            Some(when) => Err(Error::invalid(format!(
                "a {} workspace cannot be created yet: it arrives with milestone {when} of the \
                 Analytics UI",
                self.label()
            ))),
            None => Ok(()),
        }
    }
}

/// Identifies a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub Uuid);

impl WorkspaceId {
    /// Mint an id for a new workspace.
    pub fn new() -> WorkspaceId {
        WorkspaceId(Uuid::new_v4())
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        WorkspaceId::new()
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// Stable identity.
    pub id: WorkspaceId,
    /// What the list shows.
    pub name: String,
    /// What it is for.
    pub kind: WorkspaceKind,
    /// The kind's own state, restored when it is opened; `{}` for a new one.
    pub state: Json,
    /// Who created it.
    pub created_by: Option<Uuid>,
    /// When it was last changed, its state included.
    pub updated_at: DateTime<Utc>,
}

impl Workspace {
    /// A new workspace with an empty state.
    pub fn new(
        name: impl Into<String>,
        kind: WorkspaceKind,
        created_by: Option<Uuid>,
    ) -> Workspace {
        Workspace {
            id: WorkspaceId::new(),
            name: name.into(),
            kind,
            state: Json::Object(serde_json::Map::new()),
            created_by,
            updated_at: Utc::now(),
        }
    }
}

const COL_ID: &str = "id";
const COL_NAME: &str = "name";
const COL_KIND: &str = "kind";
const COL_STATE: &str = "state";
const COL_CREATED_BY: &str = "created_by";
const COL_UPDATED_AT: &str = "updated_at";

fn workspace_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField::plain(COL_NAME, TypeRef::Basic(BasicType::Text)).required(),
        DataField::plain(COL_KIND, TypeRef::Basic(BasicType::Text)).required(),
        DataField::plain(COL_STATE, TypeRef::Basic(BasicType::Json)).required(),
        DataField::plain(COL_CREATED_BY, TypeRef::Basic(BasicType::Uuid)),
        DataField::plain(COL_UPDATED_AT, TypeRef::Basic(BasicType::Timestamp)).required(),
    ]
}

/// Ensure `_fd_workspaces` exists. Idempotent.
pub async fn bootstrap_workspaces(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(WORKSPACES_TABLE, &workspace_fields())
        .await
}

/// Create a workspace. Refused for an empty name. Any kind is stored: whether
/// one can be created yet is [`WorkspaceKind::check_available`], asked by the
/// API.
pub async fn create_workspace(catalog: &Catalog, workspace: &Workspace) -> Result<()> {
    check(workspace)?;
    let insert = Insert::row(
        WORKSPACES_TABLE,
        [
            COL_ID,
            COL_NAME,
            COL_KIND,
            COL_STATE,
            COL_CREATED_BY,
            COL_UPDATED_AT,
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect(),
        vec![
            Expr::lit(workspace.id.0),
            Expr::lit(workspace.name.trim()),
            Expr::lit(workspace.kind.as_str()),
            Expr::Lit(Value::Json(workspace.state.clone())),
            Expr::Lit(workspace.created_by.map_or(Value::Null, Value::Uuid)),
            Expr::Lit(Value::Timestamp(Utc::now())),
        ],
    );
    exec(catalog, insert.into()).await
}

/// Rename a workspace; its kind and state are untouched.
pub async fn rename_workspace(catalog: &Catalog, id: WorkspaceId, name: &str) -> Result<Workspace> {
    let mut workspace = require_workspace(catalog, id).await?;
    workspace.name = name.trim().to_owned();
    check(&workspace)?;
    update(
        catalog,
        id,
        vec![Assignment::new(COL_NAME, Expr::lit(workspace.name.clone()))],
    )
    .await?;
    require_workspace(catalog, id).await
}

/// Replace a workspace's state — what its screen does, debounced, as it
/// changes.
pub async fn save_workspace_state(
    catalog: &Catalog,
    id: WorkspaceId,
    state: Json,
) -> Result<Workspace> {
    if !state.is_object() {
        return Err(Error::invalid("a workspace's state is a JSON object"));
    }
    require_workspace(catalog, id).await?;
    update(
        catalog,
        id,
        vec![Assignment::new(COL_STATE, Expr::Lit(Value::Json(state)))],
    )
    .await?;
    require_workspace(catalog, id).await
}

/// Delete a workspace, answering whether there was one.
pub async fn delete_workspace(catalog: &Catalog, id: WorkspaceId) -> Result<bool> {
    if load_workspace(catalog, id).await?.is_none() {
        return Ok(false);
    }
    exec(
        catalog,
        Delete::from(WORKSPACES_TABLE)
            .filter(Expr::col(COL_ID).eq(Expr::lit(id.0)))
            .into(),
    )
    .await?;
    Ok(true)
}

/// The workspace with this id.
pub async fn load_workspace(catalog: &Catalog, id: WorkspaceId) -> Result<Option<Workspace>> {
    let select =
        Select::from(Source::table(WORKSPACES_TABLE)).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(from_row(row)?)),
        None => Ok(None),
    }
}

/// The workspace with this id, or not-found naming it.
pub async fn require_workspace(catalog: &Catalog, id: WorkspaceId) -> Result<Workspace> {
    load_workspace(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("there is no workspace with id {id}")))
}

/// Every workspace, most recently changed first.
pub async fn list_workspaces(catalog: &Catalog) -> Result<Vec<Workspace>> {
    let mut select = Select::from(Source::table(WORKSPACES_TABLE));
    select.order = vec![
        OrderBy::desc(Expr::col(COL_UPDATED_AT)),
        OrderBy::asc(Expr::col(COL_NAME)),
    ];
    rows(catalog, select).await?.iter().map(from_row).collect()
}

fn check(workspace: &Workspace) -> Result<()> {
    if workspace.name.trim().is_empty() {
        return Err(Error::invalid("a workspace needs a name"));
    }
    Ok(())
}

async fn update(catalog: &Catalog, id: WorkspaceId, mut set: Vec<Assignment>) -> Result<()> {
    set.push(Assignment::new(
        COL_UPDATED_AT,
        Expr::Lit(Value::Timestamp(Utc::now())),
    ));
    exec(
        catalog,
        Update::new(WORKSPACES_TABLE, set)
            .filter(Expr::col(COL_ID).eq(Expr::lit(id.0)))
            .into(),
    )
    .await
}

fn from_row(row: &Row) -> Result<Workspace> {
    let bad = |column: &str, v: Option<&Value>| {
        Error::invalid(format!(
            "{WORKSPACES_TABLE}.{column} is not readable ({})",
            v.map_or("missing", Value::kind)
        ))
    };
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => WorkspaceId(*u),
        other => return Err(bad(COL_ID, other)),
    };
    let name = match row.get(COL_NAME) {
        Some(Value::Text(t)) => t.clone(),
        other => return Err(bad(COL_NAME, other)),
    };
    let kind = match row.get(COL_KIND) {
        Some(Value::Text(t)) => WorkspaceKind::parse(t)?,
        other => return Err(bad(COL_KIND, other)),
    };
    let state = match row.get(COL_STATE) {
        Some(Value::Json(j)) => j.clone(),
        Some(Value::Text(t)) => serde_json::from_str(t).map_err(|e| {
            Error::invalid(format!("workspace `{name}`: its state is not JSON: {e}"))
        })?,
        other => return Err(bad(COL_STATE, other)),
    };
    let created_by = match row.get(COL_CREATED_BY) {
        Some(Value::Uuid(u)) => Some(*u),
        Some(Value::Null) | None => None,
        other => return Err(bad(COL_CREATED_BY, other)),
    };
    let updated_at = match row.get(COL_UPDATED_AT) {
        Some(Value::Timestamp(t)) => *t,
        other => return Err(bad(COL_UPDATED_AT, other)),
    };
    Ok(Workspace {
        id,
        name,
        kind,
        state,
        created_by,
        updated_at,
    })
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
    fn all_seven_kinds_parse_and_none_is_here_before_a2() {
        for kind in WorkspaceKind::ALL {
            assert_eq!(WorkspaceKind::parse(kind.as_str()).expect("parses"), kind);
        }
        assert!(WorkspaceKind::ALL.iter().all(|k| !k.is_available()));
        assert_eq!(WorkspaceKind::Map.arrives_in(), Some("A5"));
        let err = WorkspaceKind::DataExplorer
            .check_available()
            .expect_err("A2");
        assert!(err.to_string().contains("milestone A2"), "{err}");
        // The Dataset editor is not a kind of workspace.
        assert!(WorkspaceKind::parse("dataset_editor").is_err());
        assert!(WorkspaceKind::parse("spreadsheet").is_err());
    }
}

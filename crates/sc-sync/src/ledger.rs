//! The deployment ledger and the sync lock (APP_SYNC_PROPOSAL §6 steps 6 and
//! 10, §7.1).
//!
//! - `_fd_deployments` — one row per deploy, discard, commit or registry save:
//!   what was asked for, what happened (`converged`, `drifted`, `failed`), the
//!   operations applied and deferred, the backups written, and for a registry
//!   save the entity before and after. The history screen and "restore this
//!   version" read it.
//! - `_fd_sync_lock` — at most one row, held while a deploy or discard runs, so
//!   two of them (the UI and the CLI, or two browser tabs) never apply plans
//!   over each other. A row is a lock every process sharing the database sees;
//!   one older than [`STALE_AFTER`] is a process that died holding it, and is
//!   taken over.
//!
//! Both tables are additive and created on first use (and by the bootstrap
//! from S4.1), so no migration is needed.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value as Json;
use uuid::Uuid;

use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Delete, Expr, Insert, OrderBy, Select, Source as From, Statement, Value};
use sc_types::{BasicType, TypeRef};

/// The ledger.
pub const DEPLOYMENTS_TABLE: &str = "_fd_deployments";
/// The sync lock.
pub const SYNC_LOCK_TABLE: &str = "_fd_sync_lock";

/// How long a lock may be held before another process takes it over: far
/// longer than any deploy, which is a few schema batches and saves.
pub const STALE_AFTER: Duration = Duration::hours(1);

/// What a ledger row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// The tip (or a zip) applied to live.
    Deploy,
    /// Local changes returned to the base.
    Discard,
    /// Local changes committed and pushed.
    Commit,
    /// A registry save (§7.6).
    Edit,
}

impl LedgerKind {
    fn as_str(self) -> &'static str {
        match self {
            LedgerKind::Deploy => "deploy",
            LedgerKind::Discard => "discard",
            LedgerKind::Commit => "commit",
            LedgerKind::Edit => "edit",
        }
    }

    fn parse(raw: &str) -> Result<LedgerKind> {
        Ok(match raw {
            "deploy" => LedgerKind::Deploy,
            "discard" => LedgerKind::Discard,
            "commit" => LedgerKind::Commit,
            "edit" => LedgerKind::Edit,
            other => return Err(bad("kind", other)),
        })
    }
}

/// How it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Everything ticked was applied, and live now matches the target.
    Converged,
    /// Everything ticked was applied, but live still differs from the target
    /// in ways the plan did not account for (§6 step 9).
    Drifted,
    /// An operation failed; what came before it is applied.
    Failed,
}

impl Outcome {
    /// The word on the receipt and in the ledger.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Converged => "converged",
            Outcome::Drifted => "drifted",
            Outcome::Failed => "failed",
        }
    }

    fn parse(raw: &str) -> Result<Outcome> {
        Ok(match raw {
            "converged" => Outcome::Converged,
            "drifted" => Outcome::Drifted,
            "failed" => Outcome::Failed,
            other => return Err(bad("result", other)),
        })
    }
}

/// Where it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The admin UI or the admin API.
    Ui,
    /// `feldspar sync` on the server.
    Cli,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Ui => "ui",
            Source::Cli => "cli",
        }
    }

    fn parse(raw: &str) -> Result<Source> {
        Ok(match raw {
            "ui" => Source::Ui,
            "cli" => Source::Cli,
            other => return Err(bad("source", other)),
        })
    }
}

/// One row of the ledger.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Deployment {
    /// Its id, which the receipt carries.
    pub id: Uuid,
    /// What it was.
    pub kind: LedgerKind,
    /// How it ended.
    pub result: Outcome,
    /// The commit deployed (or committed), if one.
    pub commit: Option<String>,
    /// The branch, if one.
    pub branch: Option<String>,
    /// Who: the admin's email, or the operator.
    pub actor: Option<String>,
    /// From where.
    pub source: Source,
    /// What happened: the plan's counts, the operations applied and not, the
    /// notes, the drift.
    pub summary: Json,
    /// The operations left for a later deploy.
    pub deferred: Vec<String>,
    /// The backups written before applying.
    pub backups: Vec<String>,
    /// For a registry save, the entity before.
    pub before: Option<Json>,
    /// For a registry save, the entity after.
    pub after: Option<Json>,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// When it finished.
    pub finished_at: DateTime<Utc>,
}

fn uuid() -> TypeRef {
    TypeRef::Basic(BasicType::Uuid)
}
fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}
fn json() -> TypeRef {
    TypeRef::Basic(BasicType::Json)
}
fn timestamp() -> TypeRef {
    TypeRef::Basic(BasicType::Timestamp)
}

fn deployment_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", uuid()).required().primary_key(),
        DataField::plain("kind", text()).required(),
        DataField::plain("result", text()).required(),
        DataField::plain("commit_hash", text()),
        DataField::plain("branch", text()),
        DataField::plain("actor", text()),
        DataField::plain("source", text()).required(),
        DataField::plain("summary", json()).required(),
        DataField::plain("deferred", json()).required(),
        DataField::plain("backups", json()).required(),
        DataField::plain("before_json", json()),
        DataField::plain("after_json", json()),
        DataField::plain("started_at", timestamp()).required(),
        DataField::plain("finished_at", timestamp()).required(),
    ]
}

fn lock_fields() -> Vec<DataField> {
    vec![
        DataField::plain("name", text()).required().primary_key(),
        DataField::plain("holder", uuid()).required(),
        DataField::plain("what", text()).required(),
        DataField::plain("taken_at", timestamp()).required(),
    ]
}

/// Ensure the ledger and the lock table exist. Idempotent.
pub async fn bootstrap_ledger(catalog: &Catalog) -> Result<()> {
    if catalog.get(DEPLOYMENTS_TABLE)?.is_none() {
        catalog
            .bootstrap_table(DEPLOYMENTS_TABLE, &deployment_fields())
            .await?;
    }
    if catalog.get(SYNC_LOCK_TABLE)?.is_none() {
        catalog
            .bootstrap_table(SYNC_LOCK_TABLE, &lock_fields())
            .await?;
    }
    Ok(())
}

/// Add a row to the ledger.
pub async fn record(catalog: &Catalog, d: &Deployment) -> Result<()> {
    bootstrap_ledger(catalog).await?;
    let opt = |v: &Option<String>| v.clone().map_or(Value::Null, Value::Text);
    let list =
        |v: &[String]| Value::Json(Json::Array(v.iter().cloned().map(Json::String).collect()));
    let columns = [
        "id",
        "kind",
        "result",
        "commit_hash",
        "branch",
        "actor",
        "source",
        "summary",
        "deferred",
        "backups",
        "before_json",
        "after_json",
        "started_at",
        "finished_at",
    ];
    let values = vec![
        Value::Uuid(d.id),
        Value::Text(d.kind.as_str().to_owned()),
        Value::Text(d.result.as_str().to_owned()),
        opt(&d.commit),
        opt(&d.branch),
        opt(&d.actor),
        Value::Text(d.source.as_str().to_owned()),
        Value::Json(d.summary.clone()),
        list(&d.deferred),
        list(&d.backups),
        d.before.clone().map_or(Value::Null, Value::Json),
        d.after.clone().map_or(Value::Null, Value::Json),
        Value::Timestamp(d.started_at),
        Value::Timestamp(d.finished_at),
    ];
    let insert = Insert::row(
        DEPLOYMENTS_TABLE,
        columns.iter().map(|c| (*c).to_owned()).collect(),
        values.into_iter().map(Expr::Lit).collect(),
    );
    run(catalog, Statement::from(insert)).await
}

/// The newest `limit` rows of the ledger, newest first.
pub async fn list_deployments(catalog: &Catalog, limit: u64) -> Result<Vec<Deployment>> {
    if catalog.get(DEPLOYMENTS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    let mut select = Select::from(From::table(DEPLOYMENTS_TABLE)).limit(limit);
    select.order = vec![OrderBy::desc(Expr::col("started_at"))];
    rows(catalog, select)
        .await?
        .iter()
        .map(deployment_from_row)
        .collect()
}

fn deployment_from_row(row: &Row) -> Result<Deployment> {
    let text = |col: &str| match row.get(col) {
        Some(Value::Text(s)) => Ok(Some(s.clone())),
        None | Some(Value::Null) => Ok(None),
        Some(other) => Err(bad(col, &format!("{other:?}"))),
    };
    let required = |col: &str| {
        text(col)?.ok_or_else(|| Error::msg(format!("`{DEPLOYMENTS_TABLE}.{col}` is empty")))
    };
    let json = |col: &str| match row.get(col) {
        Some(Value::Json(v)) => Some(v.clone()),
        _ => None,
    };
    let list = |col: &str| -> Vec<String> {
        json(col)
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let at = |col: &str| match row.get(col) {
        Some(Value::Timestamp(t)) => Ok(*t),
        other => Err(bad(col, &format!("{other:?}"))),
    };
    Ok(Deployment {
        id: match row.get("id") {
            Some(Value::Uuid(id)) => *id,
            other => return Err(bad("id", &format!("{other:?}"))),
        },
        kind: LedgerKind::parse(&required("kind")?)?,
        result: Outcome::parse(&required("result")?)?,
        commit: text("commit_hash")?,
        branch: text("branch")?,
        actor: text("actor")?,
        source: Source::parse(&required("source")?)?,
        summary: json("summary").unwrap_or(Json::Null),
        deferred: list("deferred"),
        backups: list("backups"),
        before: json("before_json"),
        after: json("after_json"),
        started_at: at("started_at")?,
        finished_at: at("finished_at")?,
    })
}

// --- the lock ---------------------------------------------------------------------

/// The lock's one row's name.
const LOCK_NAME: &str = "sync";

/// The sync lock, held. Give it back with [`release`](SyncLock::release); a
/// process that dies holding it leaves a row the next one takes over once it
/// is [stale](STALE_AFTER).
#[derive(Debug)]
#[must_use = "a sync lock is held until it is released"]
pub struct SyncLock {
    holder: Uuid,
}

/// Take the sync lock for `what` (`deploy`, `discard`, …), or refuse with a
/// sentence saying what holds it.
pub async fn take_lock(catalog: &Catalog, what: &str) -> Result<SyncLock> {
    bootstrap_ledger(catalog).await?;
    let filter = || Expr::col("name").eq(Expr::lit(LOCK_NAME));
    let select = Select::from(From::table(SYNC_LOCK_TABLE)).filter(filter());
    if let Some(row) = rows(catalog, select).await?.first() {
        let taken_at = match row.get("taken_at") {
            Some(Value::Timestamp(t)) => *t,
            _ => DateTime::<Utc>::MIN_UTC,
        };
        if Utc::now() - taken_at < STALE_AFTER {
            let held_for = match row.get("what") {
                Some(Value::Text(w)) => w.clone(),
                _ => "a sync operation".to_owned(),
            };
            return Err(Error::invalid(format!(
                "another {held_for} has been running on this installation since {}; try again \
                 when it has finished",
                taken_at.format("%Y-%m-%d %H:%M:%S UTC")
            )));
        }
        // Its process died holding it.
        let holder = match row.get("holder") {
            Some(Value::Uuid(h)) => *h,
            _ => Uuid::nil(),
        };
        let delete = Delete::from(SYNC_LOCK_TABLE)
            .filter(filter().and(Expr::col("holder").eq(Expr::lit(holder))));
        run(catalog, Statement::from(delete)).await?;
    }
    let holder = Uuid::new_v4();
    let insert = Insert::row(
        SYNC_LOCK_TABLE,
        ["name", "holder", "what", "taken_at"]
            .iter()
            .map(|c| (*c).to_owned())
            .collect(),
        vec![
            Value::Text(LOCK_NAME.to_owned()),
            Value::Uuid(holder),
            Value::Text(what.to_owned()),
            Value::Timestamp(Utc::now()),
        ]
        .into_iter()
        .map(Expr::Lit)
        .collect(),
    );
    // Two processes that both found no row race for the insert; the primary
    // key lets one of them have it.
    run(catalog, Statement::from(insert)).await.map_err(|_| {
        Error::invalid(format!(
            "another sync operation started on this installation just now; try the {what} \
             again when it has finished"
        ))
    })?;
    Ok(SyncLock { holder })
}

impl SyncLock {
    /// Give the lock back.
    pub async fn release(self, catalog: &Catalog) -> Result<()> {
        let delete = Delete::from(SYNC_LOCK_TABLE).filter(
            Expr::col("name")
                .eq(Expr::lit(LOCK_NAME))
                .and(Expr::col("holder").eq(Expr::lit(self.holder))),
        );
        run(catalog, Statement::from(delete)).await
    }
}

// --- the database -----------------------------------------------------------------

/// The sync's own tables' access to the primary database: the one
/// `Unchecked::system` this crate makes (docs/SECURITY_REVIEW_2026-10.md's
/// system-authority inventory).
const INTERNAL_TABLES: sc_catalog::Unchecked<'static> =
    sc_catalog::Unchecked::system("sc-sync: the deployment ledger and the sync lock");

async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary(INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary(INTERNAL_TABLES)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

fn bad(column: &str, found: &str) -> Error {
    Error::msg(format!(
        "`{DEPLOYMENTS_TABLE}.{column}` holds `{found}`, which this version does not read"
    ))
}

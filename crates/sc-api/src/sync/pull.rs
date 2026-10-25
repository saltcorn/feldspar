//! The pull: a device asks "what changed since my watermark?" and gets the
//! current rows of what changed, read with its user's rights.
//!
//! The change-log (`_fd_changes`) only says *which* rows changed; the rows
//! themselves are read through [`ownership::read_rows_as`], the reader every
//! list endpoint uses, so a pull never shows a device more than a list would.
//!
//! **One pull is one moment.** Everything is read in one read-only
//! `REPEATABLE READ` transaction, the watermark included: all tables as they
//! were at the same instant, so a child row never arrives without its parent,
//! and the watermark names exactly what was read, so nothing comes twice. A
//! device that loses an answer asks again from its old watermark and gets it
//! again; that is the only repeat, and it does no harm.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use sc_auth::User;
use sc_catalog::{
    Catalog, ChangedEntry, SharedTx, SyncWatermark, Table, changed_entries, sync_watermark,
};
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_query::{Expr, InSet};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use super::device::{check_device, devices_table};
use crate::ownership;
use crate::rows::{self, Executor, RowQuery};
use crate::schema::{StructField, TypeSchema};

/// How many keys one read asks for: a long `IN (…)` list is still one
/// statement, but not an unbounded one.
const KEYS_PER_READ: usize = 500;

/// `POST {mount}/sync/pull`'s body.
///
/// - `watermark`: the one the device got from its last pull and has applied,
///   or null for a device with nothing yet.
/// - `tables`: the synced tables the device already holds. A synced table
///   missing from it (switched on since, or newly readable) comes in whole.
pub(crate) fn pull_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("client_id", TypeSchema::text()),
        StructField::new("watermark", TypeSchema::optional(TypeSchema::text())),
        StructField::new("tables", TypeSchema::array(TypeSchema::text())),
    ])
}

/// The answer: the next watermark, and one entry for **every** table the device
/// should hold — a table it has that is not listed is no longer its to keep.
///
/// Per table, `reset` means "replace your copy with `rows`"; otherwise `rows`
/// are written over the local ones and `removed` are deleted (gone, or no longer
/// readable by this user).
pub(crate) fn pull_output_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("watermark", TypeSchema::text()),
        StructField::new(
            "tables",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("table", TypeSchema::text()),
                StructField::new("reset", TypeSchema::bool()),
                StructField::new("rows", TypeSchema::array(TypeSchema::json())),
                StructField::new("removed", TypeSchema::array(TypeSchema::text())),
            ])),
        ),
    ])
}

/// Answer one pull of a device of `user`, over `exposed` — the application's
/// tables, of which the synced ones the user may read are delivered.
pub(crate) async fn pull(
    cat: &Catalog,
    application: Uuid,
    exposed: &[String],
    user: &User,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
    body: &Json,
) -> Result<Json> {
    let mut request = PullRequest::parse(body)?;
    let devices = devices_table(cat)?;
    let last_pull = check_device(cat, &devices, request.client_id, application, user).await?;
    // The watermark is from the device's last pull. Older than sync's
    // retention, the log may no longer hold what the device missed, so it
    // starts over (§14.11 "Cleanup").
    let horizon =
        chrono::Utc::now() - chrono::Duration::days(sc_catalog::retention::SYNC_KEEP_DAYS);
    if last_pull.is_some_and(|at| at < horizon) {
        request.watermark = None;
    }

    let synced: Vec<Table> = exposed
        .iter()
        .filter_map(|name| cat.get(name).ok().flatten())
        .filter(|table| table.sync_enabled() && may_read(table, user.role))
        .collect();

    let tx = SharedTx::begin_snapshot(cat).await?;
    let read = read_tables(cat, &tx, &request, &synced, user, evaluator).await;
    // Read-only: there is nothing to commit, whichever way the reads went.
    let _ = tx.rollback().await;
    let (watermark, since, tables) = read?;

    // What the device says it has applied — kept for the cleanup that will
    // remove entries every device is past.
    let mut seen = Map::new();
    seen.insert(
        "last_seen".into(),
        Json::String(chrono::Utc::now().to_rfc3339()),
    );
    if let Some(since) = &since {
        seen.insert("watermark".into(), json!(since.to_string()));
    }
    rows::update_row(
        cat,
        &devices,
        &request.client_id.to_string(),
        &Json::Object(seen),
    )
    .await?;

    Ok(json!({ "watermark": watermark.to_string(), "tables": tables }))
}

/// The watermark, the device's watermark when it is still usable, and one entry
/// per synced table, all read on `tx` (one snapshot).
async fn read_tables(
    cat: &Catalog,
    tx: &SharedTx,
    request: &PullRequest,
    synced: &[Table],
    user: &User,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
) -> Result<(SyncWatermark, Option<SyncWatermark>, Vec<Json>)> {
    let watermark = sync_watermark(cat, tx).await?;
    // A watermark this database never handed out (restored from a backup, or
    // another server's) says nothing about what the device has: start over.
    let since = request
        .watermark
        .clone()
        .filter(|w| w.not_after(&watermark));
    let executor = Executor::Transaction(tx.clone());
    let mut tables = Vec::with_capacity(synced.len());
    for table in synced {
        let entry = match since {
            Some(ref since) if request.tables.contains(&table.name) => {
                changes_of(cat, tx, table, since, &watermark, user, evaluator).await?
            }
            _ => json!({
                "table": table.name,
                "reset": true,
                "rows": ownership::read_rows_as_in(
                    cat, table, &RowQuery::new(), user.role, Some(user), evaluator, &executor,
                ).await?,
                "removed": [],
            }),
        };
        tables.push(entry);
    }
    Ok((watermark, since, tables))
}

/// One pull's request, checked.
struct PullRequest {
    client_id: Uuid,
    watermark: Option<SyncWatermark>,
    tables: BTreeSet<String>,
}

impl PullRequest {
    fn parse(body: &Json) -> Result<PullRequest> {
        let client_id = body
            .get("client_id")
            .and_then(Json::as_str)
            .and_then(|s| Uuid::parse_str(s.trim()).ok())
            .ok_or_else(|| Error::invalid("`client_id` must be the id `sync/register` answered"))?;
        let watermark = match body.get("watermark") {
            None | Some(Json::Null) => None,
            Some(Json::String(w)) => Some(SyncWatermark::parse(w)?),
            Some(_) => return Err(Error::invalid("`watermark` must be text or null")),
        };
        let tables = match body.get("tables") {
            None | Some(Json::Null) => BTreeSet::new(),
            Some(Json::Array(items)) => items
                .iter()
                .map(|t| {
                    t.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| Error::invalid("`tables` must list table names"))
                })
                .collect::<Result<_>>()?,
            Some(_) => return Err(Error::invalid("`tables` must list table names")),
        };
        Ok(PullRequest {
            client_id,
            watermark,
            tables,
        })
    }
}

/// Whether `role` may read `table` at all: the table's floor, or an ownership
/// formula or row-level security that decides row by row.
fn may_read(table: &Table, role: u8) -> bool {
    role <= table.access.min_role_read || table.ownership.is_some() || table.rls_enabled
}

/// What changed in `table` since the device's last pull: the changed rows as
/// they are now, and the keys of changed rows the user can no longer see
/// (deleted, or not theirs anymore) **but could before** — see [`was_held`].
async fn changes_of(
    cat: &Catalog,
    tx: &SharedTx,
    table: &Table,
    since: &SyncWatermark,
    watermark: &SyncWatermark,
    user: &User,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
) -> Result<Json> {
    let entries = changed_entries(cat, tx, &table.name, since, watermark).await?;
    let changed: BTreeSet<String> = entries.iter().map(|e| e.key.clone()).collect();
    let executor = Executor::Transaction(tx.clone());
    let pk = rows::single_pk(table)?;
    let mut found: BTreeMap<String, Json> = BTreeMap::new();
    let keys: Vec<&String> = changed.iter().collect();
    for chunk in keys.chunks(KEYS_PER_READ) {
        let set = chunk
            .iter()
            .map(|key| rows::column_value(table, &pk, &Json::String((*key).clone())).map(Expr::lit))
            .collect::<Result<Vec<_>>>()?;
        let query = RowQuery {
            filter: Some(Expr::In {
                e: Box::new(Expr::col(&pk)),
                set: InSet::List(set),
            }),
            ..RowQuery::new()
        };
        let read = ownership::read_rows_as_in(
            cat,
            table,
            &query,
            user.role,
            Some(user),
            evaluator,
            &executor,
        )
        .await?;
        for row in read.as_array().into_iter().flatten() {
            if let Some(key) = row.get(&pk).map(key_text) {
                found.insert(key, row.clone());
            }
        }
    }
    let unseen: Vec<&String> = changed.iter().filter(|k| !found.contains_key(*k)).collect();
    let removed = was_held(cat, table, &entries, &unseen, user, evaluator, &executor).await?;
    Ok(json!({
        "table": table.name,
        "reset": false,
        "rows": found.into_values().collect::<Vec<_>>(),
        "removed": removed,
    }))
}

/// Of the changed keys the user cannot read now, those whose row they could
/// read **as it was at the device's watermark** — the rows the device holds
/// and must drop (security review #29). Every other key is left out: listing
/// a changed key the user never could read would tell a device that another
/// user's (or tenant's) row exists, its key, and when it changed.
///
/// The row as it was is rebuilt from the change log: a row whose first entry
/// since the watermark is an insert did not exist then; otherwise it is the
/// row as it is now (read as the system; nothing, if deleted), with each
/// entry's `old_values` laid over it from the latest entry back to the
/// earliest, so every changed column takes the value it had before the first
/// change. That image is judged by the read rule ([`ownership::RowImage`]),
/// with the user's role now. A verdict that cannot be reached withholds the
/// key: the device keeps a stale row rather than learn of somebody else's.
async fn was_held(
    cat: &Catalog,
    table: &Table,
    entries: &[ChangedEntry],
    unseen: &[&String],
    user: &User,
    evaluator: Option<&Arc<dyn JsEvaluator>>,
    executor: &Executor,
) -> Result<Vec<String>> {
    if unseen.is_empty() {
        return Ok(Vec::new());
    }
    let pk = rows::single_pk(table)?;
    // The rows as they are now, whoever may read them.
    let mut current: BTreeMap<String, Map<String, Json>> = BTreeMap::new();
    for chunk in unseen.chunks(KEYS_PER_READ) {
        let set = chunk
            .iter()
            .map(|key| rows::column_value(table, &pk, &Json::String((*key).clone())).map(Expr::lit))
            .collect::<Result<Vec<_>>>()?;
        let query = RowQuery {
            filter: Some(Expr::In {
                e: Box::new(Expr::col(&pk)),
                set: InSet::List(set),
            }),
            ..RowQuery::new()
        };
        let read = ownership::read_rows_as_in(
            cat,
            table,
            &query,
            sc_auth::ROLE_ADMIN,
            None,
            evaluator,
            executor,
        )
        .await?;
        for row in read.as_array().into_iter().flatten() {
            if let (Some(key), Json::Object(map)) = (row.get(&pk).map(key_text), row) {
                current.insert(key, map.clone());
            }
        }
    }
    let mut held = Vec::new();
    for key in unseen {
        // `entries` is in log order, so this row's are too.
        let mine: Vec<&ChangedEntry> = entries.iter().filter(|e| &e.key == *key).collect();
        if mine.first().is_none_or(|first| first.op == "i") {
            continue;
        }
        let mut image = current.get(*key).cloned().unwrap_or_default();
        for entry in mine.iter().rev() {
            for (column, value) in entry.old_values.iter().flatten() {
                image.insert(column.clone(), value.clone());
            }
        }
        let image = Json::Object(image);
        let readable = ownership::RowImage::new(table, &image)
            .readable(cat, user.role, Some(user), evaluator)
            .await
            .unwrap_or(false);
        if readable {
            held.push((*key).clone());
        }
    }
    Ok(held)
}

/// A key as the change-log writes it: a text key as itself, any other as its
/// JSON rendering (`42`), which is what `->>` gives for a number.
pub(super) fn key_text(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

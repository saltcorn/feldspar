//! The change log's one cleanup job, shared by offline sync and row history
//! (§14.11 "Cleanup", §14.12 "Schema edits and retention").
//!
//! An entry is removed once **both** features are done with it:
//!
//! - *Sync* is done with it when the table is not synced, or when a later entry
//!   for the same row supersedes it and it is older than [`SYNC_KEEP_DAYS`].
//!   The row's latest entry stays while the table is synced: it is what tells a
//!   device the row changed, or is gone.
//! - *History* is done with it when the table has no history, or when the entry
//!   is older than the table's `keep_days` (none means kept forever).
//!
//! Then the files of rows whose delete entries went, when nothing references
//! them any more, then the changesets left with no entries, then sync's key
//! pairs older than its retention. Entries of a table that is no longer in the
//! catalog are removed whatever their age.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use sc_error::Result;
use sc_query::{Statement, Value};
use serde_json::Value as Json;

use crate::caller::CallerContext;
use crate::catalog::Catalog;
use crate::change_log::{ATTR_HISTORY, CHANGES_TABLE};
use crate::changeset::CHANGESETS_TABLE;
use crate::field::{Attrs, DbId};
use crate::sync::SYNC_KEYS_TABLE;
use crate::table::Table;

/// How long sync keeps an entry a later one has superseded: what a device's
/// field-wise merge looks back over (S3.3). A device whose last pull is older
/// than this reloads its tables.
pub const SYNC_KEEP_DAYS: i64 = 30;

/// How old an empty changeset must be before it is removed. A changeset's row
/// is written with its first entry, so an empty one is one whose entries have
/// all gone; the margin keeps the job clear of a write that is joining it.
const EMPTY_CHANGESET_GRACE_DAYS: i64 = 1;

/// The administrator's role, under which the job asks whether a row still
/// references a file: row-level security must not hide a row from it.
const ROLE_ADMIN: u8 = 1;

/// What one run removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleanup {
    /// Entries of `_fd_changes`.
    pub entries: u64,
    /// Files of deleted rows, from their stores.
    pub files: u64,
    /// Changesets with no entries left.
    pub changesets: u64,
    /// Sync's key pairs (`_fd_sync_keys`).
    pub sync_keys: u64,
}

/// How many days a table's history keeps an entry (`attributes.history
/// .keep_days`); `None` keeps it forever.
pub fn history_keep_days(attributes: &Attrs) -> Option<i64> {
    attributes
        .get(ATTR_HISTORY)
        .and_then(|h| h.get("keep_days"))
        .and_then(Json::as_i64)
        .filter(|days| *days >= 0)
}

/// Set how many days a table's history keeps an entry; `0` keeps it forever,
/// and removes the key. Only while history is on, like `min_role`.
pub fn set_history_keep_days(attributes: &mut Attrs, days: u32) {
    let Some(Json::Object(settings)) = attributes.get_mut(ATTR_HISTORY) else {
        return;
    };
    if days == 0 {
        settings.remove("keep_days");
    } else {
        settings.insert("keep_days".into(), Json::from(days));
    }
}

/// The condition under which an entry `c` of `table` in the log `log` may go,
/// with its binds numbered from `$2` (`$1` is the table's name), or `None` when
/// nothing of it may go yet.
fn removable(
    table: &Table,
    log: &str,
    now: DateTime<Utc>,
    binds: &mut Vec<Value>,
) -> Option<String> {
    let mut parts = Vec::new();
    if table.history_enabled() {
        let days = history_keep_days(&table.attributes)?;
        binds.push(Value::Timestamp(now - Duration::days(days)));
        parts.push(format!("c.at < ${}", binds.len()));
    }
    if table.sync_enabled() {
        binds.push(Value::Timestamp(now - Duration::days(SYNC_KEEP_DAYS)));
        parts.push(format!(
            "c.at < ${} AND EXISTS (SELECT 1 FROM {log} l WHERE l.table_name = c.table_name \
             AND l.row_key = c.row_key AND l.seq > c.seq)",
            binds.len(),
        ));
    }
    Some(if parts.is_empty() {
        "TRUE".to_owned()
    } else {
        parts.join(" AND ")
    })
}

async fn query(catalog: &Catalog, sql: String, binds: Vec<Value>) -> Result<Vec<sc_db::Row>> {
    catalog
        .primary()
        .query(&Statement::raw(sql, binds))
        .await?
        .try_collect()
        .await
}

fn count_of(rows: &[sc_db::Row]) -> u64 {
    match rows.first().and_then(|row| row.get_index(0)) {
        Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
        _ => 0,
    }
}

/// Run the cleanup as at `now`. A no-op on a backend with no change log.
pub async fn clean_change_log(catalog: &Catalog, now: DateTime<Utc>) -> Result<Cleanup> {
    let mut done = Cleanup::default();
    if !catalog.primary().capabilities().change_triggers || catalog.get(CHANGES_TABLE)?.is_none() {
        return Ok(done);
    }
    let log = catalog.primary().dialect().quote_ident(CHANGES_TABLE);
    let tables: Vec<Table> = catalog
        .tables()?
        .into_iter()
        .filter(|t| !t.is_system() && t.database == DbId::primary())
        .collect();

    // A file whose delete entry went, as `(table, column, store, path)`.
    let mut released: BTreeSet<(String, String, String, String)> = BTreeSet::new();
    for table in &tables {
        let mut binds = vec![Value::Text(table.name.clone())];
        let Some(condition) = removable(table, &log, now, &mut binds) else {
            continue;
        };
        let files = table.files_removed_on_delete();
        // The deleted rows' file paths, one array per removed delete entry.
        let paths = match files.is_empty() {
            true => "NULL::jsonb".to_owned(),
            false => format!(
                "jsonb_agg(jsonb_build_array({})) FILTER (WHERE op = 'd' AND old_values IS NOT NULL)",
                files
                    .iter()
                    .map(|(column, _)| format!("old_values ->> '{}'", column.replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let sql = format!(
            "WITH gone AS (DELETE FROM {log} c WHERE c.table_name = $1 AND {condition} \
               RETURNING c.op, c.old_values) \
             SELECT count(*)::int8 AS n, {paths} AS paths FROM gone"
        );
        let rows = query(catalog, sql, binds).await?;
        done.entries += count_of(&rows);
        let images = match rows.first().and_then(|r| r.get("paths")) {
            Some(Value::Json(Json::Array(images))) => images.clone(),
            Some(Value::Text(text)) => match serde_json::from_str(text) {
                Ok(Json::Array(images)) => images,
                _ => continue,
            },
            _ => continue,
        };
        for image in images {
            let Json::Array(values) = image else { continue };
            for ((column, store), path) in files.iter().zip(values) {
                if let Some(path) = path.as_str().filter(|p| !p.is_empty()) {
                    released.insert((
                        table.name.clone(),
                        (*column).to_owned(),
                        (*store).to_owned(),
                        path.to_owned(),
                    ));
                }
            }
        }
    }

    // Entries of tables that are gone from the catalog.
    let names = Json::Array(
        tables
            .iter()
            .map(|t| Json::String(t.name.clone()))
            .collect(),
    );
    let rows = query(
        catalog,
        format!(
            "WITH gone AS (DELETE FROM {log} \
               WHERE table_name NOT IN (SELECT jsonb_array_elements_text($1::jsonb)) \
               RETURNING 1) SELECT count(*)::int8 FROM gone"
        ),
        vec![Value::Json(names)],
    )
    .await?;
    done.entries += count_of(&rows);

    for (table, column, store, path) in &released {
        let Some(table) = tables.iter().find(|t| &t.name == table) else {
            continue;
        };
        match file_referenced(catalog, table, column, path).await {
            Ok(false) => {}
            Ok(true) => continue,
            Err(e) => {
                eprintln!(
                    "feldspar: keeping `{path}` of `{}`: checking for other references \
                     failed: {}",
                    table.name,
                    sc_error::format_chain(&e)
                );
                continue;
            }
        }
        let Ok(Some(store)) = catalog.file_store(store) else {
            continue;
        };
        match store.delete(path).await {
            Ok(_) => done.files += 1,
            Err(e) => eprintln!(
                "feldspar: removing `{path}`, whose row's history expired: {}",
                sc_error::format_chain(&e)
            ),
        }
    }

    let sets = catalog.primary().dialect().quote_ident(CHANGESETS_TABLE);
    let rows = query(
        catalog,
        format!(
            "WITH gone AS (DELETE FROM {sets} s WHERE s.at < $1 \
               AND NOT EXISTS (SELECT 1 FROM {log} c WHERE c.changeset = s.id) \
               RETURNING 1) SELECT count(*)::int8 FROM gone"
        ),
        vec![Value::Timestamp(
            now - Duration::days(EMPTY_CHANGESET_GRACE_DAYS),
        )],
    )
    .await?;
    done.changesets = count_of(&rows);

    if catalog.get(SYNC_KEYS_TABLE)?.is_some() {
        let keys = catalog.primary().dialect().quote_ident(SYNC_KEYS_TABLE);
        let rows = query(
            catalog,
            format!(
                "WITH gone AS (DELETE FROM {keys} WHERE at < $1 RETURNING 1) \
                 SELECT count(*)::int8 FROM gone"
            ),
            vec![Value::Timestamp(now - Duration::days(SYNC_KEEP_DAYS))],
        )
        .await?;
        done.sync_keys = count_of(&rows);
    }
    Ok(done)
}

/// Whether a row of `table`, or one of its remaining change-log entries, still
/// has `path` in `column`. Asked as the admin, so row-level security hides no
/// row from it.
async fn file_referenced(
    catalog: &Catalog,
    table: &Table,
    column: &str,
    path: &str,
) -> Result<bool> {
    // Quoted up front: the dialect is not `Send`, so it is not held across
    // the awaits below.
    let (quoted_table, quoted_column, log) = {
        let dialect = catalog.primary().dialect();
        (
            dialect.quote_ident(&table.name),
            dialect.quote_ident(column),
            dialect.quote_ident(CHANGES_TABLE),
        )
    };
    let row = Statement::raw(
        format!("SELECT 1 FROM {quoted_table} WHERE {quoted_column}::text = $1 LIMIT 1"),
        vec![Value::Text(path.to_owned())],
    );
    let context = CallerContext::new(ROLE_ADMIN, None);
    if !crate::rls::run_in_context_read_only(catalog, &context, &row)
        .await?
        .is_empty()
    {
        return Ok(true);
    }
    let entries = query(
        catalog,
        format!(
            "SELECT 1 FROM {log} WHERE table_name = $1 \
             AND (old_values ->> $2 = $3 OR new_values ->> $2 = $3) LIMIT 1"
        ),
        vec![
            Value::Text(table.name.clone()),
            Value::Text(column.to_owned()),
            Value::Text(path.to_owned()),
        ],
    )
    .await?;
    Ok(!entries.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_days_is_read_from_the_history_settings() {
        let mut attrs = Attrs::new();
        assert_eq!(history_keep_days(&attrs), None);
        attrs.insert(
            ATTR_HISTORY.into(),
            serde_json::json!({ "enabled": true, "keep_days": 30 }),
        );
        assert_eq!(history_keep_days(&attrs), Some(30));
        attrs.insert(ATTR_HISTORY.into(), serde_json::json!({ "enabled": true }));
        assert_eq!(history_keep_days(&attrs), None);
        set_history_keep_days(&mut attrs, 7);
        assert_eq!(history_keep_days(&attrs), Some(7));
        set_history_keep_days(&mut attrs, 0);
        assert_eq!(attrs[ATTR_HISTORY], serde_json::json!({ "enabled": true }));
    }

    #[test]
    fn keep_days_is_not_kept_without_history() {
        let mut attrs = Attrs::new();
        set_history_keep_days(&mut attrs, 7);
        assert!(attrs.is_empty());
    }
}

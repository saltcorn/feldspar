//! Offline sync's own part (§14.11, `TODO-offline-sync.md`): the devices, their
//! watermarks, and which keys changed between two of them.
//!
//! Recording the changes themselves is the change log's job, shared with row
//! history: see `change_log.rs`.

use std::collections::BTreeSet;

use sc_db::ColumnGenerator;
use sc_error::{Error, Result};
use sc_query::{Statement, Value};
use sc_types::{BasicType, TypeRef};
use serde_json::Value as Json;

use crate::catalog::Catalog;
use crate::field::{Attrs, DataField};
use crate::tx::SharedTx;

/// One row per registered device: whose it is, for which application, and the
/// watermark of the last pull it confirmed having applied.
pub const SYNC_CLIENTS_TABLE: &str = "_fd_sync_clients";

/// The real key of each row a device created with a temporary one (a table whose
/// key the server issues), by the device's change number: a retried upload gets
/// the key again, and a later change still naming the temporary key is translated.
pub const SYNC_KEYS_TABLE: &str = "_fd_sync_keys";

/// `_fd_tables.attributes` key holding a table's sync settings, an object
/// (`{ "enabled": true }`) so later settings have somewhere to go.
pub const ATTR_SYNC: &str = "sync";

/// The device table's columns. `watermark` is what the device last said it has
/// (it sends it with each pull), not what the server last handed out: an answer
/// lost on the way must not count as received.
fn clients_fields() -> Vec<DataField> {
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let timestamp = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain("id", uuid()).required().primary_key(),
        DataField::plain("user_id", uuid()).required(),
        DataField::plain("application", uuid()).required(),
        DataField::plain("name", TypeRef::Basic(BasicType::Text)),
        DataField::plain("watermark", TypeRef::Basic(BasicType::Text)),
        // The highest change number of this device's uploads already handled,
        // applied or refused: a retried upload skips everything up to it.
        DataField::plain("last_mutation", TypeRef::Basic(BasicType::Int)),
        DataField::plain("created_at", timestamp())
            .required()
            .generated(ColumnGenerator::Default("now()".into())),
        DataField::plain("last_seen", timestamp()),
    ]
}

/// The key table's columns: one row per device change that created a row with a
/// temporary key.
fn keys_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    vec![
        DataField::plain("client_id", TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField::plain("mutation", TypeRef::Basic(BasicType::Int))
            .required()
            .primary_key(),
        DataField::plain("table_name", text()).required(),
        DataField::plain("row_key", text()).required(),
        // When the pair was made: the cleanup removes it after sync's
        // retention, by which time the device has long had the answer.
        DataField::plain("at", TypeRef::Basic(BasicType::Timestamp))
            .generated(ColumnGenerator::Default("now()".into())),
    ]
}

/// Ensure the device table and the key table exist;
/// [`bootstrap_change_log`](crate::bootstrap_change_log) calls this.
pub(crate) async fn bootstrap_sync_clients(catalog: &Catalog) -> Result<()> {
    catalog
        .bootstrap_table(SYNC_CLIENTS_TABLE, &clients_fields())
        .await?;
    catalog
        .bootstrap_table(SYNC_KEYS_TABLE, &keys_fields())
        .await?;
    Ok(())
}

/// Where a pull stopped: a Postgres snapshot (`xmin:xmax:running`), as text on
/// the wire. Every transaction below `xmin` had finished, `running` had not,
/// and none from `xmax` on had started.
///
/// A whole snapshot, not one number, so a transaction that stays open (a long
/// import, a forgotten `BEGIN` in psql) holds back only its own changes: the
/// next pull picks it up by its id, and everything else moves on.
///
/// On SQLite, which runs one write transaction at a time, the log's order is
/// commit order, so the watermark is the last `seq` given out (`n:n:`, a
/// snapshot with nothing running), and the keys changed since are those of
/// entries with a later `seq` — which a merge moves forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncWatermark {
    xmin: i64,
    xmax: i64,
    running: Vec<i64>,
}

impl SyncWatermark {
    /// Read a watermark a pull answered; anything else is refused.
    pub fn parse(text: &str) -> Result<SyncWatermark> {
        let bad = || Error::invalid("`watermark` must be the one the last pull answered");
        let number = |s: &str| s.parse::<i64>().map_err(|_| bad());
        let mut parts = text.trim().split(':');
        let (Some(xmin), Some(xmax), Some(running), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        let (xmin, xmax) = (number(xmin)?, number(xmax)?);
        let running = running
            .split(',')
            .filter(|s| !s.is_empty())
            .map(number)
            .collect::<Result<Vec<_>>>()?;
        if xmin > xmax || running.iter().any(|x| *x < xmin || *x >= xmax) {
            return Err(bad());
        }
        Ok(SyncWatermark {
            xmin,
            xmax,
            running,
        })
    }

    /// The watermark at log position `seq` (SQLite).
    fn at_seq(seq: i64) -> SyncWatermark {
        SyncWatermark {
            xmin: seq,
            xmax: seq,
            running: Vec::new(),
        }
    }

    /// Whether this watermark is from before `other`, or the same — false for
    /// one this database never handed out (a backup's, another server's).
    pub fn not_after(&self, other: &SyncWatermark) -> bool {
        self.xmax <= other.xmax
    }
}

impl std::fmt::Display for SyncWatermark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let running: Vec<String> = self.running.iter().map(i64::to_string).collect();
        write!(f, "{}:{}:{}", self.xmin, self.xmax, running.join(","))
    }
}

fn require_sync(catalog: &Catalog) -> Result<()> {
    if catalog.primary().capabilities().change_triggers {
        Ok(())
    } else {
        Err(Error::invalid(
            "offline sync needs a database that can record changes with triggers, and this one \
             cannot",
        ))
    }
}

/// The watermark of `tx`: the snapshot its statements read. On a
/// [`SharedTx::begin_snapshot`] transaction that is the snapshot every read of
/// it sees, so what the reads return and what the watermark names agree.
pub async fn sync_watermark(catalog: &Catalog, tx: &SharedTx) -> Result<SyncWatermark> {
    require_sync(catalog)?;
    if catalog.primary().dialect().is_sqlite() {
        let rows = tx
            .run(
                None,
                &Statement::raw(
                    format!(
                        "SELECT max(coalesce((SELECT seq FROM sqlite_sequence WHERE name = '{log}'), 0), \
                         coalesce((SELECT max(seq) FROM \"{log}\"), 0)) AS seq",
                        log = crate::change_log::CHANGES_TABLE
                    ),
                    Vec::new(),
                ),
            )
            .await?;
        return match rows.first().and_then(|row| row.get_index(0)) {
            Some(Value::Int(seq)) => Ok(SyncWatermark::at_seq(*seq)),
            other => Err(Error::database(format!(
                "the database answered the watermark with {other:?}"
            ))),
        };
    }
    let rows = tx
        .run(
            None,
            &Statement::raw("SELECT txid_current_snapshot()::text", Vec::new()),
        )
        .await?;
    match rows.first().and_then(|row| row.get_index(0)) {
        Some(Value::Text(snapshot)) => SyncWatermark::parse(snapshot),
        other => Err(Error::database(format!(
            "the database answered the watermark with {other:?}"
        ))),
    }
}

/// The keys of `table` changed by a transaction that had committed at `until`
/// but not yet at `since`, read on `tx`.
pub async fn changed_keys(
    catalog: &Catalog,
    tx: &SharedTx,
    table: &str,
    since: &SyncWatermark,
    until: &SyncWatermark,
) -> Result<BTreeSet<String>> {
    Ok(changed_entries(catalog, tx, table, since, until)
        .await?
        .into_iter()
        .map(|entry| entry.key)
        .collect())
}

/// One change-log entry of a row: what a pull needs to tell what the row was
/// before the changes it is being sent.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedEntry {
    pub key: String,
    /// The changeset's net operation on the row: `i`, `u` or `d`.
    pub op: String,
    /// An update's changed columns before, or a deleted row whole.
    pub old_values: Option<serde_json::Map<String, Json>>,
    /// The entry's place in the log: entries of one row are in the order
    /// their changesets first wrote it.
    pub seq: i64,
}

/// The change-log entries of `table` written by a transaction that had
/// committed at `until` but not yet at `since`, read on `tx`, in `seq` order.
pub async fn changed_entries(
    catalog: &Catalog,
    tx: &SharedTx,
    table: &str,
    since: &SyncWatermark,
    until: &SyncWatermark,
) -> Result<Vec<ChangedEntry>> {
    require_sync(catalog)?;
    let (sql, binds) = if catalog.primary().dialect().is_sqlite() {
        (
            format!(
                "SELECT row_key, op, old_values, seq FROM \"{log}\" \
                 WHERE table_name = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq",
                log = crate::change_log::CHANGES_TABLE
            ),
            vec![
                Value::Text(table.to_owned()),
                Value::Int(since.xmax),
                Value::Int(until.xmax),
            ],
        )
    } else {
        // The txid bounds only narrow the search to the index; the snapshots
        // decide.
        (
            format!(
                "SELECT row_key, op, old_values, seq FROM {log} \
                 WHERE table_name = $1 AND txid >= $2 AND txid < $3 \
                   AND txid_visible_in_snapshot(txid, $4::text::txid_snapshot) \
                   AND NOT txid_visible_in_snapshot(txid, $5::text::txid_snapshot) \
                 ORDER BY seq",
                log = catalog
                    .primary()
                    .dialect()
                    .quote_ident(crate::change_log::CHANGES_TABLE),
            ),
            vec![
                Value::Text(table.to_owned()),
                Value::Int(since.xmin),
                Value::Int(until.xmax),
                Value::Text(until.to_string()),
                Value::Text(since.to_string()),
            ],
        )
    };
    let rows = tx.run(None, &Statement::raw(sql, binds)).await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let Some(Value::Text(key)) = row.get_index(0) else {
                return None;
            };
            let op = match row.get_index(1) {
                Some(Value::Text(op)) => op.clone(),
                _ => String::new(),
            };
            let old_values = match row.get_index(2) {
                Some(Value::Json(Json::Object(map))) => Some(map.clone()),
                Some(Value::Text(text)) => match serde_json::from_str(text) {
                    Ok(Json::Object(map)) => Some(map),
                    _ => None,
                },
                _ => None,
            };
            let seq = match row.get_index(3) {
                Some(Value::Int(seq)) => *seq,
                _ => 0,
            };
            Some(ChangedEntry {
                key: key.clone(),
                op,
                old_values,
                seq,
            })
        })
        .collect())
}

/// Whether `attributes` (a table's, or its overlay row's) has sync switched on.
pub fn sync_enabled(attributes: &Attrs) -> bool {
    attributes
        .get(ATTR_SYNC)
        .and_then(|s| s.get("enabled"))
        .and_then(Json::as_bool)
        .unwrap_or(false)
}

/// Switch sync on or off in `attributes`. Off removes the key, so a table that
/// never synced and one that stopped look the same.
pub fn set_sync_enabled(attributes: &mut Attrs, enabled: bool) {
    if enabled {
        attributes.insert(ATTR_SYNC.into(), serde_json::json!({ "enabled": true }));
    } else {
        attributes.remove(ATTR_SYNC);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_is_an_object_in_the_attributes_and_off_leaves_nothing() {
        let mut attrs = Attrs::new();
        assert!(!sync_enabled(&attrs));
        set_sync_enabled(&mut attrs, true);
        assert!(sync_enabled(&attrs));
        assert_eq!(attrs[ATTR_SYNC], serde_json::json!({ "enabled": true }));
        set_sync_enabled(&mut attrs, false);
        assert!(attrs.is_empty());
    }
}

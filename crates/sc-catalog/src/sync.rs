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
use uuid::Uuid;

use crate::CallerContext;
use crate::catalog::Catalog;
use crate::field::{Attrs, DataField};
use crate::tx::SharedTx;

/// Set by a device's upload for the length of its transaction, so the entries
/// it causes name the device (§14.11). Unset, the entry's `client_id` is null.
pub const SYNC_CLIENT_GUC: &str = "sc.sync_client";
/// The device's own number for the change being applied.
pub const SYNC_MUTATION_GUC: &str = "sc.sync_mutation";

/// One row per registered device: whose it is, for which application, and the
/// watermark of the last pull it confirmed having applied.
pub const SYNC_CLIENTS_TABLE: &str = "_fd_sync_clients";

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

/// Ensure the device table exists; [`bootstrap_change_log`](crate::bootstrap_change_log)
/// calls this.
pub(crate) async fn bootstrap_sync_clients(catalog: &Catalog) -> Result<()> {
    catalog
        .bootstrap_table(SYNC_CLIENTS_TABLE, &clients_fields())
        .await?;
    Ok(())
}

/// Mark the rest of `tx` as a device's upload: every change-log entry it writes
/// names `client` and the device's own number for the change, `mutation`.
pub async fn mark_sync_device(
    tx: &SharedTx,
    caller: Option<&CallerContext>,
    client: Uuid,
    mutation: i64,
) -> Result<()> {
    tx.run(
        caller,
        &Statement::raw(
            "SELECT set_config($1, $2, true), set_config($3, $4, true)",
            vec![
                Value::Text(SYNC_CLIENT_GUC.to_owned()),
                Value::Text(client.to_string()),
                Value::Text(SYNC_MUTATION_GUC.to_owned()),
                Value::Text(mutation.to_string()),
            ],
        ),
    )
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
            "offline sync needs a Postgres database; this one cannot record changes yet",
        ))
    }
}

/// The watermark of `tx`: the snapshot its statements read. On a
/// [`SharedTx::begin_snapshot`] transaction that is the snapshot every read of
/// it sees, so what the reads return and what the watermark names agree.
pub async fn sync_watermark(catalog: &Catalog, tx: &SharedTx) -> Result<SyncWatermark> {
    require_sync(catalog)?;
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
    require_sync(catalog)?;
    // The txid bounds only narrow the search to the index; the snapshots decide.
    let sql = format!(
        "SELECT DISTINCT row_key FROM {log} \
         WHERE table_name = $1 AND txid >= $2 AND txid < $3 \
           AND txid_visible_in_snapshot(txid, $4::text::txid_snapshot) \
           AND NOT txid_visible_in_snapshot(txid, $5::text::txid_snapshot)",
        log = catalog
            .primary()
            .dialect()
            .quote_ident(crate::change_log::CHANGES_TABLE),
    );
    let rows = tx
        .run(
            None,
            &Statement::raw(
                sql,
                vec![
                    Value::Text(table.to_owned()),
                    Value::Int(since.xmin),
                    Value::Int(until.xmax),
                    Value::Text(until.to_string()),
                    Value::Text(since.to_string()),
                ],
            ),
        )
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| match row.get_index(0) {
            Some(Value::Text(key)) => Some(key.clone()),
            _ => None,
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

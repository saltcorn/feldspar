//! The change log offline sync and row history share (§14.11, §14.12): recording
//! every change to a table that has either switched on.
//!
//! A table with sync **or** history switched on (`_fd_tables.attributes.sync` or
//! `.history`, each `{ "enabled": true }`) gets one row trigger, and every
//! insert, update and delete on it writes an entry to
//! [`_fd_changes`](CHANGES_TABLE) **in the same transaction** — so a change and
//! its entry commit or roll back together, and a write made outside Saltcorn
//! (`psql`, another program) is recorded like any other. The trigger stays while
//! either feature is on, and is dropped only when both are off.
//!
//! All tables share one `plpgsql` function, [`RECORD_FUNCTION`]; the trigger
//! passes it the name of the table's primary-key column. Reading the row as JSON
//! means a column added later is recorded without touching the trigger.
//!
//! User tables gain nothing: no version column, no "deleted" flag. A delete is an
//! entry in the log.
//!
//! What only sync needs — the devices, their watermarks — is in `sync.rs`.

use sc_db::ColumnGenerator;
use sc_error::{Error, Result};
use sc_query::SqlDialect;
use sc_types::{BasicType, TypeRef};
use serde_json::Value as Json;

use crate::catalog::{Catalog, SchemaStep};
use crate::field::{Attrs, DataField};
use crate::sync::{SYNC_CLIENT_GUC, SYNC_MUTATION_GUC, sync_enabled};
use crate::table::Table;

/// The change log: one entry per recorded insert, update or delete.
pub const CHANGES_TABLE: &str = "_fd_changes";
/// The function every change-log trigger calls.
pub const RECORD_FUNCTION: &str = "_fd_record_change";
/// The name of the trigger on a recorded table — one per table, so a table's
/// trigger is found and dropped by this name. (Postgres names triggers per
/// table, so sharing the log's name clashes with nothing.)
pub const CHANGES_TRIGGER: &str = "_fd_changes";

/// `_fd_tables.attributes` key holding a table's history settings, an object
/// (`{ "enabled": true, "keep_days": 365 }`) like sync's.
pub const ATTR_HISTORY: &str = "history";

/// Which feature asked for the change log, so a refusal can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeLogFeature {
    /// Offline sync (§14.11).
    Sync,
    /// Row history (§14.12).
    History,
}

impl ChangeLogFeature {
    /// The feature's name in a sentence.
    pub fn name(self) -> &'static str {
        match self {
            ChangeLogFeature::Sync => "offline sync",
            ChangeLogFeature::History => "row history",
        }
    }
}

/// The log's columns. `txid` is the writing transaction's id, which a pull
/// checks against its watermark snapshots, so a transaction that commits late
/// is never skipped.
fn changes_fields() -> Vec<DataField> {
    let int = || TypeRef::Basic(BasicType::Int);
    let text = || TypeRef::Basic(BasicType::Text);
    vec![
        DataField::plain("seq", int())
            .required()
            .primary_key()
            .generated(ColumnGenerator::Identity),
        DataField::plain("table_name", text()).required(),
        DataField::plain("row_key", text()).required(),
        DataField::plain("op", text()).required(),
        // An update's changed columns, as a JSON array of names; null otherwise.
        DataField::plain("fields", TypeRef::Basic(BasicType::Json)),
        DataField::plain("txid", int()).required(),
        DataField::plain("client_id", TypeRef::Basic(BasicType::Uuid)),
        DataField::plain("mutation", int()),
        DataField::plain("at", TypeRef::Basic(BasicType::Timestamp))
            .required()
            .generated(ColumnGenerator::Default("now()".into())),
    ]
}

/// Ensure the change log, its indexes, the trigger function and sync's device
/// table exist.
///
/// Idempotent, and a no-op on a backend that cannot record changes with a
/// trigger: switching a table's sync or history on is refused there with a
/// sentence saying so.
pub async fn bootstrap_change_log(catalog: &Catalog) -> Result<()> {
    if !catalog.primary().capabilities().change_triggers {
        return Ok(());
    }
    catalog
        .bootstrap_table(CHANGES_TABLE, &changes_fields())
        .await?;
    crate::sync::bootstrap_sync_clients(catalog).await?;
    let dialect = catalog.primary().dialect();
    let table = dialect.quote_ident(CHANGES_TABLE);
    catalog
        .apply_schema_batch(&[SchemaStep::Sql(format!(
            "CREATE INDEX IF NOT EXISTS {by_tx} ON {table} (txid, seq);\n\
             CREATE INDEX IF NOT EXISTS {by_row} ON {table} (table_name, row_key, txid);\n\
             {function}",
            by_tx = dialect.quote_ident("_fd_changes_txid"),
            by_row = dialect.quote_ident("_fd_changes_row"),
            function = record_function_sql(),
        ))])
        .await
}

/// The trigger function, `CREATE OR REPLACE`d on every boot so a fix to it
/// reaches every recorded table at once.
///
/// - An update that changes nothing records nothing.
/// - A change of primary key is recorded as a delete of the old key and an
///   insert of the new one: to a device, they are two different rows.
/// - The device and its change number come from transaction-local settings,
///   which only a device's upload sets.
/// - It runs with its owner's rights (`SECURITY DEFINER`), so whoever writes a
///   recorded table needs no rights on the change log, and cannot forge entries
///   in it. Its `search_path` is pinned to the schema the change log was created
///   in, so nobody can slip another `_fd_changes` in front of it.
fn record_function_sql() -> String {
    format!(
        r#"CREATE OR REPLACE FUNCTION {RECORD_FUNCTION}() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER AS $fd$
DECLARE
  pk      text  := TG_ARGV[0];
  new_row jsonb := CASE WHEN TG_OP <> 'DELETE' THEN to_jsonb(NEW) END;
  old_row jsonb := CASE WHEN TG_OP <> 'INSERT' THEN to_jsonb(OLD) END;
  changed jsonb;
  cid     uuid   := nullif(current_setting('{SYNC_CLIENT_GUC}', true), '')::uuid;
  mut     bigint := nullif(current_setting('{SYNC_MUTATION_GUC}', true), '')::bigint;
BEGIN
  IF TG_OP = 'UPDATE' THEN
    SELECT jsonb_agg(n.key ORDER BY n.key) INTO changed
      FROM jsonb_each(new_row) n
     WHERE n.value IS DISTINCT FROM old_row -> n.key;
    IF changed IS NULL THEN
      RETURN NULL;
    END IF;
    IF old_row ->> pk IS DISTINCT FROM new_row ->> pk THEN
      INSERT INTO {CHANGES_TABLE} (table_name, row_key, op, txid, client_id, mutation)
      VALUES (TG_TABLE_NAME, old_row ->> pk, 'd', txid_current(), cid, mut),
             (TG_TABLE_NAME, new_row ->> pk, 'i', txid_current(), cid, mut);
      RETURN NULL;
    END IF;
  END IF;
  INSERT INTO {CHANGES_TABLE} (table_name, row_key, op, fields, txid, client_id, mutation)
  VALUES (TG_TABLE_NAME, coalesce(new_row, old_row) ->> pk, lower(left(TG_OP, 1)),
          changed, txid_current(), cid, mut);
  RETURN NULL;
END
$fd$;
DO $fd$ BEGIN
  EXECUTE format('ALTER FUNCTION {RECORD_FUNCTION}() SET search_path = %I, pg_temp',
                 current_schema());
END $fd$;"#
    )
}

/// The DDL that starts recording `table`'s changes, keyed by the column `pk`.
/// Drops a trigger left from before first, so it also re-points the trigger
/// when the primary key changed.
pub fn enable_change_log_sql(dialect: &dyn SqlDialect, table: &str, pk: &str) -> String {
    format!(
        "{drop}\nCREATE TRIGGER {trigger} AFTER INSERT OR UPDATE OR DELETE ON {table} \
         FOR EACH ROW EXECUTE FUNCTION {RECORD_FUNCTION}({pk});",
        drop = disable_change_log_sql(dialect, table),
        trigger = dialect.quote_ident(CHANGES_TRIGGER),
        table = dialect.quote_ident(table),
        pk = dialect.quote_literal(pk),
    )
}

/// The DDL that stops recording `table`'s changes. The entries already in the
/// log stay: a device may still need them, and history reads them.
pub fn disable_change_log_sql(dialect: &dyn SqlDialect, table: &str) -> String {
    format!(
        "DROP TRIGGER IF EXISTS {} ON {};",
        dialect.quote_ident(CHANGES_TRIGGER),
        dialect.quote_ident(table)
    )
}

/// Whether `attributes` (a table's, or its overlay row's) has history switched on.
pub fn history_enabled(attributes: &Attrs) -> bool {
    attributes
        .get(ATTR_HISTORY)
        .and_then(|s| s.get("enabled"))
        .and_then(Json::as_bool)
        .unwrap_or(false)
}

/// Switch history on or off in `attributes`. On keeps the object's other
/// settings (`keep_days`); off removes the key, so a table that never kept
/// history and one that stopped look the same.
pub fn set_history_enabled(attributes: &mut Attrs, enabled: bool) {
    if enabled {
        let mut settings = match attributes.remove(ATTR_HISTORY) {
            Some(Json::Object(settings)) => settings,
            _ => serde_json::Map::new(),
        };
        settings.insert("enabled".into(), Json::Bool(true));
        attributes.insert(ATTR_HISTORY.into(), Json::Object(settings));
    } else {
        attributes.remove(ATTR_HISTORY);
    }
}

/// Whether `attributes` asks for the change-log trigger: sync or history is on.
pub fn change_log_enabled(attributes: &Attrs) -> bool {
    sync_enabled(attributes) || history_enabled(attributes)
}

/// Why `feature` cannot be switched on for `table`, or `Ok` with the
/// primary-key column its trigger is keyed by.
///
/// An entry addresses a row by its key, so the key must be one column. Rows
/// must be in Saltcorn's own database, since the trigger is created there, and
/// not in a metadata table.
pub fn change_log_key(
    catalog: &Catalog,
    table: &Table,
    feature: ChangeLogFeature,
) -> Result<String> {
    let name = &table.name;
    let feature = feature.name();
    if !catalog.primary().capabilities().change_triggers {
        return Err(Error::invalid(format!(
            "{feature} needs a Postgres database; this one cannot record changes yet"
        )));
    }
    if table.is_system() {
        return Err(Error::invalid(format!(
            "table `{name}` is one of Saltcorn's own tables; {feature} cannot be switched on \
             for it"
        )));
    }
    if let Some((module, provider)) = table.provider() {
        return Err(Error::invalid(format!(
            "table `{name}` is served by the table provider `{provider}` of `{module}`; \
             its rows are not in a database, so there is nothing to record changes in"
        )));
    }
    if table.database != crate::DbId::primary() {
        return Err(Error::invalid(format!(
            "table `{name}` lives in database connection `{}`; {feature} records changes \
             in Saltcorn's own database only",
            table.database.0
        )));
    }
    match table.primary_key.as_slice() {
        [pk] => Ok(pk.clone()),
        [] => Err(Error::invalid(format!(
            "table `{name}` has no primary key; {feature} finds a row by its key, so the \
             table needs one"
        ))),
        _ => Err(Error::invalid(format!(
            "table `{name}` has a primary key of several columns; {feature} needs a key \
             of one column"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::set_sync_enabled;

    #[test]
    fn history_is_an_object_in_the_attributes_and_off_leaves_nothing() {
        let mut attrs = Attrs::new();
        assert!(!history_enabled(&attrs));
        set_history_enabled(&mut attrs, true);
        assert!(history_enabled(&attrs));
        assert_eq!(attrs[ATTR_HISTORY], serde_json::json!({ "enabled": true }));
        set_history_enabled(&mut attrs, false);
        assert!(attrs.is_empty());
    }

    #[test]
    fn switching_history_on_keeps_its_other_settings() {
        let mut attrs = Attrs::new();
        attrs.insert(ATTR_HISTORY.into(), serde_json::json!({ "keep_days": 30 }));
        set_history_enabled(&mut attrs, true);
        assert_eq!(
            attrs[ATTR_HISTORY],
            serde_json::json!({ "enabled": true, "keep_days": 30 })
        );
    }

    #[test]
    fn the_log_is_wanted_while_either_feature_is_on() {
        let mut attrs = Attrs::new();
        assert!(!change_log_enabled(&attrs));
        set_sync_enabled(&mut attrs, true);
        set_history_enabled(&mut attrs, true);
        assert!(change_log_enabled(&attrs));
        set_sync_enabled(&mut attrs, false);
        assert!(change_log_enabled(&attrs));
        set_history_enabled(&mut attrs, false);
        assert!(!change_log_enabled(&attrs));
    }
}

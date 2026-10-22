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
//! On Postgres all tables share one `plpgsql` function, [`RECORD_FUNCTION`]; the
//! trigger passes it the name of the table's primary-key column. Reading the row
//! as JSON means a column added later is recorded without touching the trigger.
//! On SQLite each table's triggers are generated from its columns
//! (`change_log_sqlite`).
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
use crate::changeset::{CHANGE_LOG_GUC, CHANGESET_GUC, CHANGESETS_TABLE, changesets_fields};
use crate::field::{Attrs, DataField};
use crate::sync::sync_enabled;
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

/// The log's columns (§14.12 Storage): one entry per row per changeset.
/// `txid` is the **last** transaction that touched the entry, which a pull
/// checks against its watermark snapshots, so a transaction that commits late
/// is never skipped, and an entry merged into later is seen again.
fn changes_fields() -> Vec<DataField> {
    let int = || TypeRef::Basic(BasicType::Int);
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    vec![
        DataField::plain("seq", int())
            .required()
            .primary_key()
            .generated(ColumnGenerator::Identity),
        DataField::plain("changeset", TypeRef::Basic(BasicType::Uuid)).required(),
        DataField::plain("table_name", text()).required(),
        DataField::plain("row_key", text()).required(),
        // The net operation of the changeset on the row: `i`, `u` or `d`.
        DataField::plain("op", text()).required(),
        // An update's changed columns before, or a deleted row whole.
        DataField::plain("old_values", json()),
        // An update's changed columns after, or an inserted row whole.
        DataField::plain("new_values", json()),
        DataField::plain("txid", int()).required(),
        DataField::plain("at", TypeRef::Basic(BasicType::Timestamp))
            .required()
            .generated(ColumnGenerator::Default("now()".into())),
    ]
}

/// The function that merges one write into its changeset's entry for the row.
pub const MERGE_FUNCTION: &str = "_fd_merge_change";

/// The transaction-local marker naming the changeset whose `_fd_changesets`
/// row this transaction has already seen to, and whether it created it
/// (`<id>:new`) or found it (`<id>:old`).
const CHANGESET_TX_GUC: &str = "sc.changeset_tx";

/// Ensure the change log, the changesets, their indexes, the trigger function
/// and sync's device table exist.
///
/// Idempotent, and a no-op on a backend that cannot record changes with a
/// trigger: switching a table's sync or history on is refused there with a
/// sentence saying so.
pub async fn bootstrap_change_log(catalog: &Catalog) -> Result<()> {
    if !catalog.primary().capabilities().change_triggers {
        return Ok(());
    }
    let sqlite = catalog.primary().dialect().is_sqlite();
    if sqlite && catalog.get(CHANGES_TABLE)?.is_none() {
        // Its own DDL on SQLite, for `AUTOINCREMENT`: `seq` is the watermark
        // there, and must never be given out twice.
        catalog
            .apply_schema_batch(&[SchemaStep::Sql(
                crate::change_log_sqlite::changes_table_sql(),
            )])
            .await?;
        catalog.reload().await?;
    }
    catalog
        .bootstrap_table(CHANGESETS_TABLE, &changesets_fields())
        .await?;
    catalog
        .bootstrap_table(CHANGES_TABLE, &changes_fields())
        .await?;
    crate::sync::bootstrap_sync_clients(catalog).await?;
    let dialect = catalog.primary().dialect();
    let table = dialect.quote_ident(CHANGES_TABLE);
    let function = if sqlite {
        crate::change_log_sqlite::bootstrap_sql()
    } else {
        record_function_sql()
    };
    catalog
        .apply_schema_batch(&[SchemaStep::Sql(format!(
            "CREATE INDEX IF NOT EXISTS {by_tx} ON {table} (txid, seq);\n\
             CREATE INDEX IF NOT EXISTS {by_row} ON {table} (table_name, row_key, txid);\n\
             CREATE UNIQUE INDEX IF NOT EXISTS {entry} ON {table} (changeset, table_name, row_key);\n\
             CREATE INDEX IF NOT EXISTS {by_session} ON {sets} (session, at);\n\
             {function}",
            sets = dialect.quote_ident(CHANGESETS_TABLE),
            by_session = dialect.quote_ident("_fd_changesets_session"),
            by_tx = dialect.quote_ident("_fd_changes_txid"),
            by_row = dialect.quote_ident("_fd_changes_row"),
            entry = dialect.quote_ident("_fd_changes_entry"),
        ))])
        .await
}

/// The trigger function and its merge, `CREATE OR REPLACE`d on every boot so a
/// fix to them reaches every recorded table at once (§14.12 "The trigger
/// function, and the merge").
///
/// - An update that changes nothing records nothing.
/// - A change of primary key is recorded as a delete of the old key and an
///   insert of the new one: to a device, they are two different rows.
/// - The changeset is the writer's ([`CHANGESET_GUC`]), or one minted for the
///   transaction (`source = 'sql'`) when nothing set one. Its `_fd_changesets`
///   row is written with its first entry.
/// - Writes to one row in one changeset are merged into one entry: the earlier
///   old value and the later new value win, the operation is the net one, and
///   columns that ended where they started are dropped. An entry left with
///   nothing is removed only if its changeset began in this transaction (so
///   everything merged into it did too); otherwise it is kept without values,
///   because a device may have pulled the state in between.
/// - [`CHANGE_LOG_GUC`] `= 'off'` skips recording.
/// - It runs with its owner's rights (`SECURITY DEFINER`), so whoever writes a
///   recorded table needs no rights on the change log, and cannot forge entries
///   in it. Its `search_path` is pinned to the schema the change log was created
///   in, so nobody can slip another `_fd_changes` in front of it.
fn record_function_sql() -> String {
    format!(
        r#"CREATE OR REPLACE FUNCTION {MERGE_FUNCTION}(cs uuid, created boolean, tbl text,
  key text, op text, old_v jsonb, new_v jsonb) RETURNS void LANGUAGE plpgsql AS $fd$
DECLARE
  prev  {CHANGES_TABLE}%ROWTYPE;
  m_op  text;
  m_old jsonb;
  m_new jsonb;
  d_old jsonb;
  d_new jsonb;
BEGIN
  INSERT INTO {CHANGES_TABLE} (changeset, table_name, row_key, op, old_values, new_values, txid)
  VALUES (cs, tbl, key, op, old_v, new_v, txid_current())
  ON CONFLICT (changeset, table_name, row_key) DO NOTHING;
  IF FOUND THEN
    RETURN;
  END IF;
  SELECT * INTO prev FROM {CHANGES_TABLE}
   WHERE changeset = cs AND table_name = tbl AND row_key = key FOR UPDATE;
  m_old := coalesce(prev.old_values, '{{}}');
  m_new := coalesce(prev.new_values, '{{}}');
  IF prev.op = 'i' AND prev.new_values IS NULL THEN
    -- An insert taken back: the row was not there before, so this write
    -- starts afresh.
    m_op := op;  m_old := old_v;  m_new := new_v;
  ELSIF op = 'i' THEN
    IF prev.op = 'd' THEN
      m_op := 'u';  m_new := new_v;
    ELSE
      m_op := prev.op;  m_new := m_new || new_v;
    END IF;
  ELSIF op = 'u' THEN
    IF prev.op = 'i' THEN
      m_op := 'i';  m_old := NULL;
    ELSE
      m_op := 'u';  m_old := old_v || m_old;
    END IF;
    m_new := m_new || new_v;
  ELSIF prev.op = 'i' THEN
    m_op := 'i';  m_old := NULL;  m_new := NULL;
  ELSE
    -- The whole original row: the deleted one, overlaid with what the
    -- changeset had already changed.
    m_op := 'd';  m_old := old_v || m_old;  m_new := NULL;
  END IF;
  IF m_op = 'u' THEN
    SELECT jsonb_object_agg(k.key, m_old -> k.key), jsonb_object_agg(k.key, m_new -> k.key)
      INTO d_old, d_new
      FROM (SELECT jsonb_object_keys(m_old || m_new) AS key) k
     WHERE (m_old -> k.key) IS DISTINCT FROM (m_new -> k.key);
    m_old := d_old;  m_new := d_new;
  END IF;
  IF m_new IS NULL AND m_old IS NULL THEN
    IF created THEN
      DELETE FROM {CHANGES_TABLE} WHERE seq = prev.seq;
      RETURN;
    END IF;
  END IF;
  UPDATE {CHANGES_TABLE}
     SET op = m_op, old_values = m_old, new_values = m_new, txid = txid_current(), at = now()
   WHERE seq = prev.seq;
END
$fd$;
REVOKE ALL ON FUNCTION {MERGE_FUNCTION}(uuid, boolean, text, text, text, jsonb, jsonb) FROM PUBLIC;
CREATE OR REPLACE FUNCTION {RECORD_FUNCTION}() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER AS $fd$
DECLARE
  pk      text := TG_ARGV[0];
  new_row jsonb;
  old_row jsonb;
  old_v   jsonb;
  new_v   jsonb;
  cs      jsonb;
  cs_id   uuid;
  marker  text;
  created boolean;
BEGIN
  IF current_setting('{CHANGE_LOG_GUC}', true) = 'off' THEN
    RETURN NULL;
  END IF;
  new_row := CASE WHEN TG_OP <> 'DELETE' THEN to_jsonb(NEW) END;
  old_row := CASE WHEN TG_OP <> 'INSERT' THEN to_jsonb(OLD) END;
  IF TG_OP = 'UPDATE' THEN
    SELECT jsonb_object_agg(n.key, old_row -> n.key), jsonb_object_agg(n.key, n.value)
      INTO old_v, new_v
      FROM jsonb_each(new_row) n
     WHERE n.value IS DISTINCT FROM old_row -> n.key;
    IF new_v IS NULL THEN
      RETURN NULL;
    END IF;
  END IF;

  cs := nullif(current_setting('{CHANGESET_GUC}', true), '')::jsonb;
  IF cs IS NULL THEN
    cs := jsonb_strip_nulls(jsonb_build_object(
      'id', gen_random_uuid(), 'source', 'sql',
      'user_id', nullif(current_setting('sc.user', true), '')::jsonb ->> 'id'));
    PERFORM set_config('{CHANGESET_GUC}', cs::text, true);
  END IF;
  cs_id := (cs ->> 'id')::uuid;
  marker := coalesce(current_setting('{CHANGESET_TX_GUC}', true), '');
  IF marker LIKE cs_id::text || ':%' THEN
    created := marker = cs_id::text || ':new';
  ELSE
    INSERT INTO {CHANGESETS_TABLE} (id, txid, user_id, session, source, application,
                                description, cause, undo_table, client_id, mutation)
    VALUES (cs_id, txid_current(), (cs ->> 'user_id')::uuid, cs ->> 'session',
            coalesce(cs ->> 'source', 'sql'), (cs ->> 'application')::uuid,
            cs ->> 'description', (cs ->> 'cause')::uuid, cs ->> 'undo_table',
            (cs ->> 'client_id')::uuid, (cs ->> 'mutation')::bigint)
    ON CONFLICT (id) DO NOTHING;
    created := FOUND;
    PERFORM set_config('{CHANGESET_TX_GUC}',
                       cs_id::text || CASE WHEN created THEN ':new' ELSE ':old' END, true);
  END IF;

  IF TG_OP = 'INSERT' THEN
    PERFORM {MERGE_FUNCTION}(cs_id, created, TG_TABLE_NAME, new_row ->> pk, 'i', NULL, new_row);
  ELSIF TG_OP = 'DELETE' THEN
    PERFORM {MERGE_FUNCTION}(cs_id, created, TG_TABLE_NAME, old_row ->> pk, 'd', old_row, NULL);
  ELSIF old_row ->> pk IS DISTINCT FROM new_row ->> pk THEN
    PERFORM {MERGE_FUNCTION}(cs_id, created, TG_TABLE_NAME, old_row ->> pk, 'd', old_row, NULL);
    PERFORM {MERGE_FUNCTION}(cs_id, created, TG_TABLE_NAME, new_row ->> pk, 'i', NULL, new_row);
  ELSE
    PERFORM {MERGE_FUNCTION}(cs_id, created, TG_TABLE_NAME, new_row ->> pk, 'u', old_v, new_v);
  END IF;
  RETURN NULL;
END
$fd$;
DO $fd$ BEGIN
  EXECUTE format('ALTER FUNCTION {RECORD_FUNCTION}() SET search_path = %I, pg_temp',
                 current_schema());
  EXECUTE format('ALTER FUNCTION {MERGE_FUNCTION}(uuid, boolean, text, text, text, jsonb, jsonb) '
                 'SET search_path = %I, pg_temp', current_schema());
END $fd$;"#
    )
}

/// The DDL that starts recording `table`'s changes, keyed by the column `pk`.
/// Drops a trigger left from before first, so it also re-points the trigger
/// when the primary key changed.
///
/// On SQLite the triggers are generated from `table`'s stored columns, so it
/// must be the table as the batch leaves it.
pub fn enable_change_log_sql(dialect: &dyn SqlDialect, table: &Table, pk: &str) -> String {
    if dialect.is_sqlite() {
        return crate::change_log_sqlite::enable_sql(&table.name, pk, &stored_columns(table));
    }
    format!(
        "{drop}\nCREATE TRIGGER {trigger} AFTER INSERT OR UPDATE OR DELETE ON {name} \
         FOR EACH ROW EXECUTE FUNCTION {RECORD_FUNCTION}({pk});",
        drop = disable_change_log_sql(dialect, &table.name),
        trigger = dialect.quote_ident(CHANGES_TRIGGER),
        name = dialect.quote_ident(&table.name),
        pk = dialect.quote_literal(pk),
    )
}

/// The DDL that stops recording `table`'s changes. The entries already in the
/// log stay: a device may still need them, and history reads them.
pub fn disable_change_log_sql(dialect: &dyn SqlDialect, table: &str) -> String {
    if dialect.is_sqlite() {
        return crate::change_log_sqlite::disable_sql(table);
    }
    format!(
        "DROP TRIGGER IF EXISTS {} ON {};",
        dialect.quote_ident(CHANGES_TRIGGER),
        dialect.quote_ident(table)
    )
}

/// `table`'s stored columns, with the SQL type each is declared with: every
/// field but a calculated one, which has no column.
pub(crate) fn stored_columns(table: &Table) -> Vec<(String, String)> {
    table
        .fields
        .iter()
        .filter(|f| !f.is_calc())
        .map(|f| (f.base.name.clone(), f.base.type_.sql_type().to_owned()))
        .collect()
}

/// A row `alias` of `table` as the JSON object the log records rows in: what
/// the trigger would record for it, to compare with an entry or rebuild a
/// version from.
pub fn row_json_sql(dialect: &dyn SqlDialect, table: &Table, alias: &str) -> String {
    if dialect.is_sqlite() {
        crate::change_log_sqlite::row_json_sql(alias, &stored_columns(table))
    } else {
        format!("to_jsonb({alias})")
    }
}

/// A column (or any expression) as text, to compare with a `row_key`.
pub fn as_text_sql(expr: &str) -> String {
    format!("CAST({expr} AS TEXT)")
}

/// The texts of a JSON array bound as `bind` (`$1`), as a subquery for `IN`.
pub fn json_texts_sql(dialect: &dyn SqlDialect, bind: &str) -> String {
    if dialect.is_sqlite() {
        format!("SELECT value FROM json_each({bind})")
    } else {
        format!("SELECT jsonb_array_elements_text({bind}::jsonb)")
    }
}

/// The aggregate collecting `expr`'s distinct values into a JSON array.
pub fn json_distinct_array_sql(dialect: &dyn SqlDialect, expr: &str) -> String {
    if dialect.is_sqlite() {
        format!("json_group_array(DISTINCT {expr})")
    } else {
        format!("jsonb_agg(DISTINCT {expr})")
    }
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
            "{feature} needs a database that can record changes with triggers, and this \
             one cannot"
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

//! The change log on SQLite (§14.12, TODO-history H6.2): the same log, the same
//! changesets and the same merge as Postgres's `plpgsql` function, in the SQL a
//! SQLite trigger can run.
//!
//! Two differences shape everything here:
//!
//! - **A SQLite trigger cannot read a row generically** — there is no
//!   `to_jsonb(NEW)` — so each recorded table gets three triggers (insert,
//!   update, delete) generated from its columns, and they are regenerated with
//!   every schema change to the table ([`crate::Catalog::apply_schema_batch_in`]
//!   drops them before the batch and writes them again after it). They only
//!   turn the row into JSON and insert it into the view [`RECORD_VIEW`].
//! - **There are no variables and no functions**, so the merge is one
//!   `INSTEAD OF INSERT` trigger on that view, written once, whose statements
//!   find the changeset, write its row, upsert the entry, merge and diff it,
//!   and remove it when it is emptied — each statement selecting what it needs.
//!
//! **The writer's changeset** reaches the trigger through
//! [`TX_SETTINGS_TABLE`], where SQLite's
//! [`set_local`](sc_db::Transaction::set_local) keeps it. A write with no
//! changeset — from outside Saltcorn, or from a transaction that set none —
//! mints one with `source = 'sql'`: for a Saltcorn transaction it is kept for
//! the rest of the transaction, as on Postgres, and for another program's write
//! it is removed again at once, so such a write gets **one changeset per row**
//! (nothing tells a trigger where another program's transaction ends).
//!
//! **`seq` is the sync watermark here** (one writer at a time, so the log's
//! order is commit order), so a merge moves the entry's `seq` past every
//! number given out so far, `AUTOINCREMENT` keeps a number from being given out
//! twice, and `txid` holds the `seq` the write was given (SQLite has no
//! transaction ids).
//!
//! Values are recorded as SQLite stores them, with two exceptions so an entry
//! reads like Postgres's: a boolean column is JSON `true`/`false`, and a JSON
//! column holding valid JSON is that JSON rather than a string.

use sc_db::{SchemaChange, TX_SETTINGS_TABLE};
use sc_error::Result;
use sc_query::{Statement, Value};

use crate::change_log::CHANGES_TABLE;
use crate::changeset::{CHANGE_LOG_GUC, CHANGESET_GUC, CHANGESETS_TABLE};

/// The view the per-table triggers insert into; its `INSTEAD OF` trigger, of
/// the same name, records the change.
pub const RECORD_VIEW: &str = "_fd_record_change";

/// The settings key marking the changeset whose row this transaction has seen
/// to (`<id>:new` when it created it, `<id>:old` when it found it).
const CHANGESET_TX: &str = "sc.changeset_tx";
/// The settings key marking a changeset minted by the trigger.
const MINTED: &str = "sc.minted";
/// A setting every Saltcorn transaction keeps (`sc.role`): its absence means a
/// write from another program, whose minted changeset is removed at once.
const ROLE: &str = "sc.role";
/// The signed-in user, whose id a minted changeset carries.
const USER: &str = "sc.user";

/// How a column's value is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A boolean, stored as 0/1, recorded as JSON `true`/`false`.
    Bool,
    /// JSON, stored as text, recorded as the JSON it holds.
    Json,
    /// Anything else, as stored (a blob as `\x` and its hex, as Postgres does).
    Plain,
}

impl Shape {
    /// The shape of a column declared `sql_type`.
    fn of(sql_type: &str) -> Shape {
        let name = sql_type
            .split('(')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match name.as_str() {
            "bool" | "boolean" => Shape::Bool,
            "json" | "jsonb" => Shape::Json,
            _ => Shape::Plain,
        }
    }
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A trigger's name: SQLite names triggers per database, so it carries the
/// table's.
fn trigger_name(table: &str, op: &str) -> String {
    quote(&format!("_fd_changes_{table}_{op}"))
}

/// Whether a trigger called `name` is one of this module's, for `table`.
pub fn is_table_trigger(name: &str, table: &str) -> bool {
    ["ins", "upd", "del"]
        .iter()
        .any(|op| name == format!("_fd_changes_{table}_{op}"))
}

/// Column `column` of `row` (`NEW`/`OLD`) as a JSON value, for `json_object`.
fn cell(row: &str, column: &str, shape: Shape) -> String {
    let x = format!("{row}.{}", quote(column));
    match shape {
        Shape::Bool => {
            format!(
                "CASE WHEN {x} IS NULL THEN NULL WHEN {x} THEN json('true') ELSE json('false') END"
            )
        }
        Shape::Json => format!("CASE WHEN json_valid({x}) THEN json({x}) ELSE {x} END"),
        Shape::Plain => {
            format!("CASE WHEN typeof({x}) = 'blob' THEN '\\x' || lower(hex({x})) ELSE {x} END")
        }
    }
}

/// The value `v` of JSON type `t` (as `json_each` names types, plus `json` for
/// text that is JSON) as a JSON value again: what a value loses passing through
/// a subquery, a boolean or nested JSON, put back.
fn rewrap(v: &str, t: &str) -> String {
    format!(
        "CASE {t} WHEN 'true' THEN json('true') WHEN 'false' THEN json('false') \
         WHEN 'object' THEN json({v}) WHEN 'array' THEN json({v}) WHEN 'json' THEN json({v}) \
         ELSE {v} END"
    )
}

/// The whole of `row` as a JSON object.
fn row_object(row: &str, columns: &[(String, Shape)]) -> String {
    let pairs: Vec<String> = columns
        .iter()
        .map(|(c, shape)| format!("{}, {}", literal(c), cell(row, c, *shape)))
        .collect();
    format!("json_object({})", pairs.join(", "))
}

/// The columns an update changed, as `row` (`OLD` or `NEW`) has them.
fn changed_object(row: &str, columns: &[(String, Shape)]) -> String {
    let parts: Vec<String> = columns
        .iter()
        .map(|(c, shape)| {
            let x = format!("{row}.{}", quote(c));
            let (v, t) = match shape {
                Shape::Bool => (
                    format!(
                        "CASE WHEN {x} IS NULL THEN NULL WHEN {x} THEN 'true' ELSE 'false' END"
                    ),
                    format!(
                        "CASE WHEN {x} IS NULL THEN 'null' WHEN {x} THEN 'true' ELSE 'false' END"
                    ),
                ),
                Shape::Json => (
                    x.clone(),
                    format!("CASE WHEN json_valid({x}) THEN 'json' ELSE 'text' END"),
                ),
                Shape::Plain => (
                    format!(
                        "CASE WHEN typeof({x}) = 'blob' THEN '\\x' || lower(hex({x})) ELSE {x} END"
                    ),
                    "'text'".to_owned(),
                ),
            };
            format!(
                "SELECT {} AS k, {v} AS v, {t} AS t WHERE NEW.{q} IS NOT OLD.{q}",
                literal(c),
                q = quote(c)
            )
        })
        .collect();
    format!(
        "(SELECT json_group_object(k, {}) FROM ({}))",
        rewrap("v", "t"),
        parts.join(" UNION ALL ")
    )
}

/// The DDL that records `table`'s changes, keyed by `pk`, over its stored
/// `columns` (name and declared SQL type). Drops the triggers left from
/// before first, so it also regenerates them.
pub fn enable_sql(table: &str, pk: &str, columns: &[(String, String)]) -> String {
    let columns: Vec<(String, Shape)> = columns
        .iter()
        .map(|(name, sql_type)| (name.clone(), Shape::of(sql_type)))
        .collect();
    let on = quote(table);
    let name = literal(table);
    let key = |row: &str| format!("CAST({row}.{} AS TEXT)", quote(pk));
    let pk_q = quote(pk);
    let off = format!(
        "NOT EXISTS (SELECT 1 FROM {TX_SETTINGS_TABLE} WHERE name = {} AND value = 'off')",
        literal(CHANGE_LOG_GUC)
    );
    let changed: Vec<String> = columns
        .iter()
        .map(|(c, _)| format!("NEW.{q} IS NOT OLD.{q}", q = quote(c)))
        .collect();
    let insert =
        format!("INSERT INTO {RECORD_VIEW} (table_name, row_key, op, old_values, new_values)");
    format!(
        "{drop}\n\
         CREATE TRIGGER {ins} AFTER INSERT ON {on} WHEN {off} BEGIN\n  \
           {insert}\n  VALUES ({name}, {new_key}, 'i', NULL, {new_row});\n\
         END;\n\
         CREATE TRIGGER {del} AFTER DELETE ON {on} WHEN {off} BEGIN\n  \
           {insert}\n  VALUES ({name}, {old_key}, 'd', {old_row}, NULL);\n\
         END;\n\
         CREATE TRIGGER {upd} AFTER UPDATE ON {on} WHEN ({changed}) AND {off} BEGIN\n  \
           {insert}\n  SELECT {name}, {old_key}, 'd', {old_row}, NULL WHERE NEW.{pk_q} IS NOT OLD.{pk_q};\n  \
           {insert}\n  SELECT {name}, {new_key}, 'i', NULL, {new_row} WHERE NEW.{pk_q} IS NOT OLD.{pk_q};\n  \
           {insert}\n  SELECT {name}, {new_key}, 'u', {old_changed}, {new_changed} \
             WHERE NEW.{pk_q} IS OLD.{pk_q};\n\
         END;",
        drop = disable_sql(table),
        ins = trigger_name(table, "ins"),
        del = trigger_name(table, "del"),
        upd = trigger_name(table, "upd"),
        changed = if changed.is_empty() {
            "0".to_owned()
        } else {
            changed.join(" OR ")
        },
        new_key = key("NEW"),
        old_key = key("OLD"),
        new_row = row_object("NEW", &columns),
        old_row = row_object("OLD", &columns),
        old_changed = changed_object("OLD", &columns),
        new_changed = changed_object("NEW", &columns),
    )
}

/// The DDL that stops recording `table`'s changes.
pub fn disable_sql(table: &str) -> String {
    ["ins", "upd", "del"]
        .iter()
        .map(|op| format!("DROP TRIGGER IF EXISTS {};", trigger_name(table, op)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The row `alias` of a table with these stored `columns` as the JSON object
/// the log records rows in — the SQLite spelling of `to_jsonb(alias)`.
pub fn row_json_sql(alias: &str, columns: &[(String, String)]) -> String {
    let columns: Vec<(String, Shape)> = columns
        .iter()
        .map(|(name, sql_type)| (name.clone(), Shape::of(sql_type)))
        .collect();
    row_object(alias, &columns)
}

/// The next `seq`: past every number given out, including those of entries
/// removed since.
fn next_seq() -> String {
    format!(
        "(SELECT max(coalesce((SELECT seq FROM sqlite_sequence WHERE name = {log_lit}), 0), \
         coalesce((SELECT max(seq) FROM {CHANGES_TABLE}), 0)) + 1)",
        log_lit = literal(CHANGES_TABLE),
    )
}

/// `a` and `b` merged, `b`'s value winning for a key both have.
fn merge(a: &str, b: &str) -> String {
    format!(
        "(SELECT json_group_object(key, {}) FROM (\
           SELECT key, value, type FROM json_each({a}) \
            WHERE key NOT IN (SELECT key FROM json_each({b})) \
           UNION ALL SELECT key, value, type FROM json_each({b})))",
        rewrap("value", "type")
    )
}

/// An entry's `old_values` (`side` "old") or `new_values` ("new") left with
/// only the columns whose value changed; `NULL` when none did.
fn diff(side: &str) -> String {
    let (v, t) = if side == "old" {
        ("ov", "ot")
    } else {
        ("nv", "nt")
    };
    format!(
        "(SELECT nullif(json_group_object(k, {}), '{{}}') FROM (\
           SELECT u.key AS k, o.value AS ov, o.type AS ot, n.value AS nv, n.type AS nt FROM \
             (SELECT key FROM json_each(old_values) UNION SELECT key FROM json_each(new_values)) u \
             LEFT JOIN json_each(old_values) o ON o.key = u.key \
             LEFT JOIN json_each(new_values) n ON n.key = u.key) \
         WHERE ov IS NOT nv OR ot IS NOT nt)",
        rewrap(v, t)
    )
}

/// The settings table, the view and the trigger that records a change:
/// dropped and created again on every boot, so a fix reaches every database at
/// once (the counterpart of `CREATE OR REPLACE FUNCTION`).
///
/// The merge is Postgres's (`change_log::record_function_sql`): the earlier
/// old value and the later new value win, the operation is the net one,
/// columns that ended where they started are dropped, and an emptied entry is
/// removed only when its changeset began in this transaction.
pub fn bootstrap_sql() -> String {
    let settings = TX_SETTINGS_TABLE;
    let log = CHANGES_TABLE;
    let sets = CHANGESETS_TABLE;
    let cs_lit = literal(CHANGESET_GUC);
    let cs = format!("(SELECT value FROM {settings} WHERE name = {cs_lit})");
    let cs_id = format!("json_extract({cs}, '$.id')");
    let next = next_seq();
    let now = "strftime('%Y-%m-%dT%H:%M:%f', 'now') || 'Z'";
    let uuid = concat!(
        "lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' || ",
        "substr(lower(hex(randomblob(2))), 2) || '-' || ",
        "substr('89ab', abs(random()) % 4 + 1, 1) || ",
        "substr(lower(hex(randomblob(2))), 2) || '-' || lower(hex(randomblob(6)))"
    );
    let marker_lit = literal(CHANGESET_TX);
    let minted_lit = literal(MINTED);
    let seen = format!(
        "EXISTS (SELECT 1 FROM {settings} WHERE name = {marker_lit} \
         AND value LIKE json_extract(cs, '$.id') || ':%')"
    );
    let taken_back = format!("{log}.op = 'i' AND {log}.new_values IS NULL");
    let p_old = format!("coalesce({log}.old_values, '{{}}')");
    let p_new = format!("coalesce({log}.new_values, '{{}}')");
    let m_op = format!(
        "CASE WHEN {taken_back} THEN excluded.op \
           WHEN excluded.op = 'i' THEN CASE WHEN {log}.op = 'd' THEN 'u' ELSE {log}.op END \
           WHEN excluded.op = 'u' THEN CASE WHEN {log}.op = 'i' THEN 'i' ELSE 'u' END \
           ELSE CASE WHEN {log}.op = 'i' THEN 'i' ELSE 'd' END END"
    );
    let m_old = format!(
        "CASE WHEN {taken_back} THEN excluded.old_values \
           WHEN excluded.op = 'i' THEN {p_old} \
           WHEN {log}.op = 'i' THEN NULL \
           ELSE {merged} END",
        merged = merge("excluded.old_values", &p_old)
    );
    let m_new = format!(
        "CASE WHEN {taken_back} THEN excluded.new_values \
           WHEN excluded.op = 'i' THEN CASE WHEN {log}.op = 'd' THEN excluded.new_values \
             ELSE {merged} END \
           WHEN excluded.op = 'u' THEN {merged} \
           ELSE NULL END",
        merged = merge(&p_new, "excluded.new_values")
    );
    let entry =
        format!("changeset = {cs_id} AND table_name = NEW.table_name AND row_key = NEW.row_key");
    format!(
        "CREATE TABLE IF NOT EXISTS {settings} (name TEXT PRIMARY KEY, value TEXT);\n\
         DROP VIEW IF EXISTS {RECORD_VIEW};\n\
         CREATE VIEW {RECORD_VIEW} AS SELECT NULL AS table_name, NULL AS row_key, NULL AS op, \
           NULL AS old_values, NULL AS new_values WHERE 0;\n\
         CREATE TRIGGER {RECORD_VIEW} INSTEAD OF INSERT ON {RECORD_VIEW} BEGIN\n  \
           INSERT INTO {settings} (name, value)\n  \
           SELECT {cs_lit}, json_object('id', {uuid}, 'source', 'sql', 'user_id',\n    \
             (SELECT json_extract(value, '$.id') FROM {settings} \
               WHERE name = {user_lit} AND value <> ''))\n   \
            WHERE NOT EXISTS (SELECT 1 FROM {settings} WHERE name = {cs_lit} AND value <> '')\n  \
           ON CONFLICT (name) DO UPDATE SET value = excluded.value;\n  \
           INSERT OR REPLACE INTO {settings} (name, value) SELECT {minted_lit}, '1' \
             WHERE changes() > 0;\n  \
           INSERT OR IGNORE INTO {sets} (id, txid, user_id, session, source, application, \
             description, cause, undo_table, client_id, mutation)\n  \
           SELECT json_extract(cs, '$.id'), {next}, json_extract(cs, '$.user_id'), \
             json_extract(cs, '$.session'), coalesce(json_extract(cs, '$.source'), 'sql'), \
             json_extract(cs, '$.application'), json_extract(cs, '$.description'), \
             json_extract(cs, '$.cause'), json_extract(cs, '$.undo_table'), \
             json_extract(cs, '$.client_id'), json_extract(cs, '$.mutation')\n    \
             FROM (SELECT {cs} AS cs) WHERE NOT {seen};\n  \
           INSERT OR REPLACE INTO {settings} (name, value)\n  \
           SELECT {marker_lit}, json_extract(cs, '$.id') || \
             CASE WHEN changes() > 0 THEN ':new' ELSE ':old' END\n    \
             FROM (SELECT {cs} AS cs) WHERE NOT {seen};\n  \
           INSERT INTO {log} (changeset, table_name, row_key, op, old_values, new_values, txid)\n  \
           VALUES ({cs_id}, NEW.table_name, NEW.row_key, NEW.op, NEW.old_values, NEW.new_values, \
             {next})\n  \
           ON CONFLICT (changeset, table_name, row_key) DO UPDATE SET\n    \
             op = {m_op},\n    old_values = {m_old},\n    new_values = {m_new},\n    \
             seq = {next}, txid = {next}, at = {now};\n  \
           UPDATE {log} SET old_values = {diff_old}, new_values = {diff_new} \
             WHERE {entry} AND op = 'u';\n  \
           UPDATE sqlite_sequence SET seq = (SELECT max(seq) FROM {log}) \
             WHERE name = {log_lit} AND seq < (SELECT max(seq) FROM {log});\n  \
           DELETE FROM {log} WHERE {entry} AND old_values IS NULL AND new_values IS NULL \
             AND EXISTS (SELECT 1 FROM {settings} WHERE name = {marker_lit} \
               AND value = {cs_id} || ':new');\n  \
           DELETE FROM {settings} WHERE name IN ({cs_lit}, {marker_lit}, {minted_lit}) \
             AND EXISTS (SELECT 1 FROM {settings} WHERE name = {minted_lit}) \
             AND NOT EXISTS (SELECT 1 FROM {settings} WHERE name = {role_lit});\n\
         END;",
        user_lit = literal(USER),
        role_lit = literal(ROLE),
        log_lit = literal(CHANGES_TABLE),
        diff_old = diff("old"),
        diff_new = diff("new"),
    )
}

/// The change log's own table on SQLite: `seq` is `AUTOINCREMENT`, so a number
/// is never given out twice even when the entry that had it is removed — a
/// device's watermark is a `seq` here.
pub fn changes_table_sql() -> String {
    let now = "strftime('%Y-%m-%dT%H:%M:%f', 'now') || 'Z'";
    format!(
        "CREATE TABLE IF NOT EXISTS {log} (\
           seq INTEGER PRIMARY KEY AUTOINCREMENT, changeset uuid NOT NULL, \
           table_name text NOT NULL, row_key text NOT NULL, op text NOT NULL, \
           old_values jsonb, new_values jsonb, txid int8 NOT NULL, \
           at timestamptz NOT NULL DEFAULT ({now}));",
        log = quote(CHANGES_TABLE),
    )
}

/// The table `change` alters in place: whose triggers must be dropped before
/// it and generated again after. `None` for a change that creates or drops a
/// table, or touches only an index or a comment.
pub fn altered_table(change: &SchemaChange) -> Option<&str> {
    match change {
        SchemaChange::AddColumn { table, .. }
        | SchemaChange::DropColumn { table, .. }
        | SchemaChange::RenameColumn { table, .. }
        | SchemaChange::SetPrimaryKey { table, .. }
        | SchemaChange::SetColumnGenerator { table, .. }
        | SchemaChange::SetColumnNullable { table, .. }
        | SchemaChange::SetColumnReference { table, .. }
        | SchemaChange::AddUniqueConstraint { table, .. }
        | SchemaChange::DropConstraint { table, .. } => Some(table),
        _ => None,
    }
}

/// Apply `change` on `tx`, keeping the change-log triggers of the table it
/// alters: they are dropped first (SQLite refuses to drop a column a trigger
/// names, and a table rebuild would write them again from their old text),
/// and generated again afterwards from the columns the table has now.
///
/// A table whose key is no longer one column keeps no triggers; a table
/// without them is left as it is.
pub async fn apply_keeping_triggers(
    tx: &mut dyn sc_db::Transaction,
    change: &SchemaChange,
) -> Result<()> {
    let Some(table) = altered_table(change) else {
        return tx.apply_schema(change).await;
    };
    let triggers = tx
        .query(&Statement::raw(
            "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = $1",
            vec![Value::Text(table.to_owned())],
        ))
        .await?
        .try_collect()
        .await?;
    let logged = triggers.iter().any(
        |row| matches!(row.get("name"), Some(Value::Text(name)) if is_table_trigger(name, table)),
    );
    if !logged {
        return tx.apply_schema(change).await;
    }
    tx.batch(&disable_sql(table)).await?;
    tx.apply_schema(change).await?;
    let columns = tx
        .query(&Statement::raw(
            "SELECT name, type, pk FROM pragma_table_info($1) ORDER BY cid",
            vec![Value::Text(table.to_owned())],
        ))
        .await?
        .try_collect()
        .await?;
    let text = |row: &sc_db::Row, column: &str| match row.get(column) {
        Some(Value::Text(text)) => text.clone(),
        _ => String::new(),
    };
    let keys: Vec<String> = columns
        .iter()
        .filter(|row| matches!(row.get("pk"), Some(Value::Int(n)) if *n > 0))
        .map(|row| text(row, "name"))
        .collect();
    let [pk] = keys.as_slice() else {
        return Ok(());
    };
    let stored: Vec<(String, String)> = columns
        .iter()
        .map(|row| (text(row, "name"), text(row, "type")))
        .collect();
    tx.batch(&enable_sql(table, pk, &stored)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_are_recorded_by_their_declared_type() {
        assert_eq!(Shape::of("boolean"), Shape::Bool);
        assert_eq!(Shape::of("jsonb"), Shape::Json);
        assert_eq!(Shape::of("timestamptz"), Shape::Plain);
        assert_eq!(Shape::of("VARCHAR(20)"), Shape::Plain);
    }

    #[test]
    fn a_tables_triggers_are_named_after_it_and_dropped_first() {
        let sql = enable_sql("to do", "id", &[("id".into(), "int8".into())]);
        assert!(
            sql.starts_with("DROP TRIGGER IF EXISTS \"_fd_changes_to do_ins\";"),
            "{sql}"
        );
        assert!(sql.contains("AFTER UPDATE ON \"to do\""), "{sql}");
        assert!(is_table_trigger("_fd_changes_to do_upd", "to do"));
        assert!(!is_table_trigger("_fd_changes_to do_upd", "to"));
    }
}

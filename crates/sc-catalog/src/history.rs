//! Reading row history (§14.12 "Reading"): a row's entries, the deleted rows of
//! a table, a row rebuilt as it was, and the changesets.
//!
//! These are the log as it is stored, for the tables whose history is on. Who
//! may read them — the admin, or `history.min_role` and the row's read rule —
//! is decided by the caller (`sc_api::history`), which also hides secrets.
//!
//! Entries with no values are skipped: they are what is left of a change taken
//! back in a later transaction of its changeset, kept so a device's pull still
//! sees the key, and they changed nothing.

use chrono::{DateTime, Utc};
use sc_error::{Error, Result};
use sc_query::{Statement, Value};
use serde_json::{Map, Value as Json};
use uuid::Uuid;

use crate::catalog::Catalog;
use crate::change_log::{ATTR_HISTORY, CHANGES_TABLE};
use crate::changeset::CHANGESETS_TABLE;
use crate::field::Attrs;
use crate::table::Table;

/// The administrator's role, who may always read history.
const ROLE_ADMIN: u8 = 1;

/// A changeset as the log keeps it, without the session key, which is for
/// finding a session's changesets (the Undo action) and is never shown.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangesetRecord {
    /// Its id.
    pub id: Uuid,
    /// When its first entry was written.
    pub at: DateTime<Utc>,
    /// The originating caller, when there was one.
    pub user_id: Option<Uuid>,
    /// `request`, `import`, `workflow`, `undo`, `restore`, `sync`, `mcp` or `sql`.
    pub source: String,
    /// The application it was made in.
    pub application: Option<Uuid>,
    /// An optional reason.
    pub description: Option<String>,
    /// What an undo undid, or the changeset that started a workflow run.
    pub cause: Option<Uuid>,
    /// For an undo of one table's entries, that table.
    pub undo_table: Option<String>,
    /// For a sync upload, the device.
    pub client_id: Option<Uuid>,
    /// For a sync upload, the device's number for the change.
    pub mutation: Option<i64>,
}

/// One entry: what one changeset did to one row.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    /// The entry's place in the log.
    pub seq: i64,
    /// The row's version this entry made: its position among the row's
    /// entries, from 1. Set by [`row_history`]; 0 elsewhere.
    pub version: i64,
    /// The table.
    pub table: String,
    /// The row's key, as text.
    pub key: String,
    /// The net operation: `i`, `u` or `d`.
    pub op: String,
    /// An update's changed columns before, or a deleted row whole.
    pub old_values: Option<Map<String, Json>>,
    /// An update's changed columns after, or an inserted row whole.
    pub new_values: Option<Map<String, Json>>,
    /// When the entry was last written.
    pub at: DateTime<Utc>,
    /// The changeset it belongs to.
    pub changeset: ChangesetRecord,
}

/// A changeset in a list: its row, and how many entries it has in which tables.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangesetSummary {
    /// The changeset.
    pub changeset: ChangesetRecord,
    /// Its entries with values, in tables with history on.
    pub entries: i64,
    /// Those entries' tables, by name.
    pub tables: Vec<String>,
}

/// What [`list_changesets`] is limited to.
#[derive(Debug, Clone, Default)]
pub struct ChangesetFilter {
    /// Only this user's.
    pub user: Option<Uuid>,
    /// Only those with an entry in this table.
    pub table: Option<String>,
    /// Only those from this source.
    pub source: Option<String>,
    /// Only those started at or after this.
    pub since: Option<DateTime<Utc>>,
    /// At most this many (newest first).
    pub limit: u64,
    /// Skipping this many.
    pub offset: u64,
}

/// Where [`row_as_of`] rebuilds a row: at a time, or just after an entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AsOf {
    /// As it was at this time: every entry written after it is undone.
    Time(DateTime<Utc>),
    /// As it was just after the entry `seq`: every later entry is undone.
    Seq(i64),
}

impl AsOf {
    /// Read `text`: a number is an entry's `seq`, anything else a time (RFC
    /// 3339, or a date meaning its midnight UTC).
    pub fn parse(text: &str) -> Result<AsOf> {
        let text = text.trim();
        if let Ok(seq) = text.parse::<i64>() {
            return Ok(AsOf::Seq(seq));
        }
        if let Ok(time) = DateTime::parse_from_rfc3339(text) {
            return Ok(AsOf::Time(time.with_timezone(&Utc)));
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
            return Ok(AsOf::Time(
                date.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc(),
            ));
        }
        Err(Error::invalid(format!(
            "`as_of` must be a time (like 2026-10-22T09:30:00Z) or an entry's number, not `{text}`"
        )))
    }
}

/// A row as it was, split by whether each column still exists.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RowAsOf {
    /// The row's columns that the table still has; `None` when the row did not
    /// exist then.
    pub row: Option<Map<String, Json>>,
    /// Its values of columns that have been dropped since.
    pub removed: Map<String, Json>,
}

/// The role a table's history is open to (`attributes.history.min_role`): the
/// admin unless it says otherwise.
pub fn history_min_role(attributes: &Attrs) -> u8 {
    attributes
        .get(ATTR_HISTORY)
        .and_then(|h| h.get("min_role"))
        .and_then(Json::as_u64)
        .and_then(|r| u8::try_from(r).ok())
        .unwrap_or(ROLE_ADMIN)
}

/// Set who may read a table's history. Only while history is on (switching it
/// off forgets this too); the admin's role removes the key.
pub fn set_history_min_role(attributes: &mut Attrs, role: u8) {
    let Some(Json::Object(settings)) = attributes.get_mut(ATTR_HISTORY) else {
        return;
    };
    if role == ROLE_ADMIN {
        settings.remove("min_role");
    } else {
        settings.insert("min_role".into(), Json::from(role));
    }
}

/// The columns every entry query selects, `c` the entry and `s` its changeset.
const ENTRY_COLUMNS: &str = "c.seq, c.table_name, c.row_key, c.op, c.old_values, c.new_values, \
     c.at, s.id, s.at AS cs_at, s.user_id, s.source, s.application, s.description, s.cause, \
     s.undo_table, s.client_id, s.mutation";

/// The changeset columns, `s`.
const CHANGESET_COLUMNS: &str = "s.id, s.at AS cs_at, s.user_id, s.source, s.application, \
     s.description, s.cause, s.undo_table, s.client_id, s.mutation";

/// An entry that recorded something.
const HAS_VALUES: &str = "(c.old_values IS NOT NULL OR c.new_values IS NOT NULL)";

fn quote(catalog: &Catalog, ident: &str) -> String {
    catalog.primary().dialect().quote_ident(ident)
}

async fn query(catalog: &Catalog, sql: String, binds: Vec<Value>) -> Result<Vec<sc_db::Row>> {
    catalog
        .primary()
        .query(&Statement::raw(sql, binds))
        .await?
        .try_collect()
        .await
}

fn uuid_of(row: &sc_db::Row, column: &str) -> Option<Uuid> {
    match row.get(column) {
        Some(Value::Uuid(id)) => Some(*id),
        Some(Value::Text(text)) => text.parse().ok(),
        _ => None,
    }
}

fn text_of(row: &sc_db::Row, column: &str) -> Option<String> {
    match row.get(column) {
        Some(Value::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

fn int_of(row: &sc_db::Row, column: &str) -> Option<i64> {
    match row.get(column) {
        Some(Value::Int(n)) => Some(*n),
        _ => None,
    }
}

fn time_of(row: &sc_db::Row, column: &str) -> DateTime<Utc> {
    match row.get(column) {
        Some(Value::Timestamp(at)) => *at,
        _ => DateTime::<Utc>::UNIX_EPOCH,
    }
}

fn object_of(row: &sc_db::Row, column: &str) -> Option<Map<String, Json>> {
    match row.get(column) {
        Some(Value::Json(Json::Object(map))) => Some(map.clone()),
        Some(Value::Text(text)) => match serde_json::from_str(text) {
            Ok(Json::Object(map)) => Some(map),
            _ => None,
        },
        _ => None,
    }
}

fn changeset_of(row: &sc_db::Row) -> ChangesetRecord {
    ChangesetRecord {
        id: uuid_of(row, "id").unwrap_or_default(),
        at: time_of(row, "cs_at"),
        user_id: uuid_of(row, "user_id"),
        source: text_of(row, "source").unwrap_or_default(),
        application: uuid_of(row, "application"),
        description: text_of(row, "description"),
        cause: uuid_of(row, "cause"),
        undo_table: text_of(row, "undo_table"),
        client_id: uuid_of(row, "client_id"),
        mutation: int_of(row, "mutation"),
    }
}

fn entry_of(row: &sc_db::Row) -> HistoryEntry {
    HistoryEntry {
        seq: int_of(row, "seq").unwrap_or_default(),
        version: 0,
        table: text_of(row, "table_name").unwrap_or_default(),
        key: text_of(row, "row_key").unwrap_or_default(),
        op: text_of(row, "op").unwrap_or_default(),
        old_values: object_of(row, "old_values"),
        new_values: object_of(row, "new_values"),
        at: time_of(row, "at"),
        changeset: changeset_of(row),
    }
}

/// The names of the tables whose history is on, as a JSON array to bind.
fn history_tables(catalog: &Catalog) -> Result<Value> {
    Ok(Value::Json(Json::Array(
        catalog
            .tables()?
            .iter()
            .filter(|t| t.history_enabled())
            .map(|t| Json::String(t.name.clone()))
            .collect(),
    )))
}

/// `table`'s single key column, which the change log addresses rows by.
fn key_column(table: &Table) -> Result<&str> {
    match table.primary_key.as_slice() {
        [pk] => Ok(pk),
        _ => Err(Error::invalid(format!(
            "table `{}` has no one-column primary key, so it has no history",
            table.name
        ))),
    }
}

/// The entries of `table`'s row `key`, **newest first**, each with its
/// version number.
pub async fn row_history(catalog: &Catalog, table: &Table, key: &str) -> Result<Vec<HistoryEntry>> {
    let sql = format!(
        "SELECT {ENTRY_COLUMNS} FROM {log} c JOIN {sets} s ON s.id = c.changeset \
         WHERE c.table_name = $1 AND c.row_key = $2 AND {HAS_VALUES} ORDER BY c.seq",
        log = quote(catalog, CHANGES_TABLE),
        sets = quote(catalog, CHANGESETS_TABLE),
    );
    let rows = query(
        catalog,
        sql,
        vec![Value::Text(table.name.clone()), Value::Text(key.to_owned())],
    )
    .await?;
    let mut entries: Vec<HistoryEntry> = rows.iter().map(entry_of).collect();
    for (n, entry) in entries.iter_mut().enumerate() {
        entry.version = n as i64 + 1;
    }
    entries.reverse();
    Ok(entries)
}

/// The rows of `table` whose last entry is a delete and whose key is not in the
/// table now, newest delete first: each delete entry, whose `old_values` is
/// the deleted row. `limit` bounds the answer, when given.
pub async fn deleted_rows(
    catalog: &Catalog,
    table: &Table,
    limit: Option<u64>,
    offset: u64,
) -> Result<Vec<HistoryEntry>> {
    let pk = key_column(table)?;
    let bound = match limit {
        Some(limit) => format!(" LIMIT {limit} OFFSET {offset}"),
        None if offset > 0 => format!(" OFFSET {offset}"),
        None => String::new(),
    };
    let sql = format!(
        "SELECT {ENTRY_COLUMNS} FROM \
           (SELECT DISTINCT ON (c.row_key) c.* FROM {log} c \
             WHERE c.table_name = $1 AND {HAS_VALUES} ORDER BY c.row_key, c.seq DESC) c \
         JOIN {sets} s ON s.id = c.changeset \
         WHERE c.op = 'd' \
           AND NOT EXISTS (SELECT 1 FROM {table} t WHERE t.{pk}::text = c.row_key) \
         ORDER BY c.seq DESC{bound}",
        log = quote(catalog, CHANGES_TABLE),
        sets = quote(catalog, CHANGESETS_TABLE),
        table = quote(catalog, &table.name),
        pk = quote(catalog, pk),
    );
    let rows = query(catalog, sql, vec![Value::Text(table.name.clone())]).await?;
    Ok(rows.iter().map(entry_of).collect())
}

/// The row `key` of `table` as it is now, every stored column, or `None` when
/// there is no such row. The same JSON the log records rows in.
async fn current_row(
    catalog: &Catalog,
    table: &Table,
    key: &str,
) -> Result<Option<Map<String, Json>>> {
    let sql = format!(
        "SELECT to_jsonb(t) AS row FROM {table} t WHERE t.{pk}::text = $1",
        table = quote(catalog, &table.name),
        pk = quote(catalog, key_column(table)?),
    );
    let rows = query(catalog, sql, vec![Value::Text(key.to_owned())]).await?;
    Ok(rows.first().and_then(|row| object_of(row, "row")))
}

/// The last image of a row that is gone: its newest entry, when that is a
/// delete and there is no row with the key now. `None` otherwise.
pub async fn deleted_image(
    catalog: &Catalog,
    table: &Table,
    key: &str,
) -> Result<Option<Map<String, Json>>> {
    if current_row(catalog, table, key).await?.is_some() {
        return Ok(None);
    }
    let latest = row_history(catalog, table, key).await?.into_iter().next();
    Ok(latest
        .filter(|entry| entry.op == "d")
        .and_then(|entry| entry.old_values))
}

/// The row `key` of `table` as it was at `point`: the row now (or nothing, if
/// it is gone), with every later entry undone, newest first. An update gives
/// back its old values, a delete the deleted row, and an insert means there
/// was no row before it.
pub async fn row_as_of(
    catalog: &Catalog,
    table: &Table,
    key: &str,
    point: AsOf,
) -> Result<RowAsOf> {
    let mut state = current_row(catalog, table, key).await?;
    let (after, bind) = match point {
        AsOf::Seq(seq) => ("c.seq > $3", Value::Int(seq)),
        AsOf::Time(at) => ("c.at > $3", Value::Timestamp(at)),
    };
    let sql = format!(
        "SELECT c.op, c.old_values, c.new_values FROM {log} c \
         WHERE c.table_name = $1 AND c.row_key = $2 AND {after} AND {HAS_VALUES} \
         ORDER BY c.seq DESC",
        log = quote(catalog, CHANGES_TABLE),
    );
    let later = query(
        catalog,
        sql,
        vec![
            Value::Text(table.name.clone()),
            Value::Text(key.to_owned()),
            bind,
        ],
    )
    .await?;
    for entry in &later {
        let old = object_of(entry, "old_values");
        match text_of(entry, "op").as_deref() {
            Some("u") => {
                if let (Some(row), Some(old)) = (state.as_mut(), old) {
                    row.extend(old);
                }
            }
            Some("d") => state = old,
            Some("i") => state = None,
            _ => {}
        }
    }
    let Some(values) = state else {
        return Ok(RowAsOf::default());
    };
    let (mut row, mut removed) = (Map::new(), Map::new());
    for (column, value) in values {
        if table.field(&column).is_some() {
            row.insert(column, value);
        } else {
            removed.insert(column, value);
        }
    }
    Ok(RowAsOf {
        row: Some(row),
        removed,
    })
}

/// The changesets that recorded something in a table with history on, newest
/// first, as `filter` limits them.
pub async fn list_changesets(
    catalog: &Catalog,
    filter: &ChangesetFilter,
) -> Result<Vec<ChangesetSummary>> {
    let mut binds = vec![history_tables(catalog)?];
    let mut wheres = Vec::new();
    let mut bind = |value: Value| {
        binds.push(value);
        binds.len()
    };
    if let Some(user) = filter.user {
        wheres.push(format!("s.user_id = ${}", bind(Value::Uuid(user))));
    }
    if let Some(source) = &filter.source {
        wheres.push(format!("s.source = ${}", bind(Value::Text(source.clone()))));
    }
    if let Some(since) = filter.since {
        wheres.push(format!("s.at >= ${}", bind(Value::Timestamp(since))));
    }
    if let Some(table) = &filter.table {
        wheres.push(format!(
            "bool_or(c.table_name = ${})",
            bind(Value::Text(table.clone()))
        ));
    }
    // The table filter is over the group's entries, so every condition is a
    // `HAVING`; the others are constant over the group.
    let having = if wheres.is_empty() {
        String::new()
    } else {
        format!(" HAVING {}", wheres.join(" AND "))
    };
    let group = CHANGESET_COLUMNS.replace(" AS cs_at", "");
    let sql = format!(
        "SELECT {CHANGESET_COLUMNS}, count(*) AS entries, \
                jsonb_agg(DISTINCT c.table_name) AS tables \
           FROM {sets} s JOIN {log} c ON c.changeset = s.id \
          WHERE {HAS_VALUES} AND c.table_name IN (SELECT jsonb_array_elements_text($1::jsonb)) \
          GROUP BY {group}{having} \
          ORDER BY s.at DESC, s.id LIMIT {limit} OFFSET {offset}",
        log = quote(catalog, CHANGES_TABLE),
        sets = quote(catalog, CHANGESETS_TABLE),
        limit = filter.limit,
        offset = filter.offset,
    );
    let rows = query(catalog, sql, binds).await?;
    Ok(rows
        .iter()
        .map(|row| ChangesetSummary {
            changeset: changeset_of(row),
            entries: int_of(row, "entries").unwrap_or_default(),
            tables: match row.get("tables") {
                Some(Value::Json(Json::Array(names))) => names
                    .iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            },
        })
        .collect())
}

/// One changeset and its entries in tables with history on, in the order they
/// were first written; `None` when there is no such changeset.
pub async fn changeset_entries(
    catalog: &Catalog,
    id: Uuid,
) -> Result<Option<(ChangesetRecord, Vec<HistoryEntry>)>> {
    let sets = quote(catalog, CHANGESETS_TABLE);
    let found = query(
        catalog,
        format!("SELECT {CHANGESET_COLUMNS} FROM {sets} s WHERE s.id = $1"),
        vec![Value::Uuid(id)],
    )
    .await?;
    let Some(changeset) = found.first().map(changeset_of) else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {ENTRY_COLUMNS} FROM {log} c JOIN {sets} s ON s.id = c.changeset \
         WHERE c.changeset = $1 AND {HAS_VALUES} \
           AND c.table_name IN (SELECT jsonb_array_elements_text($2::jsonb)) \
         ORDER BY c.seq",
        log = quote(catalog, CHANGES_TABLE),
    );
    let rows = query(
        catalog,
        sql,
        vec![Value::Uuid(id), history_tables(catalog)?],
    )
    .await?;
    Ok(Some((changeset, rows.iter().map(entry_of).collect())))
}

/// How much of a changeset has been undone (§14.12): an undo changeset whose
/// `cause` is it undoes the whole of it, or with `undo_table` that table's
/// entries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Undone {
    /// An undo of the whole changeset exists.
    pub whole: bool,
    /// The tables whose entries an undo of one table has taken back.
    pub tables: Vec<String>,
}

impl Undone {
    /// Whether `table`'s entries of the changeset have been undone.
    pub fn covers(&self, table: &str) -> bool {
        self.whole || self.tables.iter().any(|t| t == table)
    }
}

/// What has been undone of changeset `id`.
pub async fn undone(catalog: &Catalog, id: Uuid) -> Result<Undone> {
    let sql = format!(
        "SELECT s.undo_table FROM {sets} s WHERE s.cause = $1 AND s.source = 'undo'",
        sets = quote(catalog, CHANGESETS_TABLE),
    );
    let mut out = Undone::default();
    for row in query(catalog, sql, vec![Value::Uuid(id)]).await? {
        match text_of(&row, "undo_table") {
            Some(table) => out.tables.push(table),
            None => out.whole = true,
        }
    }
    Ok(out)
}

/// The changeset the Undo action would take back for `session` (§14.12 "The
/// Undo action"): its most recent one with an entry not yet undone (in
/// `table` only, when given), that is neither an undo nor `except` (the
/// request running the action). `None` when there is nothing to undo.
pub async fn session_undo_target(
    catalog: &Catalog,
    session: &str,
    table: Option<&str>,
    except: Option<Uuid>,
) -> Result<Option<Uuid>> {
    let mut binds = vec![
        Value::Text(session.to_owned()),
        history_tables(catalog)?,
        Value::Uuid(except.unwrap_or_default()),
    ];
    let in_table = match table {
        Some(table) => {
            binds.push(Value::Text(table.to_owned()));
            " AND c.table_name = $4"
        }
        None => "",
    };
    let sql = format!(
        "SELECT s.id FROM {sets} s \
          WHERE s.session = $1 AND s.source <> 'undo' AND s.id <> $3 \
            AND EXISTS (SELECT 1 FROM {log} c \
                         WHERE c.changeset = s.id AND {HAS_VALUES}{in_table} \
                           AND c.table_name IN (SELECT jsonb_array_elements_text($2::jsonb)) \
                           AND NOT EXISTS (SELECT 1 FROM {sets} u \
                                            WHERE u.cause = s.id AND u.source = 'undo' \
                                              AND (u.undo_table IS NULL \
                                                   OR u.undo_table = c.table_name))) \
          ORDER BY s.at DESC, s.txid DESC LIMIT 1",
        log = quote(catalog, CHANGES_TABLE),
        sets = quote(catalog, CHANGESETS_TABLE),
    );
    Ok(query(catalog, sql, binds)
        .await?
        .first()
        .and_then(|row| uuid_of(row, "id")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change_log::set_history_enabled;

    #[test]
    fn as_of_is_a_seq_a_time_or_a_date() {
        assert_eq!(AsOf::parse("17").unwrap(), AsOf::Seq(17));
        assert_eq!(
            AsOf::parse("2026-10-22T09:30:00+02:00").unwrap(),
            AsOf::Time("2026-10-22T07:30:00Z".parse().unwrap())
        );
        assert_eq!(
            AsOf::parse("2026-10-22").unwrap(),
            AsOf::Time("2026-10-22T00:00:00Z".parse().unwrap())
        );
        assert!(AsOf::parse("yesterday").is_err());
    }

    #[test]
    fn history_is_the_admins_unless_opened_and_switching_off_forgets() {
        let mut attrs = Attrs::new();
        assert_eq!(history_min_role(&attrs), 1);
        // Nothing to open while history is off.
        set_history_min_role(&mut attrs, 40);
        assert!(attrs.is_empty());
        set_history_enabled(&mut attrs, true);
        set_history_min_role(&mut attrs, 40);
        assert_eq!(history_min_role(&attrs), 40);
        set_history_min_role(&mut attrs, 1);
        assert_eq!(attrs[ATTR_HISTORY], serde_json::json!({ "enabled": true }));
        set_history_min_role(&mut attrs, 40);
        set_history_enabled(&mut attrs, false);
        assert_eq!(history_min_role(&attrs), 1);
    }
}

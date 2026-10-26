//! Reading the live schema out of a SQLite database.
//!
//! There is no discovery step (design §5, §9): everything a connection can see
//! is returned here and is immediately usable. SQLite's answer to
//! `information_schema` is `sqlite_master` plus the `pragma_*` table-valued
//! functions, which is what this asks — the *live* shape, never a stored copy.
//!
//! Two SQLite-specific readings are worth stating:
//!
//! - **The declared type is kept verbatim.** SQLite records the type name a
//!   column was declared with and never normalises it, so a column this driver
//!   created as `timestamptz` reads back as `timestamptz` and the type layer
//!   (§6) maps it exactly as it maps Postgres's. A file created by something
//!   else says `INTEGER` or `VARCHAR(64)`, which that layer also knows.
//! - **`INTEGER PRIMARY KEY` is an identity column.** It is an alias for the
//!   rowid, so SQLite fills it in when a write omits it — which is precisely
//!   what [`ColumnGenerator::Identity`] means, and what makes a table created
//!   here insertable from a form.
//!
//! Internal tables (`sqlite_*`) are left out: they are the database's own
//! bookkeeping, not tables an admin can use.

use rusqlite::{Connection, OptionalExtension};
use sc_db::{
    Column, ColumnGenerator, ForeignKey, PhysicalConstraint, PhysicalConstraintKind, PhysicalTable,
};
use sc_error::{Error, Result};

/// The table this driver keeps object comments in — see [`crate::ddl`].
pub(crate) const COMMENTS_TABLE: &str = "_fd_object_comments";

/// One row of `pragma_table_info`, before it becomes a [`Column`].
#[derive(Debug, Clone)]
pub(crate) struct RawColumn {
    pub name: String,
    /// The declared type, exactly as written; empty when the column was
    /// declared without one.
    pub decl_type: String,
    pub not_null: bool,
    pub default_sql: Option<String>,
    /// 1-based position in the primary key, or 0 for a column outside it.
    pub pk: i64,
}

/// One row of `pragma_index_list`, with the columns it covers.
#[derive(Debug, Clone)]
pub(crate) struct RawIndex {
    pub name: String,
    pub unique: bool,
    /// `c` for `CREATE INDEX`, `u` for a `UNIQUE` table constraint, `pk` for the
    /// index behind a primary key.
    pub origin: String,
    /// The indexed columns; an entry is `None` where the index is over an
    /// expression.
    pub columns: Vec<Option<String>>,
    /// The `CREATE INDEX` statement, for the indexes that have one (a
    /// constraint's implicit index does not).
    pub sql: Option<String>,
}

impl RawIndex {
    /// Whether this index exists only because a constraint does.
    pub(crate) fn is_constraint_index(&self) -> bool {
        self.origin != "c"
    }
}

/// Every user table in the database, with its columns, keys and constraints.
pub(crate) fn introspect(conn: &Connection) -> Result<Vec<PhysicalTable>> {
    let mut tables = Vec::new();
    for name in table_names(conn)? {
        tables.push(physical_table(conn, &name)?);
    }
    Ok(tables)
}

/// The names of the user tables, in name order.
///
/// `sqlite_*` is SQLite's own reserved prefix (the sequence table behind
/// `AUTOINCREMENT`, the statistics tables); a table named there is not one an
/// admin made and not one they can use. Nor is a virtual table or the shadow
/// tables behind one: a full-text index on SQLite is an FTS5 table (see
/// [`fts5_indexes`]), and it is reported as a constraint of the table it
/// indexes rather than as five tables of its own.
pub(crate) fn table_names(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = prepare(
        conn,
        "SELECT name FROM pragma_table_list \
         WHERE schema = 'main' AND type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| database(e, "listing tables"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| database(e, "listing tables"))?;
    Ok(names)
}

/// Whether a table of this name exists.
pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let mut stmt = prepare(
        conn,
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
    )?;
    let found: Option<i64> = stmt
        .query_row([table], |row| row.get(0))
        .optional()
        .map_err(|e| database(e, "looking for a table"))?;
    Ok(found.is_some())
}

/// One table's live shape.
pub(crate) fn physical_table(conn: &Connection, table: &str) -> Result<PhysicalTable> {
    let raw = columns(conn, table)?;
    let indexes = indexes(conn, table)?;
    let single_pk = raw.iter().filter(|c| c.pk > 0).count() == 1;

    let columns = raw
        .iter()
        .map(|c| Column {
            name: c.name.clone(),
            sql_type: c.decl_type.clone(),
            nullable: !c.not_null && c.pk == 0,
            generated: generator(c, single_pk),
        })
        .collect();

    let mut key: Vec<&RawColumn> = raw.iter().filter(|c| c.pk > 0).collect();
    key.sort_by_key(|c| c.pk);
    let primary_key = key.into_iter().map(|c| c.name.clone()).collect();

    let mut constraints = Vec::new();
    for index in &indexes {
        // The primary key is reported as the primary key, not a second time as
        // an index — the same rule the Postgres driver follows.
        if index.origin == "pk" {
            continue;
        }
        let named: Vec<String> = index.columns.iter().flatten().cloned().collect();
        let is_expression = index.columns.iter().any(Option::is_none);
        let kind = if index.unique && !is_expression {
            PhysicalConstraintKind::Unique { columns: named }
        } else {
            PhysicalConstraintKind::Index {
                columns: if is_expression { Vec::new() } else { named },
                expression: is_expression
                    .then(|| index.sql.as_deref().and_then(index_expression))
                    .flatten(),
                // SQLite has one index kind, so there is one name to report;
                // saying `btree` (what it is) beats saying nothing.
                method: "btree".to_owned(),
            }
        };
        constraints.push(PhysicalConstraint {
            name: index.name.clone(),
            kind,
            comment: comment(conn, "index", "", &index.name)?,
        });
    }
    for (name, columns) in fts5_indexes(conn, table)? {
        constraints.push(PhysicalConstraint {
            comment: comment(conn, "index", "", &name)?,
            name,
            kind: PhysicalConstraintKind::Index {
                columns,
                expression: None,
                method: "fts5".to_owned(),
            },
        });
    }
    for trigger in triggers(conn, table)? {
        constraints.push(PhysicalConstraint {
            name: trigger.clone(),
            kind: PhysicalConstraintKind::RowTrigger,
            comment: comment(conn, "trigger", table, &trigger)?,
        });
    }

    Ok(PhysicalTable {
        name: table.to_owned(),
        // SQLite has one schema per connected file, so there is nothing to
        // qualify with: `None` is the driver's default schema.
        schema: None,
        columns,
        primary_key,
        foreign_keys: foreign_keys(conn, table)?,
        constraints,
    })
}

/// How a column fills itself in when a write omits it.
///
/// `INTEGER PRIMARY KEY` — and only that spelling, on a single-column key — is
/// the rowid alias SQLite numbers itself; anything else with a default reports
/// the default.
fn generator(column: &RawColumn, single_pk: bool) -> Option<ColumnGenerator> {
    if let Some(default) = &column.default_sql {
        return Some(ColumnGenerator::Default(default.clone()));
    }
    if single_pk && column.pk > 0 && column.decl_type.eq_ignore_ascii_case("integer") {
        return Some(ColumnGenerator::Identity);
    }
    None
}

/// One table's columns, in declaration order.
pub(crate) fn columns(conn: &Connection, table: &str) -> Result<Vec<RawColumn>> {
    let mut stmt = prepare(
        conn,
        "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1)",
    )?;
    let rows = stmt
        .query_map([table], |row| {
            Ok(RawColumn {
                name: row.get(0)?,
                decl_type: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                not_null: row.get::<_, i64>(2)? != 0,
                default_sql: row.get::<_, Option<String>>(3)?,
                pk: row.get(4)?,
            })
        })
        .map_err(|e| database(e, "reading columns"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| database(e, "reading columns"))?;
    if rows.is_empty() && !table_exists(conn, table)? {
        return Err(Error::not_found(format!("no table named `{table}`")));
    }
    Ok(rows)
}

/// One table's foreign keys, one entry per constraint (a composite key's parts
/// arrive as several rows sharing an id).
pub(crate) fn foreign_keys(conn: &Connection, table: &str) -> Result<Vec<ForeignKey>> {
    let mut stmt = prepare(
        conn,
        "SELECT id, seq, \"table\", \"from\", \"to\" \
         FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
    )?;
    let rows = stmt
        .query_map([table], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|e| database(e, "reading foreign keys"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| database(e, "reading foreign keys"))?;

    let mut keys: Vec<(i64, ForeignKey)> = Vec::new();
    for (id, referenced_table, from, to) in rows {
        // A `REFERENCES t` with no column names the primary key of `t`; the
        // trait's descriptor names columns positionally, so it is resolved here
        // rather than left as a hole every reader would have to fill.
        let to = match to {
            Some(column) => column,
            None => primary_key(conn, &referenced_table)?
                .first()
                .cloned()
                .ok_or_else(|| {
                    Error::database(format!(
                        "table `{table}` references `{referenced_table}`, which has no primary key"
                    ))
                })?,
        };
        match keys.iter_mut().find(|(existing, _)| *existing == id) {
            Some((_, key)) => {
                key.columns.push(from);
                key.referenced_columns.push(to);
            }
            None => keys.push((
                id,
                ForeignKey {
                    columns: vec![from],
                    referenced_table,
                    referenced_columns: vec![to],
                },
            )),
        }
    }
    Ok(keys.into_iter().map(|(_, key)| key).collect())
}

/// One table's primary key columns, in key order.
pub(crate) fn primary_key(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut key: Vec<RawColumn> = columns(conn, table)?
        .into_iter()
        .filter(|c| c.pk > 0)
        .collect();
    key.sort_by_key(|c| c.pk);
    Ok(key.into_iter().map(|c| c.name).collect())
}

/// One table's indexes, each with the columns it covers.
pub(crate) fn indexes(conn: &Connection, table: &str) -> Result<Vec<RawIndex>> {
    let mut stmt = prepare(
        conn,
        "SELECT name, \"unique\", origin FROM pragma_index_list(?1)",
    )?;
    let listed = stmt
        .query_map([table], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| database(e, "reading indexes"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| database(e, "reading indexes"))?;

    let mut out = Vec::new();
    for (name, unique, origin) in listed {
        let mut cols = prepare(
            conn,
            "SELECT name FROM pragma_index_info(?1) ORDER BY seqno",
        )?;
        let columns = cols
            .query_map([&name], |row| row.get::<_, Option<String>>(0))
            .map_err(|e| database(e, "reading index columns"))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| database(e, "reading index columns"))?;
        out.push(RawIndex {
            name: name.clone(),
            unique,
            origin,
            columns,
            sql: object_sql(conn, "index", &name)?,
        });
    }
    Ok(out)
}

/// The FTS5 tables whose content is `table`, each with the columns it indexes.
///
/// An external-content FTS5 table names its table only inside its own `CREATE
/// VIRTUAL TABLE` statement (`content='t'`), so that is where this looks.
pub(crate) fn fts5_indexes(conn: &Connection, table: &str) -> Result<Vec<(String, Vec<String>)>> {
    let mut stmt = prepare(
        conn,
        "SELECT name, sql FROM sqlite_master \
         WHERE type = 'table' AND sql LIKE 'CREATE VIRTUAL TABLE%' ORDER BY name",
    )?;
    let virtual_tables = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| database(e, "reading virtual tables"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| database(e, "reading virtual tables"))?;
    Ok(virtual_tables
        .into_iter()
        .filter_map(|(name, sql)| {
            let (content, columns) = fts5_arguments(&sql)?;
            (content == table).then_some((name, columns))
        })
        .collect())
}

/// The content table and the columns of an FTS5 table's `CREATE VIRTUAL
/// TABLE … USING fts5(…)` statement, or `None` when it is not an
/// external-content FTS5 table.
fn fts5_arguments(sql: &str) -> Option<(String, Vec<String>)> {
    let lower = sql.to_ascii_lowercase();
    let open = lower.find("using fts5(")? + "using fts5(".len();
    let body = sql.get(open..sql.rfind(')')?)?;
    let mut content = None;
    let mut columns = Vec::new();
    for arg in split_arguments(body) {
        let arg = arg.trim();
        match arg.split_once('=') {
            Some((option, value)) => {
                if option.trim().eq_ignore_ascii_case("content") {
                    content = Some(unquote(value.trim()));
                }
            }
            None => columns.push(unquote(arg)),
        }
    }
    Some((content.filter(|c| !c.is_empty())?, columns))
}

/// The arguments of an FTS5 declaration, split on the commas outside quotes.
fn split_arguments(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in body.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == ',' => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            None => {}
        }
        current.push(c);
    }
    out.push(current);
    out
}

/// An identifier or string as written in SQL, without its quotes. A doubled
/// quote inside is one quote — and reading `"a""b"` as two words would be
/// wrong about a name somebody chose.
fn unquote(text: &str) -> String {
    for q in ['"', '\'', '`'] {
        if text.len() >= 2 && text.starts_with(q) && text.ends_with(q) {
            let doubled: String = [q, q].iter().collect();
            return text[1..text.len() - 1].replace(&doubled, &q.to_string());
        }
    }
    text.to_owned()
}

/// The names of the triggers on a table.
pub(crate) fn triggers(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = prepare(
        conn,
        "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name",
    )?;
    let names = stmt
        .query_map([table], |row| row.get::<_, String>(0))
        .map_err(|e| database(e, "reading triggers"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| database(e, "reading triggers"))?;
    Ok(names)
}

/// The `CREATE …` statement an object was made with, when SQLite kept one.
pub(crate) fn object_sql(conn: &Connection, kind: &str, name: &str) -> Result<Option<String>> {
    let mut stmt = prepare(
        conn,
        "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
    )?;
    let sql = stmt
        .query_row([kind, name], |row| row.get::<_, Option<String>>(0))
        .optional()
        .map_err(|e| database(e, "reading an object's definition"))?
        .flatten();
    Ok(sql)
}

/// Every `CREATE …` statement for the objects of one kind attached to a table —
/// what a rebuild has to put back after the table is dropped.
pub(crate) fn objects_sql(conn: &Connection, kind: &str, table: &str) -> Result<Vec<String>> {
    let mut stmt = prepare(
        conn,
        "SELECT sql FROM sqlite_master \
         WHERE type = ?1 AND tbl_name = ?2 AND sql IS NOT NULL ORDER BY name",
    )?;
    let rows = stmt
        .query_map([kind, table], |row| row.get::<_, String>(0))
        .map_err(|e| database(e, "reading object definitions"))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| database(e, "reading object definitions"))?;
    Ok(rows)
}

/// The comment stored for one object, if this database has any.
///
/// SQLite has no `COMMENT ON`, so [`crate::ddl`] keeps comments in a table of
/// its own; a database nobody ever commented on does not have that table, and
/// "no comments anywhere" is an answer rather than a failure.
pub(crate) fn comment(
    conn: &Connection,
    kind: &str,
    table: &str,
    name: &str,
) -> Result<Option<String>> {
    if !table_exists(conn, COMMENTS_TABLE)? {
        return Ok(None);
    }
    let mut stmt = prepare(
        conn,
        &format!(
            "SELECT comment FROM \"{COMMENTS_TABLE}\" \
             WHERE kind = ?1 AND \"table\" = ?2 AND name = ?3"
        ),
    )?;
    let found = stmt
        .query_row([kind, table, name], |row| row.get::<_, Option<String>>(0))
        .optional()
        .map_err(|e| database(e, "reading a comment"))?
        .flatten();
    Ok(found)
}

/// The expression an index is over, taken out of its `CREATE INDEX` statement.
///
/// SQLite reports an expression index's columns as nulls and keeps the
/// expression only in the statement it was created with, so the text between the
/// outermost parentheses is where it is. Best effort by nature: a partial index
/// (`… WHERE x`) yields the indexed part, which is the part being described.
fn index_expression(sql: &str) -> Option<String> {
    let open = sql.find('(')?;
    let close = sql.rfind(')')?;
    (close > open + 1).then(|| sql[open + 1..close].trim().to_owned())
}

/// Prepare a statement, reporting a failure as a database error.
fn prepare<'a>(conn: &'a Connection, sql: &str) -> Result<rusqlite::Statement<'a>> {
    conn.prepare(sql)
        .map_err(|e| Error::database(format!("introspection query failed: {e}\n  sql: {sql}")))
}

/// A rusqlite error while introspecting, as ours.
fn database(error: rusqlite::Error, doing: &str) -> Error {
    Error::database(format!("{doing}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expression_index_reports_the_expression_it_is_over() {
        assert_eq!(
            index_expression("CREATE INDEX i ON t (lower(name))").as_deref(),
            Some("lower(name)")
        );
        assert_eq!(index_expression("CREATE INDEX i ON t").as_deref(), None);
    }
}

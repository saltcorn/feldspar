//! Running statements against a SQLite connection.
//!
//! Factored out of the driver so that the same code path runs a statement
//! whether it was issued on a pooled connection ([`SqliteDriver`](crate::SqliteDriver))
//! or inside a transaction ([`SqliteTransaction`](crate::transaction::SqliteTransaction)).
//!
//! **Everything here is blocking**, and deliberately so: SQLite is a library in
//! this process, not a server on a socket, so a query is a function call that
//! returns when the work is done. The async surface the [`DatabaseDriver`] trait
//! requires is put on in [`crate::driver`], which runs these on tokio's blocking
//! pool — the honest shape, rather than an async signature wrapped round a call
//! that will block the reactor thread anyway.

use std::sync::Arc;

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use sc_db::{DescribedColumn, Row, RowStream, SchemaChange};
use sc_error::{Error, Result};
use sc_query::{SqlDialect, Statement};

use crate::dialect::SqliteDialect;
use crate::value::{self, Declared};

/// What a statement with no bind parameters passes to [`sc_log::log_sql`] — DDL,
/// a raw batch, a transaction verb.
pub(crate) const NO_BINDS: &[sc_query::Value] = &[];

/// What went wrong, as SQLite said it, with the **SQLSTATE the rest of Saltcorn
/// classifies failures by**.
///
/// SQLite reports a constraint violation as an extended result code rather than
/// as a five-character SQLSTATE, and the layers above (`sc_access::rows`, the
/// catalog's constraint mapping) read the Postgres spelling: `<message>
/// [<sqlstate>]`. Translating here rather than teaching every reader a second
/// vocabulary is what makes a unique violation on SQLite reach a form as the
/// admin's own error message, exactly as it does on Postgres.
pub(crate) fn db_error(error: &rusqlite::Error) -> String {
    let rusqlite::Error::SqliteFailure(code, message) = error else {
        return error.to_string();
    };
    let sqlstate = match code.extended_code {
        // 23505 unique_violation — a unique index or a primary key.
        rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
        | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
        | rusqlite::ffi::SQLITE_CONSTRAINT_ROWID => Some("23505"),
        // 23502 not_null_violation.
        rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL => Some("23502"),
        // 23503 foreign_key_violation.
        rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => Some("23503"),
        // 23514 check_violation — a CHECK, or a trigger that raised (which is
        // what a row constraint is).
        rusqlite::ffi::SQLITE_CONSTRAINT_CHECK | rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER => {
            Some("23514")
        }
        _ => None,
    };
    let message = message.clone().unwrap_or_else(|| code.to_string());
    match sqlstate {
        Some(state) => format!("{message} [{state}]"),
        None => message,
    }
}

/// Render `stmt` to SQLite SQL, run it on `conn`, and materialise the rows.
///
/// Any statement kind is accepted; a mutation without `RETURNING` simply yields
/// no rows — SQLite executes it on the first step either way.
pub(crate) fn run_query(
    conn: &Connection,
    dialect: &SqliteDialect,
    stmt: &Statement,
) -> Result<Vec<Row>> {
    let (sql, binds) = dialect.render(stmt)?;
    // Every statement this backend sends passes through here, which is why the
    // echo is here and not at each of the call sites that build one.
    sc_log::log_sql(&sql, &binds);

    let params: Vec<SqlValue> = binds.iter().map(value::bind).collect();
    let mut prepared = conn
        .prepare(&sql)
        .map_err(|e| query_failed(e, &sql, &binds))?;

    // The declared type of each result column, read once: it is what turns the
    // text back into the timestamp, uuid or json it was written as (see
    // `crate::value`). An expression column has none, and decodes by storage
    // class.
    let described = prepared.columns();
    let columns: Arc<Vec<String>> =
        Arc::new(described.iter().map(|c| c.name().to_owned()).collect());
    let declared: Vec<Declared> = described
        .iter()
        .map(|c| c.decl_type().map_or(Declared::Unknown, value::declared))
        .collect();
    drop(described);

    let mut rows = Vec::new();
    let mut cursor = prepared
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|e| query_failed(e, &sql, &binds))?;
    while let Some(row) = cursor.next().map_err(|e| query_failed(e, &sql, &binds))? {
        let mut values = Vec::with_capacity(columns.len());
        for (i, decl) in declared.iter().enumerate() {
            let raw = row
                .get_ref(i)
                .map_err(|e| Error::database(format!("decode column {i}: {e}")))?;
            values.push(value::decode(raw, *decl)?);
        }
        rows.push(Row::new(columns.clone(), values)?);
    }
    Ok(rows)
}

/// The same, as the [`RowStream`] the trait hands back.
pub(crate) fn run_query_stream(
    conn: &Connection,
    dialect: &SqliteDialect,
    stmt: &Statement,
) -> Result<RowStream> {
    run_query(conn, dialect, stmt).map(RowStream::from_rows)
}

/// Prepare `sql` and report the result columns SQLite says it will produce.
///
/// `param_types` is accepted and **not used**: SQLite has no typed parameters —
/// a bind carries its own type, and a prepared statement makes no promise about
/// what it will be given. Preparing still does the half of the job that matters
/// most: a statement that will not parse, or that names a column that is not
/// there, comes back as SQLite's own message while its author is still looking
/// at it.
///
/// A column that is a plain table column reports that column's declared type; an
/// expression reports **no type**, which is the truthful answer — SQLite decides
/// an expression's type per value, at run time, and the type layer reads an
/// unrecognised type name as text.
pub(crate) fn describe(
    conn: &Connection,
    sql: &str,
    _param_types: &[String],
) -> Result<Vec<DescribedColumn>> {
    // Preparing is not running, but it is still a statement an admin typed, so
    // a SQL log that omitted it would be missing the one they are debugging.
    sc_log::log_sql(sql, NO_BINDS);
    let prepared = conn.prepare(sql).map_err(|e| {
        Error::database(format!(
            "this SQL will not prepare: {}",
            db_error_of(&e, sql)
        ))
    })?;
    Ok(prepared
        .columns()
        .iter()
        .map(|column| DescribedColumn {
            name: column.name().to_owned(),
            sql_type: column.decl_type().unwrap_or_default().to_owned(),
        })
        .collect())
}

/// Apply a schema change (see [`crate::ddl`]).
pub(crate) fn run_ddl(
    conn: &Connection,
    dialect: &SqliteDialect,
    change: &SchemaChange,
) -> Result<()> {
    // `DROP COLUMN` has no `IF EXISTS` in SQLite, so the question is asked here
    // rather than rendered — the caller asked for "gone if it is there", and
    // that is what they get.
    if let SchemaChange::DropColumn {
        table,
        if_exists: true,
        ..
    } = change
        && !crate::introspect::table_exists(conn, table)?
    {
        return Ok(());
    }
    if let SchemaChange::DropColumn {
        table,
        column,
        if_exists: true,
    } = change
        && !crate::introspect::columns(conn, table)?
            .iter()
            .any(|c| &c.name == column)
    {
        return Ok(());
    }
    crate::ddl::apply(conn, dialect, change)
}

/// Run a raw, multi-statement script.
pub(crate) fn run_batch(conn: &Connection, sql: &str) -> Result<()> {
    sc_log::log_sql(sql, NO_BINDS);
    conn.execute_batch(sql)
        .map_err(|e| Error::database(format!("batch failed: {}\n  sql: {sql}", db_error(&e))))
}

/// A failed statement, with the SQL that failed and how many binds it had — the
/// values themselves are left out, because they may hold secrets and this
/// message reaches the client.
fn query_failed(error: rusqlite::Error, sql: &str, binds: &[sc_query::Value]) -> Error {
    Error::database(format!(
        "query failed: {}\n  sql: {sql}\n  ({} bind parameter(s))",
        db_error(&error),
        binds.len()
    ))
}

/// A rusqlite error with its statement, for the paths that have no binds.
fn db_error_of(error: &rusqlite::Error, sql: &str) -> String {
    format!("{}\n  sql: {sql}", db_error(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rest of Saltcorn tells a unique violation from a check violation by
    /// SQLSTATE, so a SQLite failure has to arrive wearing one.
    #[test]
    fn a_constraint_violation_carries_the_sqlstate_the_api_reads() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE t (a text UNIQUE); INSERT INTO t VALUES ('x')")
            .expect("seed");
        let error = conn
            .execute_batch("INSERT INTO t VALUES ('x')")
            .expect_err("a second `x` is a unique violation");
        let text = db_error(&error);
        assert!(text.contains("[23505]"), "{text}");
        assert!(text.contains("UNIQUE constraint failed"), "{text}");
    }
}

//! Statement and DDL execution against a `tokio_postgres` connection.
//!
//! Factored out of the driver so the exact same code path runs a statement
//! whether it is issued on a pooled connection ([`PgDriver`](crate::PgDriver))
//! or inside a transaction ([`PgTransaction`](crate::transaction::PgTransaction)).
//! Both hold a `deadpool` connection that derefs to a `tokio_postgres::Client`.

use std::sync::Arc;

use sc_db::{DescribedColumn, Row, RowStream, SchemaChange};
use sc_error::{Error, Result};
use sc_query::{SqlDialect, Statement};
use tokio_postgres::Client;
use tokio_postgres::types::{ToSql, Type};

use crate::dialect::PgDialect;
use crate::value::{PgParam, decode};

/// What went wrong, as the *server* said it — the part a
/// `tokio_postgres::Error` hides behind a terse "db error".
///
/// Three things come out of a `DbError` and all three matter upstream:
///
/// - the **SQLSTATE**, which is the only reliable classification of a failure
///   (`23505` unique violation, `23514` check violation, `42501` policy
///   violation) — matching on message text is guesswork, and `sc-catalog`'s
///   policy mapping was reduced to exactly that for want of the code;
/// - the server's **message**, which for a row constraint's trigger *is* the
///   sentence the admin wrote for this moment;
/// - the **constraint** it names, which is how the layer above knows *which*
///   rule was broken and whose message to say instead.
///
/// The format is stable because upper layers read it back
/// (`sc_access::rows::constraint_message`): `<message> [<sqlstate>]` with, when
/// there is one, ` (constraint "<name>")`.
pub(crate) fn db_error(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => {
            let constraint = db
                .constraint()
                .map(|c| format!(" (constraint \"{c}\")"))
                .unwrap_or_default();
            format!("{} [{}]{constraint}", db.message(), db.code().code())
        }
        // A connection or protocol failure has no SQLSTATE; the chain is all
        // there is, and it is what this always printed.
        None => sc_error::format_chain(e),
    }
}

/// What a statement with no bind parameters passes to
/// [`sc_log::log_sql`] — DDL, a raw batch, a transaction verb.
pub(crate) const NO_BINDS: &[sc_query::Value] = &[];

/// Render `stmt` to Postgres SQL, run it on `client`, and materialise the rows.
///
/// Any statement kind is accepted; a non-`RETURNING` mutation simply yields no
/// rows. Rows are buffered into the stream for the MVP — the `RowStream`
/// signature keeps a later switch to true server-side streaming invisible.
pub(crate) async fn run_query(
    client: &Client,
    dialect: &PgDialect,
    stmt: &Statement,
) -> Result<RowStream> {
    let (sql, binds) = dialect.render(stmt)?;
    // Every statement this backend sends passes through here, which is why the
    // echo is here and not at each of the dozen call sites that build one.
    sc_log::log_sql(&sql, &binds);

    // Bind values are wrapped so the whole ordered set can be passed as
    // `&[&(dyn ToSql + Sync)]`.
    let params: Vec<PgParam> = binds.iter().map(PgParam).collect();
    let param_refs: Vec<&(dyn ToSql + Sync)> =
        params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();

    // A raw statement may state its parameter types (see `Statement::Raw`).
    // When it does, they are the types it was *described* under, so preparing
    // with them is what makes running it agree with the shape its endpoint
    // promises — and is the only way a placeholder whose type Postgres cannot
    // infer (`WHERE :q IS NULL`) can be sent at all.
    let prepared = match stmt {
        Statement::Raw { param_types, .. } if !param_types.is_empty() => {
            let types: Vec<Type> = param_types
                .iter()
                .map(|name| crate::value::pg_type(name))
                .collect::<Result<_>>()?;
            Some(client.prepare_typed(&sql, &types).await.map_err(|e| {
                Error::database(format!(
                    "query failed: {}\n  sql: {sql}",
                    sc_error::format_chain(&e)
                ))
            })?)
        }
        _ => None,
    };

    let pg_rows = match &prepared {
        Some(prepared) => client.query(prepared, &param_refs).await,
        None => client.query(&sql, &param_refs).await,
    }
    // `tokio_postgres::Error` displays as a terse "db error"; the real
    // cause (the server's SQLSTATE + message) is only in its source chain.
    // The failing SQL is included so the error is actionable; the bind
    // *values* are not — only their count — because they may hold secrets
    // (passwords, tokens) and this message is also returned to the client.
    .map_err(|e| {
        Error::database(format!(
            "query failed: {}\n  sql: {sql}\n  ({} bind parameter(s))",
            db_error(&e),
            binds.len(),
        ))
    })?;

    // All rows in a result share one column list; build it once.
    let columns: Arc<Vec<String>> = Arc::new(
        pg_rows
            .first()
            .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
            .unwrap_or_default(),
    );

    let mut rows = Vec::with_capacity(pg_rows.len());
    for pg in &pg_rows {
        let mut values = Vec::with_capacity(pg.len());
        for i in 0..pg.len() {
            values.push(decode(pg, i)?);
        }
        rows.push(Row::new(columns.clone(), values)?);
    }
    Ok(RowStream::from_rows(rows))
}

/// Prepare `sql` with its parameters typed as `param_types`, and report the
/// result columns Postgres says it will produce.
///
/// `prepare_typed` plans the statement without running it, and the returned
/// `Statement::columns()` carries each output column's name and type — which is
/// how a custom SQL query is typed by the database rather than by an
/// administrator's declaration (§13.4). The prepared statement is dropped
/// immediately; nothing is cached and nothing is executed.
///
/// A statement that will not prepare comes back as **Postgres's own message**,
/// the whole point of describing at save time: "column `titel` does not exist"
/// lands the author on the typo.
pub(crate) async fn describe(
    client: &Client,
    sql: &str,
    param_types: &[String],
) -> Result<Vec<DescribedColumn>> {
    let types: Vec<Type> = param_types
        .iter()
        .map(|name| crate::value::pg_type(name))
        .collect::<Result<_>>()?;
    // Preparing is not running, but it *is* traffic to the database and the
    // statement is one an admin typed, so an SQL log that omitted it would be
    // missing exactly the statement they are debugging.
    sc_log::log_sql(sql, NO_BINDS);
    let prepared = client.prepare_typed(sql, &types).await.map_err(|e| {
        // As in `run_query`: the terse Display hides the server's own message,
        // which here is the only useful part.
        Error::database(format!(
            "this SQL will not prepare: {}",
            sc_error::format_chain(&e)
        ))
    })?;
    Ok(prepared
        .columns()
        .iter()
        .map(|c| DescribedColumn {
            name: c.name().to_owned(),
            sql_type: c.type_().name().to_owned(),
        })
        .collect())
}

/// Render `change` to a single DDL statement and run it over the simple-query
/// protocol (DDL takes no binds).
pub(crate) async fn run_ddl(
    client: &Client,
    dialect: &PgDialect,
    change: &SchemaChange,
) -> Result<()> {
    let sql = crate::ddl::render(dialect, change)?;
    sc_log::log_sql(&sql, NO_BINDS);
    client.batch_execute(&sql).await.map_err(|e| {
        Error::database(format!(
            "apply_schema failed: {}\n  sql: {sql}",
            db_error(&e)
        ))
    })?;
    Ok(())
}

//! [`MigrationCtx`]: what a migration's body is handed.

use sc_db::{Row, Transaction};
use sc_error::Result;
use sc_query::{Statement, Value};

/// The transaction a migration runs in — the same one that records it, so the
/// change and the record commit together — and the few questions a migration
/// needs answered about the schema it finds.
///
/// Deliberately nothing more: no catalog and no table metadata, because those
/// describe the current release's schema and a migration is written against the
/// previous one.
pub struct MigrationCtx {
    tx: Box<dyn Transaction>,
    sqlite: bool,
}

impl MigrationCtx {
    pub(crate) fn new(tx: Box<dyn Transaction>, sqlite: bool) -> MigrationCtx {
        MigrationCtx { tx, sqlite }
    }

    pub(crate) fn into_transaction(self) -> Box<dyn Transaction> {
        self.tx
    }

    /// Whether the database is SQLite (otherwise PostgreSQL).
    pub fn is_sqlite(&self) -> bool {
        self.sqlite
    }

    /// The `n`th (1-based) bind placeholder in this backend's spelling: `$1`
    /// or `?1`.
    pub fn placeholder(&self, n: usize) -> String {
        placeholder(self.sqlite, n)
    }

    /// Run a script of one or more statements, with no binds.
    pub async fn batch(&mut self, sql: &str) -> Result<()> {
        self.tx.batch(sql).await
    }

    /// Run one statement with binds (written with [`placeholder`](Self::placeholder)'s
    /// spelling) and collect its rows.
    pub async fn query(&mut self, sql: &str, binds: Vec<Value>) -> Result<Vec<Row>> {
        self.tx
            .query(&Statement::raw(sql, binds))
            .await?
            .try_collect()
            .await
    }

    /// Run one statement with binds, ignoring any rows it returns.
    pub async fn execute(&mut self, sql: &str, binds: Vec<Value>) -> Result<()> {
        self.query(sql, binds).await.map(drop)
    }

    /// Whether the table `name` exists (in the connection's schema, on
    /// Postgres).
    pub async fn table_exists(&mut self, name: &str) -> Result<bool> {
        let (sql, binds) = table_exists_query(self.sqlite, name);
        Ok(!self.query(sql, binds).await?.is_empty())
    }

    /// Whether the table `table` has the column `column`. False when the table
    /// does not exist.
    pub async fn column_exists(&mut self, table: &str, column: &str) -> Result<bool> {
        let (sql, binds) = column_exists_query(self.sqlite, table, column);
        Ok(!self.query(sql, binds).await?.is_empty())
    }

    /// The transaction itself, for what the methods above do not cover.
    pub fn transaction(&mut self) -> &mut dyn Transaction {
        self.tx.as_mut()
    }
}

pub(crate) fn placeholder(sqlite: bool, n: usize) -> String {
    if sqlite {
        format!("?{n}")
    } else {
        format!("${n}")
    }
}

/// A query that returns a row when the table exists. `to_regclass` resolves the
/// name through `search_path`, which is how a driver scoped to a schema sees
/// its tables; the name is quoted so that it is taken as written.
pub(crate) fn table_exists_query(sqlite: bool, name: &str) -> (&'static str, Vec<Value>) {
    if sqlite {
        (
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            vec![Value::Text(name.to_owned())],
        )
    } else {
        (
            "SELECT 1 FROM pg_catalog.pg_class \
             WHERE oid = to_regclass($1) AND relkind IN ('r', 'p')",
            vec![Value::Text(quote_ident(name))],
        )
    }
}

fn column_exists_query(sqlite: bool, table: &str, column: &str) -> (&'static str, Vec<Value>) {
    if sqlite {
        (
            "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
            vec![
                Value::Text(table.to_owned()),
                Value::Text(column.to_owned()),
            ],
        )
    } else {
        (
            "SELECT 1 FROM pg_catalog.pg_attribute \
             WHERE attrelid = to_regclass($1) AND attname::text = $2 \
               AND attnum > 0 AND NOT attisdropped",
            vec![
                Value::Text(quote_ident(table)),
                Value::Text(column.to_owned()),
            ],
        )
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

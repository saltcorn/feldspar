//! [`SqliteTransaction`] — a transaction on one pooled SQLite connection.
//!
//! A transaction owns its connection for as long as it lives, because `BEGIN` is
//! a property of a connection rather than of a statement: sharing one would pull
//! every query running beside it into the transaction. `commit` and `rollback`
//! consume the boxed handle, so a transaction is finalised exactly once, and a
//! handle dropped without either rolls back — so the connection goes back to the
//! pool clean rather than mid-transaction.
//!
//! `BEGIN IMMEDIATE`, not a plain `BEGIN`. SQLite's default deferred transaction
//! takes its write lock at the first write, which means two transactions that
//! both read and then write can reach a point where neither can proceed and one
//! is thrown out with `SQLITE_BUSY` *after* doing its work — the classic SQLite
//! upgrade deadlock, and it cannot be waited out. Taking the write lock up front
//! turns that into an ordinary wait at the start, covered by the busy timeout.

use async_trait::async_trait;
use sc_db::{RowStream, SchemaChange, Transaction};
use sc_error::{Error, Result};
use sc_query::Statement;

use crate::dialect::SqliteDialect;
use crate::pool::PooledConnection;

/// A transaction bound to one pooled SQLite connection.
pub(crate) struct SqliteTransaction {
    /// The connection, held until the transaction is committed, rolled back or
    /// dropped. `None` after any of those.
    connection: Option<PooledConnection>,
    dialect: SqliteDialect,
}

impl SqliteTransaction {
    /// Open a transaction on `connection`.
    pub(crate) async fn begin(
        connection: PooledConnection,
        dialect: SqliteDialect,
    ) -> Result<SqliteTransaction> {
        let mut tx = SqliteTransaction {
            connection: Some(connection),
            dialect,
        };
        // The transaction verbs are logged like any other statement: a SQL log
        // in which the statements appear but the `BEGIN` that groups them does
        // not would misdescribe what ran.
        tx.on_connection(|conn| crate::exec::run_batch(conn, "BEGIN IMMEDIATE"))
            .await
            .map_err(|e| Error::database(format!("begin transaction: {e}")))?;
        Ok(tx)
    }

    /// Run `work` on the transaction's connection, on the blocking pool.
    ///
    /// The connection is moved into the blocking task and moved back out, which
    /// is what lets a `&mut self` method hand an owned, non-`Sync` connection to
    /// another thread and still hold it afterwards.
    async fn on_connection<T, F>(&mut self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
    {
        let connection = self
            .connection
            .take()
            .ok_or_else(|| Error::database("transaction already finished"))?;
        let (connection, result) = tokio::task::spawn_blocking(move || {
            let result = connection.connection().and_then(work);
            (connection, result)
        })
        .await
        .map_err(|e| Error::database(format!("sqlite worker: {e}")))?;
        self.connection = Some(connection);
        result
    }

    /// Take the connection to finalise the transaction.
    fn take(&mut self) -> Result<PooledConnection> {
        self.connection
            .take()
            .ok_or_else(|| Error::database("transaction already finished"))
    }

    /// Finish the transaction with `verb`, releasing its connection.
    async fn finish(mut self: Box<Self>, verb: &'static str) -> Result<()> {
        let connection = self.take()?;
        tokio::task::spawn_blocking(move || crate::exec::run_batch(connection.connection()?, verb))
            .await
            .map_err(|e| Error::database(format!("sqlite worker: {e}")))?
            .map_err(|e| Error::database(format!("{} transaction: {e}", verb.to_lowercase())))
    }
}

#[async_trait]
impl Transaction for SqliteTransaction {
    async fn query(&mut self, stmt: &Statement) -> Result<RowStream> {
        let dialect = self.dialect;
        let stmt = stmt.clone();
        let rows = self
            .on_connection(move |conn| crate::exec::run_query(conn, &dialect, &stmt))
            .await?;
        Ok(RowStream::from_rows(rows))
    }

    // `set_local` is deliberately not implemented: SQLite has no
    // transaction-local settings, so the trait's default — an error saying so —
    // is the honest answer. Nothing asks for one, because the authorization
    // strategy that reads them is row-level security, which this backend does
    // not advertise.

    async fn set_read_only(&mut self) -> Result<()> {
        // `query_only` is a *connection* setting rather than a transaction one,
        // so it is cleared again when the transaction finishes and hands its
        // connection back — see `finish` and `Drop`. Without that it would
        // travel with the connection into the pool and make the next writer
        // fail.
        self.on_connection(|conn| crate::exec::run_batch(conn, "PRAGMA query_only = ON"))
            .await
    }

    async fn set_repeatable_read(&mut self) -> Result<()> {
        // Already so: `BEGIN IMMEDIATE` holds the write lock from the start, so
        // nothing else commits while this transaction reads.
        Ok(())
    }

    async fn defer_constraints(&mut self) -> Result<()> {
        // Scoped to the transaction by SQLite itself: `defer_foreign_keys`
        // reverts at commit or rollback, and the keys are all checked then.
        self.on_connection(|conn| crate::exec::run_batch(conn, "PRAGMA defer_foreign_keys = ON"))
            .await
    }

    async fn apply_schema(&mut self, change: &SchemaChange) -> Result<()> {
        let dialect = self.dialect;
        let change = change.clone();
        self.on_connection(move |conn| crate::exec::run_ddl(conn, &dialect, &change))
            .await
    }

    async fn batch(&mut self, sql: &str) -> Result<()> {
        let sql = sql.to_owned();
        self.on_connection(move |conn| crate::exec::run_batch(conn, &sql))
            .await
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        self.finish("COMMIT").await
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        self.finish("ROLLBACK").await
    }
}

impl Drop for SqliteTransaction {
    fn drop(&mut self) {
        // `commit`/`rollback` already took the connection; nothing to do.
        let Some(connection) = self.connection.take() else {
            return;
        };
        // Abandoned without an explicit finish: roll back, so the pooled
        // connection is not returned mid-transaction. This is a local call, not
        // a round trip, so unlike a network backend it can simply be made here.
        sc_log::log_sql("ROLLBACK -- abandoned transaction", crate::exec::NO_BINDS);
        if let Ok(conn) = connection.connection() {
            let _ = conn.execute_batch("ROLLBACK");
            let _ = conn.execute_batch("PRAGMA query_only = OFF");
        }
    }
}

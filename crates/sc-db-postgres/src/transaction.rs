//! [`PgTransaction`] — a Postgres transaction on a pooled connection.
//!
//! Rather than borrow a `tokio_postgres::Transaction` from the pooled client
//! (which would make this a self-referential struct), the transaction owns its
//! connection and drives `BEGIN`/`COMMIT`/`ROLLBACK` directly. Because
//! [`commit`](sc_db::Transaction::commit) and
//! [`rollback`](sc_db::Transaction::rollback) consume the boxed handle, a
//! transaction is finalised exactly once; the owned connection is taken out at
//! that point (leaving `None`), so a `Drop` after an explicit finish is a no-op.
//! A handle dropped *without* finishing rolls back so the connection returns to
//! the pool clean rather than mid-transaction.

use async_trait::async_trait;
use deadpool_postgres::Object;
use sc_db::{RowStream, SchemaChange, Transaction};
use sc_error::{Error, Result};
use sc_query::Statement;

use crate::dialect::PgDialect;

/// A transaction bound to one pooled Postgres connection.
pub(crate) struct PgTransaction {
    /// The connection, held until the transaction is committed, rolled back, or
    /// dropped. `None` after any of those.
    client: Option<Object>,
    dialect: PgDialect,
}

impl PgTransaction {
    /// Open a transaction on `client` by issuing `BEGIN`.
    pub(crate) async fn begin(client: Object, dialect: PgDialect) -> Result<Self> {
        // The transaction verbs are logged like any other statement: a SQL log
        // in which the statements appear but the `BEGIN` that groups them does
        // not would misdescribe what ran.
        sc_log::log_sql("BEGIN", crate::exec::NO_BINDS);
        client
            .batch_execute("BEGIN")
            .await
            .map_err(|e| Error::database(format!("begin transaction: {e}")))?;
        Ok(PgTransaction {
            client: Some(client),
            dialect,
        })
    }

    /// Borrow the live connection, or error if the transaction is already
    /// finished.
    fn client(&self) -> Result<&Object> {
        self.client
            .as_ref()
            .ok_or_else(|| Error::database("transaction already finished"))
    }

    /// Take the connection to finalise the transaction, or error if it is
    /// already finished.
    fn take_client(&mut self) -> Result<Object> {
        self.client
            .take()
            .ok_or_else(|| Error::database("transaction already finished"))
    }
}

#[async_trait]
impl Transaction for PgTransaction {
    async fn query(&mut self, stmt: &Statement) -> Result<RowStream> {
        let client = self.client()?;
        crate::exec::run_query(client, &self.dialect, stmt).await
    }

    async fn set_local(&mut self, name: &str, value: &str) -> Result<()> {
        let client = self.client()?;
        // `set_config(name, value, is_local=true)` is the function form of
        // `SET LOCAL`, and unlike the `SET` statement it takes the setting
        // *value* as a bind — so it is parameterised, never interpolated. The
        // setting *name* is a fixed constant from our own code (`sc.role`,
        // `sc.user`), not user input.
        sc_log::log_sql("SELECT set_config($1, $2, true)", &[name, value]);
        client
            .query("SELECT set_config($1, $2, true)", &[&name, &value])
            .await
            .map_err(|e| {
                Error::database(format!(
                    "set local `{name}`: {}",
                    sc_error::format_chain(&e)
                ))
            })?;
        Ok(())
    }

    async fn set_read_only(&mut self) -> Result<()> {
        // `SET TRANSACTION` may only be issued before the transaction's first
        // statement, which the trait states and every caller does.
        self.simple("SET TRANSACTION READ ONLY").await
    }

    async fn set_repeatable_read(&mut self) -> Result<()> {
        // Postgres takes the snapshot at the first statement and keeps it.
        self.simple("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .await
    }

    async fn defer_constraints(&mut self) -> Result<()> {
        // Every foreign key this driver creates is `DEFERRABLE INITIALLY
        // IMMEDIATE` (see `crate::ddl`), which is what makes this possible at
        // all: a constraint declared without it cannot be deferred later.
        self.simple("SET CONSTRAINTS ALL DEFERRED").await
    }

    async fn apply_schema(&mut self, change: &SchemaChange) -> Result<()> {
        let client = self.client()?;
        crate::exec::run_ddl(client, &self.dialect, change).await
    }

    async fn batch(&mut self, sql: &str) -> Result<()> {
        let client = self.client()?;
        sc_log::log_sql(sql, crate::exec::NO_BINDS);
        client.batch_execute(sql).await.map_err(|e| {
            Error::database(format!(
                "batch failed: {}\n  sql: {sql}",
                crate::exec::db_error(&e)
            ))
        })?;
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        let mut this = self;
        let client = this.take_client()?;
        sc_log::log_sql("COMMIT", crate::exec::NO_BINDS);
        client
            .batch_execute("COMMIT")
            .await
            // The last thing a deferred constraint can fail at is the commit
            // itself (§5), so this error carries the same detail a statement's
            // does — without it, "commit transaction: db error" would be the
            // whole report of a row constraint that was put off to here.
            .map_err(|e| {
                Error::database(format!("commit transaction: {}", crate::exec::db_error(&e)))
            })?;
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        let mut this = self;
        let client = this.take_client()?;
        sc_log::log_sql("ROLLBACK", crate::exec::NO_BINDS);
        client
            .batch_execute("ROLLBACK")
            .await
            .map_err(|e| Error::database(format!("rollback transaction: {e}")))?;
        Ok(())
    }
}

impl PgTransaction {
    /// Run one statement that takes no binds and returns nothing, reporting a
    /// failure with the server's own message.
    async fn simple(&mut self, sql: &'static str) -> Result<()> {
        let client = self.client()?;
        sc_log::log_sql(sql, crate::exec::NO_BINDS);
        client
            .batch_execute(sql)
            .await
            .map_err(|e| Error::database(format!("{sql}: {}", crate::exec::db_error(&e))))
    }
}

impl Drop for PgTransaction {
    fn drop(&mut self) {
        // `commit`/`rollback` already took the connection; nothing to do.
        let Some(client) = self.client.take() else {
            return;
        };
        // Abandoned without an explicit finish: roll back so the pooled
        // connection is not returned mid-transaction. `Drop` is sync, so run the
        // rollback on the runtime if we are on one, else on a short-lived thread.
        sc_log::log_sql("ROLLBACK -- abandoned transaction", crate::exec::NO_BINDS);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = client.batch_execute("ROLLBACK").await;
                });
            }
            Err(_) => {
                std::thread::spawn(move || {
                    if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        rt.block_on(async move {
                            let _ = client.batch_execute("ROLLBACK").await;
                        });
                    }
                });
            }
        }
    }
}

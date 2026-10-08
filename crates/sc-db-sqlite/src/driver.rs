//! [`SqliteDriver`] — one connected SQLite database, and its [`DatabaseDriver`]
//! implementation.
//!
//! The driver owns a small pool of connections ([`crate::pool`]) and renders
//! statements with [`SqliteDialect`]. Statement execution lives in
//! [`crate::exec`] (shared with transactions), introspection in
//! [`crate::introspect`], DDL in [`crate::ddl`], transactions in
//! [`crate::transaction`].
//!
//! **The async surface is put on here.** SQLite is a library in this process:
//! every call into it blocks until the work is done. So each method hands its
//! blocking work to `tokio::task::spawn_blocking` and awaits the result, which
//! keeps a long query off the reactor thread — the alternative, an `async fn`
//! that simply calls into SQLite, would stall every other task on that thread
//! for the duration of the query.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use sc_db::{
    DatabaseDriver, DbCapabilities, DescribedColumn, PhysicalTable, RowStream, SchemaChange,
    Transaction,
};
use sc_error::{Error, Result};
use sc_query::{SqlDialect, Statement};

use crate::dialect::SqliteDialect;
use crate::pool::{SqlitePool, SqliteSource};

/// A connected SQLite database: a file (or a scratch in-memory database), a pool
/// of connections to it, and the SQL dialect that talks to it.
#[derive(Debug)]
pub struct SqliteDriver {
    pool: Arc<SqlitePool>,
    dialect: SqliteDialect,
}

impl SqliteDriver {
    /// Open the database at `path`, **creating the file** when it is not there.
    ///
    /// This is how the primary database is opened: a deployment that names a
    /// SQLite file in `feldspar.toml` and starts the server for the first time
    /// means for that file to come into existence, exactly as `createdb` is part
    /// of setting Postgres up.
    pub fn open(path: impl AsRef<Path>) -> Result<SqliteDriver> {
        SqliteDriver::from_source(SqliteSource::File(path.as_ref().to_path_buf()), true)
    }

    /// Open the database at `path`, **refusing** to create it.
    ///
    /// This is how a *secondary* connection is opened: the admin picked an
    /// existing file out of a file store, so a path that is not there is a
    /// mistake — and creating an empty database at it would answer the mistake
    /// with a connection that works and has no tables, which is the least
    /// helpful possible reply.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<SqliteDriver> {
        let path = path.as_ref();
        if !crate::pool::file_exists(path) {
            return Err(Error::not_found(format!(
                "there is no SQLite database at {}",
                path.display()
            )));
        }
        SqliteDriver::from_source(SqliteSource::File(path.to_path_buf()), false)
    }

    /// A private in-memory database, which lives as long as the driver does.
    pub fn open_in_memory() -> Result<SqliteDriver> {
        SqliteDriver::from_source(SqliteSource::memory(), true)
    }

    /// Open a pool onto `source`.
    fn from_source(source: SqliteSource, create: bool) -> Result<SqliteDriver> {
        Ok(SqliteDriver {
            pool: Arc::new(SqlitePool::new(source, create)?),
            dialect: SqliteDialect::new(),
        })
    }

    /// The file this driver is connected to, if it is connected to one.
    pub fn path(&self) -> Option<&PathBuf> {
        match self.pool.source() {
            SqliteSource::File(path) => Some(path),
            SqliteSource::Memory(_) => None,
        }
    }

    /// How this connection reads in a log line or an error.
    pub fn target(&self) -> String {
        self.pool.source().describe()
    }

    /// The SQLite dialect used to render statements for this driver.
    pub fn dialect(&self) -> &SqliteDialect {
        &self.dialect
    }

    /// What this SQLite backend supports.
    ///
    /// Two of the five are `false` and both are load-bearing:
    ///
    /// - **No row-level security.** SQLite has no policies and no session
    ///   settings to write them against, so authorization is enforced above the
    ///   database — which is what the capability flag exists to tell the
    ///   authorization layer (§7). Saying `true` here would mean policies were
    ///   generated and silently not applied.
    /// - **No unlogged tables.** There is nothing to switch off; the flag is a
    ///   performance property, and a table created without it is an ordinary
    ///   one, which the flag's own contract allows.
    ///
    /// Composite primary keys, `RETURNING` and — through the driver's own
    /// rebuild path — the schema changes `ALTER TABLE` cannot make are all
    /// supported.
    pub fn capabilities(&self) -> DbCapabilities {
        DbCapabilities {
            row_level_security: false,
            composite_pk: true,
            listen_notify: false,
            returning: true,
            // A rowid key derives its next value from the table, so there is no
            // sequence that can fall behind rows written with explicit keys —
            // and nothing for an import to wind forward afterwards.
            identity_sequences: false,
            unlogged_tables: false,
            native_temporal_types: false,
        }
    }

    /// Read the live schema of every user table in the database.
    pub async fn introspect(&self) -> Result<Vec<PhysicalTable>> {
        self.with_connection(crate::introspect::introspect).await
    }

    /// Render `stmt` to SQLite SQL, run it, and return the rows.
    pub async fn query(&self, stmt: &Statement) -> Result<RowStream> {
        let dialect = self.dialect;
        let stmt = stmt.clone();
        self.with_connection(move |conn| crate::exec::run_query_stream(conn, &dialect, &stmt))
            .await
    }

    /// Apply a single schema change. Creating a table emits exactly the columns
    /// given — no `id` column is invented (see [`crate::ddl`]).
    pub async fn apply_schema(&self, change: &SchemaChange) -> Result<()> {
        let dialect = self.dialect;
        let change = change.clone();
        self.with_connection(move |conn| crate::exec::run_ddl(conn, &dialect, &change))
            .await
    }

    /// Render `change` as the SQLite DDL `apply_schema` would run, without
    /// running it — the generated `schema.sql` an application carries (§13.3).
    pub fn render_ddl(&self, change: &SchemaChange) -> Result<String> {
        crate::ddl::render(&self.dialect, change)
    }

    /// Prepare `sql` and report the result columns SQLite says it will produce,
    /// without running it (see [`crate::exec::describe`]).
    pub async fn describe(
        &self,
        sql: &str,
        param_types: &[String],
    ) -> Result<Vec<DescribedColumn>> {
        let sql = sql.to_owned();
        let param_types = param_types.to_vec();
        self.with_connection(move |conn| crate::exec::describe(conn, &sql, &param_types))
            .await
    }

    /// Begin a transaction on a connection of its own.
    pub async fn begin(&self) -> Result<Box<dyn Transaction>> {
        let pool = self.pool.clone();
        let connection = tokio::task::spawn_blocking(move || pool.get())
            .await
            .map_err(|e| Error::database(format!("sqlite worker: {e}")))??;
        Ok(Box::new(
            crate::transaction::SqliteTransaction::begin(connection, self.dialect).await?,
        ))
    }

    /// Run `work` on a pooled connection, on tokio's blocking pool.
    async fn with_connection<T, F>(&self, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let connection = pool.get()?;
            work(connection.connection()?)
        })
        .await
        .map_err(|e| Error::database(format!("sqlite worker: {e}")))?
    }
}

// The trait impl delegates to the inherent methods above. Inherent methods
// shadow trait methods in resolution, so the `SqliteDriver::method(self, …)`
// calls below refer to the inherent implementations (no recursion).
#[async_trait]
impl DatabaseDriver for SqliteDriver {
    async fn introspect(&self) -> Result<Vec<PhysicalTable>> {
        SqliteDriver::introspect(self).await
    }

    async fn query(&self, stmt: &Statement) -> Result<RowStream> {
        SqliteDriver::query(self, stmt).await
    }

    async fn apply_schema(&self, change: &SchemaChange) -> Result<()> {
        SqliteDriver::apply_schema(self, change).await
    }

    fn render_ddl(&self, change: &SchemaChange) -> Result<String> {
        SqliteDriver::render_ddl(self, change)
    }

    async fn describe(&self, sql: &str, param_types: &[String]) -> Result<Vec<DescribedColumn>> {
        SqliteDriver::describe(self, sql, param_types).await
    }

    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        SqliteDriver::begin(self).await
    }

    fn capabilities(&self) -> DbCapabilities {
        SqliteDriver::capabilities(self)
    }

    fn dialect(&self) -> &dyn SqlDialect {
        &self.dialect
    }

    /// SpatiaLite is out of scope (analytics TODO, "Explicitly out of scope").
    fn spatial(&self) -> sc_db::SpatialSupport {
        sc_db::SpatialSupport::not_postgres("SQLite")
    }
}

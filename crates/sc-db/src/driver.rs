//! The [`DatabaseDriver`] trait — one connected database — and the
//! [`Transaction`] handle it hands out.
//!
//! A driver is instantiated once per connected database and **must** be written
//! in Rust (unlike most extension points, this one is not exposed to guest code
//! — technical design §5, §15). The catalog holds drivers behind
//! `Arc<dyn DatabaseDriver>`, so the trait is object-safe.

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_query::{SqlDialect, Statement};

use crate::capabilities::DbCapabilities;
use crate::row::RowStream;
use crate::schema::{DescribedColumn, PhysicalTable, SchemaChange};
use crate::spatial::SpatialSupport;

/// A single connected database: introspect its schema, run queries, change its
/// schema, and open transactions (technical design §5).
///
/// The **primary** database is just the one driver that additionally hosts the
/// `_fd_*` metadata and `users` tables; the trait itself draws no distinction.
#[async_trait]
pub trait DatabaseDriver: Send + Sync {
    /// Read the live schema (via `information_schema` or the backend
    /// equivalent). There is no separate discovery step: every table a
    /// connection can see is returned here and is immediately usable.
    async fn introspect(&self) -> Result<Vec<PhysicalTable>>;

    /// Run a statement and stream back its rows. Literals in `stmt` are already
    /// parameterised by [`sc_query`] rendering, so nothing is interpolated here.
    async fn query(&self, stmt: &Statement) -> Result<RowStream>;

    /// Apply a single schema change (create/drop table, add/drop column).
    /// Creating a table adds no implicit primary-key column.
    async fn apply_schema(&self, change: &SchemaChange) -> Result<()>;

    /// Render `change` as this backend's DDL **without applying it** — the same
    /// text [`apply_schema`](DatabaseDriver::apply_schema) would run.
    ///
    /// It exists so that the `schema.sql` in an application's generated
    /// directory (§13.3) — what a coding agent reads before writing a custom SQL
    /// query against these tables — is produced by the thing that renders the
    /// real DDL, rather than by a second DDL writer that would drift from it the
    /// first time a type or a constraint changed. Nothing about it is
    /// backend-agnostic: DDL is dialect-specific, which is exactly why it is the
    /// driver's to render.
    ///
    /// The default errors rather than returning something plausible: a backend
    /// that cannot render its own DDL cannot describe its schema either, and a
    /// file full of Postgres text for a database that is not Postgres would be a
    /// lie in a file that exists to be trusted.
    fn render_ddl(&self, change: &SchemaChange) -> Result<String> {
        let _ = change;
        Err(Error::database(
            "this database backend cannot render DDL, so an application's \
             generated `schema.sql` cannot be written for it",
        ))
    }

    /// Prepare `sql` — with its bind parameters typed by `param_types`, named as
    /// this backend names its types — and report the **result columns** the
    /// backend says it will produce, without running it.
    ///
    /// This is how a custom SQL query (§13.4) gets its result type: the database
    /// is the thing that knows what `SELECT sum(price), author FROM …` returns,
    /// so it is asked, rather than an administrator being made to declare a
    /// shape that goes stale the first time anyone edits the SQL. It doubles as
    /// validation — a statement that will not prepare comes back as the
    /// backend's own error, so a broken query is refused while its author is
    /// still looking at it.
    ///
    /// Preparing must have **no effect**: it is a plan, not an execution.
    ///
    /// The default errors rather than returning no columns: a backend that
    /// cannot describe a statement cannot type one either, and an empty answer
    /// would read as "this query returns nothing".
    async fn describe(&self, sql: &str, param_types: &[String]) -> Result<Vec<DescribedColumn>> {
        let _ = (sql, param_types);
        Err(Error::database(
            "this database backend cannot describe a statement, so a custom SQL \
             query cannot be typed against it",
        ))
    }

    /// Begin a transaction. Each metadata mutation (and, later, each workflow
    /// step) runs inside one; the returned handle is committed or rolled back
    /// exactly once.
    async fn begin(&self) -> Result<Box<dyn Transaction>>;

    /// What this backend supports. The authorization and message-bus layers
    /// branch on these flags rather than assuming a Postgres feature set.
    fn capabilities(&self) -> DbCapabilities;

    /// The SQL dialect used to render a [`Statement`] for this backend. A
    /// Postgres-dialect migration is translated to the driver's own dialect
    /// through this.
    fn dialect(&self) -> &dyn SqlDialect;

    /// Whether this database can hold geometry, as last found out by
    /// [`detect_spatial`](DatabaseDriver::detect_spatial) or
    /// [`enable_spatial`](DatabaseDriver::enable_spatial) (analytics TODO A5.1).
    ///
    /// The default is the answer for every backend that is not PostgreSQL.
    fn spatial(&self) -> SpatialSupport {
        SpatialSupport::not_postgres("not PostgreSQL")
    }

    /// Ask the database whether PostGIS is installed, and remember the answer.
    async fn detect_spatial(&self) -> Result<SpatialSupport> {
        Ok(self.spatial())
    }

    /// Install PostGIS where the connection's role may, then answer as
    /// [`detect_spatial`](DatabaseDriver::detect_spatial) does. A role that may
    /// not is not an error: the answer is "unavailable", saying why.
    async fn enable_spatial(&self) -> Result<SpatialSupport> {
        self.detect_spatial().await
    }
}

/// An in-progress transaction on a [`DatabaseDriver`].
///
/// Queries and schema changes issued through the handle are scoped to the
/// transaction; it is finished by [`commit`](Transaction::commit) or
/// [`rollback`](Transaction::rollback), both of which consume the handle so it
/// can be finalised only once. Dropping the handle without either rolls back.
#[async_trait]
pub trait Transaction: Send {
    /// Run a statement within the transaction.
    async fn query(&mut self, stmt: &Statement) -> Result<RowStream>;

    /// Set a **transaction-local** configuration parameter (`SET LOCAL`), so it
    /// is scoped to this transaction and reverts on commit/rollback — it never
    /// leaks to the next user of a pooled connection.
    ///
    /// This is how the authorization layer (§7.3) hands the current caller's
    /// role and identity to row-level-security policies: the policies read the
    /// parameter with `current_setting(name, true)`, and a transaction that
    /// forgets to set it sees the parameter as `NULL` — which the policies are
    /// generated to treat as "no access", so a forgotten context **fails
    /// closed**. `value` is bound, never interpolated; `name` is a fixed
    /// constant chosen by the caller, not user input.
    ///
    /// The default implementation errors: a backend without transaction-local
    /// settings cannot support this authorization mode, and saying so beats a
    /// silent no-op that would make policies see the wrong caller.
    async fn set_local(&mut self, name: &str, value: &str) -> Result<()> {
        let _ = (name, value);
        Err(Error::database(
            "this database backend does not support transaction-local settings (SET LOCAL)",
        ))
    }

    /// Make this transaction **read-only**: a write issued on it fails rather
    /// than happening.
    ///
    /// A defence in depth for the statements Saltcorn has already decided are
    /// reads — a custom SQL query declared read-only, reached by a `GET` (§13.4).
    /// The declaration is checked before the statement is sent; this is what
    /// makes the check true of the *database* rather than only of the parser, so
    /// an `UPDATE` that got past it is refused by the server that would have run
    /// it.
    ///
    /// Must be issued before the transaction's first statement, which is what
    /// Postgres's `SET TRANSACTION` requires and what every caller does anyway.
    /// The default errors: a backend that cannot make a transaction read-only
    /// must say so rather than return a transaction that will happily write.
    async fn set_read_only(&mut self) -> Result<()> {
        Err(Error::database(
            "this database backend cannot make a transaction read-only",
        ))
    }

    /// Put foreign-key checking off to the end of the transaction, so a row may
    /// reference one that arrives later in the same transaction.
    ///
    /// What a CSV import of a self-referencing table depends on (§13.1): the
    /// rows arrive in the file's order, and a parent three lines further down is
    /// there by the time the transaction commits. The keys are still checked —
    /// at commit, all of them — so a reference to a row that never arrives is
    /// still refused.
    ///
    /// The default errors rather than doing nothing, because "nothing" is the
    /// difference between an import that works and one that refuses every
    /// forward reference, and a caller told it succeeded would never know which.
    async fn defer_constraints(&mut self) -> Result<()> {
        Err(Error::database(
            "this database backend cannot defer foreign-key checking to commit",
        ))
    }

    /// Apply a schema change within the transaction.
    async fn apply_schema(&mut self, change: &SchemaChange) -> Result<()>;

    /// Run a raw, multi-statement SQL script with no bind parameters, in order,
    /// within the transaction.
    ///
    /// The escape hatch for backend-specific DDL that the structured
    /// [`SchemaChange`] does not model — row-level-security policies (§7.3),
    /// whose `CREATE POLICY` carries an arbitrary boolean expression. The SQL is
    /// generated by trusted code (formula translation, with literals rendered
    /// through the dialect's own quoting), never assembled from raw user input.
    /// The default errors, so a backend that has not opted in cannot silently
    /// skip the DDL.
    async fn batch(&mut self, sql: &str) -> Result<()> {
        let _ = sql;
        Err(Error::database(
            "this database backend does not support raw SQL batches",
        ))
    }

    /// Commit the transaction, making its effects durable.
    async fn commit(self: Box<Self>) -> Result<()>;

    /// Roll the transaction back, discarding its effects.
    async fn rollback(self: Box<Self>) -> Result<()>;
}

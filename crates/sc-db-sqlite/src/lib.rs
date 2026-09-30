//! SQLite backend for the database layer (layer 2).
//!
//! Implements the `sc-db` [`DatabaseDriver`](sc_db::DatabaseDriver) contract
//! against a SQLite database file:
//!
//! - [`SqliteDialect`] — the SQLite [`SqlDialect`](sc_query::SqlDialect), which
//!   renders a [`Statement`](sc_query::Statement) to `(sql, binds)` with
//!   double-quoted identifiers and `?n` placeholders.
//! - [`SqliteDriver`] — a pooled connection implementing `DatabaseDriver`: run a
//!   statement, introspect the live schema, apply schema changes, and open
//!   transactions.
//!
//! **Why a second backend at all.** Postgres is what a deployment runs; SQLite
//! is what a laptop, a Raspberry Pi and a one-file backup run. It is the primary
//! database of an installation that names a file in `feldspar.toml` — no server,
//! no role, no `createdb` — and it is a *secondary* connection to any `.sqlite`
//! file sitting in a file store, whose tables then join the tables list beside
//! everything else.
//!
//! **What SQLite is not.** It has no row-level security, so this driver does not
//! advertise the capability and authorization is enforced above the database
//! (§7); it has no `LISTEN`/`NOTIFY`, so the message bus does not use it (§16);
//! and it has no `COMMENT ON`, so the driver keeps object comments in a table of
//! its own (see [`ddl`](crate::ddl)). Everything else the design asks of a
//! database — composite keys, foreign keys onto non-key columns, `RETURNING`,
//! transactions, and the schema changes `ALTER TABLE` cannot make — is here.

mod ddl;
mod dialect;
mod driver;
mod exec;
mod functions;
mod introspect;
mod pool;
mod transaction;
mod value;

pub use dialect::SqliteDialect;
pub use driver::SqliteDriver;
pub use pool::SqliteSource;

//! Postgres backend for the database layer (layer 2).
//!
//! Implements the `sc-db` [`DatabaseDriver`](sc_db::DatabaseDriver) contract
//! against a real Postgres server:
//!
//! - [`PgDialect`] — the Postgres [`SqlDialect`](sc_query::SqlDialect) that
//!   renders a [`Statement`](sc_query::Statement) to `(sql, binds)` with
//!   double-quoted identifiers and `$n` placeholders.
//! - [`PgDriver`] — a pooled connection implementing `DatabaseDriver`: run a
//!   statement (streaming rows back as a [`RowStream`](sc_db::RowStream)),
//!   introspect the live schema, apply schema changes, and open transactions.

mod ddl;
mod dialect;
mod driver;
mod exec;
mod geometry;
mod introspect;
mod spatial;
mod transaction;
mod value;

pub use dialect::PgDialect;
pub use driver::{PgConnectParams, PgDriver};
pub use value::PgParam;

//! Migrations of the primary database (technical design §5.3).
//!
//! A release that needs the database in a different shape brings it there on
//! its first start. The bootstrap (`Catalog::bootstrap_table` and every crate's
//! `bootstrap_*`) creates a missing table and adds a missing column; a
//! **migration** does what the bootstrap cannot — a rename, a type or
//! nullability change, rewriting stored JSON, a backfill, deleting rows a
//! release no longer reads. Each runs **once** per database and is recorded in
//! [`MIGRATIONS_TABLE`], so a start never re-runs one and a migration need not
//! be idempotent.
//!
//! [`run`] is called by `sc_cli::connect_catalog` after connecting and before
//! the catalog reads the schema, so every migration sees the schema of the
//! release before it, and every command that opens the database migrates it.
//!
//! # Adding a migration
//!
//! Append it to [`MIGRATIONS`] (in `registry.rs`), newest last, with an id of
//! today's date and a few words: `2026-11-02-agent-traits`. Its body is either
//!
//! - **SQL**, one script per backend, kept in `migrations/<id>/postgres.sql` and
//!   `migrations/<id>/sqlite.sql` and `include_str!`ed — DDL and set-based
//!   updates. `None` for a backend it has nothing to do on (it is still
//!   recorded there). There is no translation between the two dialects; or
//! - **Rust** — a function over a [`MigrationCtx`], for stored JSON (read the
//!   rows, change each as a `serde_json::Value`, write it back) and for steps
//!   that depend on what is there. One function serves both backends.
//!
//! **A migration is frozen.** It is written against the schema of the release
//! before it, so it must not use anything that describes the current one — the
//! catalog, field metadata, a crate's row structs — which a later release will
//! change under it. That is why this crate depends on `sc-db` alone.
//!
//! Give it a test: build the shape it migrates from, run it alone with
//! [`run_one`], and check the result, on each backend it has a body for.

mod ctx;
mod registry;
mod runner;

pub use ctx::MigrationCtx;
pub use registry::MIGRATIONS;
pub use runner::{
    AppliedRecord, Event, Report, Status, check_registry, run, run_one, run_with, status,
};

// What a migration is written with, so one needs nothing but this crate.
pub use futures::future::BoxFuture;
pub use sc_db::Row;
pub use sc_error::Result;
pub use sc_query::Value;

/// The table that records which migrations have been applied to a database.
///
/// Clear all leaves it alone: it describes the schema, not the data, and an
/// emptied record would make the next start re-run every migration against a
/// schema that already has them.
pub const MIGRATIONS_TABLE: &str = "_fd_migrations";

/// One migration: an id that orders it, a sentence that describes it, and the
/// body that runs.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Date-prefixed, lowercase, `-`-separated: `2026-11-02-agent-traits`.
    /// Sorting the ids is the order they run in, so two branches that each add
    /// one do not collide over a number.
    pub id: &'static str,
    /// One sentence, for the log and `feldspar migrate status`.
    pub description: &'static str,
    /// What it does.
    pub body: Body,
}

/// What a [`Migration`] runs, inside the transaction that records it.
#[derive(Clone, Copy)]
pub enum Body {
    /// One script per backend, run as a batch. `None`: nothing to do on that
    /// backend.
    Sql {
        /// The script for PostgreSQL.
        postgres: Option<&'static str>,
        /// The script for SQLite.
        sqlite: Option<&'static str>,
    },
    /// A function, for what SQL says badly.
    Rust(RustStep),
}

/// The function a [`Body::Rust`] migration is: written as
///
/// ```ignore
/// fn step(ctx: &mut MigrationCtx) -> BoxFuture<'_, Result<()>> {
///     Box::pin(async move { /* … */ Ok(()) })
/// }
/// ```
pub type RustStep = fn(&mut MigrationCtx) -> BoxFuture<'_, Result<()>>;

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Sql { postgres, sqlite } => f
                .debug_struct("Sql")
                .field("postgres", &postgres.is_some())
                .field("sqlite", &sqlite.is_some())
                .finish(),
            Body::Rust(_) => f.write_str("Rust"),
        }
    }
}

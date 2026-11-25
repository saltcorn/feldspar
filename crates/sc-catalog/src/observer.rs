//! The schema-change seam: how a table or field change becomes something the
//! rest of the process can react to (TODO Phase 7).
//!
//! A **mounted application** built its REST projection from its tables as they
//! were at mount time (§4, §13.2), so a field added or dropped underneath it
//! leaves it serving endpoints for a column that is gone — or missing the ones
//! for a column that arrived — until a restart. The admin handlers used to fix
//! that by calling `AppMounts::refresh_table` themselves, which worked only
//! because every schema change went through a handler. Once an *agent* can edit
//! the schema (§11.3's `admin_copilot`), that is no longer true.
//!
//! So the notification moves to where the change is made, behind the same shape
//! the write seam already uses ([`TableEvents`](crate::TableEvents)): the mount
//! registry is `sc-server`'s and the schema editor is `sc-api`'s, neither can
//! name the other, and the catalog — which both already hold — owns the seam. A
//! server installs its observer once at boot with
//! [`Catalog::set_schema_observer`](crate::Catalog::set_schema_observer); a
//! process with none (a build tool, a test) simply changes the schema
//! unobserved.

use crate::catalog::Catalog;

/// What changed about a table's shape or configuration.
///
/// Coarse on purpose: every consumer so far reacts to "this table is not what it
/// was" by re-projecting it, and a finer enum would be states nobody branches
/// on. The table's *name* is what a consumer selects on, so that is what every
/// variant carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaChanged {
    /// A table was created.
    TableCreated(String),
    /// A table's settings, fields or access rules changed.
    TableChanged(String),
    /// A table was dropped.
    TableDropped(String),
}

impl SchemaChanged {
    /// The table this is about.
    pub fn table(&self) -> &str {
        match self {
            SchemaChanged::TableCreated(name)
            | SchemaChanged::TableChanged(name)
            | SchemaChanged::TableDropped(name) => name,
        }
    }
}

/// One mounted application the observer re-projected because of a change.
///
/// The seam carries this for the same reason it carries [`SchemaChanged`]: the
/// mount registry is `sc-server`'s and the schema editor is `sc-api`'s, neither
/// can name the other, and what the editor has to report is *what the reaction
/// did*. Without it a schema edit is a change with an invisible consequence —
/// which is exactly the half-finished state §13.6 spends its report avoiding: an
/// application whose generated client was just rewritten and whose **bundle was
/// not**.
///
/// So [`wants_build`](ReprojectedApp::wants_build) is the load-bearing field.
/// Re-projection deliberately runs no bundler — an access change alters who may
/// reach an app's data, not a byte it serves — but a *schema* change alters the
/// generated TypeScript client, and a code framework serves a built bundle. An
/// app whose framework declares a build step is therefore one the caller must be
/// told to rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReprojectedApp {
    /// The application's id — what a rebuild names it by.
    pub id: String,
    /// The subdomain it is served on: what an admin and an agent both call it.
    pub subdomain: String,
    /// Whether its framework has a build step, and therefore wants one run.
    pub wants_build: bool,
}

/// What it means to observe schema changes. `sc-server`'s mount registry is the
/// implementation; the catalog holds it as `dyn`.
pub trait SchemaObserver: Send + Sync {
    /// React to a change that has already been applied, and say which
    /// applications reacted.
    ///
    /// Called **after** the DDL committed and the catalog reloaded, so an
    /// observer reads the new schema simply by asking the catalog. An `Err` here
    /// means the *reaction* failed, never the change: the caller reports it
    /// beside the successful result rather than pretending the schema did not
    /// move.
    ///
    /// The returned list is the reaction's own report, and the caller passes it
    /// on: a schema edit that silently leaves an application serving a stale
    /// bundle is a change whose consequence nobody can see.
    fn schema_changed(
        &self,
        catalog: &Catalog,
        change: &SchemaChanged,
    ) -> sc_error::Result<Vec<ReprojectedApp>>;
}

/// What it means to follow a table being **renamed**.
///
/// A table is referenced by name from rows the catalog does not own — a view's
/// table, a trigger's table and the `table` its action writes to, an
/// application's table subset, a dataset's base, an agent trait's table — and
/// the crates that own those rows sit above the schema editor, which cannot
/// name them. So the catalog carries this seam as it carries the
/// [`SchemaObserver`], and `sc-server` installs the implementation at boot.
///
/// Async and separate from [`SchemaObserver`] because following a rename is
/// rewriting stored rows, not re-projecting what is in memory.
#[async_trait::async_trait]
pub trait TableRenameHook: Send + Sync {
    /// Rewrite every stored reference to table `from` so it names `to`.
    ///
    /// Called **after** the DDL committed, the catalog's own overlay rows moved
    /// and the catalog reloaded, and before the [`SchemaObserver`] hears of the
    /// change, so that what it re-projects already names the new table. The
    /// answer is a list of notes for the caller — what was rewritten, and what
    /// could not be. An `Err` is the reaction failing, never the rename.
    async fn table_renamed(
        &self,
        catalog: &Catalog,
        from: &str,
        to: &str,
    ) -> sc_error::Result<Vec<String>>;
}

/// An application's stored record was changed below the admin API — a
/// module's `schema` ask connected a table it created to the application
/// whose request ran it (TODO ST1.3) — so what is mounted must be read again.
///
/// The catalog carries the seam for the reason it carries the
/// [`TableRenameHook`]: the mount registry is `sc-server`'s, far above the
/// module host that made the change, and `sc-server` installs it at boot.
#[async_trait::async_trait]
pub trait ApplicationHook: Send + Sync {
    /// Serve application `app` as it is now stored. The answer is a list of
    /// notes for the caller; an `Err` is the remount failing, never the change.
    async fn application_changed(
        &self,
        catalog: &Catalog,
        app: uuid::Uuid,
    ) -> sc_error::Result<Vec<String>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_change_names_the_table_it_is_about() {
        assert_eq!(SchemaChanged::TableCreated("a".into()).table(), "a");
        assert_eq!(SchemaChanged::TableChanged("b".into()).table(), "b");
        assert_eq!(SchemaChanged::TableDropped("c".into()).table(), "c");
    }
}

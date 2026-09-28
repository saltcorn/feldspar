//! Library half of the `feldspar` binary (layer 10).
//!
//! The binary ([`main`](../main/index.html)) stays thin; the reusable pieces —
//! parsing the database connection ([`DbConfig`]), reading the per-environment
//! configuration file (`sc-config-file`, re-exported here as [`config_file`]),
//! the coding agent's evaluation harness ([`eval`]), the `i18n` commands and the
//! domains they read ([`i18n`]), the `cmdstan` commands' arguments and status
//! report ([`cmdstan`]), parsing the `api` commands' flags
//! ([`api`]), the `get-cfg`/`set-cfg` commands' arguments ([`config`]) and
//! standing up a connected [`Catalog`] ([`connect_catalog`]) —
//! live here so integration tests can drive the same boot path the CLI uses.

pub mod api;
pub mod auth;
pub mod cmdstan;
pub mod config;
pub mod db;
pub mod eval;
pub mod i18n;

/// The `feldspar.toml` reader. It lives in its own layer-0 crate because the
/// integration-test harness reads the same file (for the `test` environment),
/// and the harness cannot depend on the binary that sits at the top of the
/// workspace.
pub use sc_config_file as config_file;

use std::sync::Arc;

use sc_catalog::{
    Catalog, DbConnections, FileStoreConnections, connect_all_db_connections,
    connect_all_file_stores,
};
use sc_error::{Context, Error, Result};
use sc_files::{FileStoreDef, connect_from_def};

pub use config_file::{ConfigFile, Environment, SelectedEnvironment};
pub use db::{DbConfig, Serving};

/// Connect to the primary database described by `db`, initialise the
/// [`Catalog`] from its live schema, and ensure the platform tables (`users`,
/// `_fd_applications`, `_fd_file_stores`, `_fd_tables`) exist.
///
/// This is the whole "bring the data layer up" step of `feldspar serve`. The
/// first real connection happens inside [`Catalog::init`] (introspection), so a
/// database that is unreachable or misconfigured fails here — with the redacted
/// [`DbConfig::target`] in the message rather than a silent, half-booted server.
///
/// Every bootstrap is idempotent (each no-ops when its table already exists), so
/// this runs on every boot: a legacy database gains the tables on first serve,
/// and the admin UI can list/create applications without a migration step.
///
/// `_fd_tables` is bootstrapped here rather than lazily on first use because it
/// is an **overlay**: [`Catalog::reload`] consults it on every reload, and a
/// table that only appears once someone saves an overlay would mean the merge
/// silently does nothing on exactly the databases nobody has configured yet —
/// which is all of them, until they are.
pub async fn connect_catalog(db: &DbConfig) -> Result<Arc<Catalog>> {
    let driver = db
        .connect()
        .await
        .with_context(|| format!("connecting to database {}", db.target()))?;
    let catalog = Catalog::init(driver)
        .await
        .with_context(|| format!("reading the schema of database {}", db.target()))?;
    let catalog = Arc::new(catalog);
    sc_auth::bootstrap(&catalog)
        .await
        .context("ensuring the users table exists")?;
    sc_app::bootstrap(&catalog)
        .await
        .context("ensuring the applications table exists")?;
    sc_viewpattern::bootstrap(&catalog)
        .await
        .context("ensuring the views and pages tables exist")?;
    sc_catalog::bootstrap_file_stores(&catalog)
        .await
        .context("ensuring the file stores table exists")?;
    sc_catalog::bootstrap_db_connections(&catalog)
        .await
        .context("ensuring the database connections table exists")?;
    sc_catalog::bootstrap_table_meta(&catalog)
        .await
        .context("ensuring the table overlay table exists")?;
    sc_catalog::bootstrap_field_meta(&catalog)
        .await
        .context("ensuring the field overlay table exists")?;
    sc_llm::bootstrap_llm_providers(&catalog)
        .await
        .context("ensuring the LLM providers table exists")?;
    sc_agent::bootstrap_agents(&catalog)
        .await
        .context("ensuring the agents table exists")?;
    sc_agent::bootstrap_runs(&catalog)
        .await
        .context("ensuring the runs table exists")?;
    sc_config::bootstrap(&catalog)
        .await
        .context("ensuring the configuration tables exist")?;
    // The stored Localisation settings, on the same footing and here for the
    // same reason: what a `feldspar` command prints to an admin — and what a
    // server negotiates a request into — is a stored setting, so it has to be
    // read as soon as there is a database to read it from (§16.1).
    //
    // **Before** the Development settings, and that order is load-bearing: the
    // switch below turns SQL echoing on for this process, and a read performed
    // after it would print its own `SELECT` to stdout — which is exactly what
    // `get-cfg KEY` promises not to do.
    let localisation = sc_config::apply_localisation_settings(&catalog)
        .await
        .context("reading the localisation settings")?;
    if localisation.is_multilingual() {
        eprintln!(
            "feldspar: serving {} (default {}) \u{2014} Settings \u{2192} Localisation",
            localisation
                .enabled()
                .iter()
                .map(sc_i18n::Locale::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            localisation.default_locale().as_str(),
        );
    }
    // The stored Development settings become this process's logging switches as
    // soon as there is a database to read them from — here rather than in
    // `serve_command`, so a `feldspar` *command* run against an installation
    // with the SQL echo on prints its SQL too. Everything before this line runs
    // at the default verbosity, which is the price of the settings living in the
    // database the connection is being made to.
    let development = sc_config::apply_development_settings(&catalog)
        .await
        .context("reading the development settings")?;
    // Said out loud when it is not the default, because both switches are ones
    // somebody turns on to debug an afternoon and then forgets: a log full of
    // every statement, six weeks later, should name the checkbox that is doing
    // it.
    if development.log_sql || development.verbosity != sc_log::DEFAULT_VERBOSITY {
        eprintln!(
            "feldspar: log verbosity {}{} (Settings → Development)",
            development.verbosity.as_str(),
            if development.log_sql {
                ", logging every SQL statement to stdout"
            } else {
                ""
            }
        );
    }
    Ok(catalog)
}

/// Connect every **stored** file store, logging the outcome, and return the
/// report.
///
/// One store that fails to connect — a disk unmounted since it was defined — is
/// logged and skipped, never fatal. That is the same rule `mount_all` applies to
/// an application whose build fails (§13.2), and for the same reason: a server
/// that otherwise works should not refuse to boot over one broken store the
/// admin can repoint in the UI. The reason is recorded on the catalog, so the
/// admin API can still answer "why is this store not connected?" long after the
/// boot log has scrolled away.
pub async fn connect_stored_file_stores(catalog: &Catalog) -> Result<FileStoreConnections> {
    let report = connect_all_file_stores(catalog).await?;
    for name in &report.connected {
        eprintln!("feldspar: connected file store `{name}`");
    }
    for (name, error) in &report.failed {
        eprintln!("feldspar: file store `{name}` is defined but could not be connected: {error}");
    }
    Ok(report)
}

/// Connect every **stored** database connection, logging the outcome, and return
/// the report.
///
/// The same rule as `connect_stored_file_stores`, and it matters more here: a
/// secondary database that is unreachable must not stop a server whose *primary*
/// database is fine. Its tables simply are not in the catalog, the reason is
/// recorded for the admin API, and the Connections screen is where it gets
/// fixed.
///
/// The catalog is reloaded inside [`connect_all_db_connections`] when anything
/// connected, so the foreign tables are in the tables list by the time the
/// server starts serving.
pub async fn connect_stored_databases(catalog: &Catalog) -> Result<DbConnections> {
    let report = connect_all_db_connections(catalog).await?;
    for name in &report.connected {
        eprintln!("feldspar: connected database `{name}`");
    }
    for (name, error) in &report.failed {
        eprintln!("feldspar: database `{name}` is defined but could not be connected: {error}");
    }
    Ok(report)
}

/// Pull `--file-store NAME=PATH` flags (repeatable) out of `args`, returning the
/// parsed `NAME=PATH` specs and the arguments that were **not** consumed (for the
/// server config to parse). Like [`DbConfig::extract`], unknown flags pass through
/// so they still fail loudly in the server parser rather than here.
pub fn extract_file_stores<I, S>(args: I) -> Result<(Vec<String>, Vec<String>)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut specs = Vec::new();
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if arg.as_ref() == "--file-store" {
            let value = it
                .next()
                .map(|s| s.as_ref().to_owned())
                .ok_or_else(|| Error::config("--file-store requires a NAME=PATH value"))?;
            specs.push(value);
        } else {
            rest.push(arg.as_ref().to_owned());
        }
    }
    Ok((specs, rest))
}

/// Connect each `NAME=PATH` spec as a local file store on `catalog`.
///
/// The store's name is the part before the first `=`; the rest is a local
/// directory path, which must already exist. A malformed spec or an unreadable
/// directory is an [`Error::Config`], so a typo fails at boot rather than
/// surfacing as a puzzling 404 later.
///
/// Each spec is turned into a [`FileStoreDef`] and connected through
/// [`connect_from_def`], rather than building a `LocalFileStore` here: that
/// function is meant to be *the* place a definition becomes an instance, and a
/// second construction path would be a second place for the two to drift — a
/// flag-connected store would quietly not behave like a stored one.
///
/// **How the flag coexists with stored stores (TODO §1.3, resolved).** These
/// definitions are *not* persisted: the flag stays an ephemeral,
/// process-lifetime convenience, which is how the tests use it and how a
/// developer points at a scratch directory without touching the database. It is
/// applied **after** the stored stores, and a name already connected is a
/// **startup error** rather than a silent override.
///
/// Refusing is the important part. `Catalog::connect_file_store` replaces on a
/// repeated name, so a clash would otherwise mean the flag silently shadowed a
/// store the admin had configured in the UI — the admin would edit a store, see
/// their change saved, and watch the server keep serving a different directory,
/// with nothing anywhere saying why. An error at boot costs one restart; that
/// costs an afternoon.
pub fn connect_file_stores(catalog: &Catalog, specs: &[String]) -> Result<()> {
    for spec in specs {
        let (name, path) = spec.split_once('=').ok_or_else(|| {
            Error::config(format!("invalid --file-store `{spec}`: expected NAME=PATH"))
        })?;
        if name.is_empty() {
            return Err(Error::config(format!(
                "invalid --file-store `{spec}`: the name must not be empty"
            )));
        }
        if catalog.file_store(name)?.is_some() {
            return Err(Error::config(format!(
                "--file-store `{name}` clashes with a configured file store of the same name; \
                 rename one, or drop the flag and edit the store in the admin UI"
            )));
        }
        let store = connect_from_def(&FileStoreDef::local(name, path))
            .with_context(|| format!("connecting file store `{name}` at `{path}`"))?;
        catalog.connect_file_store(store)?;
    }
    Ok(())
}

//! The `_fd_db_connections` table: its schema, bootstrap, and the
//! [`DbConnectionDef`] ⇄ row mapping — the *other* databases an admin has
//! connected (design §5, §9).
//!
//! Saltcorn has always had exactly one database, and the [`DbId`] on every
//! [`Table`] has always been a placeholder waiting for the day it meant
//! something. This is that day: an admin adds a connection in the Connections
//! screen, and from then on that database's tables are in the catalog beside the
//! primary's, listed together, queried the same way, and told apart by the badge
//! the admin UI puts next to the name.
//!
//! **Why a row and not `feldspar.toml`.** The file names the *primary*
//! connection, which the process needs before it can read anything at all, and
//! which is therefore an operator's to write down. A secondary connection is not
//! like that: it is a thing an admin adds while the server is running, with
//! immediate effect and no restart, exactly as they add a file store or an LLM
//! provider. So it is a row, and it has the shape §9 requires of every system
//! metadata table.
//!
//! **A connection is a database Saltcorn can administer, and only where asked.**
//! Rows are read and written through the ordinary paths; tables that were
//! already there keep their shape unless an admin changes it deliberately, and a
//! table *created* on the connection — the New table dialog's Database chooser —
//! is theirs to alter and drop like any other. Everything routes by
//! [`Table::database`], so the one thing that never happens is a statement meant
//! for one database arriving at another.
//!
//! **Two kinds of database, one row.** A connection names a `backend`, and the
//! rest of the row is read according to it: a `postgres` connection is a host,
//! port, database, user, password and schema; a `sqlite` connection is a **file
//! store and a path inside it**, because a SQLite database *is* a file and
//! Saltcorn already has a place where files live (§14.1). That is the whole of
//! the difference — a connected SQLite file's tables are in the tables list
//! beside the primary's, read and written through the same paths, exactly as a
//! foreign Postgres schema's are.
//!
//! **Reading is strict**, exactly as it is for file stores: a column that is
//! missing or of the wrong shape is an [`Error::invalid`] naming the connection
//! and the column, not a silently defaulted field.
//!
//! [`DbId`]: crate::DbId
//! [`Table`]: crate::Table

use std::sync::Arc;

use sc_db::{DatabaseDriver, Row};
use sc_db_postgres::{PgConnectParams, PgDriver};
use sc_db_sqlite::SqliteDriver;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::catalog::Catalog;
use crate::field::DataField;
use crate::table::Table;

/// Name of the database-connections table in the primary database.
pub const DB_CONNECTIONS_TABLE: &str = "_fd_db_connections";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The connection's name — the key its tables are stamped with and the badge the
/// admin UI shows.
pub const COL_NAME: &str = "name";
/// The human-readable description column (§9).
pub const COL_DESCRIPTION: &str = "description";
/// Host name, or a Unix socket directory when it starts with `/`.
pub const COL_HOST: &str = "host";
/// TCP port.
pub const COL_PORT: &str = "port";
/// The database to connect to.
pub const COL_DATABASE: &str = "database";
/// The role to connect as.
pub const COL_USERNAME: &str = "username";
/// That role's password — a **secret**, redacted wherever this record is
/// serialised (§11.1).
pub const COL_PASSWORD: &str = "password";
/// The one schema the connection presents.
pub const COL_SCHEMA: &str = "schema";
/// Which kind of database this is — [`POSTGRES_BACKEND`] or [`SQLITE_BACKEND`].
pub const COL_BACKEND: &str = "backend";
/// For a SQLite connection: the file store the database file lives in.
pub const COL_FILE_STORE: &str = "file_store";
/// For a SQLite connection: the path of the file inside that store.
pub const COL_FILE_PATH: &str = "file_path";
/// The sparse per-connection values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";

/// A connection to another Postgres server.
pub const POSTGRES_BACKEND: &str = "postgres";
/// A connection to a SQLite file in one of the file stores.
pub const SQLITE_BACKEND: &str = "sqlite";
/// The backends a connection may name, in the order the UI offers them.
pub const BACKENDS: [&str; 2] = [POSTGRES_BACKEND, SQLITE_BACKEND];

/// The port a Postgres server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 5432;
/// The schema a connection presents unless the admin names another.
pub const DEFAULT_SCHEMA: &str = "public";

/// A connection's row identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DbConnectionId(pub Uuid);

impl DbConnectionId {
    /// Mint a fresh id for a connection that has never been stored.
    pub fn new() -> DbConnectionId {
        DbConnectionId(Uuid::new_v4())
    }
}

impl Default for DbConnectionId {
    fn default() -> Self {
        DbConnectionId::new()
    }
}

/// Another database an admin has connected: which kind, what to dial, as who,
/// and which part of it to present.
///
/// One struct with a [`backend`](DbConnectionDef::backend) rather than an enum,
/// because it is one row of one table and the admin form is one form: the fields
/// that do not apply to the chosen backend are simply empty, which is what the
/// row holds and what the screen shows. The two backends are told apart exactly
/// where it matters — validation, [`dial`], and the description in a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbConnectionDef {
    /// Row identity, stable across a rename.
    pub id: DbConnectionId,
    /// Which kind of database: [`POSTGRES_BACKEND`] or [`SQLITE_BACKEND`].
    pub backend: String,
    /// The name its tables are stamped with. Unique, for the reason a file
    /// store's is: it is what everything resolves the connection through.
    pub name: String,
    /// What this connection is for, in the admin's words. May be empty.
    pub description: String,
    /// Host name, or a Unix socket directory.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// The database to connect to.
    pub database: String,
    /// The role to connect as.
    pub username: String,
    /// That role's password; empty means "no password", which is a real state
    /// for a socket connection using peer authentication.
    pub password: String,
    /// The one schema whose tables this connection presents.
    pub schema: String,
    /// For a SQLite connection: the file store holding the database file.
    ///
    /// A store rather than a bare path, because a bare path would let an admin
    /// open any file the server process can read — the file stores are where
    /// Saltcorn's files are, with the access rules an admin already set, and a
    /// path is resolved *inside* one exactly as every other file reference is.
    pub file_store: String,
    /// For a SQLite connection: the path of the database file in that store.
    pub file_path: String,
    /// Sparse per-connection values (§9).
    pub attributes: Attrs,
}

impl DbConnectionDef {
    /// A Postgres connection with a fresh id, the default port and the `public`
    /// schema.
    pub fn new(
        name: impl Into<String>,
        host: impl Into<String>,
        database: impl Into<String>,
    ) -> Self {
        DbConnectionDef {
            id: DbConnectionId::new(),
            backend: POSTGRES_BACKEND.to_owned(),
            name: name.into(),
            description: String::new(),
            host: host.into(),
            port: DEFAULT_PORT,
            database: database.into(),
            username: String::new(),
            password: String::new(),
            schema: DEFAULT_SCHEMA.to_owned(),
            file_store: String::new(),
            file_path: String::new(),
            attributes: Attrs::new(),
        }
    }

    /// A SQLite connection to `path` inside the file store `store`.
    pub fn sqlite(
        name: impl Into<String>,
        store: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        DbConnectionDef {
            backend: SQLITE_BACKEND.to_owned(),
            host: String::new(),
            database: String::new(),
            schema: String::new(),
            file_store: store.into(),
            file_path: path.into(),
            ..DbConnectionDef::new(name, "", "")
        }
    }

    /// Whether this connection is a SQLite file rather than a Postgres server.
    pub fn is_sqlite(&self) -> bool {
        self.backend.trim() == SQLITE_BACKEND
    }

    /// The same connection as the driver's connect parameters.
    pub fn connect_params(&self) -> PgConnectParams {
        PgConnectParams {
            host: self.host.trim().to_owned(),
            port: self.port,
            database: self.database.trim().to_owned(),
            user: self.username.trim().to_owned(),
            password: self.password.clone(),
            schema: self.schema.trim().to_owned(),
        }
    }

    /// How the connection reads in a log line or an error: `user@host:port/db`,
    /// schema included, **password never** — or, for SQLite, the file and the
    /// store it is in.
    pub fn target(&self) -> String {
        if self.is_sqlite() {
            return format!(
                "the SQLite file `{}` in file store `{}`",
                self.file_path, self.file_store
            );
        }
        format!(
            "{}@{}:{}/{} (schema {})",
            self.username, self.host, self.port, self.database, self.schema
        )
    }
}

/// The fields of the `_fd_db_connections` table, in declaration order.
fn db_connection_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let int = || TypeRef::Basic(BasicType::Int);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_HOST, text()).required(),
        DataField::plain(COL_PORT, int()).required(),
        DataField::plain(COL_DATABASE, text()).required(),
        DataField::plain(COL_USERNAME, text()).required(),
        // Not required: "no password" is a real connection, and an empty string
        // would be indistinguishable from one that was never set if this were
        // NOT NULL with a default.
        DataField::plain(COL_PASSWORD, text()),
        DataField::plain(COL_SCHEMA, text()).required(),
        DataField::plain(COL_BACKEND, text()).required(),
        // Empty for a Postgres connection, so neither is required — the row
        // holds what the chosen backend uses and nothing more.
        DataField::plain(COL_FILE_STORE, text()),
        DataField::plain(COL_FILE_PATH, text()),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure the `_fd_db_connections` table exists, creating it if absent, and
/// return it. Idempotent; call once at startup, before connecting stored
/// connections.
pub async fn bootstrap_db_connections(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(DB_CONNECTIONS_TABLE, &db_connection_fields())
        .await
}

/// Everything [`save_db_connection`] checks before it writes.
///
/// The name is checked against the **primary** database's id as well as against
/// the other rows: `primary` is what every table of the primary database is
/// stamped with, and a connection claiming it would make a table's origin
/// ambiguous in the one direction the routing depends on.
pub async fn check_db_connection_saveable(catalog: &Catalog, def: &DbConnectionDef) -> Result<()> {
    let name = def.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("a database connection needs a name"));
    }
    if name == crate::DbId::primary().0 {
        return Err(Error::invalid(format!(
            "`{name}` is the name of the primary database and cannot name a connection"
        )));
    }
    if !BACKENDS.contains(&def.backend.trim()) {
        return Err(Error::invalid(format!(
            "database connection `{name}` names the backend `{}`; it is one of: {}",
            def.backend,
            BACKENDS.join(", ")
        )));
    }
    if def.is_sqlite() {
        // A SQLite database is a file, so what it needs is where the file is —
        // and the store has to be one that exists, or the connection could only
        // ever fail with a message about a store rather than about a database.
        if def.file_store.trim().is_empty() {
            return Err(Error::invalid(format!(
                "database connection `{name}` needs the file store its SQLite file is in"
            )));
        }
        if def.file_path.trim().is_empty() {
            return Err(Error::invalid(format!(
                "database connection `{name}` needs the path of its SQLite file"
            )));
        }
        if crate::load_file_store_by_name(catalog, def.file_store.trim())
            .await?
            .is_none()
        {
            return Err(Error::invalid(format!(
                "database connection `{name}` names the file store `{}`, which does not exist",
                def.file_store.trim()
            )));
        }
    } else {
        if def.host.trim().is_empty() {
            return Err(Error::invalid(format!(
                "database connection `{name}` needs a host"
            )));
        }
        if def.database.trim().is_empty() {
            return Err(Error::invalid(format!(
                "database connection `{name}` needs a database name"
            )));
        }
        if def.username.trim().is_empty() {
            return Err(Error::invalid(format!(
                "database connection `{name}` needs a user to connect as"
            )));
        }
        if def.schema.trim().is_empty() {
            return Err(Error::invalid(format!(
                "database connection `{name}` needs a schema"
            )));
        }
    }

    if let Some(other) = load_db_connection_by_name(catalog, name).await?
        && other.id != def.id
    {
        return Err(Error::invalid(format!(
            "database connection name `{name}` is already used; \
             each connection is stamped onto its tables under its own name"
        )));
    }
    Ok(())
}

/// Save a connection: insert its row, or update it in place if a row with its
/// [`DbConnectionId`] already exists.
///
/// Saving does **not** connect, and deliberately does not require that it could
/// be connected — a connection whose host is down must stay editable, since
/// editing it is how the admin fixes it. Reachability is
/// [`connect_db_connection`]'s question.
pub async fn save_db_connection(catalog: &Catalog, def: &DbConnectionDef) -> Result<()> {
    check_db_connection_saveable(catalog, def).await?;

    let columns = connection_columns();
    let values = connection_values(def);

    if load_db_connection(catalog, def.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            // The id is the row's identity, not something to reassign.
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = sc_query::Update::new(DB_CONNECTIONS_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(def.id.0)));
        run(catalog, Statement::from(update)).await?;
    } else {
        let insert = Insert::row(
            DB_CONNECTIONS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await?;
    }
    Ok(())
}

/// Load the connection with this id, if it exists.
pub async fn load_db_connection(
    catalog: &Catalog,
    id: DbConnectionId,
) -> Result<Option<DbConnectionDef>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the connection named `name`, if any.
pub async fn load_db_connection_by_name(
    catalog: &Catalog,
    name: &str,
) -> Result<Option<DbConnectionDef>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// Every stored connection, ordered by name — what the server connects at boot
/// and what the admin UI lists.
pub async fn list_db_connections(catalog: &Catalog) -> Result<Vec<DbConnectionDef>> {
    let select = Select::from(Source::table(DB_CONNECTIONS_TABLE));
    let mut defs: Vec<DbConnectionDef> = rows(catalog, select)
        .await?
        .iter()
        .map(connection_from_row)
        .collect::<Result<_>>()?;
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(defs)
}

/// Delete a connection's row, returning whether one was there to delete.
///
/// **This removes the row and nothing else.** Not one byte of the foreign
/// database is touched — disconnecting is not consent to destroy, for the same
/// reason deleting a file store leaves its directory alone.
///
/// Disconnecting the live driver and reloading the catalog so its tables stop
/// appearing is the caller's job; this only removes the definition.
pub async fn delete_db_connection(catalog: &Catalog, id: DbConnectionId) -> Result<bool> {
    let Some(def) = load_db_connection(catalog, id).await? else {
        return Ok(false);
    };
    let delete =
        Delete::from(DB_CONNECTIONS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(def.id.0)));
    run(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// The outcome of connecting the stored databases (see
/// [`connect_all_db_connections`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbConnections {
    /// Names of the connections that came up.
    pub connected: Vec<String>,
    /// Connections that did not, as `(name, reason)`.
    pub failed: Vec<(String, String)>,
}

impl DbConnections {
    /// Whether every stored connection came up.
    pub fn all_connected(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Connect one stored definition into the catalog's registry, recording the
/// reason on failure so the admin UI can show a defined-but-unusable connection.
///
/// **Reachability is proved, not assumed.** Building a pool is a local
/// operation that succeeds against a host that does not exist, so this
/// introspects once before registering: a connection reported as connected has
/// answered a query. Without that, an admin would type a typo, be told it
/// worked, and find an empty table list with nothing saying why.
///
/// The catalog is **not** reloaded here — the caller does that once, after
/// connecting however many it is connecting.
pub async fn connect_db_connection(catalog: &Catalog, def: &DbConnectionDef) -> Result<()> {
    match dial(catalog, def).await {
        Ok(driver) => catalog.connect_database(def.name.trim(), driver),
        Err(e) => {
            // The whole causal chain: "connecting database `reporting`" alone
            // tells an admin that it failed but not that the host is unknown.
            // This message is shown in the UI, so it has to be actionable.
            catalog.record_database_error(def.name.trim(), sc_error::format_causes(&e))?;
            Err(e)
        }
    }
}

/// Build a driver for `def` and prove it answers.
///
/// Separate from [`connect_db_connection`] because a *test* button wants exactly
/// this and nothing else: no registry entry, no recorded error, no effect on the
/// catalog at all.
///
/// The catalog is needed — even for a test that touches nothing — because a
/// SQLite connection is a path *inside a file store*, and the stores are the
/// catalog's.
pub async fn dial(catalog: &Catalog, def: &DbConnectionDef) -> Result<Arc<dyn DatabaseDriver>> {
    let driver: Arc<dyn DatabaseDriver> = if def.is_sqlite() {
        Arc::new(SqliteDriver::open_existing(sqlite_path(catalog, def)?)?)
    } else {
        Arc::new(PgDriver::connect_params(&def.connect_params())?)
    };
    driver
        .introspect()
        .await
        .map_err(|e| Error::database(format!("connecting to {}: {e}", def.target())))?;
    // Whether it can hold geometry (analytics TODO A5.1): asked, never
    // installed, since the database is someone else's.
    let _ = driver.detect_spatial().await;
    Ok(driver)
}

/// Where a SQLite connection's file actually is on disk.
///
/// Two things have to be true, and each has its own message because each has its
/// own repair: the store must be **connected** (a store that is only defined has
/// no path to resolve against), and it must be a store with a local path at all
/// — an object store has none, and SQLite cannot open a database over an API. A
/// path that would escape the store's root is refused by the store itself, as it
/// is for every other file.
fn sqlite_path(catalog: &Catalog, def: &DbConnectionDef) -> Result<std::path::PathBuf> {
    let store_name = def.file_store.trim();
    let store = catalog.file_store(store_name)?.ok_or_else(|| {
        Error::invalid(format!(
            "database connection `{}` is in file store `{store_name}`, which is not connected",
            def.name
        ))
    })?;
    store
        .local_path(def.file_path.trim())?
        .ok_or_else(|| {
            Error::invalid(format!(
                "file store `{store_name}` has no local files, so the SQLite database                  `{}` cannot be opened from it — SQLite reads a file, not an API",
                def.file_path
            ))
        })
}

/// Load every stored connection and connect each one, returning what happened.
///
/// **One connection that fails must not stop the others, and must not stop the
/// server** — the same rule `connect_all_file_stores` follows, for the same
/// reason: a database that is down is a thing for the admin to fix in the UI,
/// not a reason for a server that otherwise works to refuse to boot.
///
/// The catalog is reloaded once at the end when anything connected, so the
/// foreign tables are in the cache by the time this returns.
pub async fn connect_all_db_connections(catalog: &Catalog) -> Result<DbConnections> {
    let mut report = DbConnections::default();
    for def in list_db_connections(catalog).await? {
        match connect_db_connection(catalog, &def).await {
            Ok(()) => report.connected.push(def.name),
            Err(e) => report.failed.push((def.name, sc_error::format_causes(&e))),
        }
    }
    if !report.connected.is_empty() {
        catalog.reload().await?;
    }
    Ok(report)
}

/// The row's columns, in the order [`connection_values`] produces them.
fn connection_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_HOST,
        COL_PORT,
        COL_DATABASE,
        COL_USERNAME,
        COL_PASSWORD,
        COL_SCHEMA,
        COL_BACKEND,
        COL_FILE_STORE,
        COL_FILE_PATH,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The definition serialised to its row's values, in [`connection_columns`]
/// order.
fn connection_values(def: &DbConnectionDef) -> Vec<Value> {
    vec![
        Value::Uuid(def.id.0),
        Value::Text(def.name.trim().to_owned()),
        Value::Text(def.description.clone()),
        Value::Text(def.host.trim().to_owned()),
        Value::Int(i64::from(def.port)),
        Value::Text(def.database.trim().to_owned()),
        Value::Text(def.username.trim().to_owned()),
        Value::Text(def.password.clone()),
        Value::Text(def.schema.trim().to_owned()),
        Value::Text(def.backend.trim().to_owned()),
        Value::Text(def.file_store.trim().to_owned()),
        Value::Text(def.file_path.trim().to_owned()),
        Value::Json(Json::Object(def.attributes.clone())),
    ]
}

/// Rebuild a [`DbConnectionDef`] from its `_fd_db_connections` row.
fn connection_from_row(row: &Row) -> Result<DbConnectionDef> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => DbConnectionId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };

    // A port outside the TCP range is a corrupt row, not something to clamp:
    // silently dialling a different port is worse than saying the row is wrong.
    let port = match row.get(COL_PORT) {
        Some(Value::Int(i)) => u16::try_from(*i).map_err(|_| {
            Error::invalid(format!(
                "{DB_CONNECTIONS_TABLE}.{COL_PORT} should be a TCP port, got {i}"
            ))
        })?,
        other => return Err(bad_column(COL_PORT, "an integer port", other)),
    };

    Ok(DbConnectionDef {
        id,
        backend: text(row, COL_BACKEND)?,
        name: text(row, COL_NAME)?,
        description: optional_text(row, COL_DESCRIPTION)?,
        host: text(row, COL_HOST)?,
        port,
        database: text(row, COL_DATABASE)?,
        username: text(row, COL_USERNAME)?,
        password: optional_text(row, COL_PASSWORD)?,
        schema: text(row, COL_SCHEMA)?,
        file_store: optional_text(row, COL_FILE_STORE)?,
        file_path: optional_text(row, COL_FILE_PATH)?,
        attributes: object(row, COL_ATTRIBUTES)?,
    })
}

/// A required text column.
fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A text column where NULL means "none given" rather than a broken row.
fn optional_text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        Some(Value::Null) | None => Ok(String::new()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A JSON column that must hold an object.
fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{DB_CONNECTIONS_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{DB_CONNECTIONS_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run a select and collect its rows.
async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

/// Load the single definition matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<DbConnectionDef>> {
    let select = Select::from(Source::table(DB_CONNECTIONS_TABLE))
        .filter(filter)
        .limit(1);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(connection_from_row(row)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = db_connection_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required && by_name(COL_NAME).unique);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
        assert!(!by_name(COL_DESCRIPTION).required);
        // "No password" is a real connection, so the column cannot be NOT NULL.
        assert!(!by_name(COL_PASSWORD).required);
    }

    #[test]
    fn the_table_is_a_hidden_system_table() {
        assert!(DB_CONNECTIONS_TABLE.starts_with("_fd_"));
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        // The insert pairs these two positionally, so a column added to one and
        // not the other would write the password into the schema column.
        let def = DbConnectionDef::new("reporting", "db.example.com", "analytics");
        assert_eq!(connection_columns().len(), connection_values(&def).len());
        assert_eq!(connection_columns().len(), db_connection_fields().len());
    }

    #[test]
    fn a_new_connection_defaults_to_the_postgres_port_and_public_schema() {
        let def = DbConnectionDef::new("reporting", "db.example.com", "analytics");
        assert_eq!(def.port, DEFAULT_PORT);
        assert_eq!(def.schema, DEFAULT_SCHEMA);
    }

    #[test]
    fn the_target_string_never_carries_the_password() {
        let mut def = DbConnectionDef::new("reporting", "db.example.com", "analytics");
        def.username = "reader".into();
        def.password = "hunter2".into();
        let target = def.target();
        assert!(target.contains("reader@db.example.com:5432/analytics"));
        assert!(target.contains("schema public"));
        assert!(!target.contains("hunter2"));
    }
}

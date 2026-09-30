//! Connections to one SQLite database, and the small pool that hands them out.
//!
//! SQLite has no server and no wire protocol, so "connecting" is opening a file.
//! There is still a pool, for two reasons that have nothing to do with the cost
//! of connecting:
//!
//! - **A transaction owns a connection.** `BEGIN` is a property of a connection,
//!   so a transaction that shared one with the queries running beside it would
//!   pull them into itself. Each transaction therefore takes a connection of its
//!   own and gives it back at commit.
//! - **Readers do not have to queue behind a writer.** In WAL mode SQLite allows
//!   any number of concurrent readers alongside one writer, and that concurrency
//!   is between *connections*: one connection under a mutex would serialise
//!   reads that the database was perfectly happy to run at once.
//!
//! Every connection is opened the same way ([`open`]), so a connection taken
//! from the pool is never subtly different from a fresh one — the pragmas that
//! matter (foreign keys, the busy timeout, WAL) are per-connection settings, and
//! a connection that missed one would enforce a different set of rules than its
//! neighbour.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags};
use sc_error::{Error, Result};

/// How long a connection waits for a writer to finish before giving up.
///
/// SQLite's default is zero — an immediate `SQLITE_BUSY` — which under any
/// concurrency at all turns "another request is writing" into an error the user
/// sees. Five seconds is long enough to cover a normal write and short enough
/// that a genuine deadlock is still reported rather than hung on.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// How many idle connections are kept for re-use. Beyond this they are closed
/// when returned: a burst of parallel requests should not leave the process
/// holding a file handle for each of them forever.
const MAX_IDLE: usize = 8;

/// The database a pool opens connections to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteSource {
    /// A file on disk. Created when it is not there, if the pool was built to
    /// create it.
    File(PathBuf),
    /// A private in-memory database, shared between this pool's connections by
    /// the URI that names it. For tests and for a scratch database; nothing is
    /// written anywhere and the database ends with the pool.
    Memory(String),
}

impl SqliteSource {
    /// A fresh, uniquely named in-memory database.
    ///
    /// Named, and `cache=shared`, because a pool opens more than one connection
    /// and every plain `:memory:` connection is a *separate empty database* —
    /// so an unnamed one would give each connection its own, and a table created
    /// on one would not exist on the next.
    pub fn memory() -> SqliteSource {
        SqliteSource::Memory(format!(
            "file:sc-sqlite-{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        ))
    }

    /// How this source reads in a log line or an error.
    pub fn describe(&self) -> String {
        match self {
            SqliteSource::File(path) => path.display().to_string(),
            SqliteSource::Memory(_) => "an in-memory database".to_owned(),
        }
    }

    /// The string SQLite is asked to open.
    fn target(&self) -> String {
        match self {
            SqliteSource::File(path) => path.display().to_string(),
            SqliteSource::Memory(uri) => uri.clone(),
        }
    }
}

/// A pool of connections to one SQLite database.
#[derive(Debug)]
pub(crate) struct SqlitePool {
    source: SqliteSource,
    /// Whether a missing file is created rather than refused.
    create: bool,
    idle: Mutex<Vec<Connection>>,
    /// One connection held open for the lifetime of the pool, for an in-memory
    /// database only: a shared-cache memory database exists only while some
    /// connection to it is open, so without this an idle moment would delete it.
    _keeper: Mutex<Option<Connection>>,
}

impl SqlitePool {
    /// Build a pool, opening one connection immediately so that a database that
    /// cannot be opened is reported now rather than at the first query.
    pub(crate) fn new(source: SqliteSource, create: bool) -> Result<SqlitePool> {
        let first = open(&source, create)?;
        let keeper = match source {
            SqliteSource::Memory(_) => Some(open(&source, create)?),
            SqliteSource::File(_) => None,
        };
        Ok(SqlitePool {
            source,
            create,
            idle: Mutex::new(vec![first]),
            _keeper: Mutex::new(keeper),
        })
    }

    /// The database this pool is for.
    pub(crate) fn source(&self) -> &SqliteSource {
        &self.source
    }

    /// Check out a connection, opening a new one if none is idle.
    pub(crate) fn get(self: &std::sync::Arc<Self>) -> Result<PooledConnection> {
        let existing = self
            .idle
            .lock()
            .map_err(|_| Error::database("sqlite connection pool is poisoned"))?
            .pop();
        let connection = match existing {
            Some(connection) => connection,
            None => open(&self.source, self.create)?,
        };
        Ok(PooledConnection {
            connection: Some(connection),
            pool: self.clone(),
        })
    }

    /// Take a connection back, keeping it for re-use unless the pool is full.
    fn put(&self, connection: Connection) {
        if let Ok(mut idle) = self.idle.lock()
            && idle.len() < MAX_IDLE
        {
            idle.push(connection);
        }
    }
}

/// A connection borrowed from a pool, returned when it is dropped.
#[derive(Debug)]
pub(crate) struct PooledConnection {
    connection: Option<Connection>,
    pool: std::sync::Arc<SqlitePool>,
}

impl PooledConnection {
    /// The connection itself.
    ///
    /// A `Result` rather than a panic on the impossible case: the `Option` is
    /// only `None` while the value is being dropped, and a driver has no
    /// business bringing the process down over its own bookkeeping.
    pub(crate) fn connection(&self) -> Result<&Connection> {
        self.connection
            .as_ref()
            .ok_or_else(|| Error::database("this pooled connection has already been returned"))
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.pool.put(connection);
        }
    }
}

/// Open one connection, with the settings every connection to this database
/// must share.
fn open(source: &SqliteSource, create: bool) -> Result<Connection> {
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_URI
        // Each connection is used by one thread at a time (the blocking task
        // holding it), so SQLite's own per-connection mutex is not needed.
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let target = source.target();
    let connection = Connection::open_with_flags(&target, flags).map_err(|e| {
        Error::database(format!(
            "could not open the SQLite database {}: {e}",
            source.describe()
        ))
    })?;

    connection
        .busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))
        .map_err(|e| Error::database(format!("setting the SQLite busy timeout: {e}")))?;
    // Foreign keys are **off** by default in SQLite, for backwards
    // compatibility. Saltcorn's tables declare them and every other backend
    // enforces them, so a key that was silently unenforced here would be a
    // difference nobody asked for and nobody would notice until the data was
    // wrong.
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| Error::database(format!("enabling SQLite foreign keys: {e}")))?;
    crate::functions::register(&connection)
        .map_err(|e| Error::database(format!("registering SQL functions: {e}")))?;
    if matches!(source, SqliteSource::File(_)) {
        // Write-ahead logging: readers do not block the writer and the writer
        // does not block readers, which is the difference between a server that
        // serves during a write and one that does not. It is a property of the
        // *database file*, so setting it on every connection is idempotent.
        let _mode: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|e| Error::database(format!("setting SQLite journal mode: {e}")))?;
    }
    Ok(connection)
}

/// Whether a path names a file SQLite can open — used to refuse a connection to
/// something that is not there before it becomes an empty database.
pub(crate) fn file_exists(path: &Path) -> bool {
    path.is_file()
}

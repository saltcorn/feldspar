//! Shared integration-test harness (principle 4: integration-tested against a
//! real database).
//!
//! Every test gets its **own freshly created Postgres database**, so tests are
//! fully isolated — including schema/DDL and `information_schema` introspection,
//! which the catalog relies on — and may run in parallel. The database is
//! dropped when the [`TestDb`] handle is dropped.
//!
//! **Where the base connection comes from**, in order of authority:
//!
//! 1. The `DATABASE_URL` environment variable (the one CI sets).
//! 2. The `test` environment of `feldspar.toml` — the same file `feldspar serve`
//!    reads, found on the same search paths (see `sc_config_file`). A developer
//!    whose Postgres is not the one CI runs writes it down once, there, and
//!    `cargo test` needs no environment at all.
//! 3. A conventional local default matching CI's Postgres service.
//!
//! The template database each per-test database is cloned from resolves the same
//! way: `SC_TEST_TEMPLATE` first, then the `test` environment's `test_template`,
//! then the server default (`template1`). Set one on a box whose `template1`
//! carries a stale glibc collation version, which makes a bare `CREATE DATABASE`
//! fail; it must name an empty database, since every per-test database inherits
//! whatever is in it.
//!
//! `SC_TEST_ENVIRONMENT` names a section other than `test`, and `FELDSPAR_CONFIG`
//! names the file outright. Naming a section the file does not define is an error
//! rather than a silent fall-through to the default connection (principle 5) — as
//! is a file that does not parse, or a `FELDSPAR_CONFIG` path that does not exist.
//! No file at all is not a misconfiguration, and is quiet.
//!
//! `FELDSPAR_ENV`, which selects the *server's* environment, is deliberately not
//! read here: a shell that exports it to serve production must not thereby point
//! a test run at production.
//!
//! The named database in the resolved connection is only used as the
//! *maintenance* connection from which per-test databases are created and
//! dropped — the tests themselves never touch it.
//!
//! ```no_run
//! # async fn ex() -> sc_error::Result<()> {
//! let db = sc_test_harness::TestDb::new().await?;
//! let client = db.client().await?;
//! client.batch_execute("create table t (id int)").await.unwrap();
//! // ... db is dropped (and the database deleted) at end of scope ...
//! # Ok(()) }
//! ```
//!
//! It also carries [`TestSmtp`], a local SMTP server that accepts one message,
//! for the same reason: a test of the mail transport that never puts bytes on a
//! socket is a test of a mock.

pub mod smtp;

pub use smtp::{SmtpMessage, TestSmtp};

use std::path::PathBuf;
use std::sync::OnceLock;

use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod};
use sc_config_file::{ConfigFile, Environment, FILE_NAME};
use sc_error::{Error, Result};
use tokio_postgres::config::Host;
use tokio_postgres::{Client, Config, NoTls};
use uuid::Uuid;

/// Fallback used when neither `DATABASE_URL` nor the configuration file says
/// where Postgres is. Matches the Postgres service wired up in CI so a bare
/// `cargo test` works there without extra config.
const DEFAULT_URL: &str = "postgres://saltcorn:saltcorn@localhost:5432/saltcorn_test";

/// Environment variable naming the template database per-test databases are
/// cloned from. Overrides the configuration file's `test_template`.
pub const TEMPLATE_VAR: &str = "SC_TEST_TEMPLATE";

/// Environment variable naming the template a **PostGIS** test database is
/// cloned from (see [`TestDb::with_postgis`]). Defaults to
/// [`DEFAULT_POSTGIS_TEMPLATE`].
pub const POSTGIS_TEMPLATE_VAR: &str = "SC_TEST_POSTGIS_TEMPLATE";

/// The template a PostGIS test database is cloned from when
/// [`POSTGIS_TEMPLATE_VAR`] is unset. Made once, by a superuser, because
/// installing PostGIS needs one (`OPERATIONS.md` §10):
///
/// ```text
/// sudo -u postgres createdb -O <test role> feldspar_postgis_template
/// sudo -u postgres psql -d feldspar_postgis_template -c 'CREATE EXTENSION postgis'
/// ```
pub const DEFAULT_POSTGIS_TEMPLATE: &str = "feldspar_postgis_template";

/// Environment variable naming which `feldspar.toml` section the harness reads.
/// Rarely needed; it exists so a machine with two test databases can point one
/// test run at each.
pub const TEST_ENVIRONMENT_VAR: &str = "SC_TEST_ENVIRONMENT";

/// The `feldspar.toml` section read when [`TEST_ENVIRONMENT_VAR`] is unset.
///
/// Deliberately *not* the file's `default_environment`: that is production on a
/// deployed box, and a test run must never fall into it by default.
pub const DEFAULT_TEST_ENVIRONMENT: &str = "test";

/// Host used when the configuration file's section names none.
const DEFAULT_HOST: &str = "localhost";
/// Port used when the configuration file's section names none.
const DEFAULT_PORT: u16 = 5432;

/// The maintenance database used to create and drop per-test databases. Present
/// on every standard Postgres install.
const MAINTENANCE_DB: &str = "postgres";

/// A handle to a freshly created, isolated Postgres database for one test.
///
/// Dropping the handle deletes the database (best effort). Prefer letting it
/// drop at the end of the test; the underlying connections are closed first so
/// the `DROP DATABASE` succeeds.
pub struct TestDb {
    /// Name of the per-test database (a unique, identifier-safe string).
    name: String,
    /// Pool of connections to the per-test database, handed to the test.
    pool: Pool,
    /// Connection config pointing at the maintenance database, used to drop the
    /// per-test database on teardown.
    admin_config: Config,
    /// The resolved base connection, kept so [`url`](TestDb::url) can render a
    /// connection string for the code paths that take one.
    base: Config,
}

impl TestDb {
    /// Create a new, empty Postgres database and return a handle with a pool
    /// connected to it.
    pub async fn new() -> Result<TestDb> {
        let section = test_section()?;
        TestDb::create(template(env_var(TEMPLATE_VAR), section.as_ref())).await
    }

    /// A new database with **PostGIS** installed, or `None` — with a line on
    /// stderr saying what to do — on a machine that has no PostGIS template.
    ///
    /// Installing PostGIS needs a superuser, which the test role is not
    /// expected to be, so the database is cloned from a template that already
    /// has it ([`DEFAULT_POSTGIS_TEMPLATE`], or [`POSTGIS_TEMPLATE_VAR`]). A test
    /// that needs PostGIS returns early on `None`: it skips with the message
    /// rather than failing on a machine without it (analytics TODO, "Both
    /// databases").
    pub async fn with_postgis() -> Result<Option<TestDb>> {
        let template =
            env_var(POSTGIS_TEMPLATE_VAR).unwrap_or_else(|| DEFAULT_POSTGIS_TEMPLATE.to_owned());
        let base = base_config()?;
        let admin = connect(&with_dbname(&base, MAINTENANCE_DB)).await?;
        let found = admin
            .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&template])
            .await
            .map_err(|e| Error::database(format!("looking for {template}: {e}")))?;
        if found.is_empty() {
            eprintln!(
                "skipped: this test needs PostGIS, and there is no `{template}` database to \
                 clone one from (see OPERATIONS.md §10, \"Geometry with PostGIS\")"
            );
            return Ok(None);
        }
        let db = TestDb::create(Some(template.clone())).await?;
        let has_postgis = db
            .client()
            .await?
            .query("SELECT 1 FROM pg_extension WHERE extname = 'postgis'", &[])
            .await
            .map_err(|e| Error::database(format!("looking for PostGIS: {e}")))?;
        if has_postgis.is_empty() {
            eprintln!(
                "skipped: this test needs PostGIS, and `{template}` does not have it; run \
                 CREATE EXTENSION postgis in it as a superuser"
            );
            return Ok(None);
        }
        Ok(Some(db))
    }

    /// Create a database cloned from `template` (the server's default when
    /// `None`).
    async fn create(template: Option<String>) -> Result<TestDb> {
        let base = base_config()?;

        // Unique, identifier-safe name: `simple` renders 32 hex chars, so with
        // the `sc_test_` prefix the whole thing is a valid unquoted identifier.
        let name = format!("sc_test_{}", Uuid::new_v4().simple());

        let admin_config = with_dbname(&base, MAINTENANCE_DB);
        let admin = connect(&admin_config).await?;
        // Identifiers cannot be parameterised; `name` is generated by us from a
        // UUID (no attacker input), so interpolation here is safe. Quoted
        // defensively all the same.
        //
        // The template database, if this machine names one — see `template`.
        let create = match template {
            Some(t) => format!("CREATE DATABASE \"{name}\" TEMPLATE \"{t}\""),
            None => format!("CREATE DATABASE \"{name}\""),
        };
        admin
            .batch_execute(&create)
            .await
            .map_err(|e| Error::database(format!("create database {name}: {e}")))?;

        let pool = build_pool(&with_dbname(&base, &name))?;

        Ok(TestDb {
            name,
            pool,
            admin_config,
            base,
        })
    }

    /// The name of the per-test database.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The connection pool for the per-test database.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// A connection **URL** for the per-test database.
    ///
    /// The same connection the pool uses, rendered as a string for the code
    /// paths that take one — `feldspar`'s `--database-url`, a `url =` line in a
    /// `feldspar.toml` fixture. It is built from the resolved base connection,
    /// so a test that hands it out works whether that came from `DATABASE_URL`,
    /// from the configuration file's test environment or from the default. A
    /// Unix-socket connection comes back as `?host=/the/socket/dir`, which is
    /// how a connection string spells one.
    pub fn url(&self) -> String {
        connection_url(&self.base, &self.name)
    }

    /// The connection as **parts** rather than a URL: host, port, user,
    /// password, database.
    ///
    /// For the code paths that take the parts because an admin typed them into
    /// six boxes — a database *connection* (`_fd_db_connections`), which is
    /// stored as columns precisely so the password can be a column that redacts
    /// itself. A test of that path cannot use [`url`](TestDb::url) without
    /// re-parsing it, and re-parsing a URL the harness just rendered is a test of
    /// the harness.
    ///
    /// A Unix-socket connection reports the socket directory as the host, which
    /// is how libpq spells one and what the driver accepts.
    pub fn parts(&self) -> ConnectionParts {
        let host = match self.base.get_hosts().first() {
            Some(Host::Tcp(host)) => host.clone(),
            #[cfg(unix)]
            Some(Host::Unix(path)) => path.display().to_string(),
            _ => DEFAULT_HOST.to_owned(),
        };
        ConnectionParts {
            host,
            port: self.base.get_ports().first().copied().unwrap_or(5432),
            user: self.base.get_user().unwrap_or("postgres").to_owned(),
            password: self
                .base
                .get_password()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_default(),
            database: self.name.clone(),
        }
    }

    /// Check out a pooled connection to the per-test database.
    pub async fn client(&self) -> Result<Object> {
        self.pool
            .get()
            .await
            .map_err(|e| Error::database(format!("checkout connection: {e}")))
    }
}

/// A connection stated as its parts — what [`TestDb::parts`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionParts {
    /// Host name, or a Unix socket directory.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// The role to connect as.
    pub user: String,
    /// That role's password; empty when the connection needs none.
    pub password: String,
    /// The per-test database's name.
    pub database: String,
}

impl Drop for TestDb {
    fn drop(&mut self) {
        // Close pooled connections so nothing blocks `DROP DATABASE`.
        self.pool.close();

        let name = self.name.clone();
        let admin_config = self.admin_config.clone();

        // Drop is synchronous and we may be inside a tokio runtime, so we cannot
        // block on the current runtime. Run the async teardown on a dedicated
        // thread with its own single-threaded runtime.
        let _ = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            runtime.block_on(async move {
                if let Ok(admin) = connect(&admin_config).await {
                    // `WITH (FORCE)` (PG13+) terminates any lingering backends.
                    let _ = admin
                        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                        .await;
                }
            });
        })
        .join();
    }
}

/// The base connection: `DATABASE_URL`, else the configuration file's test
/// environment, else [`DEFAULT_URL`]. See the module documentation.
fn base_config() -> Result<Config> {
    if let Some(url) = env_var("DATABASE_URL") {
        return url
            .parse::<Config>()
            .map_err(|e| Error::config(format!("invalid DATABASE_URL: {e}")));
    }
    let section = test_section()?;
    if let Some(config) = section.as_ref().and_then(section_config) {
        return config;
    }
    DEFAULT_URL
        .parse::<Config>()
        .map_err(|e| Error::config(format!("invalid default connection URL: {e}")))
}

/// The template database per-test databases are cloned from: `SC_TEST_TEMPLATE`
/// (passed in as `from_env`) first, then the configuration file's
/// `test_template`, then none — which leaves Postgres to use `template1`.
///
/// Both inputs are arguments rather than things this function reads, so the
/// precedence rule is testable without setting a process-wide variable.
fn template(from_env: Option<String>, section: Option<&Environment>) -> Option<String> {
    from_env.or_else(|| {
        section
            .and_then(|s| s.test_template.clone())
            .filter(|t| !t.is_empty())
    })
}

/// The connection a configuration-file section describes: its `url` wholesale if
/// it has one, else its parts. `None` when the section says nothing at all, so
/// an empty `[environments.test]` placeholder falls through to the default
/// rather than quietly becoming localhost.
fn section_config(section: &Environment) -> Option<Result<Config>> {
    if section.is_empty() {
        return None;
    }
    if let Some(url) = &section.url {
        return Some(url.parse::<Config>().map_err(|e| {
            Error::config(format!(
                "invalid `url` in the test environment of {}: {e}",
                FILE_NAME
            ))
        }));
    }
    let mut config = Config::new();
    // A host beginning with `/` is a Unix socket directory to tokio-postgres,
    // which is how a peer-authenticated local Postgres is reached.
    config.host(
        section
            .host
            .clone()
            .unwrap_or_else(|| DEFAULT_HOST.to_owned()),
    );
    config.port(section.port.unwrap_or(DEFAULT_PORT));
    if let Some(user) = &section.user {
        config.user(user);
    }
    if let Some(password) = &section.password {
        config.password(password);
    }
    if let Some(database) = &section.database {
        config.dbname(database);
    }
    Some(Ok(config))
}

/// The configuration file's test environment, read once per test binary.
///
/// `Ok(None)` covers the three cases that are not misconfigurations: no file on
/// any search path, a file defining no environments, and a file that simply has
/// no `test` section. A file that does not parse, or a *named*
/// [`TEST_ENVIRONMENT_VAR`] section that is not defined, is an error — the run
/// would otherwise carry on against a database nobody chose.
fn test_section() -> Result<Option<Environment>> {
    static SECTION: OnceLock<std::result::Result<Option<Environment>, String>> = OnceLock::new();
    SECTION
        .get_or_init(|| load_test_section().map_err(|e| e.to_string()))
        .clone()
        .map_err(Error::config)
}

/// The uncached half of [`test_section`].
fn load_test_section() -> Result<Option<Environment>> {
    let name = env_var(TEST_ENVIRONMENT_VAR);
    let named = name.is_some();
    let name = name.unwrap_or_else(|| DEFAULT_TEST_ENVIRONMENT.to_owned());

    // A path given outright must exist; a searched one need not — same rule the
    // CLI's `--config` follows.
    let (path, required) = match env_var(sc_config_file::CONFIG_PATH_VAR) {
        Some(p) => (PathBuf::from(p), true),
        None => match sc_config_file::locate() {
            Some(p) => (p, false),
            None if named => {
                return Err(Error::config(format!(
                    "{TEST_ENVIRONMENT_VAR}={name} was set, but no {FILE_NAME} was found"
                )));
            }
            None => return Ok(None),
        },
    };

    let Some(file) = ConfigFile::load(&path)? else {
        return if required {
            Err(Error::config(format!(
                "{} names the configuration file `{}`, which does not exist",
                sc_config_file::CONFIG_PATH_VAR,
                path.display()
            )))
        } else if named {
            Err(Error::config(format!(
                "{TEST_ENVIRONMENT_VAR}={name} was set, but the configuration file `{}` \
                 does not exist",
                path.display()
            )))
        } else {
            Ok(None)
        };
    };

    match file.environments.get(&name) {
        Some(section) => Ok(Some(section.clone())),
        // `environment` renders the "no such section, here are the ones there
        // are" message; it errors here by construction, the section being absent.
        None if named => file.environment(&name, &path).map(|s| Some(s.clone())),
        None => Ok(None),
    }
}

/// Render `base` as a connection string pointing at the database `dbname`.
///
/// A Unix-socket connection is spelled the way libpq spells one — an empty host
/// in the authority and `?host=/the/socket/dir` — because that is the form that
/// parses back to exactly one host. Writing the socket path in a `host=`
/// parameter *beside* a `localhost` authority parses as two hosts and connects
/// over whichever answers first.
fn connection_url(base: &Config, dbname: &str) -> String {
    let socket = match base.get_hosts().first() {
        #[cfg(unix)]
        Some(Host::Unix(path)) => Some(path.display().to_string()),
        _ => None,
    };
    let host = match base.get_hosts().first() {
        Some(Host::Tcp(host)) => host.clone(),
        _ if socket.is_some() => String::new(),
        _ => DEFAULT_HOST.to_owned(),
    };
    let port = base.get_ports().first().copied();

    let mut url = String::from("postgres://");
    if let Some(user) = base.get_user() {
        url.push_str(&encode(user));
        if let Some(password) = base.get_password() {
            url.push(':');
            url.push_str(&encode(&String::from_utf8_lossy(password)));
        }
        url.push('@');
    }
    url.push_str(&host);
    if socket.is_none()
        && let Some(port) = port
    {
        url.push_str(&format!(":{port}"));
    }
    url.push('/');
    url.push_str(dbname);
    if let Some(socket) = socket {
        url.push_str("?host=");
        url.push_str(&socket);
        if let Some(port) = port {
            url.push_str(&format!("&port={port}"));
        }
    }
    url
}

/// Percent-encode the characters that would end a URL's user-information field
/// early. Passwords in particular are arbitrary bytes and routinely contain
/// `@` or `:`.
fn encode(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| match c {
            ':' | '@' | '/' | '?' | '#' | '%' | '[' | ']' => {
                format!("%{:02X}", c as u32).chars().collect::<Vec<_>>()
            }
            c => vec![c],
        })
        .collect()
}

/// Read an environment variable, treating empty as absent.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Clone `config` with its target database name replaced.
fn with_dbname(config: &Config, dbname: &str) -> Config {
    let mut c = config.clone();
    c.dbname(dbname);
    c
}

/// Open a one-off connection and drive it on a background task.
async fn connect(config: &Config) -> Result<Client> {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .map_err(|e| Error::database(format!("connect: {e}")))?;
    tokio::spawn(async move {
        // When the client is dropped the connection future resolves; ignore the
        // result since teardown races are expected.
        let _ = connection.await;
    });
    Ok(client)
}

/// Build a connection pool for an already-created database.
fn build_pool(config: &Config) -> Result<Pool> {
    let mgr_config = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };
    let manager = Manager::from_config(config.clone(), NoTls, mgr_config);
    Pool::builder(manager)
        .max_size(8)
        .build()
        .map_err(|e| Error::database(format!("build pool: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `[environments.test]` section as a developer would write it for a local
    /// Postgres reached over the Unix socket by peer authentication.
    fn local_section() -> Environment {
        Environment {
            host: Some("/var/run/postgresql".to_owned()),
            user: Some("dev".to_owned()),
            database: Some("saltcorn_test".to_owned()),
            test_template: Some("saltcorn_v2_template".to_owned()),
            ..Environment::default()
        }
    }

    #[test]
    fn the_environment_outranks_the_file_for_the_template() {
        let section = local_section();
        assert_eq!(
            template(Some("from_env".to_owned()), Some(&section)).as_deref(),
            Some("from_env"),
            "SC_TEST_TEMPLATE must win over the file"
        );
    }

    #[test]
    fn the_file_supplies_the_template_when_the_environment_does_not() {
        let section = local_section();
        assert_eq!(
            template(None, Some(&section)).as_deref(),
            Some("saltcorn_v2_template"),
            "a machine that wrote the template down must not need the variable"
        );
        assert_eq!(
            template(None, None),
            None,
            "with neither, Postgres uses its own default template"
        );
        assert_eq!(
            template(None, Some(&Environment::default())),
            None,
            "a section that names no template names no template"
        );
    }

    #[test]
    fn a_sections_parts_build_the_connection() {
        let config = section_config(&local_section())
            .expect("the section says something")
            .expect("valid parameters");
        // A host beginning with `/` is a socket directory, not a hostname.
        assert_eq!(
            config.get_hosts(),
            [Host::Unix(PathBuf::from("/var/run/postgresql"))]
        );
        assert_eq!(config.get_ports(), [DEFAULT_PORT]);
        assert_eq!(config.get_user(), Some("dev"));
        assert_eq!(config.get_dbname(), Some("saltcorn_test"));
    }

    #[test]
    fn a_sections_url_is_used_wholesale() {
        let section = Environment {
            url: Some("postgres://sc:pw@db.internal:6000/scdb".to_owned()),
            // Ignored: `url` wins over the parts, as it does on the command line.
            host: Some("elsewhere".to_owned()),
            ..Environment::default()
        };
        let config = section_config(&section)
            .expect("the section says something")
            .expect("valid url");
        assert_eq!(config.get_hosts(), [Host::Tcp("db.internal".to_owned())]);
        assert_eq!(config.get_ports(), [6000]);
        assert_eq!(config.get_dbname(), Some("scdb"));
    }

    #[test]
    fn an_empty_section_is_no_configuration() {
        assert!(
            section_config(&Environment::default()).is_none(),
            "a placeholder section must not quietly become localhost"
        );
    }

    #[test]
    fn an_unparseable_url_is_an_error_naming_the_file() {
        let section = Environment {
            url: Some("not a connection string".to_owned()),
            ..Environment::default()
        };
        let err = section_config(&section)
            .expect("the section says something")
            .expect_err("the url does not parse");
        assert!(err.to_string().contains(FILE_NAME), "{err}");
    }

    #[test]
    fn a_socket_connection_renders_as_a_url_that_parses_back() {
        let base = section_config(&local_section())
            .expect("the section says something")
            .expect("valid parameters");
        let url = connection_url(&base, "sc_test_1234");
        assert_eq!(
            url,
            "postgres://dev@/sc_test_1234?host=/var/run/postgresql&port=5432"
        );
        // The point of the rendering: it survives the round trip, so a test may
        // hand it to anything that takes a connection string.
        let parsed: Config = url.parse().expect("the url parses");
        assert_eq!(
            parsed.get_hosts(),
            [Host::Unix(PathBuf::from("/var/run/postgresql"))]
        );
        assert_eq!(parsed.get_dbname(), Some("sc_test_1234"));
        assert_eq!(parsed.get_user(), Some("dev"));
    }

    #[test]
    fn a_password_with_url_punctuation_survives_the_rendering() {
        let base: Config = "postgres://sc:p@ss:word@db.internal:6000/x"
            .replace("p@ss:word", "plain")
            .parse()
            .expect("parse");
        let mut base = base;
        base.password("p@ss:word/#");
        let url = connection_url(&base, "sc_test_1");
        let parsed: Config = url.parse().expect("the url parses");
        assert_eq!(
            parsed.get_password(),
            Some("p@ss:word/#".as_bytes()),
            "rendered as {url}"
        );
    }

    #[test]
    fn only_the_test_section_is_read_by_default() {
        // Never the file's `default_environment`, which on a deployed machine is
        // production: a test run creates and drops databases.
        assert_eq!(DEFAULT_TEST_ENVIRONMENT, "test");
        assert_ne!(
            DEFAULT_TEST_ENVIRONMENT,
            sc_config_file::DEFAULT_ENVIRONMENT
        );
    }
}

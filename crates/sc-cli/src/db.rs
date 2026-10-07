//! Primary-database connection configuration for the `feldspar` binary.
//!
//! [`DbConfig`] gathers where the primary database lives from three places, in
//! this order of authority:
//!
//! 1. **CLI flags** — either a single connection URL (`--database-url`) or the
//!    individual `host`/`port`/`user`/`password`/`db` parts (`--db-host` etc.).
//! 2. **The environment** — `DATABASE_URL`, or the conventional `PG*` variables.
//! 3. **The configuration file** — the environment selected out of
//!    `feldspar.toml` ([`crate::config_file`]), which is where a deployment keeps
//!    production's, staging's and test's parameters side by side.
//!
//! A URL, wherever it comes from, wins wholesale over the parts; otherwise the
//! parts build a [`tokio_postgres::Config`]. Either way [`DbConfig::connect`]
//! yields the pooled driver the CLI hands to the catalog.
//!
//! **Or a file.** `--sqlite PATH` (or a `sqlite = "…"` in the selected
//! environment) says the primary database is a SQLite file rather than a
//! Postgres server — no host, no role, nothing to start, and the file is created
//! if it is not there. It is checked first and it is exclusive: an environment
//! that names both is refused by the configuration reader, and a `--database-url`
//! typed on the command line beside a `sqlite` in the file wins, because a flag
//! always outranks the file.
//!
//! **Naming an environment inverts 2 and 3.** With no `--environment`, the
//! ambient variables outrank the file, which is the stated rule: the file
//! supplies what the environment does not. But `--environment staging` is an
//! instruction, and an operator who gives it on a box where `DATABASE_URL`
//! happens to point at production must not be quietly connected to production.
//! So a *named* environment whose section says anything is authoritative: the
//! `PG*`/`DATABASE_URL` variables are ignored entirely for that run, and only
//! explicit flags still override it. (Ignored *entirely*, not per field — a
//! section giving host and database, with `DATABASE_URL` still filling in the
//! URL, would be the same accident wearing a smaller hat.)
//!
//! Parsing is separated from resolution on purpose: [`DbConfig::extract`] records
//! what was passed on the command line and loads the selected environment (and
//! returns the arguments it did not consume, so the server flags parse cleanly
//! afterwards), while the environment fallbacks and defaults are applied later in
//! [`connect`](DbConfig::connect) / [`target`](DbConfig::target). No silent
//! failures: an unreadable value (e.g. a non-numeric port) is an error, and
//! connection failures are surfaced by the caller with the redacted
//! [`target`](DbConfig::target) for context.

use std::sync::Arc;

use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::{Error, Result};

use crate::config_file::{self, Environment, SelectedEnvironment};

/// The serving settings of a selected environment: where this deployment's
/// applications are reachable in a browser.
///
/// A **view** over the configuration file's section rather than a resolved
/// value, because who resolves it differs: `serve` merges it with its own flags
/// and its own defaults (it has a bind address whether or not anyone configured
/// one), while a command-line build has only this and gives up gracefully when
/// it says nothing.
#[derive(Debug, Clone, Copy)]
pub struct Serving<'a> {
    section: Option<&'a Environment>,
}

impl Serving<'_> {
    /// The configured base domain, if the file gave one.
    pub fn base_domain(&self) -> Option<&str> {
        self.section.and_then(|s| s.base_domain.as_deref())
    }

    /// The further domains the file says the applications answer under.
    pub fn extra_base_domains(&self) -> &[String] {
        self.section
            .map(|s| s.extra_base_domains.as_slice())
            .unwrap_or_default()
    }

    /// The configured bind address, if the file gave one.
    pub fn bind(&self) -> Option<&str> {
        self.section.and_then(|s| s.bind.as_deref())
    }

    /// Whether the file says the deployment is behind TLS.
    pub fn secure_cookies(&self) -> Option<bool> {
        self.section.and_then(|s| s.secure_cookies)
    }

    /// The configured HTTPS port, if the file gave one.
    pub fn https_port(&self) -> Option<u16> {
        self.section.and_then(|s| s.https_port)
    }

    /// The TLS settings the file pins over `_fd_config`, keyed and typed as the
    /// settings they override — what [`sc_config::set_host_config`] takes.
    /// `ssl_extra_domains` is a list in the file and one name per line in the
    /// setting.
    pub fn host_config(&self) -> sc_types::Attrs {
        use serde_json::Value as Json;
        let mut out = sc_types::Attrs::new();
        let Some(s) = self.section else {
            return out;
        };
        let texts = [
            (sc_config::SSL_MODE, &s.ssl_mode),
            (sc_config::ACME_CONTACT_EMAIL, &s.acme_contact_email),
            (sc_config::ACME_DIRECTORY_URL, &s.acme_directory_url),
        ];
        for (key, value) in texts {
            if let Some(value) = value {
                out.insert(key.to_owned(), Json::from(value.clone()));
            }
        }
        if let Some(redirect) = s.redirect_http_to_https {
            out.insert(
                sc_config::REDIRECT_HTTP_TO_HTTPS.to_owned(),
                Json::Bool(redirect),
            );
        }
        if let Some(domains) = &s.ssl_extra_domains {
            out.insert(
                sc_config::SSL_EXTRA_DOMAINS.to_owned(),
                Json::from(domains.join("\n")),
            );
        }
        out
    }

    /// The configured headless browser, if the file named one.
    pub fn browser(&self) -> Option<&str> {
        self.section.and_then(|s| s.browser.as_deref())
    }

    /// Whether the file says the browser runs with its sandbox.
    pub fn browser_sandbox(&self) -> Option<bool> {
        self.section.and_then(|s| s.browser_sandbox)
    }

    /// The file's CmdStan and Stan ceilings, as the `serve` flags they mirror
    /// (Stan TODO §20).
    pub fn stan_flags(&self) -> Vec<(&'static str, String)> {
        self.section
            .map(Environment::stan_flags)
            .unwrap_or_default()
    }

    /// The port applications are reached on: the bind address's, if one was
    /// configured and parses as a socket address.
    ///
    /// A bind address that does not parse is **not** an error here. The one
    /// place that must refuse it is `serve`, which is about to bind it and does
    /// so through the same parser as `--bind`; a build that only wanted to write
    /// a URL into a comment has no business failing over a setting it is merely
    /// quoting.
    pub fn port(&self) -> Option<u16> {
        self.bind()?
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|addr| addr.port())
    }

    /// Where applications are served, when the file said enough to know: a base
    /// domain, and a port to reach it on.
    ///
    /// `base_domain` overrides the file's, for a command with a flag of its own.
    pub fn public_origin(&self, base_domain: Option<&str>) -> Option<sc_catalog::PublicOrigin> {
        let domain = base_domain.or_else(|| self.base_domain())?;
        Some(
            sc_catalog::PublicOrigin::new(domain, self.port().unwrap_or(DEFAULT_HTTP_PORT))
                .secure(self.secure_cookies().unwrap_or(false)),
        )
    }
}

/// The port an application's URL carries when nothing said which — the port
/// [`sc_server::DEFAULT_BIND`](sc_server) binds, and the one every local
/// deployment in the documentation uses. Named here rather than borrowed from
/// the server so this crate's *build* commands, which never construct a
/// `ServerConfig`, do not depend on one.
const DEFAULT_HTTP_PORT: u16 = 3032;

/// Default host when neither `--db-host` nor `PGHOST` is set.
const DEFAULT_HOST: &str = "localhost";
/// Default port when neither `--db-port` nor `PGPORT` is set.
const DEFAULT_PORT: u16 = 5432;
/// Environment variable naming a SQLite file to use as the primary database —
/// the counterpart of `DATABASE_URL` for the other kind of database.
const SQLITE_VAR: &str = "FELDSPAR_SQLITE";

/// How to reach the primary database. The flag fields hold only what was passed
/// on the command line; the environment variables and the defaults are applied
/// at [`connect`](Self::connect). `selected` is the configuration file's
/// contribution, resolved at [`extract`](Self::extract) time because reading it
/// is I/O and every later accessor is infallible.
#[derive(Debug, Default, Clone)]
pub struct DbConfig {
    url: Option<String>,
    host: Option<String>,
    port: Option<String>,
    user: Option<String>,
    password: Option<String>,
    dbname: Option<String>,
    /// A SQLite file to use instead of a Postgres server.
    sqlite: Option<String>,
    /// The environment selected out of `feldspar.toml`, if there is one.
    selected: Option<SelectedEnvironment>,
}

impl DbConfig {
    /// A config that connects with the given URL (used by tests and callers that
    /// already hold a connection string).
    pub fn from_url(url: impl Into<String>) -> DbConfig {
        DbConfig {
            url: Some(url.into()),
            ..DbConfig::default()
        }
    }

    /// A config whose primary database is the SQLite file at `path`.
    pub fn from_sqlite(path: impl Into<String>) -> DbConfig {
        DbConfig {
            sqlite: Some(path.into()),
            ..DbConfig::default()
        }
    }

    /// Pull the database flags out of `args`, returning the parsed config and the
    /// arguments that were **not** consumed (for the server config to parse).
    ///
    /// Recognised flags: `--database-url`, `--db-host`, `--db-port`, `--db-user`,
    /// `--db-password`, `--db-name`, `--sqlite`, plus `--environment` (which environment of
    /// the configuration file to use) and `--config` (which configuration file).
    /// Anything else is passed through untouched, so an unknown flag still fails
    /// loudly — in the server parser, not here.
    ///
    /// This also **loads the configuration file**, so that every command that
    /// takes database flags gets the file for free and none can forget to ask for
    /// it. A file that does not parse, or a named environment that does not
    /// exist, fails here — before anything connects.
    pub fn extract<I, S>(args: I) -> Result<(DbConfig, Vec<String>)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut cfg = DbConfig::default();
        let mut environment: Option<String> = None;
        let mut config_path: Option<String> = None;
        let mut rest = Vec::new();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            let slot = match arg.as_ref() {
                "--database-url" => &mut cfg.url,
                "--db-host" => &mut cfg.host,
                "--db-port" => &mut cfg.port,
                "--db-user" => &mut cfg.user,
                "--db-password" => &mut cfg.password,
                "--db-name" => &mut cfg.dbname,
                "--sqlite" => &mut cfg.sqlite,
                "--environment" | "--env" => &mut environment,
                "--config" => &mut config_path,
                other => {
                    rest.push(other.to_owned());
                    continue;
                }
            };
            let flag = arg.as_ref().to_owned();
            *slot = Some(next_value(&mut it, &flag)?);
        }
        cfg.selected = config_file::select(config_path.as_deref(), environment.as_deref())?;
        Ok((cfg, rest))
    }

    /// Where the configuration file put us, for the startup log: `None` when no
    /// configuration file took part in this connection.
    pub fn source(&self) -> Option<String> {
        self.selected.as_ref().map(SelectedEnvironment::describe)
    }

    /// The name of the selected environment, if a configuration file was used.
    pub fn environment(&self) -> Option<&str> {
        self.selected.as_ref().map(|s| s.name.as_str())
    }

    /// The **serving** half of the selected environment: where this deployment's
    /// applications are reachable.
    ///
    /// It rides on the database configuration because it arrives with it — one
    /// `[environments.NAME]` section says both, and every command already
    /// resolves that section to find its database. A command that wants the
    /// serving settings therefore needs no second flag, no second file and no
    /// second lookup; see [`Environment`]'s documentation for why an environment
    /// carries them at all.
    pub fn serving(&self) -> Serving<'_> {
        Serving {
            section: self.section(),
        }
    }

    /// Connect to the database, returning a pooled driver. A URL (from the flag,
    /// `DATABASE_URL` or the selected environment) is used wholesale; otherwise
    /// the individual parts — falling back to the `PG*` environment variables,
    /// the selected environment and then the host/port defaults — build the
    /// connection.
    ///
    /// This only builds the pool; the first real connection (and thus the first
    /// chance to observe an unreachable/misconfigured database) happens when the
    /// catalog introspects, so the caller wraps that with [`target`](Self::target).
    pub async fn connect(&self) -> Result<Arc<dyn DatabaseDriver>> {
        // The file first: it is a whole different kind of database, so there is
        // nothing to merge it with — a deployment either has a SQLite file or a
        // Postgres server.
        if let Some(path) = self.resolved_sqlite() {
            return Ok(Arc::new(SqliteDriver::open(&path)?));
        }
        if let Some(url) = self.resolved_url() {
            return Ok(Arc::new(PgDriver::connect(&url).await?));
        }
        let mut config = tokio_postgres::Config::new();
        config.host(self.resolved_host());
        config.port(self.resolved_port()?);
        if let Some(user) = self.resolved(&self.user, "PGUSER", |e| e.user.clone()) {
            config.user(user);
        }
        if let Some(password) = self.resolved(&self.password, "PGPASSWORD", |e| e.password.clone())
        {
            config.password(password);
        }
        if let Some(dbname) = self.resolved(&self.dbname, "PGDATABASE", |e| e.database.clone()) {
            config.dbname(dbname);
        }
        Ok(Arc::new(PgDriver::from_config(&config)?))
    }

    /// A human-readable, **password-free** description of the target, for error
    /// messages. Never includes credentials.
    pub fn target(&self) -> String {
        if let Some(path) = self.resolved_sqlite() {
            return format!("the SQLite file {path}");
        }
        match self.resolved_url() {
            Some(url) => redact(&url),
            None => self.target_from_parts(),
        }
    }

    /// The SQLite file this connection is for, if it is for one: the flag, then
    /// — in whichever order [`file_wins`](Self::file_wins) dictates —
    /// [`SQLITE_VAR`] and the selected environment's `sqlite`.
    ///
    /// A `--database-url` **typed on the command line** takes it back: the flag
    /// outranks the file, and an operator who names a Postgres database on the
    /// command line means to use it, whatever the file says. A `DATABASE_URL` in
    /// the environment does not, because a file that says `sqlite` is as
    /// ambient as the variable and rather more deliberate.
    fn resolved_sqlite(&self) -> Option<String> {
        if let Some(path) = &self.sqlite {
            return Some(path.clone());
        }
        if self.url.is_some() {
            return None;
        }
        self.resolved(&self.sqlite, SQLITE_VAR, |e| e.sqlite.clone())
    }

    /// The `host:port/db` description used when no connection URL is in play.
    fn target_from_parts(&self) -> String {
        format!(
            "{}:{}/{}",
            self.resolved_host(),
            self.port_string(),
            self.resolved(&self.dbname, "PGDATABASE", |e| e.database.clone())
                .unwrap_or_else(|| "<default>".to_owned()),
        )
    }

    /// The effective connection URL: the flag, then — in whichever order
    /// [`file_wins`](Self::file_wins) dictates — `DATABASE_URL` and the selected
    /// environment's `url`.
    fn resolved_url(&self) -> Option<String> {
        if let Some(url) = &self.url {
            return Some(url.clone());
        }
        let from_file = self.section().and_then(|e| e.url.clone());
        if self.file_wins() {
            return from_file;
        }
        env_var("DATABASE_URL").or(from_file)
    }

    /// The effective host: flag, else `PGHOST`/the file, else the default.
    fn resolved_host(&self) -> String {
        self.resolved(&self.host, "PGHOST", |e| e.host.clone())
            .unwrap_or_else(|| DEFAULT_HOST.to_owned())
    }

    /// The effective port as a `u16`, erroring if it is not a valid number.
    fn resolved_port(&self) -> Result<u16> {
        match self.resolved(&self.port, "PGPORT", |e| e.port.map(|p| p.to_string())) {
            Some(raw) => raw
                .parse()
                .map_err(|e| Error::config(format!("invalid database port `{raw}`: {e}"))),
            None => Ok(DEFAULT_PORT),
        }
    }

    /// The effective port rendered for [`target`](Self::target) (defaulted, never
    /// erroring — display only).
    fn port_string(&self) -> String {
        self.resolved(&self.port, "PGPORT", |e| e.port.map(|p| p.to_string()))
            .unwrap_or_else(|| DEFAULT_PORT.to_string())
    }

    /// One setting, resolved: the flag first, then the environment variable and
    /// the configuration file in whichever order [`file_wins`](Self::file_wins)
    /// dictates. `pick` reads the setting out of the file's section.
    fn resolved(
        &self,
        flag: &Option<String>,
        env: &str,
        pick: impl Fn(&Environment) -> Option<String>,
    ) -> Option<String> {
        if let Some(value) = flag {
            return Some(value.clone());
        }
        let from_file = self.section().and_then(pick);
        if self.file_wins() {
            return from_file;
        }
        env_var(env).or(from_file)
    }

    /// The selected environment's connection parameters, if any.
    fn section(&self) -> Option<&Environment> {
        self.selected.as_ref().map(|s| &s.section)
    }

    /// Whether the configuration file outranks the ambient `PG*`/`DATABASE_URL`
    /// variables: true exactly when the operator **named** an environment that
    /// says something. See the module docs for why naming one inverts the order.
    fn file_wins(&self) -> bool {
        self.selected
            .as_ref()
            .is_some_and(|s| s.explicit && !s.section.is_empty())
    }
}

/// Read an environment variable, treating empty as absent.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Take the value following a flag, erroring if it is missing.
fn next_value<I, S>(it: &mut I, flag: &str) -> Result<String>
where
    I: Iterator<Item = S>,
    S: AsRef<str>,
{
    it.next()
        .map(|s| s.as_ref().to_owned())
        .ok_or_else(|| Error::config(format!("{flag} requires a value")))
}

/// Remove any password from a `scheme://user:password@host/...` URL so it is safe
/// to print. Returns the input unchanged when there is no `user:password@` part.
fn redact(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_owned();
    };
    let after_scheme = scheme_end + 3;
    let rest = &url[after_scheme..];
    // The authority ends at the first '/', '?' or '#'.
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_len);
    let Some(at) = authority.rfind('@') else {
        return url.to_owned();
    };
    let userinfo = &authority[..at];
    let host = &authority[at..]; // includes the '@'
    let user = userinfo.split(':').next().unwrap_or(userinfo);
    format!("{}{}:***{}{}", &url[..after_scheme], user, host, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_s_tls_keys_become_the_settings_they_pin() {
        let section = Environment {
            ssl_mode: Some("letsencrypt".to_owned()),
            acme_contact_email: Some("ops@example.com".to_owned()),
            redirect_http_to_https: Some(false),
            ssl_extra_domains: Some(vec!["a.example.com".to_owned(), "b.example.com".to_owned()]),
            ..Environment::default()
        };
        let pinned = Serving {
            section: Some(&section),
        }
        .host_config();
        assert_eq!(
            serde_json::Value::Object(pinned.clone()),
            serde_json::json!({
                "ssl_mode": "letsencrypt",
                "acme_contact_email": "ops@example.com",
                "redirect_http_to_https": false,
                "ssl_extra_domains": "a.example.com\nb.example.com",
            })
        );
        // Every one of them is a key the host may pin, typed as declared.
        assert!(
            pinned
                .keys()
                .all(|key| sc_config::HOST_KEYS.contains(&key.as_str()))
        );
        assert!(Serving { section: None }.host_config().is_empty());
    }

    #[test]
    fn extract_pulls_db_flags_and_leaves_the_rest() {
        let (cfg, rest) = DbConfig::extract([
            "--bind",
            "0.0.0.0:80",
            "--db-host",
            "db.internal",
            "--db-port",
            "6543",
            "--db-user",
            "sc",
            "--db-password",
            "secret",
            "--db-name",
            "app",
            "--secure-cookies",
        ])
        .expect("extract");

        assert_eq!(cfg.host.as_deref(), Some("db.internal"));
        assert_eq!(cfg.port.as_deref(), Some("6543"));
        assert_eq!(cfg.user.as_deref(), Some("sc"));
        assert_eq!(cfg.password.as_deref(), Some("secret"));
        assert_eq!(cfg.dbname.as_deref(), Some("app"));
        // Non-database flags are handed back for the server parser, in order.
        assert_eq!(rest, ["--bind", "0.0.0.0:80", "--secure-cookies"]);
    }

    #[test]
    fn extract_takes_a_full_url() {
        let (cfg, rest) =
            DbConfig::extract(["--database-url", "postgres://u:p@h:5/db"]).expect("extract");
        assert_eq!(cfg.url.as_deref(), Some("postgres://u:p@h:5/db"));
        assert!(rest.is_empty());
    }

    #[test]
    fn extract_errors_on_a_flag_without_a_value() {
        assert!(DbConfig::extract(["--db-host"]).is_err());
    }

    #[test]
    fn a_bad_port_is_an_error_not_a_default() {
        let cfg = DbConfig {
            port: Some("not-a-port".to_owned()),
            ..DbConfig::default()
        };
        assert!(cfg.resolved_port().is_err());
    }

    #[test]
    fn target_hides_the_password() {
        let cfg = DbConfig::from_url("postgres://user:hunter2@host:5432/appdb");
        let target = cfg.target();
        assert!(!target.contains("hunter2"), "leaked password: {target}");
        assert!(target.contains("user"));
        assert!(target.contains("host:5432/appdb"));
    }

    #[test]
    fn target_from_parts_is_host_port_db_without_credentials() {
        let cfg = DbConfig {
            host: Some("h".to_owned()),
            port: Some("6000".to_owned()),
            dbname: Some("mydb".to_owned()),
            password: Some("secret".to_owned()),
            ..DbConfig::default()
        };
        // Call the parts formatter directly: whether `target()` takes the parts
        // branch depends on an ambient `DATABASE_URL`, which is set when running
        // the integration suite. The formatting and password omission are what
        // this test is about.
        let target = cfg.target_from_parts();
        assert_eq!(target, "h:6000/mydb");
        assert!(!target.contains("secret"));
    }

    #[test]
    fn redact_leaves_a_url_without_userinfo_unchanged() {
        assert_eq!(redact("postgres://host:5432/db"), "postgres://host:5432/db");
    }

    // --- The configuration file -------------------------------------------
    //
    // These drive `extract` with an explicit `--config`, never the search path,
    // so they cannot pick up (or be broken by) a real `feldspar.toml` on the
    // machine running them. Nothing here sets an environment variable: doing so
    // is racy across the test threads, so the assertions are written to hold
    // whatever `DATABASE_URL`/`PG*` the suite happens to be run with — which is
    // itself the property most of them are about.

    const FIXTURE: &str = r#"
default_environment = "production"

[environments.production]
host = "prod.internal"
port = 5432
user = "sc"
password = "prod-pw"
database = "saltcorn"

[environments.staging]
url = "postgres://sc:staging-pw@staging.internal:5432/saltcorn"

[environments.test]
host = "localhost"
database = "saltcorn_test"

[environments.blank]
"#;

    /// A configuration file on disk for the duration of one test, removed when
    /// the handle drops.
    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new(name: &str, contents: &str) -> Fixture {
            let path =
                std::env::temp_dir().join(format!("sc-cli-{}-{name}.toml", std::process::id()));
            std::fs::write(&path, contents).expect("write fixture");
            // 0600 both because the file holds passwords and so the loader's
            // world-readable warning does not fire on every test run.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .expect("chmod fixture");
            }
            Fixture(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().expect("utf-8 fixture path")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_named_environment_supplies_the_connection_parts() {
        let file = Fixture::new("parts", FIXTURE);
        let (cfg, rest) = DbConfig::extract([
            "--environment",
            "test",
            "--config",
            file.path(),
            "--bind",
            "x",
        ])
        .expect("extract");

        assert_eq!(rest, ["--bind", "x"]);
        assert_eq!(cfg.environment(), Some("test"));
        // Named → the file is authoritative, so this holds even when the suite
        // is run with DATABASE_URL and PG* pointing elsewhere.
        assert_eq!(cfg.resolved_url(), None);
        assert_eq!(cfg.resolved_host(), "localhost");
        assert_eq!(cfg.resolved_port().expect("port"), 5432);
        assert_eq!(cfg.target(), "localhost:5432/saltcorn_test");
    }

    #[test]
    fn a_named_environment_may_carry_a_url_and_outranks_the_environment() {
        let file = Fixture::new("url", FIXTURE);
        let (cfg, _) = DbConfig::extract(["--environment", "staging", "--config", file.path()])
            .expect("extract");

        assert_eq!(
            cfg.resolved_url().as_deref(),
            Some("postgres://sc:staging-pw@staging.internal:5432/saltcorn"),
            "naming an environment must beat an ambient DATABASE_URL"
        );
        // ...and the startup line says which one, without the password.
        let source = cfg.source().expect("a source");
        assert!(source.contains("staging"), "{source}");
        assert!(!cfg.target().contains("staging-pw"), "{}", cfg.target());
    }

    #[test]
    fn a_flag_still_beats_a_named_environment() {
        let file = Fixture::new("flag", FIXTURE);
        let (cfg, _) = DbConfig::extract([
            "--environment",
            "staging",
            "--config",
            file.path(),
            "--database-url",
            "postgres://flag/db",
        ])
        .expect("extract");
        assert_eq!(cfg.resolved_url().as_deref(), Some("postgres://flag/db"));
    }

    #[test]
    fn without_a_flag_the_files_default_environment_is_used_and_yields_to_the_environment() {
        let file = Fixture::new("default", FIXTURE);
        let (cfg, _) = DbConfig::extract(["--config", file.path()]).expect("extract");

        assert_eq!(cfg.environment(), Some("production"));
        // Not *named*, so the ambient variables still come first: the file is
        // the fallback, which is the whole point of it.
        assert!(!cfg.file_wins());
        if std::env::var_os("PGHOST").is_none() {
            assert_eq!(cfg.resolved_host(), "prod.internal");
        }
        if std::env::var_os("DATABASE_URL").is_none() {
            assert_eq!(cfg.resolved_url(), None);
        }
    }

    #[test]
    fn an_empty_named_section_does_not_take_over_from_the_environment() {
        let file = Fixture::new("blank", FIXTURE);
        let (cfg, _) = DbConfig::extract(["--environment", "blank", "--config", file.path()])
            .expect("extract");
        assert_eq!(cfg.environment(), Some("blank"));
        assert!(
            !cfg.file_wins(),
            "a section that says nothing must not silence DATABASE_URL"
        );
    }

    #[test]
    fn an_undefined_environment_is_an_error() {
        let file = Fixture::new("undefined", FIXTURE);
        let err = DbConfig::extract(["--environment", "prod", "--config", file.path()])
            .expect_err("unknown environment must fail");
        assert!(err.to_string().contains("production, staging"), "{err}");
    }

    #[test]
    fn a_config_path_that_does_not_exist_is_an_error() {
        let missing = std::env::temp_dir().join("sc-cli-absent-config-4c1e.toml");
        let err = DbConfig::extract(["--config", missing.to_str().expect("path")])
            .expect_err("a named file that is absent must fail");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn a_malformed_config_file_fails_at_parse_time_not_at_connect_time() {
        let file = Fixture::new("broken", "[environments.production\nhost = 'x'\n");
        assert!(DbConfig::extract(["--config", file.path()]).is_err());
    }
}

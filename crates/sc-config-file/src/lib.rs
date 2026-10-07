//! The `feldspar.toml` configuration file: named environments, each holding one
//! set of primary-database connection parameters.
//!
//! A deployment does not have *a* database; it has a production database, a
//! staging database and a test database, and the difference between them is a
//! handful of connection parameters. Carrying those in the environment works for
//! one of them (a container, a systemd unit) and works badly for the rest: an
//! operator on a machine with three of them either exports and re-exports
//! `DATABASE_URL`, or writes three wrapper scripts. This file is the other half
//! — the parameters live on disk, named, and the command line picks one:
//!
//! ```toml
//! default_environment = "production"
//!
//! [environments.production]
//! host = "db.internal"
//! port = 5432
//! user = "saltcorn"
//! password = "…"
//! database = "saltcorn"
//!
//! [environments.staging]
//! url = "postgres://saltcorn:…@staging.internal:5432/saltcorn"
//!
//! [environments.test]
//! database = "saltcorn_test"
//! test_template = "saltcorn_template"
//!
//! [environments.laptop]
//! sqlite = "/home/me/saltcorn/app.sqlite"
//! ```
//!
//! An environment names **one** database, and `sqlite` is the other kind it can
//! name: a file rather than a server, with no host, no role and nothing to start
//! (`sc-db-sqlite`). It is an alternative to the Postgres parameters, not a
//! setting beside them, so a section that gives both is an error rather than a
//! quiet preference for one of them.
//!
//! An environment is a **deployment**, not only a connection string, so a
//! section may also say where that deployment is served:
//!
//! ```toml
//! [environments.production]
//! url = "postgres://saltcorn:…@db.internal/saltcorn"
//! base_domain = "example.com"      # apps are at <subdomain>.example.com
//! extra_base_domains = []          # …and also at <subdomain>.<each of these>
//! bind = "0.0.0.0:80"
//! https_port = 8443                # only when TLS is not on 443
//! secure_cookies = true
//! ssl_mode = "letsencrypt"         # pins the TLS settings; the admin UI shows
//! acme_contact_email = "ops@example.com" # them read-only
//! ```
//!
//! Those mirror `serve`'s flags of the same names, so `feldspar serve
//! --environment production` needs none of them on the command line — and, less
//! obviously but more usefully, a `feldspar build-app` run against the same
//! environment writes the application's real URL into the documentation it
//! generates instead of a placeholder.
//!
//! `environments` is an ordinary TOML table, so there is nothing special about
//! the three names above — a deployment may define as many as it has databases,
//! and `--environment NAME` names any of them.
//!
//! **Where the file lives** is the operating system's business, not ours, so
//! [`search_paths`] asks the platform: the user configuration directory
//! (`$XDG_CONFIG_HOME`, `~/Library/Application Support`, `%APPDATA%`) and then
//! the system one (`/etc`, `%PROGRAMDATA%`). The system path matters as much as
//! the user one here: a server started by systemd runs as a service account that
//! may have no home directory at all.
//!
//! **No silent failures** (principle 5) is the whole design of the reader. A file
//! that does not parse is an error, not a shrug; an unknown key is an error,
//! because a misspelled `databse` that was quietly ignored would connect to the
//! wrong database rather than fail; and naming an environment that the file does
//! not define is an error listing the ones it does. The one deliberately quiet
//! path is *no file at all*, which is not a misconfiguration — it is the
//! environment-variable deployment this file exists alongside.
//!
//! **Two readers, not one.** The `feldspar` binary reads it to know where the
//! primary database is; the integration-test harness reads the `test`
//! environment to know which database it may create its per-test databases from,
//! and which template to clone them out of ([`Environment::test_template`]).
//! That is why the reader is a layer-0 crate of its own rather than a module of
//! `sc-cli`: a `cargo test` on a developer's machine should need no environment
//! variables that a `feldspar serve` on the same machine does not.
//!
//! Not to be confused with `sc-config`, which is the `_fd_config` **table** —
//! the settings an admin edits in the running server. This crate is the file on
//! disk that says which database those settings live in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sc_error::{Error, Result};
use serde::Deserialize;

/// The file's name, in whichever directory it is found.
pub const FILE_NAME: &str = "feldspar.toml";
/// The per-application directory the file sits in, under the platform's
/// configuration root.
pub const APP_DIR: &str = "feldspar";
/// Environment variable naming the configuration file outright (overrides the
/// search). The file it names must exist.
pub const CONFIG_PATH_VAR: &str = "FELDSPAR_CONFIG";
/// Environment variable selecting the environment, when `--environment` is not
/// passed. Selecting one this way is as explicit as the flag.
pub const ENVIRONMENT_VAR: &str = "FELDSPAR_ENV";
/// The environment used when the file names no `default_environment` and the
/// command line selects none.
pub const DEFAULT_ENVIRONMENT: &str = "production";

/// A parsed `feldspar.toml`.
///
/// `deny_unknown_fields`: a key we do not recognise is a typo in a file whose
/// whole job is to say which database to write to, and stepping over it would
/// mean connecting somewhere the operator did not intend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Which environment to use when the command line selects none. Defaults to
    /// [`DEFAULT_ENVIRONMENT`].
    #[serde(default)]
    pub default_environment: Option<String>,
    /// The environments, by name. Any number, any names.
    #[serde(default)]
    pub environments: BTreeMap<String, Environment>,
}

/// One environment: the database it connects to, and where it is served.
///
/// `url` and the individual parts are alternatives, and `url` wins, exactly as
/// on the command line.
///
/// The last three are the **serving** half, and they are here because an
/// environment is a deployment rather than a connection string. Two things
/// follow from having them: `feldspar serve --environment production` needs no
/// other flag, and — the reason they were added — a build run from the command
/// line writes the *same* application URL into the generated documentation that
/// a build run by the server would. Without them, `feldspar build-app` would
/// quietly rewrite `AGENTS.md` with the URL taken out, which is worse than
/// never having written it. They mirror three `serve` flags exactly.
///
/// After them come the properties of **this host** that an operator would
/// otherwise repeat on every `serve` line: its browser, and its CmdStan with
/// the Stan ceilings. Each mirrors a `serve` flag too, and a flag given on
/// the command line wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    /// A full connection string. Takes precedence over the parts below.
    pub url: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    /// The database name (`database`, not `dbname`: this is a file a person
    /// writes, and `--db-name`'s spelling is the CLI's own abbreviation).
    pub database: Option<String>,
    /// A **SQLite** file to use as the primary database, instead of a Postgres
    /// server.
    ///
    /// The whole connection: there is no host, port, user or password, because
    /// SQLite has none — the database is the file, and the process that opens it
    /// is the server. A relative path is resolved against the working directory
    /// of whatever opens it, so a deployment writes an absolute one.
    ///
    /// Mutually exclusive with the Postgres parameters above; see
    /// [`Environment::check`].
    pub sqlite: Option<String>,
    /// The domain applications are served under — `--base-domain`. An app is at
    /// `<subdomain>.<base_domain>` (design §13.2).
    pub base_domain: Option<String>,
    /// Further domains the same applications answer under —
    /// `--extra-base-domain`, once per domain. `todo.10.0.2.2.nip.io` reaches
    /// the app `todo` just as `todo.<base_domain>` does, which is how an Android
    /// emulator or a phone on the LAN reaches a development server. The base
    /// domain stays the one an application's URL is written with.
    #[serde(default)]
    pub extra_base_domains: Vec<String>,
    /// The address the server binds — `--bind`. Only its port takes part in an
    /// application's URL, but it is spelled as the flag is so there is one thing
    /// to write and one thing to read.
    pub bind: Option<String>,
    /// Whether the deployment is behind TLS — `--secure-cookies`, whose flag
    /// name says what it does to cookies and which equally decides whether an
    /// application's URL is `https`.
    pub secure_cookies: Option<bool>,
    /// The port TLS is served on, when the TLS settings turn it on —
    /// `--https-port`. Only needed when it is not 443.
    ///
    /// A property of this host, like `bind`, and not a stored setting: a port
    /// kept in the database travels with a backup into a deployment whose
    /// firewall knows nothing about it.
    pub https_port: Option<u16>,
    /// The TLS settings, pinned on this host — `ssl_mode`, `acme_contact_email`,
    /// `acme_directory_url`, `redirect_http_to_https` and `ssl_extra_domains`,
    /// the `_fd_config` keys of the same names (design §13.5). This first one is
    /// `ssl_mode`: `off`, `letsencrypt` or `custom`.
    ///
    /// Each is optional, and one given here **wins over the database**: the
    /// settings screen shows it read-only, a save cannot change it, and neither a
    /// restore nor Clear all can take it away. That is the point of having them
    /// here as well as there. TLS decides whether this host answers on the port
    /// its proxy sends traffic to, and a setting that only the admin UI can
    /// repair is a setting that, once lost, locks the admin out of the UI that
    /// would repair it. With them in a file the instance can read but not write,
    /// the operator of the machine — who on managed hosting is not the admin of
    /// the instance — decides how it serves.
    ///
    /// Inline rather than a nested struct: `serde(flatten)` and
    /// `deny_unknown_fields` do not combine, and the unknown-key check is worth
    /// more than the grouping.
    pub ssl_mode: Option<String>,
    /// `acme_contact_email`: the ACME account's contact address.
    pub acme_contact_email: Option<String>,
    /// `acme_directory_url`: the ACME directory URL.
    pub acme_directory_url: Option<String>,
    /// `redirect_http_to_https`: whether plain HTTP redirects to HTTPS.
    pub redirect_http_to_https: Option<bool>,
    /// `ssl_extra_domains`: domains to certify beyond the ones the server
    /// derives — a TOML list here, where the settings screen has one per line.
    pub ssl_extra_domains: Option<Vec<String>>,
    /// The headless Chromium the coding agent's `view_app` drives — `--browser`
    /// (TODO §7b). Unset, the server looks on `PATH` for `chromium`,
    /// `chromium-browser` and `google-chrome`, skipping a snap shim.
    ///
    /// A property of this host rather than of the database, like `bind`, which is
    /// why it is here and not a stored setting.
    pub browser: Option<String>,
    /// Whether that browser runs with its own sandbox — `false` is
    /// `--no-browser-sandbox`. On by default; off only for a kernel that refuses
    /// the unprivileged user namespaces the sandbox needs, which
    /// `scripts/setup-host.sh` detects and names.
    pub browser_sandbox: Option<bool>,
    /// The CmdStan Stan models use — `--cmdstan` (Stan TODO §20). Unset, the
    /// server takes `$CMDSTAN`, else the newest `~/.cmdstan/cmdstan-*`.
    ///
    /// This and the six `stan_*` keys below are properties of this host, like
    /// `browser`: where its CmdStan is, which disk has room for compiled
    /// programs, how many cores a chain may take and how much memory draws
    /// may. Each mirrors the `serve` flag of the same name
    /// ([`Environment::stan_flags`]), and a flag on the command line wins.
    pub cmdstan: Option<String>,
    /// `--stan-cache-dir`: where compiled Stan programs are kept.
    pub stan_cache_dir: Option<String>,
    /// `--stan-max-processes`: chain processes at once, across every fit.
    pub stan_max_processes: Option<u64>,
    /// `--stan-max-data-values`: numbers one fit's bound data may hold.
    pub stan_max_data_values: Option<u64>,
    /// `--stan-max-draws-bytes`: bytes of draws one fit may store.
    pub stan_max_draws_bytes: Option<u64>,
    /// `--stan-max-draws-response`: numbers one draws response may carry.
    pub stan_max_draws_response: Option<u64>,
    /// `--stan-summary-max-elements`: the largest generated quantity
    /// summarised when a fit finishes.
    pub stan_summary_max_elements: Option<u64>,
    /// The database the integration-test harness clones each of its per-test
    /// databases from. Only the test environment has any use for it.
    ///
    /// It is here rather than in the harness because it is a property of *this
    /// machine's* Postgres, exactly like the connection parameters beside it: a
    /// box whose `template1` carries a stale collation version cannot
    /// `CREATE DATABASE` at all, and names an empty database it owns instead.
    /// Unset means the server default (`template1`), which is what a clean CI
    /// Postgres wants.
    pub test_template: Option<String>,
}

impl Environment {
    /// Refuse a section that names two different databases.
    ///
    /// `sqlite` is not one more connection parameter, it is a *different kind of
    /// database*: a section holding both it and a Postgres host describes two,
    /// and nothing can choose between them for the operator. Silently preferring
    /// one would be exactly the accident the whole file exists to prevent — an
    /// operator who thinks they are on the laptop's file and is in fact on the
    /// production server.
    pub fn check(&self, name: &str, path: &Path) -> Result<()> {
        if self.sqlite.is_none() {
            return Ok(());
        }
        let postgres: Vec<&str> = [
            ("url", self.url.is_some()),
            ("host", self.host.is_some()),
            ("port", self.port.is_some()),
            ("user", self.user.is_some()),
            ("password", self.password.is_some()),
            ("database", self.database.is_some()),
        ]
        .into_iter()
        .filter_map(|(key, given)| given.then_some(key))
        .collect();
        if postgres.is_empty() {
            return Ok(());
        }
        Err(Error::config(format!(
            "the `{name}` environment of `{}` names a SQLite file *and* a Postgres              connection ({}); an environment is one database, so remove whichever              it is not",
            path.display(),
            postgres.join(", ")
        )))
    }

    /// The Stan keys this section sets, spelled as the `serve` flags they
    /// mirror, in a fixed order.
    ///
    /// Flags rather than values so that `serve` checks a file's `0` with the
    /// same parser, and the same sentence, as a `0` typed on the command line.
    pub fn stan_flags(&self) -> Vec<(&'static str, String)> {
        let counts = [
            ("--stan-max-processes", self.stan_max_processes),
            ("--stan-max-data-values", self.stan_max_data_values),
            ("--stan-max-draws-bytes", self.stan_max_draws_bytes),
            ("--stan-max-draws-response", self.stan_max_draws_response),
            (
                "--stan-summary-max-elements",
                self.stan_summary_max_elements,
            ),
        ];
        [
            ("--cmdstan", self.cmdstan.clone()),
            ("--stan-cache-dir", self.stan_cache_dir.clone()),
        ]
        .into_iter()
        .chain(counts.map(|(flag, n)| (flag, n.map(|n| n.to_string()))))
        .filter_map(|(flag, value)| value.map(|v| (flag, v)))
        .collect()
    }

    /// Whether this section says nothing at all. An empty section is treated as
    /// no configuration rather than as "connect to the defaults", so a
    /// placeholder `[environments.staging]` with the parameters still to be
    /// filled in does not quietly become localhost.
    pub fn is_empty(&self) -> bool {
        *self == Environment::default()
    }
}

impl ConfigFile {
    /// Parse a configuration file's text. `path` is used only in error messages.
    pub fn parse(text: &str, path: &Path) -> Result<ConfigFile> {
        toml::from_str(text).map_err(|e| {
            Error::config(format!(
                "could not read the configuration file `{}`: {e}",
                path.display()
            ))
        })
    }

    /// Read and parse the file at `path`, or `Ok(None)` if it does not exist.
    ///
    /// Only "not found" is `None`: a file that exists but cannot be read (a
    /// permission problem, say) is an error, because that is a file the operator
    /// meant us to use.
    pub fn load(path: &Path) -> Result<Option<ConfigFile>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(Error::config(format!(
                    "could not open the configuration file `{}`: {e}",
                    path.display()
                )));
            }
        };
        warn_if_world_readable(path);
        ConfigFile::parse(&text, path).map(Some)
    }

    /// The environment section `name`, erroring with the available names when it
    /// is not defined.
    pub fn environment(&self, name: &str, path: &Path) -> Result<&Environment> {
        let section = self.environments.get(name).ok_or_else(|| {
            let available = self
                .environments
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            Error::config(format!(
                "the configuration file `{}` defines no environment `{name}` \
                 (it defines: {available}); select one with --environment NAME",
                path.display(),
            ))
        })?;
        section.check(name, path)?;
        Ok(section)
    }
}

/// The environment that was selected, the section it resolved to, and where it
/// came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedEnvironment {
    /// The file the section was read from.
    pub path: PathBuf,
    /// The environment's name.
    pub name: String,
    /// Whether the operator *named* this environment (`--environment` or
    /// [`ENVIRONMENT_VAR`]) rather than falling into it by default. This is what
    /// decides whether the file outranks the ambient `PG*`/`DATABASE_URL`
    /// variables — see `sc_cli::db::DbConfig`.
    pub explicit: bool,
    /// The connection parameters.
    pub section: Environment,
}

impl SelectedEnvironment {
    /// A one-line description for the startup log: which environment, from where.
    pub fn describe(&self) -> String {
        format!("the `{}` environment of {}", self.name, self.path.display())
    }
}

/// Find the configuration file, load it, and select an environment.
///
/// `path_flag` is `--config`'s value and `env_flag` is `--environment`'s; both
/// fall back to their environment variables ([`CONFIG_PATH_VAR`],
/// [`ENVIRONMENT_VAR`]).
///
/// Returns `Ok(None)` when there is simply no configuration to apply — no file
/// on any searched path, or a file that defines no environments — provided the
/// operator did not name an environment. If they did, every one of those cases
/// is an error instead: they asked for staging, and being handed production's
/// defaults because a file was missing is the failure this is here to prevent.
pub fn select(
    path_flag: Option<&str>,
    env_flag: Option<&str>,
) -> Result<Option<SelectedEnvironment>> {
    let selected = env_flag
        .map(str::to_owned)
        .or_else(|| env_var(ENVIRONMENT_VAR));
    let explicit = selected.is_some();

    // An explicitly given path must exist; a searched one need not.
    let (path, required) = match path_flag
        .map(str::to_owned)
        .or_else(|| env_var(CONFIG_PATH_VAR))
    {
        Some(p) => (Some(PathBuf::from(p)), true),
        None => (locate(), false),
    };
    let Some(path) = path else {
        return match selected {
            Some(name) => Err(Error::config(format!(
                "--environment {name} was given, but no configuration file was found (searched: {})",
                describe_search_paths()
            ))),
            None => Ok(None),
        };
    };

    let Some(file) = ConfigFile::load(&path)? else {
        return match (required, selected) {
            (true, _) => Err(Error::config(format!(
                "the configuration file `{}` does not exist",
                path.display()
            ))),
            (false, Some(name)) => Err(Error::config(format!(
                "--environment {name} was given, but no configuration file was found (searched: {})",
                describe_search_paths()
            ))),
            (false, None) => Ok(None),
        };
    };

    if file.environments.is_empty() {
        return match selected {
            Some(name) => Err(Error::config(format!(
                "--environment {name} was given, but the configuration file `{}` \
                 defines no [environments.*] sections",
                path.display()
            ))),
            None => Ok(None),
        };
    }

    let name = selected
        .or_else(|| file.default_environment.clone())
        .unwrap_or_else(|| DEFAULT_ENVIRONMENT.to_owned());
    let section = file.environment(&name, &path)?.clone();
    Ok(Some(SelectedEnvironment {
        path,
        name,
        explicit,
        section,
    }))
}

/// The first existing file among [`search_paths`].
pub fn locate() -> Option<PathBuf> {
    search_paths().into_iter().find(|p| p.is_file())
}

/// Where the configuration file is looked for, most specific first: the user's
/// configuration directory, then the system-wide one.
///
/// The platform conventions, which is the whole point of asking rather than
/// hard-coding `~/.feldspar`:
///
/// | | user | system |
/// |---|---|---|
/// | Linux/BSD | `$XDG_CONFIG_HOME/feldspar/` (else `~/.config/feldspar/`) | `/etc/feldspar/` |
/// | macOS | `~/Library/Application Support/feldspar/` | `/etc/feldspar/` |
/// | Windows | `%APPDATA%\feldspar\` | `%PROGRAMDATA%\feldspar\` |
///
/// Written against `std::env` rather than a directories crate: these are four
/// variables and two fallbacks, and the platform seam is small enough that a
/// dependency in the tree would cost more to justify than the rules cost to
/// state. On macOS `$XDG_CONFIG_HOME` is still honoured when it is set, since a
/// developer who exports it means it.
pub fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = user_config_dir() {
        paths.push(dir.join(APP_DIR).join(FILE_NAME));
    }
    if let Some(dir) = system_config_dir() {
        paths.push(dir.join(APP_DIR).join(FILE_NAME));
    }
    paths
}

/// [`search_paths`], rendered for an error message.
fn describe_search_paths() -> String {
    let paths = search_paths();
    if paths.is_empty() {
        return "no candidate paths — neither a home nor a system configuration \
                directory could be determined"
            .to_owned();
    }
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The platform's per-user configuration directory.
fn user_config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return env_var("APPDATA").map(PathBuf::from);
    }
    if let Some(xdg) = env_var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg));
    }
    let home = PathBuf::from(env_var("HOME")?);
    if cfg!(target_os = "macos") {
        Some(home.join("Library").join("Application Support"))
    } else {
        Some(home.join(".config"))
    }
}

/// The platform's system-wide configuration directory.
fn system_config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env_var("PROGRAMDATA").map(PathBuf::from)
    } else {
        Some(PathBuf::from("/etc"))
    }
}

/// Read an environment variable, treating empty as absent.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Warn — once, on stderr — when the file other people can read holds a password.
///
/// Not an error: refusing to start over a file mode would be a bad trade for a
/// deployment that has decided its own access rules. But a database password in
/// a file other people can read is worth a sentence, and the operator is the
/// only one who can see it.
///
/// "Other people" is the part worth being careful about. `scripts/setup-host.sh`
/// writes this file `root:feldspar 0640` on purpose: the server reads its
/// configuration and never writes it, so leaving it to root means a compromised
/// server cannot rewrite the file that decides what the next restart does. The
/// group bit is what makes that readable at all, and warning about it would be
/// telling the operator to undo the hardened layout. So group access counts only
/// when the group is *not* the one this process is running as — when it really
/// is somebody else.
#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let mode = meta.permissions().mode();
    // SAFETY: `getegid` reads one field of the calling process and cannot fail.
    let own_group = unsafe { libc::getegid() };
    if reachable_by_others(mode, meta.gid(), own_group) {
        eprintln!(
            "feldspar: warning: the configuration file {} is readable by other users \
             (mode {:o}); it may contain database passwords — consider `chmod 640` \
             with a group only the server is in, or `chmod 600`",
            path.display(),
            mode & 0o777,
        );
    }
}

/// Whether anyone but the owner and the server's own group can reach the file.
///
/// Split out from [`warn_if_world_readable`] because the interesting half is the
/// decision, not the `eprintln!`, and the decision is three numbers.
#[cfg(unix)]
fn reachable_by_others(mode: u32, gid: u32, own_gid: u32) -> bool {
    let group_reaches = mode & 0o070 != 0 && gid != own_gid;
    let others_reach = mode & 0o007 != 0;
    group_reaches || others_reach
}

/// No file modes to check off Unix.
#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
default_environment = "staging"

[environments.production]
host = "db.internal"
port = 5432
user = "saltcorn"
password = "hunter2"
database = "saltcorn"

[environments.staging]
url = "postgres://sc:pw@staging:5432/sc"

[environments.test]
database = "saltcorn_test"
test_template = "saltcorn_template"
"#;

    fn parse(text: &str) -> Result<ConfigFile> {
        ConfigFile::parse(text, Path::new("feldspar.toml"))
    }

    #[test]
    fn parses_any_number_of_environments() {
        let file = parse(SAMPLE).expect("parse");
        assert_eq!(file.default_environment.as_deref(), Some("staging"));
        let names: Vec<&str> = file.environments.keys().map(String::as_str).collect();
        assert_eq!(names, ["production", "staging", "test"]);

        let prod = &file.environments["production"];
        assert_eq!(prod.host.as_deref(), Some("db.internal"));
        assert_eq!(prod.port, Some(5432));
        assert_eq!(prod.database.as_deref(), Some("saltcorn"));
        assert!(prod.url.is_none());

        assert_eq!(
            file.environments["staging"].url.as_deref(),
            Some("postgres://sc:pw@staging:5432/sc")
        );
        assert_eq!(
            file.environments["test"].database.as_deref(),
            Some("saltcorn_test")
        );
    }

    #[test]
    fn an_environment_may_list_extra_base_domains() {
        let file = parse(
            "[environments.laptop]\nbase_domain = \"localhost\"\n\
             extra_base_domains = [\"10.0.2.2.nip.io\", \"192.168.1.50.nip.io\"]\n",
        )
        .expect("parses");
        assert_eq!(
            file.environments["laptop"].extra_base_domains,
            ["10.0.2.2.nip.io", "192.168.1.50.nip.io"]
        );
        // Absent is none, not an error: most deployments have one domain.
        let file = parse("[environments.prod]\nbase_domain = \"example.com\"\n").expect("parses");
        assert!(file.environments["prod"].extra_base_domains.is_empty());
    }

    #[test]
    fn an_environment_may_name_a_sqlite_file() {
        let file = ConfigFile::parse(
            "[environments.laptop]\nsqlite = \"/home/me/app.sqlite\"\n",
            Path::new("test.toml"),
        )
        .expect("parses");
        let section = file
            .environment("laptop", Path::new("test.toml"))
            .expect("a section naming one database is fine");
        assert_eq!(section.sqlite.as_deref(), Some("/home/me/app.sqlite"));
        assert!(section.url.is_none() && section.host.is_none());
    }

    /// Two databases in one section is the accident this file exists to
    /// prevent, so it is an error and it names the keys that clash.
    #[test]
    fn an_environment_naming_a_file_and_a_server_is_refused() {
        let file = ConfigFile::parse(
            "[environments.production]\nsqlite = \"/a.sqlite\"\nhost = \"db\"\ndatabase = \"x\"\n",
            Path::new("test.toml"),
        )
        .expect("it parses; it is the reading that refuses it");
        let error = file
            .environment("production", Path::new("test.toml"))
            .expect_err("one environment is one database");
        let text = format!("{error}");
        assert!(text.contains("SQLite"), "{text}");
        assert!(text.contains("host") && text.contains("database"), "{text}");
    }

    /// The HTTPS port is a property of the host, so it lives here and not in
    /// the database; absent is the default (443), not an error.
    #[test]
    fn an_environment_may_pin_the_tls_settings() {
        let file = parse(
            "[environments.production]\nssl_mode = \"letsencrypt\"\n\
             acme_contact_email = \"ops@example.com\"\n\
             acme_directory_url = \"https://acme.example/dir\"\n\
             redirect_http_to_https = false\n\
             ssl_extra_domains = [\"www.example.com\"]\n",
        )
        .expect("parses");
        let env = &file.environments["production"];
        assert_eq!(env.ssl_mode.as_deref(), Some("letsencrypt"));
        assert_eq!(env.acme_contact_email.as_deref(), Some("ops@example.com"));
        assert_eq!(
            env.acme_directory_url.as_deref(),
            Some("https://acme.example/dir")
        );
        assert_eq!(env.redirect_http_to_https, Some(false));
        assert_eq!(
            env.ssl_extra_domains.as_deref(),
            Some(&["www.example.com".to_owned()][..])
        );
        // The certificate itself is not a host key: a pasted PEM belongs to the
        // settings screen, and a typo near these keys must still be refused.
        assert!(parse("[environments.production]\nssl_certificate = \"x\"\n").is_err());
    }

    #[test]
    fn an_environment_may_name_its_https_port() {
        let file = parse("[environments.production]\ndatabase = \"a\"\nhttps_port = 8443\n")
            .expect("parse");
        assert_eq!(file.environments["production"].https_port, Some(8443));
        let file = parse("[environments.production]\ndatabase = \"a\"\n").expect("parse");
        assert_eq!(file.environments["production"].https_port, None);
        assert!(parse("[environments.production]\nhttps_port = 70000\n").is_err());
    }

    #[test]
    fn an_environment_may_name_its_browser() {
        let file = parse(
            "[environments.production]\ndatabase = \"a\"\nbrowser = \"/usr/bin/chromium\"\nbrowser_sandbox = false\n",
        )
        .expect("parse");
        let prod = &file.environments["production"];
        assert_eq!(prod.browser.as_deref(), Some("/usr/bin/chromium"));
        assert_eq!(prod.browser_sandbox, Some(false));
        assert!(file.environments["production"].url.is_none());
    }

    /// A host's CmdStan and its Stan ceilings may live in the file (Stan TODO
    /// §20), and come back as the `serve` flags they mirror.
    #[test]
    fn an_environment_may_carry_the_stan_settings() {
        let file = parse(
            r#"
[environments.production]
database = "a"
cmdstan = "/opt/cmdstan-2.40.0"
stan_cache_dir = "/var/cache/feldspar/stan"
stan_max_processes = 6
stan_max_draws_bytes = 5000000000
stan_summary_max_elements = 0
"#,
        )
        .expect("parse");
        let prod = &file.environments["production"];
        let flags = prod.stan_flags();
        let flags: Vec<(&str, &str)> = flags.iter().map(|(f, v)| (*f, v.as_str())).collect();
        assert_eq!(
            flags,
            [
                ("--cmdstan", "/opt/cmdstan-2.40.0"),
                ("--stan-cache-dir", "/var/cache/feldspar/stan"),
                ("--stan-max-processes", "6"),
                ("--stan-max-draws-bytes", "5000000000"),
                ("--stan-summary-max-elements", "0"),
            ]
        );
        assert!(Environment::default().stan_flags().is_empty());
        // A misspelling is refused like any other key.
        assert!(parse("[environments.production]\nstan_max_proceses = 2\n").is_err());
        // A count is a number, not a string.
        assert!(parse("[environments.production]\nstan_max_processes = \"2\"\n").is_err());
    }

    #[test]
    fn the_test_environment_may_name_a_template_database() {
        let file = parse(SAMPLE).expect("parse");
        assert_eq!(
            file.environments["test"].test_template.as_deref(),
            Some("saltcorn_template"),
            "the harness reads this key; it must survive the round trip"
        );
        assert!(
            file.environments["production"].test_template.is_none(),
            "an environment that does not name one leaves it unset"
        );
    }

    #[test]
    fn a_fourth_environment_needs_no_code_change() {
        let file = parse(
            r#"
[environments.production]
database = "a"

[environments.qa-eu]
database = "b"
"#,
        )
        .expect("parse");
        assert_eq!(file.environments["qa-eu"].database.as_deref(), Some("b"));
    }

    #[test]
    fn a_misspelled_key_is_an_error_not_a_shrug() {
        let err = parse(
            r#"
[environments.production]
databse = "typo"
"#,
        )
        .expect_err("unknown key must be rejected");
        assert!(
            err.to_string().contains("databse"),
            "the error should name the offending key: {err}"
        );
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(parse("[environments.production").is_err());
    }

    #[test]
    fn a_non_numeric_port_is_an_error() {
        assert!(
            parse(
                r#"
[environments.production]
port = "5432"
"#
            )
            .is_err()
        );
    }

    #[test]
    fn selecting_an_undefined_environment_lists_the_defined_ones() {
        let file = parse(SAMPLE).expect("parse");
        let err = file
            .environment("prod", Path::new("/etc/feldspar/feldspar.toml"))
            .expect_err("unknown environment");
        let msg = err.to_string();
        assert!(msg.contains("prod"), "{msg}");
        assert!(msg.contains("production, staging, test"), "{msg}");
    }

    #[test]
    fn an_empty_section_is_empty() {
        assert!(Environment::default().is_empty());
        assert!(
            !Environment {
                database: Some("x".to_owned()),
                ..Environment::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn load_returns_none_for_a_missing_file() {
        let missing = std::env::temp_dir().join("sc-no-such-config-9f3a2b.toml");
        assert_eq!(ConfigFile::load(&missing).expect("load"), None);
    }

    #[test]
    fn search_paths_are_platform_appropriate() {
        // Whatever the platform, the file is looked for under an app directory
        // named `feldspar` and is called `feldspar.toml`.
        for path in search_paths() {
            assert!(
                path.ends_with(Path::new(APP_DIR).join(FILE_NAME)),
                "{path:?}"
            );
        }
    }

    /// Which modes are worth a word on stderr.
    ///
    /// `scripts/setup-host.sh` writes the file `root:feldspar 0640` on purpose —
    /// the server reads its configuration and never writes it, so root keeps
    /// ownership and a compromised server cannot rewrite what the next restart
    /// connects to. Warning about that would be telling the operator to undo the
    /// hardened layout, so the group bit counts only when the group is somebody
    /// else's.
    #[cfg(unix)]
    #[test]
    fn a_group_readable_config_is_only_a_problem_when_the_group_is_someone_else() {
        // The setup-host.sh layout: root:feldspar 0640, read as feldspar.
        assert!(!reachable_by_others(0o640, 42, 42));
        // The same mode, but the group is not the server's.
        assert!(reachable_by_others(0o640, 43, 42));
        // Tight enough either way.
        assert!(!reachable_by_others(0o600, 43, 42));
        // Anyone at all, however the groups fall.
        assert!(reachable_by_others(0o644, 42, 42));
        assert!(reachable_by_others(0o604, 42, 42));
        // Write, not just read: a group that can rewrite the file picks the
        // database the next restart connects to.
        assert!(reachable_by_others(0o620, 43, 42));
        assert!(!reachable_by_others(0o660, 42, 42));
    }
}

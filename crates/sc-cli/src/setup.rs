//! `feldspar setup`: make this machine an instance.
//!
//! The second of the two steps a new machine takes. `scripts/install.sh` put the
//! binary here; this decides what kind of instance it is and gives it what that
//! needs:
//!
//! - **development** — somebody's desktop. The configuration file is the user's
//!   own (`~/.config/feldspar/feldspar.toml`), the server is started by hand with
//!   `feldspar serve`, and nothing runs as anybody but them.
//! - **production** — a server. A `feldspar` system account, the configuration
//!   in `/etc/feldspar/feldspar.toml` readable by that account and nobody else,
//!   and a service started at boot: a systemd unit (Debian, Ubuntu), or an OpenRC
//!   script (Alpine).
//!
//! Either kind takes either database: a **SQLite** file, nothing to install; or
//! **PostgreSQL**, on this machine (installed when it is not already running,
//! reached over its Unix socket with peer authentication, so no password is
//! written anywhere) or on another host (a URL, or its parts, written into the
//! configuration file).
//!
//! Every choice is a flag, and the ones that are not given are asked at a
//! terminal. `--server` (production, local PostgreSQL) and `--dev`
//! (development, SQLite) settle everything and ask nothing.
//!
//! The command **plans** before it does anything: [`plan`] turns the settled
//! [`Choices`] and what was found on the [`Host`] into a list of [`Step`]s —
//! commands and files — which [`describe`] prints (`--dry-run`, and the summary
//! an interactive run confirms) and [`execute`] carries out. One list for both
//! is what makes the dry run trustworthy, and it is what the tests assert on.
//!
//! Every step is safe to repeat: an account or a database that exists is left
//! alone, and a configuration file that already has the environment is left as
//! it is when it says the same thing, and refused (without `--force`) when it
//! says something else — and so is the service file.
//!
//! The server needs two more programs to do everything it can: **Node.js**, with
//! an npm of 9.3 or newer (building an application and installing a module run
//! npm), and a **headless browser**, which the coding agent's `view_app` drives.
//! A production setup installs both, as `scripts/setup-host.sh` does: Node from
//! NodeSource on Debian and Ubuntu (their own npm is 9.2, which cannot install a
//! module — README §2.1), apt's `chromium` on Debian, Google Chrome's apt
//! repository on Ubuntu (whose `chromium-browser` is a snap, which does not run
//! under a service account), and both from apk on Alpine. Whatever is already
//! here and good enough is kept. The browser is then started once, headless, as
//! the account that will run it, so a browser that cannot start is found now
//! rather than at the agent's first `view_app`. A development setup leaves the
//! desktop's packages alone unless asked (`--node`, `--browser`, or yes at the
//! terminal), and says what is missing.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sc_config_file::ConfigFile;
use sc_error::{Error, Result};

/// Where a production instance keeps everything it writes: the service
/// account's home, the unit's working directory, and a SQLite database.
pub const STATE_DIR: &str = "/var/lib/feldspar";
/// The local PostgreSQL's socket directory, on Debian and on Alpine alike
/// (`/var/run` is a link to `/run` on both).
pub const POSTGRES_SOCKET_DIR: &str = "/var/run/postgresql";
/// What a production server listens on unless told otherwise.
pub const PRODUCTION_BIND: &str = "0.0.0.0:80";
/// The systemd unit a production instance is run by.
pub const SYSTEMD_UNIT: &str = "/etc/systemd/system/feldspar.service";
/// The OpenRC script a production instance on Alpine is run by.
pub const OPENRC_SCRIPT: &str = "/etc/init.d/feldspar";
/// Where the OpenRC service's output goes; systemd has the journal.
pub const OPENRC_LOG: &str = "/var/log/feldspar.log";

const DEFAULT_SERVICE_USER: &str = "feldspar";
const DEFAULT_DATABASE: &str = "feldspar";
const DEFAULT_PORT: &str = "3032";

/// The Node.js line installed from NodeSource; the same as `setup-host.sh` and
/// the Docker image.
pub const NODE_MAJOR: u32 = 26;
/// The oldest npm that can install a module, as (major, minor).
pub const MIN_NPM: (u32, u32) = (9, 3);
const NODE_KEYRING: &str = "/etc/apt/keyrings/nodesource.asc";
const NODE_LIST: &str = "/etc/apt/sources.list.d/nodesource.list";
const NODE_PIN: &str = "/etc/apt/preferences.d/nodesource";
const CHROME_KEYRING: &str = "/etc/apt/keyrings/google-chrome.asc";
const CHROME_LIST: &str = "/etc/apt/sources.list.d/google-chrome.list";

pub const USAGE: &str = "\
usage: feldspar setup [--server | --dev] [options]

Make this machine a Saltcorn Feldspar instance. Without --server or --dev, the
choices the flags leave open are asked at the terminal.

  --server               production, PostgreSQL on this machine, ask nothing
  --dev                  development, a SQLite file, ask nothing
  --production           a system service run by a `feldspar` account, the
                         configuration in /etc/feldspar (needs root or sudo)
  --development          started by hand with `feldspar serve`, the
                         configuration in ~/.config/feldspar

Database (default: SQLite for development, local PostgreSQL for production):
  --db sqlite|postgres|remote-postgres
  --sqlite PATH          the SQLite file (created on first start)
  --database-url URL     a PostgreSQL on another host
  --db-host H  --db-port N  --db-password P
                         ...or its parts (any of them means a remote database)
  --db-user U  --db-name D
                         the role and database: created for a local
                         PostgreSQL, connected to for a remote one

Instance:
  --domain DOMAIN        base_domain: applications at <subdomain>.DOMAIN
  --bind ADDR:PORT       listen address (production default 0.0.0.0:80)
  --environment NAME     the [environments.NAME] section to write (default
                         production / development)
  --config PATH          the configuration file to write
  --user NAME            the production service account (default feldspar)
  --binary PATH          the executable the service runs (default: this one)
  --no-start             production: enable the service, do not start it

Node.js and a headless browser (installed for production when missing, left
alone for development unless asked):
  --node / --no-node     install Node.js with npm 9.3+ (NodeSource on Debian
                         and Ubuntu, apk on Alpine)
  --browser / --no-browser
                         install a headless Chromium for the coding agent's
                         view_app, and check it starts as the account that
                         runs the server

  --force                replace an existing, different [environments.NAME]
  -y, --yes              do not ask for confirmation before acting
  -n, --dry-run          print every command and file, change nothing
  -h, --help             this message";

/// Which kind of instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A desktop: `feldspar serve` started by its user.
    Development,
    /// A server: a system account and a service.
    Production,
}

impl Mode {
    fn default_environment(self) -> &'static str {
        match self {
            Mode::Development => "development",
            Mode::Production => "production",
        }
    }
}

/// Which kind of database, before its details are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Sqlite,
    LocalPostgres,
    RemotePostgres,
}

/// A PostgreSQL on another host: a URL, or the parts of one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemotePostgres {
    pub url: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
}

impl RemotePostgres {
    fn any_part(&self) -> bool {
        self.host.is_some() || self.port.is_some() || self.password.is_some()
    }
}

/// The settled database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Database {
    Sqlite {
        path: PathBuf,
    },
    /// On this machine, over its socket: `role` is the account that connects,
    /// which peer authentication requires to be the operating-system user.
    LocalPostgres {
        role: String,
        database: String,
    },
    RemotePostgres(RemotePostgres),
}

/// What the command line said, before anything is asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupArgs {
    pub mode: Option<Mode>,
    /// `--server` or `--dev`: take a default for whatever is left, ask nothing.
    pub preset: bool,
    pub yes: bool,
    pub db: Option<DbKind>,
    pub sqlite: Option<PathBuf>,
    pub remote: RemotePostgres,
    pub db_user: Option<String>,
    pub db_name: Option<String>,
    pub domain: Option<String>,
    pub bind: Option<String>,
    pub environment: Option<String>,
    pub config: Option<PathBuf>,
    pub service_user: Option<String>,
    pub binary: Option<PathBuf>,
    pub no_start: bool,
    /// `--node` / `--no-node`: whether to install Node.js when it is missing.
    pub node: Option<bool>,
    /// `--browser` / `--no-browser`: whether to install a headless browser.
    pub browser: Option<bool>,
    pub force: bool,
    pub dry_run: bool,
    pub help: bool,
}

impl SetupArgs {
    pub fn parse(args: &[String]) -> Result<SetupArgs> {
        let mut parsed = SetupArgs::default();
        let mut rest = args.iter();
        while let Some(raw) = rest.next() {
            // `--flag=value` as well as `--flag value`.
            let (flag, mut inline) = match raw.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
                _ => (raw.as_str(), None),
            };
            let mut value = |name: &str| -> Result<String> {
                inline
                    .take()
                    .or_else(|| rest.next().cloned())
                    .ok_or_else(|| Error::config(format!("{name} needs a value\n{USAGE}")))
            };
            match flag {
                "--server" => parsed.set_mode(Mode::Production, true)?,
                "--dev" => parsed.set_mode(Mode::Development, true)?,
                "--production" => parsed.set_mode(Mode::Production, false)?,
                "--development" => parsed.set_mode(Mode::Development, false)?,
                "--db" => {
                    parsed.db = Some(match value("--db")?.as_str() {
                        "sqlite" => DbKind::Sqlite,
                        "postgres" | "postgresql" | "local-postgres" => DbKind::LocalPostgres,
                        "remote-postgres" => DbKind::RemotePostgres,
                        other => {
                            return Err(Error::config(format!(
                                "--db is sqlite, postgres or remote-postgres, not `{other}`"
                            )));
                        }
                    })
                }
                "--sqlite" => parsed.sqlite = Some(PathBuf::from(value("--sqlite")?)),
                "--database-url" => {
                    let url = value("--database-url")?;
                    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
                        return Err(Error::config(format!(
                            "--database-url should be a postgres:// URL, got `{url}` \
                             (a SQLite file is --sqlite PATH)"
                        )));
                    }
                    parsed.remote.url = Some(url);
                }
                "--db-host" => parsed.remote.host = Some(value("--db-host")?),
                "--db-port" => {
                    let port = value("--db-port")?;
                    parsed.remote.port = Some(port.parse().map_err(|_| {
                        Error::config(format!("--db-port should be a port number, got `{port}`"))
                    })?);
                }
                "--db-password" => parsed.remote.password = Some(value("--db-password")?),
                "--db-user" => parsed.db_user = Some(value("--db-user")?),
                "--db-name" => parsed.db_name = Some(value("--db-name")?),
                "--domain" => parsed.domain = Some(value("--domain")?),
                "--bind" => parsed.bind = Some(value("--bind")?),
                "--environment" => parsed.environment = Some(value("--environment")?),
                "--config" => parsed.config = Some(PathBuf::from(value("--config")?)),
                "--user" => parsed.service_user = Some(value("--user")?),
                "--binary" => parsed.binary = Some(PathBuf::from(value("--binary")?)),
                "--no-start" => parsed.no_start = true,
                "--node" => parsed.node = Some(true),
                "--no-node" => parsed.node = Some(false),
                "--browser" => parsed.browser = Some(true),
                "--no-browser" => parsed.browser = Some(false),
                "--force" => parsed.force = true,
                "-y" | "--yes" => parsed.yes = true,
                "-n" | "--dry-run" => parsed.dry_run = true,
                "-h" | "--help" => parsed.help = true,
                other => {
                    return Err(Error::config(format!(
                        "unknown setup option `{other}`\n{USAGE}"
                    )));
                }
            }
            if inline.is_some() {
                return Err(Error::config(format!("{flag} takes no value")));
            }
        }
        parsed.check()?;
        Ok(parsed)
    }

    fn set_mode(&mut self, mode: Mode, preset: bool) -> Result<()> {
        if self.mode.is_some_and(|m| m != mode) {
            return Err(Error::config(
                "choose one of --server/--production and --dev/--development",
            ));
        }
        self.mode = Some(mode);
        self.preset |= preset;
        Ok(())
    }

    /// The database kind the flags imply, if they imply one.
    pub fn db_kind(&self) -> Option<DbKind> {
        if self.db.is_some() {
            return self.db;
        }
        if self.remote.url.is_some() || self.remote.any_part() {
            Some(DbKind::RemotePostgres)
        } else if self.sqlite.is_some() {
            Some(DbKind::Sqlite)
        } else {
            None
        }
    }

    fn check(&self) -> Result<()> {
        let remote = self.remote.url.is_some() || self.remote.any_part();
        if self.sqlite.is_some() && remote {
            return Err(Error::config(
                "--sqlite and a PostgreSQL connection (--database-url, --db-host…) are \
                 two different databases; give one",
            ));
        }
        match self.db {
            Some(DbKind::Sqlite) if remote => Err(Error::config(
                "--db sqlite with PostgreSQL connection flags; drop one of them",
            )),
            Some(DbKind::LocalPostgres) if remote => Err(Error::config(
                "--db postgres is the PostgreSQL on this machine; a connection to another \
                 host is --db remote-postgres (or just the connection flags)",
            )),
            Some(DbKind::LocalPostgres | DbKind::RemotePostgres) if self.sqlite.is_some() => Err(
                Error::config("--sqlite with --db postgres; drop one of them"),
            ),
            _ => Ok(()),
        }
    }
}

/// The Linux family, which decides the package manager and the account tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Debian and Ubuntu: apt, adduser, systemd.
    Debian,
    /// Alpine: apk, busybox's adduser, OpenRC.
    Alpine,
    Other,
}

/// What starts services at boot here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceManager {
    Systemd,
    OpenRc,
    None,
}

/// What [`plan`] needs to know about this machine. Detected by [`Host::detect`];
/// built by hand in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub family: Family,
    /// `PRETTY_NAME` from `/etc/os-release`, for messages.
    pub os_name: String,
    pub root: bool,
    /// `sudo` or `doas`, when not root and one is installed.
    pub sudo: Option<String>,
    pub services: ServiceManager,
    /// The login running this command: a development PostgreSQL role is theirs.
    pub login: String,
    /// A PostgreSQL is already listening on [`POSTGRES_SOCKET_DIR`].
    pub postgres_running: bool,
    /// What `npm --version` says, when there is an npm on `PATH`.
    pub npm: Option<String>,
    /// The browser the server would find on `PATH` (not a snap), when there is one.
    pub browser: Option<PathBuf>,
    /// Ubuntu or a derivative, whose apt `chromium-browser` is a snap.
    pub ubuntu: bool,
    /// The machine, as Rust names it (`x86_64`, `aarch64`).
    pub arch: String,
    /// The service file setup would write ([`SYSTEMD_UNIT`] or
    /// [`OPENRC_SCRIPT`]), as it is now — an operator may have edited it.
    pub service_file: Option<String>,
}

impl Host {
    pub fn detect() -> Host {
        let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let (family, os_name) = parse_os_release(&release);
        let ubuntu = ubuntu_like(&release);
        // SAFETY: `geteuid` reads the process's credentials and cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        let sudo = if root {
            None
        } else {
            ["sudo", "doas"]
                .into_iter()
                .find(|tool| which(tool).is_some())
                .map(str::to_owned)
        };
        let services = if Path::new("/run/systemd/system").is_dir() {
            ServiceManager::Systemd
        } else if Path::new("/sbin/openrc-run").exists() || which("openrc-run").is_some() {
            ServiceManager::OpenRc
        } else {
            ServiceManager::None
        };
        let login = std::env::var("USER")
            .ok()
            .filter(|u| !u.is_empty())
            .or_else(|| {
                Command::new("id")
                    .arg("-un")
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            })
            .unwrap_or_default();
        let service_file = match services {
            ServiceManager::Systemd => std::fs::read_to_string(SYSTEMD_UNIT).ok(),
            ServiceManager::OpenRc => std::fs::read_to_string(OPENRC_SCRIPT).ok(),
            ServiceManager::None => None,
        };
        Host {
            family,
            os_name,
            root,
            sudo,
            services,
            login,
            service_file,
            postgres_running: Path::new(POSTGRES_SOCKET_DIR)
                .join(".s.PGSQL.5432")
                .exists(),
            npm: which("npm").and_then(|npm| {
                let out = Command::new(npm)
                    .arg("--version")
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            }),
            browser: sc_server::detect_browser(None).ok(),
            ubuntu,
            arch: std::env::consts::ARCH.to_owned(),
        }
    }

    /// The npm here is one the server can install a module with.
    pub fn npm_ok(&self) -> bool {
        self.npm.as_deref().is_some_and(npm_new_enough)
    }
}

/// Whether an npm version is [`MIN_NPM`] or newer. npm before 9.3.0 cannot
/// install a module: it dies with `Invalid comparator: file:/…` on the
/// `file:` overrides every module's package.json carries (README §2.1).
pub fn npm_new_enough(version: &str) -> bool {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let mut number = || parts.next().and_then(|p| p.parse::<u32>().ok());
    match (number(), number()) {
        (Some(major), Some(minor)) => (major, minor) >= MIN_NPM,
        _ => false,
    }
}

/// Ubuntu, or a distribution built on it (`ID_LIKE=ubuntu`): where apt's
/// Chromium is a snap.
pub fn ubuntu_like(os_release: &str) -> bool {
    os_release.lines().any(|l| {
        let value = |key: &str| {
            l.strip_prefix(key)
                .and_then(|v| v.strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_owned())
        };
        value("ID").is_some_and(|v| v == "ubuntu")
            || value("ID_LIKE").is_some_and(|v| v.split_whitespace().any(|w| w == "ubuntu"))
    })
}

/// The family and display name out of an `/etc/os-release`.
pub fn parse_os_release(text: &str) -> (Family, String) {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_owned())
            .unwrap_or_default()
    };
    let id = field("ID");
    let like = field("ID_LIKE");
    let is = |name: &str| id == name || like.split_whitespace().any(|l| l == name);
    let family = if is("debian") || is("ubuntu") {
        Family::Debian
    } else if is("alpine") {
        Family::Alpine
    } else {
        Family::Other
    };
    let name = Some(field("PRETTY_NAME"))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            if id.is_empty() {
                "an unknown system".into()
            } else {
                id
            }
        });
    (family, name)
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(tool))
        .find(|p| p.is_file())
}

/// Everything settled: what [`plan`] works from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choices {
    pub mode: Mode,
    pub database: Database,
    pub environment: String,
    pub config_path: PathBuf,
    pub domain: Option<String>,
    pub bind: Option<String>,
    /// The production service account (unused in development).
    pub service_user: String,
    /// The executable a production service runs.
    pub binary: PathBuf,
    pub start: bool,
    /// Install Node.js when the npm here is missing or too old.
    pub node: bool,
    /// Install a headless browser when there is none, and check that it starts.
    pub browser: bool,
    pub force: bool,
}

/// Questions at a terminal. Generic over its streams so a test can answer them.
pub struct Prompter<'a> {
    input: Box<dyn BufRead + 'a>,
    output: Box<dyn Write + 'a>,
    /// Whether `input` is a real terminal, where a password is read without echo.
    terminal: bool,
}

impl<'a> Prompter<'a> {
    pub fn new(input: impl BufRead + 'a, output: impl Write + 'a, terminal: bool) -> Self {
        Prompter {
            input: Box::new(input),
            output: Box::new(output),
            terminal,
        }
    }

    fn line(&mut self, prompt: &str) -> Result<String> {
        let _ = write!(self.output, "{prompt}");
        let _ = self.output.flush();
        let mut line = String::new();
        let read = self
            .input
            .read_line(&mut line)
            .map_err(|e| Error::msg(format!("reading an answer: {e}")))?;
        if read == 0 {
            return Err(Error::config(
                "no answer (end of input); pass the choices as flags — see feldspar setup --help",
            ));
        }
        Ok(line.trim().to_owned())
    }

    /// Pick one of `options` by number; empty takes `default` (0-based).
    pub fn choose(&mut self, question: &str, options: &[&str], default: usize) -> Result<usize> {
        let _ = writeln!(self.output, "\n{question}");
        for (i, option) in options.iter().enumerate() {
            let _ = writeln!(self.output, "  {}) {option}", i + 1);
        }
        loop {
            let answer = self.line(&format!("Choice [{}]: ", default + 1))?;
            if answer.is_empty() {
                return Ok(default);
            }
            match answer.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
                _ => {
                    let _ = writeln!(self.output, "  a number from 1 to {}", options.len());
                }
            }
        }
    }

    /// A free answer; empty takes `default`.
    pub fn ask(&mut self, question: &str, default: Option<&str>) -> Result<String> {
        let prompt = match default {
            Some(d) if !d.is_empty() => format!("{question} [{d}]: "),
            _ => format!("{question}: "),
        };
        let answer = self.line(&prompt)?;
        Ok(if answer.is_empty() {
            default.unwrap_or_default().to_owned()
        } else {
            answer
        })
    }

    /// An answer that must not be empty.
    pub fn require(&mut self, question: &str) -> Result<String> {
        loop {
            let answer = self.ask(question, None)?;
            if !answer.is_empty() {
                return Ok(answer);
            }
        }
    }

    /// A password: without echo at a terminal.
    pub fn secret(&mut self, question: &str) -> Result<String> {
        if self.terminal {
            crate::users::read_hidden(&format!("{question}: "))
        } else {
            self.ask(question, None)
        }
    }

    pub fn confirm(&mut self, question: &str) -> Result<bool> {
        let answer = self.line(&format!("{question} [Y/n]: "))?;
        Ok(matches!(
            answer.to_ascii_lowercase().as_str(),
            "" | "y" | "yes"
        ))
    }
}

/// Settle every choice: from the flags, then from `prompt` when there is one,
/// then from the defaults of the chosen mode.
pub fn resolve(
    args: &SetupArgs,
    host: &Host,
    mut prompt: Option<&mut Prompter>,
) -> Result<Choices> {
    if args.preset {
        prompt = None;
    }
    let mode = match (args.mode, prompt.as_deref_mut()) {
        (Some(mode), _) => mode,
        (None, Some(p)) => {
            let pick = p.choose(
                "What kind of instance is this?",
                &[
                    "development — on this desktop; you start `feldspar serve` yourself",
                    "production — a server: a `feldspar` system account and a service \
                     started at boot",
                ],
                0,
            )?;
            [Mode::Development, Mode::Production][pick]
        }
        (None, None) => {
            return Err(Error::config(
                "say what kind of instance: --dev (development, SQLite) or --server \
                 (production, local PostgreSQL), or --development/--production with \
                 the database flags — or run `feldspar setup` at a terminal to be asked",
            ));
        }
    };

    let service_user = args
        .service_user
        .clone()
        .unwrap_or_else(|| DEFAULT_SERVICE_USER.to_owned());
    identifier("--user", &service_user)?;

    let kind = match (args.db_kind(), prompt.as_deref_mut()) {
        (Some(kind), _) => kind,
        (None, Some(p)) => {
            let default = if mode == Mode::Development { 0 } else { 1 };
            let pick = p.choose(
                "Which database?",
                &[
                    "SQLite — one file, nothing to install",
                    "PostgreSQL on this machine — installed if it is not already running",
                    "PostgreSQL on another host",
                ],
                default,
            )?;
            [
                DbKind::Sqlite,
                DbKind::LocalPostgres,
                DbKind::RemotePostgres,
            ][pick]
        }
        (None, None) => match mode {
            Mode::Development => DbKind::Sqlite,
            Mode::Production => DbKind::LocalPostgres,
        },
    };

    let database = match kind {
        DbKind::Sqlite => Database::Sqlite {
            path: match &args.sqlite {
                Some(path) => absolute(path)?,
                None => match mode {
                    Mode::Development => sc_files::data_dir()?.join("feldspar.sqlite"),
                    Mode::Production => Path::new(STATE_DIR).join("feldspar.sqlite"),
                },
            },
        },
        DbKind::LocalPostgres => {
            // Peer authentication over the socket: the role is whoever connects.
            let connecting = match mode {
                Mode::Development => host.login.clone(),
                Mode::Production => service_user.clone(),
            };
            let role = args.db_user.clone().unwrap_or_else(|| connecting.clone());
            if role != connecting {
                return Err(Error::config(format!(
                    "a PostgreSQL on this machine is reached over its socket with peer \
                     authentication, so the role must be the account that connects \
                     (`{connecting}`), not `{role}`; leave --db-user out{}",
                    if mode == Mode::Production {
                        ", or change --user"
                    } else {
                        ""
                    }
                )));
            }
            identifier("the PostgreSQL role", &role)?;
            let database = args
                .db_name
                .clone()
                .unwrap_or_else(|| DEFAULT_DATABASE.to_owned());
            identifier("--db-name", &database)?;
            Database::LocalPostgres { role, database }
        }
        DbKind::RemotePostgres => {
            let mut remote = args.remote.clone();
            remote.user = remote.user.or_else(|| args.db_user.clone());
            remote.database = remote.database.or_else(|| args.db_name.clone());
            if remote.url.is_none() && remote.host.is_none() {
                let Some(p) = prompt.as_deref_mut() else {
                    return Err(Error::config(
                        "a PostgreSQL on another host needs --database-url URL, or \
                         --db-host with --db-user and --db-name",
                    ));
                };
                let url = p.ask(
                    "\nConnection URL (postgres://user:password@host:5432/database), \
                     or empty to give the parts",
                    None,
                )?;
                if url.is_empty() {
                    remote.host = Some(p.require("Host")?);
                    let port = p.ask("Port", Some("5432"))?;
                    remote.port =
                        Some(port.parse().map_err(|_| {
                            Error::config(format!("`{port}` is not a port number"))
                        })?);
                    if remote.user.is_none() {
                        remote.user = Some(p.require("User")?);
                    }
                    let password = p.secret("Password (empty for none)")?;
                    remote.password = Some(password).filter(|p| !p.is_empty());
                    if remote.database.is_none() {
                        remote.database = Some(p.ask("Database", Some(DEFAULT_DATABASE))?);
                    }
                } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
                    remote.url = Some(url);
                } else {
                    return Err(Error::config(format!("`{url}` is not a postgres:// URL")));
                }
            }
            if remote.url.is_none() && (remote.user.is_none() || remote.database.is_none()) {
                return Err(Error::config(
                    "a PostgreSQL on another host given by its parts needs --db-user and \
                     --db-name as well as --db-host",
                ));
            }
            if remote.url.is_some() && (remote.any_part() || remote.user.is_some()) {
                return Err(Error::config(
                    "give a remote PostgreSQL as --database-url or as its parts, not both",
                ));
            }
            Database::RemotePostgres(remote)
        }
    };

    let mut domain = args.domain.clone();
    let mut bind = args.bind.clone();
    if mode == Mode::Production
        && let Some(p) = prompt.as_deref_mut()
    {
        if domain.is_none() {
            let answer = p.ask(
                "\nDomain applications are served under, as <app>.DOMAIN (empty: the \
                 admin UI only, for now)",
                None,
            )?;
            domain = Some(answer).filter(|d| !d.is_empty());
        }
        if bind.is_none() {
            bind = Some(p.ask("Listen address", Some(PRODUCTION_BIND))?);
        }
    }
    if mode == Mode::Production && bind.is_none() {
        bind = Some(PRODUCTION_BIND.to_owned());
    }
    if let Some(d) = &domain
        && (d.is_empty() || d.contains(|c: char| c.is_whitespace() || c == '/' || c == ':'))
    {
        return Err(Error::config(format!(
            "--domain is a bare domain like example.com, not `{d}`"
        )));
    }

    // Node.js and a browser: a server gets them, as setup-host.sh gives them; a
    // desktop's packages are its owner's, so there they are installed only when
    // asked — and asked about only when one is missing and setup knows how.
    let mut install = |flag: Option<bool>, missing: bool, question: &str| -> Result<bool> {
        Ok(match (flag, mode, prompt.as_deref_mut()) {
            (Some(yes), _, _) => yes,
            (None, Mode::Production, _) => true,
            (None, Mode::Development, Some(p)) if missing && host.family != Family::Other => {
                p.confirm(question)?
            }
            (None, Mode::Development, _) => false,
        })
    };
    let node = install(
        args.node,
        !host.npm_ok(),
        "\nBuilding an application runs npm, and there is no npm 9.3 or newer here. \
         Install Node.js (as root)?",
    )?;
    let browser = install(
        args.browser,
        host.browser.is_none(),
        "\nThe coding agent looks at what it built through a headless browser, and \
         there is none here. Install Chromium (as root)?",
    )?;

    let environment = args
        .environment
        .clone()
        .unwrap_or_else(|| mode.default_environment().to_owned());
    if environment.is_empty()
        || !environment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::config(format!(
            "--environment is letters, digits, `_` and `-`, not `{environment}`"
        )));
    }

    let config_path = match &args.config {
        Some(path) => absolute(path)?,
        None => match mode {
            Mode::Development => sc_config_file::user_config_file(),
            Mode::Production => sc_config_file::system_config_file(),
        }
        .ok_or_else(|| {
            Error::config("cannot tell where the configuration file goes; pass --config PATH")
        })?,
    };

    let binary = match &args.binary {
        Some(path) => absolute(path)?,
        None => std::env::current_exe()
            .map_err(|e| Error::msg(format!("finding this executable: {e}")))?,
    };

    Ok(Choices {
        mode,
        database,
        environment,
        config_path,
        domain,
        bind,
        service_user,
        binary,
        start: !args.no_start,
        node,
        browser,
        force: args.force,
    })
}

/// A name that goes into SQL and shell commands unquoted: a role, a database,
/// an account. Lower-case letters, digits and `_`, which every one of them
/// accepts as it is.
fn identifier(what: &str, name: &str) -> Result<()> {
    let ok = name.len() <= 63
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::config(format!(
            "{what} `{name}` should be lower-case letters, digits and `_`"
        )))
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|e| Error::msg(format!("{}: {e}", path.display())))
}

/// One thing setup does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A heading for the steps after it.
    Heading(String),
    /// Something worth saying, done nothing about.
    Note(String),
    /// A command; as root when `root`. Skipped when `unless` is given and
    /// succeeds — which is how a step that must not be repeated is made safe to
    /// run again.
    Run {
        argv: Vec<String>,
        root: bool,
        unless: Option<Vec<String>>,
    },
    /// A file with exactly these contents, mode and (when `owner`) owner.
    Write {
        path: PathBuf,
        contents: String,
        mode: u32,
        owner: Option<String>,
        root: bool,
    },
    /// Start the browser once, headless, as `user` (as whoever runs setup when
    /// `None`), the way the server will. Reported, never fatal: a browser that
    /// does not start leaves `view_app` unavailable and everything else working.
    /// `browser` is `None` when it is installed by an earlier step, and is then
    /// found on `PATH` as the server finds it.
    CheckBrowser {
        browser: Option<PathBuf>,
        user: Option<String>,
        /// Where `browser_sandbox = false` would go, for the advice.
        config: String,
    },
}

fn run(argv: &[&str], root: bool) -> Step {
    Step::Run {
        argv: argv.iter().map(|s| (*s).to_owned()).collect(),
        root,
        unless: None,
    }
}

/// `step`, unless the plan already has it: an `apt-get update` or a package
/// that two of the installs below both need.
fn once(steps: &mut Vec<Step>, step: Step) {
    if !steps.contains(&step) {
        steps.push(step);
    }
}

fn apt_install(packages: &[&str]) -> Step {
    let mut argv = vec![
        "env",
        "DEBIAN_FRONTEND=noninteractive",
        "apt-get",
        "install",
        "-y",
    ];
    argv.extend_from_slice(packages);
    run(&argv, true)
}

/// A root-owned apt source or pin.
fn apt_file(path: &str, contents: String) -> Step {
    Step::Write {
        path: path.into(),
        contents,
        mode: 0o644,
        owner: None,
        root: true,
    }
}

/// A third-party apt repository: its key, as an ASCII-armoured `.asc` (which
/// apt reads as it is — no gpg to install), and its source line.
fn apt_repository(steps: &mut Vec<Step>, key_url: &str, keyring: &str, list: &str, line: String) {
    once(steps, run(&["apt-get", "update"], true));
    once(steps, apt_install(&["ca-certificates", "curl"]));
    once(
        steps,
        run(&["install", "-d", "-m", "0755", "/etc/apt/keyrings"], true),
    );
    steps.push(run(&["curl", "-fsSL", key_url, "-o", keyring], true));
    steps.push(run(&["chmod", "0644", keyring], true));
    steps.push(apt_file(list, line));
}

/// Node.js with an npm that can install a module, unless there is one.
fn node_steps(steps: &mut Vec<Step>, host: &Host) {
    if host.npm_ok() {
        steps.push(Step::Note(format!(
            "npm {} is already here and new enough; leaving Node.js alone",
            host.npm.as_deref().unwrap_or_default()
        )));
        return;
    }
    if let Some(old) = &host.npm {
        steps.push(Step::Note(format!(
            "npm {old} is too old to install a module ({}.{} or newer is needed)",
            MIN_NPM.0, MIN_NPM.1
        )));
    }
    match host.family {
        Family::Debian => {
            // From NodeSource, not `apt install nodejs npm`: Debian 12 and 13 and
            // Ubuntu 24.04 all package npm 9.2.0. NodeSource's `nodejs` carries
            // its own npm and replaces the distribution's.
            steps.push(Step::Heading(format!(
                "installing Node.js {NODE_MAJOR}.x from NodeSource (the distribution's npm \
                 is too old)"
            )));
            apt_repository(
                steps,
                "https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key",
                NODE_KEYRING,
                NODE_LIST,
                format!(
                    "deb [signed-by={NODE_KEYRING}] https://deb.nodesource.com/node_{NODE_MAJOR}.x \
                     nodistro main\n"
                ),
            );
            // Pinned, so a distribution `nodejs` with a higher version string
            // cannot later replace it and bring the old npm back.
            steps.push(apt_file(
                NODE_PIN,
                "Package: nodejs\nPin: origin deb.nodesource.com\nPin-Priority: 600\n".into(),
            ));
            steps.push(run(&["apt-get", "update"], true));
            steps.push(apt_install(&["nodejs"]));
        }
        Family::Alpine => {
            steps.push(Step::Heading("installing Node.js and npm".into()));
            steps.push(run(&["apk", "add", "--no-cache", "nodejs", "npm"], true));
        }
        Family::Other => steps.push(Step::Note(format!(
            "setup cannot install Node.js on {}: install it, with npm {}.{} or newer",
            host.os_name, MIN_NPM.0, MIN_NPM.1
        ))),
    }
}

/// A headless browser for `view_app`, unless there is one. Whether there will
/// be one to check afterwards.
fn browser_steps(steps: &mut Vec<Step>, c: &Choices, host: &Host) -> bool {
    if let Some(path) = &host.browser {
        steps.push(Step::Note(format!(
            "a browser the server can use is already here: {}",
            path.display()
        )));
        return true;
    }
    let unavailable = |why: String| {
        Step::Note(format!(
            "{why}: view_app will be unavailable until one is installed (and named with \
             `browser` in {} when it is not on PATH)",
            c.config_path.display()
        ))
    };
    match host.family {
        // Ubuntu's apt `chromium-browser` is a transitional package that execs
        // the snap, and a snap does not run under a service account with
        // ProtectHome and PrivateTmp; Google Chrome's own repository instead.
        Family::Debian if host.ubuntu => {
            if host.arch != "x86_64" {
                steps.push(unavailable(format!(
                    "no Chromium that is not a snap is known for Ubuntu on {}",
                    host.arch
                )));
                return false;
            }
            steps.push(Step::Heading(
                "installing Google Chrome from Google's apt repository (Ubuntu's Chromium is \
                 a snap, which does not run under a service account)"
                    .into(),
            ));
            apt_repository(
                steps,
                "https://dl.google.com/linux/linux_signing_key.pub",
                CHROME_KEYRING,
                CHROME_LIST,
                format!(
                    "deb [arch=amd64 signed-by={CHROME_KEYRING}] \
                     https://dl.google.com/linux/chrome/deb/ stable main\n"
                ),
            );
            steps.push(run(&["apt-get", "update"], true));
            steps.push(apt_install(&["google-chrome-stable"]));
        }
        Family::Debian => {
            steps.push(Step::Heading("installing Chromium".into()));
            once(steps, run(&["apt-get", "update"], true));
            steps.push(apt_install(&["chromium"]));
        }
        Family::Alpine => {
            steps.push(Step::Heading("installing Chromium".into()));
            steps.push(run(&["apk", "add", "--no-cache", "chromium"], true));
        }
        Family::Other => {
            steps.push(unavailable(format!(
                "setup cannot install a browser on {}",
                host.os_name
            )));
            return false;
        }
    }
    true
}

/// The command that starts `browser` headless and prints an empty page's DOM,
/// as `user` when there is one, with a profile directory made for it and
/// removed after.
pub fn browser_check_argv(browser: &Path, user: Option<&str>, sandbox: bool) -> Vec<String> {
    let script = format!(
        "cd / && d=$(mktemp -d /tmp/feldspar-browser-XXXXXX) || exit 1; \
         timeout 60 {} --headless --disable-gpu --no-first-run{} --user-data-dir=\"$d\" \
         --dump-dom about:blank; s=$?; rm -rf \"$d\"; exit $s",
        shell_line(&[browser.to_string_lossy().into_owned()]),
        if sandbox { "" } else { " --no-sandbox" }
    );
    match user {
        Some(user) => ["su", "-s", "/bin/sh", user, "-c", &script]
            .map(str::to_owned)
            .to_vec(),
        None => vec!["sh".into(), "-c".into(), script],
    }
}

fn run_unless(argv: &[&str], unless: &[&str]) -> Step {
    Step::Run {
        argv: argv.iter().map(|s| (*s).to_owned()).collect(),
        root: true,
        unless: Some(unless.iter().map(|s| (*s).to_owned()).collect()),
    }
}

/// What setup will do, in order. `existing_config` is the configuration file as
/// it is now, when there is one.
pub fn plan(c: &Choices, host: &Host, existing_config: Option<&str>) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let production = c.mode == Mode::Production;

    if production {
        if host.family == Family::Other {
            return Err(Error::config(format!(
                "feldspar setup --production knows Debian, Ubuntu and Alpine, and this is {}. \
                 README §2 is the same setup by hand",
                host.os_name
            )));
        }
        if host.services == ServiceManager::None {
            return Err(Error::config(
                "a production instance is run by systemd or OpenRC, and neither is running here",
            ));
        }
        if c.binary.starts_with("/home") || c.binary.starts_with("/root") {
            return Err(Error::config(format!(
                "the service would run {}, which is in a home directory the service cannot \
                 read (ProtectHome). Install it with the install script (/opt/feldspar), or \
                 copy it out and pass --binary PATH",
                c.binary.display()
            )));
        }
    }

    // --- Node.js and a headless browser ---------------------------------------
    if c.node {
        node_steps(&mut steps, host);
    }
    let check_browser = c.browser && browser_steps(&mut steps, c, host);

    // --- PostgreSQL on this machine -------------------------------------------
    if let Database::LocalPostgres { role, database } = &c.database {
        if host.postgres_running {
            steps.push(Step::Note(format!(
                "PostgreSQL is already running here ({POSTGRES_SOCKET_DIR}); using it"
            )));
        } else {
            steps.push(Step::Heading("installing PostgreSQL".into()));
            match host.family {
                Family::Debian => {
                    steps.push(run(&["apt-get", "update"], true));
                    steps.push(run(
                        &[
                            "env",
                            "DEBIAN_FRONTEND=noninteractive",
                            "apt-get",
                            "install",
                            "-y",
                            "postgresql",
                            "postgresql-client",
                        ],
                        true,
                    ));
                    if host.services == ServiceManager::Systemd {
                        steps.push(run(&["systemctl", "enable", "--now", "postgresql"], true));
                    }
                }
                Family::Alpine => {
                    steps.push(run(
                        &[
                            "apk",
                            "add",
                            "--no-cache",
                            "postgresql",
                            "postgresql-client",
                        ],
                        true,
                    ));
                    // Alpine's package leaves the cluster to `setup`, which
                    // refuses to run twice.
                    steps.push(run_unless(
                        &["rc-service", "postgresql", "setup"],
                        &["sh", "-c", "ls /var/lib/postgresql/*/data/PG_VERSION"],
                    ));
                    steps.push(run(&["rc-update", "add", "postgresql", "default"], true));
                    steps.push(run(&["rc-service", "postgresql", "start"], true));
                }
                Family::Other => {
                    return Err(Error::config(format!(
                        "no PostgreSQL is running on {POSTGRES_SOCKET_DIR}, and setup knows how to \
                         install one on Debian, Ubuntu and Alpine but not on {}. Install and start \
                         it, then run setup again — or choose --db sqlite",
                        host.os_name
                    )));
                }
            }
        }
        if production {
            service_account(&mut steps, c, host);
        }
        steps.push(Step::Heading(format!(
            "creating the PostgreSQL role {role} and database {database}"
        )));
        // As the postgres superuser, and each half only when it is missing.
        // The names are checked identifiers, so they go into SQL as they are.
        let script = format!(
            "cd / && \
             (psql -tAc \"SELECT 1 FROM pg_roles WHERE rolname='{role}'\" | grep -q 1 \
             || createuser {role}) && \
             (psql -tAc \"SELECT 1 FROM pg_database WHERE datname='{database}'\" | grep -q 1 \
             || createdb -O {role} {database})"
        );
        steps.push(run(
            &["su", "-s", "/bin/sh", "postgres", "-c", &script],
            true,
        ));
    } else if production {
        service_account(&mut steps, c, host);
    }

    // --- SQLite ---------------------------------------------------------------
    if let Database::Sqlite { path } = &c.database
        && let Some(dir) = path.parent()
        && !(production && dir == Path::new(STATE_DIR))
    {
        let dir = dir.to_string_lossy();
        steps.push(Step::Heading(format!("a directory for {}", path.display())));
        if production {
            let user = c.service_user.as_str();
            steps.push(run(
                &["install", "-d", "-m", "0750", "-o", user, "-g", user, &dir],
                true,
            ));
        } else {
            steps.push(run(&["mkdir", "-p", &dir], false));
        }
    }

    // --- The configuration file -----------------------------------------------
    let section = environment_section(c);
    match merge_config(
        existing_config,
        &c.config_path,
        &c.environment,
        &section,
        c.force,
    )? {
        Merged::Unchanged => steps.push(Step::Note(format!(
            "{} already has this [environments.{}]; leaving it",
            c.config_path.display(),
            c.environment
        ))),
        Merged::Write { contents, note } => {
            steps.push(Step::Heading(format!(
                "writing {}",
                c.config_path.display()
            )));
            if let Some(note) = note {
                steps.push(Step::Note(note));
            }
            steps.push(Step::Write {
                path: c.config_path.clone(),
                contents,
                // It may hold a password: the service account reads it, root
                // writes it — a compromised server cannot rewrite what the next
                // start connects to (README §2.5).
                mode: if production { 0o640 } else { 0o600 },
                owner: production.then(|| format!("root:{}", c.service_user)),
                root: production,
            });
        }
    }

    // --- The browser starts as the account that will run it ------------------
    // After the account exists, before the service starts: a missing library or
    // a sandbox the kernel refuses is reported now, with the browser's own error.
    if check_browser {
        let user = production.then(|| c.service_user.clone());
        steps.push(Step::Heading(format!(
            "checking that the browser starts headless as {}",
            user.as_deref().unwrap_or(&host.login)
        )));
        steps.push(Step::CheckBrowser {
            browser: host.browser.clone(),
            user,
            config: format!(
                "[environments.{}] in {}",
                c.environment,
                c.config_path.display()
            ),
        });
    }

    // --- The service ----------------------------------------------------------
    if production {
        // Like the configuration file: the same file again is left alone, and
        // one an operator has edited is only replaced when asked to.
        let service_file =
            |steps: &mut Vec<Step>, path: &str, contents: String, mode: u32| match &host
                .service_file
            {
                Some(have) if *have == contents => {
                    steps.push(Step::Note(format!("{path} is already this; leaving it")));
                    Ok(())
                }
                Some(_) if !c.force => Err(Error::config(format!(
                    "{path} is already there and says something else (edited by hand?); \
                     --force replaces it"
                ))),
                _ => {
                    steps.push(Step::Write {
                        path: path.into(),
                        contents,
                        mode,
                        owner: None,
                        root: true,
                    });
                    Ok(())
                }
            };
        match host.services {
            ServiceManager::Systemd => {
                steps.push(Step::Heading(format!("the systemd unit {SYSTEMD_UNIT}")));
                service_file(&mut steps, SYSTEMD_UNIT, systemd_unit(c), 0o644)?;
                steps.push(run(&["systemctl", "daemon-reload"], true));
                steps.push(run(&["systemctl", "enable", "feldspar"], true));
                if c.start {
                    // restart, not start: a second run that changed the file
                    // should be what is serving afterwards.
                    steps.push(run(&["systemctl", "restart", "feldspar"], true));
                }
            }
            ServiceManager::OpenRc => {
                steps.push(Step::Heading(format!("the OpenRC service {OPENRC_SCRIPT}")));
                service_file(&mut steps, OPENRC_SCRIPT, openrc_script(c), 0o755)?;
                steps.push(run(&["rc-update", "add", "feldspar", "default"], true));
                if c.start {
                    steps.push(run(&["rc-service", "feldspar", "restart"], true));
                }
            }
            ServiceManager::None => {}
        }
    }
    Ok(steps)
}

fn service_account(steps: &mut Vec<Step>, c: &Choices, host: &Host) {
    let user = c.service_user.as_str();
    steps.push(Step::Heading(format!("the {user} system account")));
    match host.family {
        Family::Alpine => {
            let group_line = format!("^{user}:");
            steps.push(run_unless(
                &["addgroup", "-S", user],
                &["grep", "-q", &group_line, "/etc/group"],
            ));
            steps.push(run_unless(
                &[
                    "adduser",
                    "-S",
                    "-D",
                    "-H",
                    "-h",
                    STATE_DIR,
                    "-s",
                    "/sbin/nologin",
                    "-G",
                    user,
                    user,
                ],
                &["id", "-u", user],
            ));
        }
        _ => steps.push(run_unless(
            &["adduser", "--system", "--group", "--home", STATE_DIR, user],
            &["id", "-u", user],
        )),
    }
    // The one writable place; systemd's StateDirectory would make it too, but
    // OpenRC has no such thing and a SQLite file lives here.
    steps.push(run(
        &[
            "install", "-d", "-m", "0750", "-o", user, "-g", user, STATE_DIR,
        ],
        true,
    ));
}

/// A TOML basic string, quoted and escaped.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `[environments.NAME]` section setup writes.
pub fn environment_section(c: &Choices) -> String {
    let mut out = format!("[environments.{}]\n", c.environment);
    match &c.database {
        Database::Sqlite { path } => {
            out += &format!("sqlite = {}\n", quoted(&path.to_string_lossy()));
        }
        Database::LocalPostgres { role, database } => {
            out += &format!(
                "host = {}   # a leading `/` is a Unix socket directory\n",
                quoted(POSTGRES_SOCKET_DIR)
            );
            out += &format!("user = {}\n", quoted(role));
            out += &format!("database = {}\n", quoted(database));
        }
        Database::RemotePostgres(r) => {
            if let Some(url) = &r.url {
                out += &format!("url = {}\n", quoted(url));
            }
            if let Some(host) = &r.host {
                out += &format!("host = {}\n", quoted(host));
            }
            if let Some(port) = r.port {
                out += &format!("port = {port}\n");
            }
            if let Some(user) = &r.user {
                out += &format!("user = {}\n", quoted(user));
            }
            if let Some(password) = &r.password {
                out += &format!("password = {}\n", quoted(password));
            }
            if let Some(database) = &r.database {
                out += &format!("database = {}\n", quoted(database));
            }
        }
    }
    match &c.domain {
        Some(domain) => out += &format!("base_domain = {}\n", quoted(domain)),
        None if c.mode == Mode::Production => {
            out += "# base_domain = \"example.com\"   # applications at <app>.example.com\n";
        }
        None => {}
    }
    if let Some(bind) = &c.bind {
        out += &format!("bind = {}\n", quoted(bind));
    }
    out
}

/// What [`merge_config`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    /// The file already has this environment, saying the same thing.
    Unchanged,
    /// Write this; `note` says what happened to what was there.
    Write {
        contents: String,
        note: Option<String>,
    },
}

/// The configuration file with `section` in it.
///
/// No file: a new one, with this environment the default. A file without the
/// environment: the section appended, everything else kept — on a desktop that
/// file may well hold the test harness's `[environments.test]`. A file whose
/// section says the same thing: [`Merged::Unchanged`]. One that says something
/// else is refused, or with `force` has its section (and the section's
/// sub-tables) replaced.
pub fn merge_config(
    existing: Option<&str>,
    path: &Path,
    environment: &str,
    section: &str,
    force: bool,
) -> Result<Merged> {
    let header = "# Written by `feldspar setup`. README §7 (\"Environments, and the \
                  configuration file\")\n# describes every key an environment may carry.\n";
    let wanted = ConfigFile::parse(section, path)?
        .environments
        .remove(environment)
        .ok_or_else(|| Error::msg("the generated section names no environment"))?;
    let Some(text) = existing else {
        return Ok(Merged::Write {
            contents: format!(
                "{header}default_environment = {}\n\n{section}",
                quoted(environment)
            ),
            note: None,
        });
    };
    let current = ConfigFile::parse(text, path).map_err(|e| {
        Error::config(format!(
            "{} is already there and does not parse; fix it (or move it aside) first: {}",
            path.display(),
            sc_error::format_chain(&e)
        ))
    })?;

    let mut note = None;
    let mut body = text.to_owned();
    if let Some(have) = current.environments.get(environment) {
        if *have == wanted {
            return Ok(Merged::Unchanged);
        }
        if !force {
            return Err(Error::config(format!(
                "{} already has an [environments.{environment}] that says something else; \
                 --force replaces it, or --environment NAME writes another one",
                path.display()
            )));
        }
        body = without_section(text, environment);
        note = Some(format!("replacing its [environments.{environment}]"));
    }
    if !body.ends_with('\n') && !body.is_empty() {
        body.push('\n');
    }
    body.push('\n');
    body.push_str(section);
    match &current.default_environment {
        None => body = format!("default_environment = {}\n{body}", quoted(environment)),
        Some(other) if other != environment => {
            note = Some(format!(
                "{}its default environment stays `{other}`: serve this one with \
                 --environment {environment}",
                note.map(|n| format!("{n}; ")).unwrap_or_default()
            ));
        }
        Some(_) => {}
    }

    // What is written must read back as exactly this environment — the section
    // removal is line-based, and a file it did not understand is not rewritten.
    let merged = ConfigFile::parse(&body, path).map_err(|e| {
        Error::config(format!(
            "could not add [environments.{environment}] to {} cleanly ({}); edit it by hand",
            path.display(),
            sc_error::format_chain(&e)
        ))
    })?;
    if merged.environments.get(environment) != Some(&wanted) {
        return Err(Error::config(format!(
            "could not replace [environments.{environment}] in {} cleanly; edit it by hand",
            path.display()
        )));
    }
    Ok(Merged::Write {
        contents: body,
        note,
    })
}

/// `text` without the `[environments.NAME]` table and its sub-tables.
fn without_section(text: &str, environment: &str) -> String {
    let own = format!("environments.{environment}");
    let mut out = String::new();
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            let name = trimmed
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or_default()
                .trim();
            skipping = name == own || name.starts_with(&format!("{own}."));
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The systemd unit: README §2.6's, for these choices.
pub fn systemd_unit(c: &Choices) -> String {
    let user = &c.service_user;
    let binary = c.binary.display();
    let env = &c.environment;
    let config = c.config_path.display();
    format!(
        "# Written by `feldspar setup`; README §2.6 explains each line.
[Unit]
Description=Saltcorn Feldspar
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=notify
WatchdogSec=30s
User={user}
Group={user}
ExecStart={binary} serve --environment {env}
Environment=FELDSPAR_CONFIG={config}
Environment=HOME={STATE_DIR}
Environment=SC_DATA_DIR={STATE_DIR}
StateDirectory=feldspar
WorkingDirectory={STATE_DIR}
Restart=on-failure
RestartSec=5s

# Bind ports 80/443 without running as root.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

# Everything read-only except the state directory.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths={STATE_DIR}

# The rest of the kernel surface.
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
ProtectProc=invisible
RestrictSUIDSGID=true
RestrictRealtime=true
LockPersonality=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
UMask=0077

[Install]
WantedBy=multi-user.target
"
    )
}

/// The OpenRC service script: the systemd unit's settings, as far as OpenRC
/// has them. `supervise-daemon` restarts a crashed server, and `capabilities`
/// (OpenRC 0.45, Alpine 3.17) is how it binds 80 and 443 as the service account.
pub fn openrc_script(c: &Choices) -> String {
    let user = &c.service_user;
    let binary = c.binary.display();
    let env = &c.environment;
    let config = c.config_path.display();
    format!(
        "#!/sbin/openrc-run
# Written by `feldspar setup`.

name=\"feldspar\"
description=\"Saltcorn Feldspar\"
supervisor=\"supervise-daemon\"
command=\"{binary}\"
command_args=\"serve --environment {env}\"
command_user=\"{user}:{user}\"
directory=\"{STATE_DIR}\"
capabilities=\"^cap_net_bind_service\"
output_log=\"{OPENRC_LOG}\"
error_log=\"{OPENRC_LOG}\"
respawn_delay=5

export FELDSPAR_CONFIG=\"{config}\"
export HOME=\"{STATE_DIR}\"
export SC_DATA_DIR=\"{STATE_DIR}\"

depend() {{
\tneed net
\tafter postgresql
}}

start_pre() {{
\tcheckpath --directory --owner {user}:{user} --mode 0750 \"{STATE_DIR}\"
\tcheckpath --file --owner {user}:{user} --mode 0640 \"{OPENRC_LOG}\"
}}
"
    )
}

fn with_sudo(argv: &[String], root: bool, host: &Host) -> Vec<String> {
    match (&host.sudo, root && !host.root) {
        (Some(sudo), true) => std::iter::once(sudo.clone())
            .chain(argv.iter().cloned())
            .collect(),
        _ => argv.to_vec(),
    }
}

/// Print `steps` as `--dry-run` shows them: every command and every file.
pub fn describe(steps: &[Step], host: &Host, out: &mut dyn Write) {
    for step in steps {
        let _ = match step {
            Step::Heading(text) => writeln!(out, "==> {text}"),
            Step::Note(text) => writeln!(out, "    {text}"),
            Step::Run { argv, root, unless } => {
                let line = shell_line(&with_sudo(argv, *root, host));
                match unless {
                    Some(probe) => {
                        writeln!(out, "    $ {line}\n      (unless: {})", shell_line(probe))
                    }
                    None => writeln!(out, "    $ {line}"),
                }
            }
            Step::Write {
                path,
                contents,
                mode,
                owner,
                ..
            } => {
                let _ = writeln!(
                    out,
                    "    write {} (mode {mode:04o}{}):",
                    path.display(),
                    owner
                        .as_ref()
                        .map(|o| format!(", owner {o}"))
                        .unwrap_or_default()
                );
                for line in contents.lines() {
                    let _ = writeln!(out, "      | {line}");
                }
                Ok(())
            }
            Step::CheckBrowser { browser, user, .. } => {
                let shown = browser.clone().unwrap_or_else(|| "BROWSER".into());
                let argv = browser_check_argv(&shown, user.as_deref(), true);
                let _ = writeln!(
                    out,
                    "    $ {}",
                    shell_line(&with_sudo(&argv, user.is_some(), host))
                );
                writeln!(
                    out,
                    "      ({}reported, never fatal; tried again with --no-sandbox when it \
                     fails)",
                    if browser.is_none() {
                        "BROWSER is the one just installed, found on PATH as the server finds \
                         it; "
                    } else {
                        ""
                    }
                )
            }
        };
    }
}

fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:=@^".contains(c))
                && !a.is_empty()
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Carry `steps` out, stopping at the first that fails.
pub fn execute(steps: &[Step], host: &Host, out: &mut dyn Write) -> Result<()> {
    let needs_root = steps.iter().any(|s| match s {
        Step::Run { root, .. } | Step::Write { root, .. } => *root,
        Step::CheckBrowser { user, .. } => user.is_some(),
        _ => false,
    });
    if needs_root && !host.root && host.sudo.is_none() {
        return Err(Error::config(
            "this setup needs root: run it as root, or install sudo",
        ));
    }
    for step in steps {
        match step {
            Step::Heading(text) => {
                let _ = writeln!(out, "==> {text}");
            }
            Step::Note(text) => {
                let _ = writeln!(out, "    {text}");
            }
            Step::Run { argv, root, unless } => {
                if let Some(probe) = unless
                    && Command::new(&probe[0])
                        .args(&probe[1..])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                {
                    let _ = writeln!(out, "    already done: {}", shell_line(argv));
                    continue;
                }
                let argv = with_sudo(argv, *root, host);
                let _ = writeln!(out, "    $ {}", shell_line(&argv));
                let _ = out.flush();
                let status = Command::new(&argv[0])
                    .args(&argv[1..])
                    .status()
                    .map_err(|e| Error::msg(format!("running {}: {e}", argv[0])))?;
                if !status.success() {
                    return Err(Error::msg(format!(
                        "`{}` failed ({status}); nothing after it was done",
                        shell_line(&argv)
                    )));
                }
            }
            Step::Write {
                path,
                contents,
                mode,
                owner,
                root,
            } => {
                let _ = writeln!(out, "    write {}", path.display());
                write_file(path, contents, *mode, owner.as_deref(), *root, host)?;
            }
            Step::CheckBrowser {
                browser,
                user,
                config,
            } => check_browser(browser.as_deref(), user.as_deref(), config, host, out),
        }
    }
    Ok(())
}

/// [`Step::CheckBrowser`], carried out: what it found is written to `out`.
fn check_browser(
    browser: Option<&Path>,
    user: Option<&str>,
    config: &str,
    host: &Host,
    out: &mut dyn Write,
) {
    let browser = match browser {
        Some(path) => path.to_owned(),
        None => match sc_server::detect_browser(None) {
            Ok(path) => path,
            Err(why) => {
                let _ = writeln!(out, "    the browser is not found: {why}");
                return;
            }
        },
    };
    // Whether the page came back, and what the browser said on stderr.
    let attempt = |sandbox: bool| {
        let argv = with_sudo(
            &browser_check_argv(&browser, user, sandbox),
            user.is_some(),
            host,
        );
        match Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()
        {
            Ok(o) => (
                String::from_utf8_lossy(&o.stdout).contains("<html"),
                String::from_utf8_lossy(&o.stderr).into_owned(),
            ),
            Err(e) => (false, format!("running {}: {e}", argv[0])),
        }
    };
    let _ = writeln!(out, "    starting {}", browser.display());
    let (ok, stderr) = attempt(true);
    if ok {
        let _ = writeln!(out, "    it starts");
    } else if attempt(false).0 {
        let _ = writeln!(
            out,
            "    it starts only without its sandbox on this kernel; to let the server do \
             the same, add  browser_sandbox = false  to {config}"
        );
    } else {
        let _ = writeln!(
            out,
            "    it does not start; view_app will fail until it does:"
        );
        let lines: Vec<&str> = stderr.lines().collect();
        for line in &lines[lines.len().saturating_sub(20)..] {
            let _ = writeln!(out, "      | {line}");
        }
    }
}

fn write_file(
    path: &Path,
    contents: &str,
    mode: u32,
    owner: Option<&str>,
    root: bool,
    host: &Host,
) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let io = |what: &str, e: std::io::Error| Error::msg(format!("{what} {}: {e}", path.display()));

    // Not root and it has to be: write a private copy, and have sudo put it in
    // place with its mode in one `install`.
    let target = if root && !host.root {
        std::env::temp_dir().join(format!("feldspar-setup-{}", std::process::id()))
    } else {
        path.to_owned()
    };
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io("creating the directory of", e))?;
    }
    // Created with its final mode, so a password is never briefly readable.
    let _ = std::fs::remove_file(&target);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if target == path { mode } else { 0o600 })
        .open(&target)
        .map_err(|e| io("creating", e))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| io("writing", e))?;
    drop(file);
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
        .map_err(|e| io("setting the mode of", e))?;

    let mut commands = Vec::new();
    if target != path {
        commands.push(vec![
            "install".to_owned(),
            "-D".to_owned(),
            "-m".to_owned(),
            format!("{mode:04o}"),
            target.to_string_lossy().into_owned(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    if let Some(owner) = owner {
        commands.push(vec![
            "chown".to_owned(),
            owner.to_owned(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    let result = commands.iter().try_for_each(|argv| {
        let argv = with_sudo(argv, true, host);
        let ok = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            Ok(())
        } else {
            Err(Error::msg(format!("`{}` failed", shell_line(&argv))))
        }
    });
    if target != path {
        let _ = std::fs::remove_file(&target);
    }
    result
}

/// The configuration file as it is now, read as root when it has to be.
fn read_existing(path: &Path, host: &Host) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && host.sudo.is_some() => {
            let argv = with_sudo(
                &["cat".to_owned(), path.to_string_lossy().into_owned()],
                true,
                host,
            );
            let out = Command::new(&argv[0])
                .args(&argv[1..])
                .output()
                .map_err(|e| Error::msg(format!("reading {}: {e}", path.display())))?;
            if !out.status.success() {
                return Err(Error::msg(format!("could not read {}", path.display())));
            }
            Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
        }
        Err(e) => Err(Error::msg(format!("reading {}: {e}", path.display()))),
    }
}

/// What to do next, once setup has run.
pub fn next_steps(c: &Choices, host: &Host, default_is_ours: bool) -> String {
    let mut out = String::new();
    let port = c
        .bind
        .as_deref()
        .and_then(|b| b.rsplit(':').next())
        .unwrap_or(DEFAULT_PORT);
    match c.mode {
        Mode::Development => {
            let flag = if default_is_ours {
                String::new()
            } else {
                format!(" --environment {}", c.environment)
            };
            out += &format!(
                "\nThis is a development instance. Start it with\n\n  feldspar serve{flag}\n\n\
                 then open http://localhost:{port}/ and create the first user — the first \
                 account is an admin.\n"
            );
        }
        Mode::Production => {
            let (status, logs) = match host.services {
                ServiceManager::OpenRc => (
                    "rc-service feldspar status".to_owned(),
                    format!("tail -f {OPENRC_LOG}"),
                ),
                _ => (
                    "systemctl status feldspar".to_owned(),
                    "journalctl -u feldspar -f".to_owned(),
                ),
            };
            let place = c.domain.as_deref().unwrap_or("this host");
            out += &format!(
                "\nThis is a production instance{}.\n  status   {status}\n  logs     {logs}\n  \
                 health   curl -s http://127.0.0.1:{port}/health\n\n\
                 Still to do (README §2.7):\n  \
                 DNS      point {place} and *.{place} at this host\n  \
                 Admin    open http://{place}/ and create the first user — the screen is \
                 open to whoever reaches it first, so do it now\n  \
                 TLS      admin UI, Settings -> SSL / TLS certificates\n",
                if c.start {
                    ", and its service is running"
                } else {
                    "; start its service when ready"
                }
            );
        }
    }
    // What was left out, and how to have it. Asked for and installed, it is
    // not mentioned; asked for where setup cannot install it, the plan said so.
    let again = match host.family {
        Family::Other => None,
        _ => Some(match c.mode {
            Mode::Development => "feldspar setup --dev",
            Mode::Production => "feldspar setup --server",
        }),
    };
    if !c.node && !host.npm_ok() {
        let what = match &host.npm {
            Some(old) => format!("npm {old} is too old to install a module"),
            None => "Node.js is not installed".to_owned(),
        };
        let fix = match again {
            Some(cmd) => format!("`{cmd} --node` installs it"),
            None => format!(
                "install Node.js with npm {}.{} or newer",
                MIN_NPM.0, MIN_NPM.1
            ),
        };
        out +=
            &format!("\n{what}: building an application or installing a module runs npm. {fix}.\n");
    }
    if !c.browser && host.browser.is_none() {
        let fix = match again {
            Some(cmd) => format!("`{cmd} --browser` installs one"),
            None => "install Chromium (not a snap)".to_owned(),
        };
        out += &format!(
            "\nThere is no headless browser: the coding agent's view_app is unavailable. \
             {fix}.\n"
        );
    }
    out
}

/// `feldspar setup …`.
pub fn run_setup(args: &[String]) -> Result<()> {
    let args = SetupArgs::parse(args)?;
    if args.help {
        println!("{USAGE}");
        return Ok(());
    }
    let host = Host::detect();
    let interactive = !args.preset && std::io::stdin().is_terminal();
    let stdin = std::io::stdin();
    let mut prompter = Prompter::new(stdin.lock(), std::io::stderr(), true);
    let choices = resolve(&args, &host, interactive.then_some(&mut prompter))?;
    let existing = read_existing(&choices.config_path, &host)?;
    let default_is_ours = match &existing {
        None => true,
        Some(text) => ConfigFile::parse(text, &choices.config_path)
            .map(|f| {
                f.default_environment
                    .is_none_or(|d| d == choices.environment)
            })
            .unwrap_or(false),
    };
    let steps = plan(&choices, &host, existing.as_deref())?;

    let mut stdout = std::io::stdout();
    let kind = match choices.mode {
        Mode::Development => "development",
        Mode::Production => "production",
    };
    println!("==> feldspar setup: a {kind} instance on {}", host.os_name);
    if args.dry_run {
        println!("==> dry run: nothing below is executed");
        describe(&steps, &host, &mut stdout);
        println!("==> dry run finished; nothing above was executed");
        return Ok(());
    }
    if interactive && !args.yes {
        println!();
        describe(&steps, &host, &mut stdout);
        if !prompter.confirm("\nGo ahead?")? {
            return Err(Error::config("nothing was done"));
        }
    }
    execute(&steps, &host, &mut stdout)?;
    print!("{}", next_steps(&choices, &host, default_is_ours));
    Ok(())
}

//! The file-store IDE's terminal: Claude Code on the server, in a tab (design
//! §12.1, "Claude Code in a terminal tab").
//!
//! The language server's sibling ([`crate::lsp`]), with the same shape: one
//! WebSocket, one process, admin-only, gated on
//! [`FileStore::local_path`](sc_files::FileStore::local_path), refusals in the
//! close frame, a bound on how many run at once. What differs is the process.
//! Claude Code is a full-screen terminal program, so it gets a **pseudo-terminal**
//! rather than pipes: it draws with escape sequences, asks the terminal how big
//! it is, and expects Ctrl-C to arrive as a signal rather than as a byte.
//!
//! ## The protocol
//!
//! - **Binary frames** are the terminal's bytes, in both directions: what the
//!   admin typed going in, what the program drew coming out.
//! - **Text frames** from the browser are control messages, one JSON object each.
//!   `{"resize": {"cols": 120, "rows": 40}}` sets the terminal's size;
//!   `{"pause": true}` and `{"pause": false}` stop and restart reading what the
//!   program writes, which is how the browser keeps up (flow control, as ttyd has
//!   it). A text frame that is not a control message is ignored rather than
//!   typed, so a client bug cannot become keystrokes.
//! - **Text frames** to the browser say which files changed on disk while the
//!   terminal was attached: `{"changed": ["src/App.tsx", ".git"], "removed":
//!   ["src/Old.tsx"]}`, store paths, `.git` standing for the store's git state
//!   ([`crate::terminal_watch`]). The workbench has no watcher of its own, and
//!   an editor left open on a file the program rewrote would overwrite it on
//!   the next save.
//! - When the program exits the socket is closed normally, with its exit status
//!   in the reason.
//!
//! ## Refusals
//!
//! As [`crate::lsp`]: only the admin check is an HTTP status. The feature being
//! off, the store or application not being there or not being on this machine's
//! disk, the executable not being installed and the bound being reached all
//! upgrade the socket and close it with a sentence the terminal prints.
//!
//! ## Wired to this server
//!
//! The program is started as Claude Code: `--continue` when the directory has a
//! conversation, so a reload resumes it; an MCP config naming this server's `/mcp`, and the proxy of
//! each MCP server offered to the terminal ([`crate::mcp_proxy`]), with a
//! **session token** ([`SessionTokens`]), in a file outside the store that goes
//! with the session; `--strict-mcp-config`, so the session sees those and
//! nothing an operator configured elsewhere; and, in an application's directory, its
//! framework's builder-agent prompt. Its login lives in the installation's data
//! directory (`CLAUDE_CONFIG_DIR`), and an Anthropic provider's key, when there
//! is one, is passed so nothing is asked.
//!
//! ## What is ours and what is not
//!
//! The pseudo-terminal itself — opening one, making it the child's controlling
//! terminal in a new session, resizing it — is `pty-process`'s. What is this
//! module's is what that crate does not know about: which directory, which
//! environment, and ending everything the program started when the socket
//! closes ([`pty::Session::kill`]). Off Unix the route refuses with a reason.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use sc_app::Application;
use sc_catalog::Catalog;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::admin_session::{AdminSession, AdminWatch};
use crate::lsp::{fit_close_reason, refuse};

/// The route the IDE opens a terminal on. `{store}` is the store the workbench
/// has open; `?app=<name>` starts it in that application's source directory
/// rather than at the store's root.
pub const TERMINAL_ROUTE: &str = "/ide/terminal/{store}";

/// How many terminals may run at once, across every admin and every store.
///
/// Fewer than [`crate::lsp::MAX_LANGUAGE_SERVERS`]: a language server is one per
/// open IDE page whether or not anyone is looking at it, while a terminal is a
/// session someone opened on purpose, and each one can run a build.
pub const MAX_TERMINALS: usize = 4;

/// The executable, as `PATH` lookup would find it.
pub(crate) const CLAUDE_BIN: &str = "claude";

/// The size a terminal starts at, before the browser says how big it is.
const DEFAULT_SIZE: TerminalSize = TerminalSize { cols: 80, rows: 24 };

/// How long an exited program's status is waited for once its output has ended.
const EXIT_STATUS_WAIT: Duration = Duration::from_secs(2);

/// How long the close frame may take to reach the browser before the session
/// is ended without it: a browser that stopped reading (a sleeping laptop, a
/// lost network) must not keep the program, its slot and its token alive.
const CLOSE_SEND_WAIT: Duration = Duration::from_secs(2);

/// The environment variables the program inherits from the server.
///
/// An allow-list, not the server's environment: that holds `DATABASE_URL` and
/// whatever else the operator started the server with, and none of it is the
/// terminal's business. An admin with a shell can still go looking, but the
/// server does not hand it over unasked.
const INHERITED_ENV: [&str; 9] = [
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TZ", "TMPDIR",
];

/// What a session token starts with, so the MCP route can tell one from a
/// table token without looking either up.
const SESSION_TOKEN_PREFIX: &str = "fdterm_";

/// What the session's MCP config calls this server.
const MCP_SERVER_KEY: &str = "feldspar";

/// The bound on concurrent terminals, as the router holds it.
pub(crate) type TerminalSlots = Arc<Semaphore>;

/// A fresh bound, one per built router.
pub(crate) fn terminal_slots() -> TerminalSlots {
    Arc::new(Semaphore::new(MAX_TERMINALS))
}

/// The MCP credentials of the terminals that are open: token → whose session.
///
/// In memory and nowhere else (design §12.1, "MCP credential"): a token is
/// issued when a terminal starts and removed when it ends, so a restart or a
/// crash cannot leave one valid, and nothing about it is in the token table.
#[derive(Clone, Default)]
pub(crate) struct SessionTokens(Arc<Mutex<HashMap<String, SessionGrant>>>);

/// Who a session token speaks for.
#[derive(Debug, Clone)]
pub(crate) struct SessionGrant {
    /// The admin session the terminal was opened from: the token speaks for
    /// it only while it is still an administrator's.
    pub(crate) admin: AdminSession,
    /// The session, which is also what the MCP route's changesets are keyed by.
    pub(crate) session: Uuid,
    /// When the terminal opened.
    pub(crate) opened: DateTime<Utc>,
}

impl SessionTokens {
    /// A fresh token for a terminal `admin` opens, valid until the returned
    /// value is dropped.
    fn issue(&self, admin: AdminSession) -> SessionToken {
        let secret = format!(
            "{SESSION_TOKEN_PREFIX}{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let grant = SessionGrant {
            admin,
            session: Uuid::new_v4(),
            opened: Utc::now(),
        };
        self.lock().insert(secret.clone(), grant);
        SessionToken {
            tokens: self.clone(),
            secret,
        }
    }

    /// The session `presented` belongs to, while its terminal is open.
    pub(crate) fn grant(&self, presented: &str) -> Option<SessionGrant> {
        if !presented.starts_with(SESSION_TOKEN_PREFIX) {
            return None;
        }
        self.lock().get(presented).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionGrant>> {
        // A panic while holding the map cannot leave it half-written: every
        // change is one insert or one remove.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A session token, valid for as long as this value lives.
struct SessionToken {
    tokens: SessionTokens,
    secret: String,
}

impl Drop for SessionToken {
    fn drop(&mut self) {
        self.tokens.lock().remove(&self.secret);
    }
}

/// The URL a terminal's Claude Code uses to reach this server, such as
/// `http://127.0.0.1:3032`, or the reason there is none.
///
/// It must be a loopback URL, because session tokens are only accepted from
/// this machine. The plain-HTTP listener is always the one named: with TLS on,
/// it serves a terminal's MCP traffic even when it redirects everything else
/// to HTTPS ([`is_terminal_mcp`]), since HTTPS on `127.0.0.1` would fail on a
/// certificate made out to a domain name. What rules a URL out is a server
/// that listens on one specific non-loopback address, such as `192.168.1.5`,
/// which 127.0.0.1 does not reach.
pub(crate) fn loopback_origin(addr: SocketAddr) -> Result<String, String> {
    if addr.port() == 0 {
        return Err("this server's port is not known".to_owned());
    }
    let host = match addr.ip() {
        IpAddr::V4(ip) if ip.is_loopback() || ip.is_unspecified() => {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }
        IpAddr::V6(ip) if ip.is_loopback() || ip.is_unspecified() => {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        }
        other => {
            return Err(format!(
                "this server listens on {other} only, which a program on it cannot \
                 reach as loopback"
            ));
        }
    };
    Ok(format!("http://{}", SocketAddr::new(host, addr.port())))
}

/// Whether `request` is a terminal's MCP traffic: to `/mcp` or the proxy, from
/// this machine, with a session token.
///
/// A server that redirects plain HTTP to HTTPS serves these on the plain
/// listener instead of redirecting them, because that listener is where
/// [`loopback_origin`] points the sessions. Nothing crosses the network
/// unencrypted: the peer is this machine. Whether the token is one an open
/// terminal holds is the routes' own check; this only sorts the request.
pub(crate) fn is_terminal_mcp(request: &axum::extract::Request) -> bool {
    let path = request.uri().path();
    let route = path == crate::mcp::MCP_ROUTE || path.starts_with(MCP_PROXY_PREFIX);
    route
        && crate::mcp::is_loopback(
            request
                .extensions()
                .get::<axum::extract::ConnectInfo<SocketAddr>>()
                .copied(),
        )
        && crate::security::bearer_token(request.headers())
            .is_some_and(|token| token.starts_with(SESSION_TOKEN_PREFIX))
}

/// The proxy route's path up to its server name.
const MCP_PROXY_PREFIX: &str = "/ide/mcp-proxy/";

/// The route's query string.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct TerminalQuery {
    /// The application whose source directory to start in, by name.
    pub(crate) app: Option<String>,
}

/// A terminal's size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) struct TerminalSize {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

/// A text frame from the browser, as sent.
#[derive(Debug, Deserialize)]
struct ControlFrame {
    resize: Option<TerminalSize>,
    pause: Option<bool>,
}

/// What a text frame from the browser asks for.
#[derive(Debug, PartialEq, Eq)]
enum Control {
    /// Set the terminal's size.
    Resize(TerminalSize),
    /// Stop (`true`) or restart (`false`) reading what the program writes.
    ///
    /// While reading is stopped the program's output waits in the kernel's
    /// buffer, and once that is full the program blocks on its next write: a
    /// build printing faster than the browser draws slows down rather than
    /// filling the page's memory.
    Pause(bool),
}

/// The control message `text` is, if it is one.
///
/// A size of zero is refused (the kernel accepts it, and a program asked to
/// draw into no columns divides by it); anything above a thousand cells is a
/// client bug rather than a monitor.
fn control_message(text: &str) -> Option<Control> {
    let frame = serde_json::from_str::<ControlFrame>(text).ok()?;
    if let Some(size) = frame.resize {
        let usable = |n: u16| (1..=1000).contains(&n);
        return (usable(size.cols) && usable(size.rows)).then_some(Control::Resize(size));
    }
    frame.pause.map(Control::Pause)
}

/// Where a terminal starts: the directory, the store's root it is in, and the
/// application it is for.
#[derive(Debug)]
pub(crate) struct TerminalPlace {
    pub(crate) dir: PathBuf,
    /// Only for the watch, which there is none of off Unix.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) root: PathBuf,
    pub(crate) app: Option<Application>,
}

/// Why this store cannot have a terminal, or where to start one.
///
/// The whole policy, apart from the executable and the bound: split out for the
/// reason [`crate::lsp::language_server_root`] is.
pub(crate) async fn terminal_dir(
    catalog: Option<&Arc<Catalog>>,
    store: &str,
    app: Option<&str>,
) -> Result<TerminalPlace, String> {
    let Some(catalog) = catalog else {
        return Err("this server has no catalog, so it has no file stores".to_owned());
    };
    if let Some(off) = terminal_off(catalog).await {
        return Err(off);
    }
    // Before the application is looked up, so an unknown store is refused as
    // that rather than as "<app>'s source is in …, not <store>".
    if !catalog
        .has_file_store(store)
        .map_err(|e| format!("file store {store}: {e}"))?
    {
        return Err(format!("the file store {store} is not connected"));
    }
    let (dir, app) = match app.map(str::trim).filter(|a| !a.is_empty()) {
        None => (String::new(), None),
        Some(app) => {
            let (dir, app) = app_source_dir(catalog, store, app).await?;
            (dir, Some(app))
        }
    };
    let dir = local_dir(catalog, store, &dir)?;
    let root = local_dir(catalog, store, "")?;
    Ok(TerminalPlace { dir, root, app })
}

/// Why a Claude session cannot start for each of `apps`, or `None` where it
/// can: what the applications list's "Build with Claude" shows.
///
/// The terminal route's own checks, in its order, minus the bound on sessions:
/// how many are open is a moment's state, not something about the application.
pub(crate) async fn claude_unavailable(
    catalog: &Catalog,
    option: Option<&Path>,
    apps: &[Application],
) -> Vec<Option<String>> {
    let everywhere = match terminal_off(catalog).await {
        Some(off) => Some(off),
        None => find_claude(Some(catalog), option).await.err(),
    };
    apps.iter()
        .map(|app| {
            everywhere
                .clone()
                .or_else(|| match sc_app::app_source_location(app) {
                    Err(_) => Some(format!("{} has no source directory", app.name)),
                    Ok((store, dir)) => local_dir(catalog, &store, &dir).err(),
                })
        })
        .collect()
}

/// Why the IDE terminal is off, if it is.
async fn terminal_off(catalog: &Catalog) -> Option<String> {
    match sc_config::ide_terminal_enabled(catalog).await {
        Ok(true) => None,
        Ok(false) => Some("the IDE terminal is turned off (Settings → Development)".to_owned()),
        Err(e) => Some(format!("could not read the settings: {e}")),
    }
}

/// The directory `dir` of `store` on this server's disk, or why there is none.
///
/// A program in a terminal is over the whole tree, as the language server is,
/// so the store is opened as the administrator the route admits
/// ([`crate::lsp::language_server_root`]'s reason).
fn local_dir(catalog: &Catalog, store: &str, dir: &str) -> Result<PathBuf, String> {
    let failed = |e: sc_error::Error| format!("file store {store}: {e}");
    if !catalog.has_file_store(store).map_err(failed)? {
        return Err(format!("the file store {store} is not connected"));
    }
    let handle = catalog
        .checked_file_store(
            store,
            None,
            sc_files::FileCaller::new(sc_types::Role::ADMIN, None),
        )
        .map_err(failed)?;
    let store_root = handle
        .local_root()
        .map_err(failed)?
        .ok_or_else(|| format!("{store} is not on this server's disk, so it has no terminal"))?;
    let path = sc_files::StorePath::parse(dir).map_err(failed)?;
    let root = if path.is_root() {
        store_root
    } else {
        store_root.join(path.as_str())
    };
    if !root.is_dir() {
        let shown = if dir.is_empty() {
            store.to_owned()
        } else {
            format!("{store}/{dir}")
        };
        return Err(format!("{shown} is not there yet"));
    }
    Ok(root)
}

/// The directory, inside `store`, that application `app`'s source is in, and
/// the application.
///
/// The application must be built from *this* store: the IDE has this store open,
/// and a terminal started in a directory the file tree does not show would edit
/// files the admin cannot see.
async fn app_source_dir(
    catalog: &Catalog,
    store: &str,
    app: &str,
) -> Result<(String, Application), String> {
    let apps = sc_app::list_applications(catalog)
        .await
        .map_err(|e| format!("could not read the applications: {e}"))?;
    let found = apps
        .into_iter()
        .find(|a| a.name == app)
        .ok_or_else(|| format!("there is no application named {app}"))?;
    let (source_store, dir) = sc_app::app_source_location(&found)
        .map_err(|_| format!("{app} has no source directory"))?;
    if source_store != store {
        return Err(format!("{app}'s source is in {source_store}, not {store}"));
    }
    Ok((dir, found))
}

/// The executable `program` names, if it is installed, looked up on `path_env`
/// when it is a bare name.
///
/// Looked up here rather than by the spawn, so that "not installed" is a
/// sentence before the socket opens rather than an `ENOENT` after it, and so
/// the child is started by absolute path.
fn locate_on(program: &Path, path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return is_executable(program).then(|| program.to_path_buf());
    }
    std::env::split_paths(path_env?)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// Where [`find_claude`] found the executable, for the Development tab's line
/// saying which `claude` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeSource {
    /// The server's `--claude-cli`.
    Option,
    /// Settings → Development's "Claude Code executable".
    Setting,
    /// `claude` on the server's `PATH`.
    Path,
    /// One of [`USUAL_LOCATIONS`], or nvm's newest Node.
    Usual,
}

impl ClaudeSource {
    /// How the status endpoint names it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ClaudeSource::Option => "option",
            ClaudeSource::Setting => "setting",
            ClaudeSource::Path => "path",
            ClaudeSource::Usual => "usual",
        }
    }
}

/// Where Claude Code's installers put `claude`, relative to the home directory,
/// in the order they are tried after `PATH`: the native installer, the older
/// local install, npm with a user prefix, bun, volta. A server started by a
/// service manager has a short `PATH` that none of these is on.
pub(crate) const USUAL_LOCATIONS: &[&str] = &[
    ".local/bin/claude",
    ".claude/local/claude",
    ".npm-global/bin/claude",
    ".bun/bin/claude",
    ".volta/bin/claude",
];

/// The Claude Code to run, and where it was found, or why there is none.
///
/// In this order: the server's `--claude-cli`, Settings → Development's
/// executable, `claude` on `PATH`, the [`USUAL_LOCATIONS`] under the home
/// directory, then the newest Node under nvm. A path given explicitly — the
/// option or the setting — is the answer or the refusal: it is never quietly
/// replaced by another `claude`, which would be a different program from the
/// one the admin named.
pub(crate) fn find_claude_in(
    option: Option<&Path>,
    setting: Option<&Path>,
    path_env: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Result<(PathBuf, ClaudeSource), String> {
    let named = |program: &Path, by: &str, source: ClaudeSource| {
        locate_on(program, path_env)
            .map(|exe| (exe, source))
            .ok_or_else(|| {
                format!(
                    "{CLAUDE_BIN} is not installed on this server: {by} names {}, which is not \
                     an executable file",
                    program.display()
                )
            })
    };
    if let Some(program) = option {
        return named(program, "--claude-cli", ClaudeSource::Option);
    }
    if let Some(program) = setting {
        return named(program, "Settings → Development", ClaudeSource::Setting);
    }
    if let Some(exe) = locate_on(Path::new(CLAUDE_BIN), path_env) {
        return Ok((exe, ClaudeSource::Path));
    }
    if let Some(home) = home {
        let usual = USUAL_LOCATIONS
            .iter()
            .map(|rel| home.join(rel))
            .chain(newest_nvm_claude(home));
        for candidate in usual {
            if is_executable(&candidate) {
                return Ok((candidate, ClaudeSource::Usual));
            }
        }
    }
    Err(format!(
        "{CLAUDE_BIN} is not installed on this server (looked on PATH and in the usual install \
         locations); give its full path under Settings → Development"
    ))
}

/// [`find_claude_in`] with this server's option, settings, `PATH` and home.
pub(crate) async fn find_claude(
    catalog: Option<&Catalog>,
    option: Option<&Path>,
) -> Result<(PathBuf, ClaudeSource), String> {
    let setting = match (option, catalog) {
        // The option comes first, so the settings need not be read.
        (Some(_), _) | (None, None) => None,
        (None, Some(catalog)) => sc_config::claude_code_path(catalog)
            .await
            .map_err(|e| format!("could not read the settings: {e}"))?,
    };
    let path_env = std::env::var_os("PATH");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    find_claude_in(
        option,
        setting.as_deref(),
        path_env.as_deref(),
        home.as_deref(),
    )
}

/// How long `claude --version` may take before the status says it did not
/// answer. A Node CLI starts in well under a second; a hung one must not hold
/// the settings screen.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Which Claude Code this server would run, for the Development tab: where it
/// is and how it was found, and what `--version` says, or why there is none.
pub(crate) async fn claude_status(catalog: &Catalog, option: Option<&Path>) -> serde_json::Value {
    let enabled = terminal_off(catalog).await.is_none();
    match find_claude(Some(catalog), option).await {
        Ok((path, source)) => {
            let (version, problem) = match claude_version(&path).await {
                Ok(version) => (Some(version), None),
                Err(problem) => (None, Some(problem)),
            };
            json!({
                "enabled": enabled,
                "path": path.display().to_string(),
                "source": source.as_str(),
                "version": version,
                "problem": problem,
            })
        }
        Err(problem) => json!({
            "enabled": enabled,
            "path": null,
            "source": null,
            "version": null,
            "problem": problem,
        }),
    }
}

/// The first line `program --version` prints, run with the environment a
/// terminal's program gets.
async fn claude_version(program: &Path) -> Result<String, String> {
    let mut command = tokio::process::Command::new(program);
    command
        .arg("--version")
        .env_clear()
        .envs(program_env())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = path_with_dir_of(program, std::env::var_os("PATH").as_deref()) {
        command.env("PATH", path);
    }
    let output = tokio::time::timeout(VERSION_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("{} --version did not answer", program.display()))?
        .map_err(|e| format!("{} could not be run: {e}", program.display()))?;
    let first = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
    };
    if !output.status.success() {
        return Err(format!(
            "{} --version failed: {}",
            program.display(),
            first(&output.stderr).unwrap_or_else(|| output.status.to_string())
        ));
    }
    first(&output.stdout).ok_or_else(|| format!("{} --version printed nothing", program.display()))
}

/// `claude` in the newest Node version nvm has, if any has one. Newest by
/// version number, not by name: `v9` sorts after `v22` as text.
fn newest_nvm_claude(home: &Path) -> Option<PathBuf> {
    let versions = std::fs::read_dir(home.join(".nvm/versions/node")).ok()?;
    let number = |name: &str| -> Vec<u64> {
        name.trim_start_matches('v')
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    versions
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let claude = entry.path().join("bin").join(CLAUDE_BIN);
            is_executable(&claude).then(|| (number(&entry.file_name().to_string_lossy()), claude))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, claude)| claude)
}

/// The child's `PATH`: the executable's own directory first. A `claude`
/// installed with npm is a Node script that starts with `#!/usr/bin/env node`,
/// and under nvm that `node` sits beside it and nowhere on a service
/// manager's `PATH`.
fn path_with_dir_of(program: &Path, path_env: Option<&std::ffi::OsStr>) -> Option<String> {
    let dir = program.parent().filter(|d| !d.as_os_str().is_empty())?;
    let mut dirs = vec![dir.to_path_buf()];
    if let Some(path) = path_env {
        dirs.extend(std::env::split_paths(path).filter(|d| d != dir));
    }
    std::env::join_paths(dirs)
        .ok()
        .map(|joined| joined.to_string_lossy().into_owned())
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// What the route needs from the server to start a terminal.
pub(crate) struct TerminalHost<'a> {
    pub(crate) catalog: Option<&'a Arc<Catalog>>,
    pub(crate) slots: &'a TerminalSlots,
    pub(crate) tokens: &'a SessionTokens,
    /// The loopback URL a terminal's Claude Code uses to reach this server
    /// (`http://127.0.0.1:<port>`), or the reason there is none.
    pub(crate) origin: &'a Result<String, String>,
    /// The server's `--claude-cli`, when it was given ([`find_claude`]).
    pub(crate) program: Option<Arc<PathBuf>>,
}

/// Serve one terminal socket for `store`, opened from the admin session
/// `admin` watches. The session is lost, and the terminal with it, when the
/// admin signs out or stops being one.
pub(crate) async fn terminal_upgrade(
    ws: WebSocketUpgrade,
    host: TerminalHost<'_>,
    admin: AdminWatch,
    store: String,
    query: TerminalQuery,
) -> axum::response::Response {
    let TerminalHost {
        catalog,
        slots,
        tokens,
        origin,
        program,
    } = host;
    let refusal = match terminal_dir(catalog, &store, query.app.as_deref()).await {
        Err(reason) => reason,
        Ok(place) => match find_claude(
            catalog.map(Arc::as_ref),
            program.as_deref().map(PathBuf::as_path),
        )
        .await
        {
            Err(reason) => reason,
            Ok((exe, _)) => match Arc::clone(slots).try_acquire_owned() {
                Ok(permit) => {
                    let token = tokens.issue(admin.session().clone());
                    let wiring = Wiring {
                        origin: origin.clone(),
                        offered: match catalog {
                            Some(catalog) => offered_servers(catalog).await,
                            None => Vec::new(),
                        },
                        prompt: place.app.as_ref().and_then(builder_prompt),
                        api_key: match catalog {
                            Some(catalog) => anthropic_provider(catalog).await,
                            None => None,
                        },
                        config_dir: sc_files::data_dir().ok().map(|d| d.join("claude")),
                    };
                    // Before the program starts, so nothing it writes first is missed.
                    let changes = watch(&store, &place);
                    match Launch::prepare(exe, place.dir, wiring, token) {
                        Ok(launch) => {
                            return ws.on_upgrade(move |socket| {
                                bridge(socket, launch, store, changes, admin, permit)
                            });
                        }
                        Err(e) => format!("could not prepare the session: {e}"),
                    }
                }
                Err(_) => format!(
                    "the server is already running its maximum of {MAX_TERMINALS} terminals"
                ),
            },
        },
    };
    ws.on_upgrade(move |socket| refuse(socket, refusal))
}

/// The environment the program starts with: the allow-listed part of the
/// server's, and a terminal type the browser's terminal renders.
fn program_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = INHERITED_ENV
        .iter()
        .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
        .collect();
    env.push(("TERM".to_owned(), "xterm-256color".to_owned()));
    env.push(("COLORTERM".to_owned(), "truecolor".to_owned()));
    env
}

/// The builder-agent prompt of `app`'s framework (§13.3), which carries what
/// the CLI would otherwise get wrong, such as which directory is generated.
fn builder_prompt(app: &Application) -> Option<String> {
    sc_app::framework_builder_agent(&app.framework, app).map(|agent| agent.system_prompt)
}

/// The first Anthropic provider with a key: `(key, base URL if not the default)`.
async fn anthropic_provider(catalog: &Catalog) -> Option<(String, Option<String>)> {
    let providers = sc_llm::list_llm_providers(catalog).await.ok()?;
    providers
        .iter()
        .filter(|p| p.backend == sc_llm::ANTHROPIC_BACKEND)
        .find_map(|p| {
            let key = p.setting(sc_llm::CFG_API_KEY)?.trim();
            let base = p
                .setting(sc_llm::CFG_BASE_URL)
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_owned);
            (!key.is_empty()).then(|| (key.to_owned(), base))
        })
}

/// The MCP servers offered to the terminal (`offer_to_terminal`), or none when
/// they cannot be read: a session without them is still a session.
async fn offered_servers(catalog: &Catalog) -> Vec<sc_mcp::McpServerDef> {
    if !matches!(catalog.get(sc_mcp::MCP_SERVERS_TABLE), Ok(Some(_))) {
        return Vec::new();
    }
    match sc_mcp::list_mcp_servers(catalog).await {
        Ok(servers) => servers
            .into_iter()
            .filter(sc_mcp::McpServerDef::offered_to_terminal)
            .collect(),
        Err(e) => {
            eprintln!("feldspar: terminal: could not read the MCP servers: {e}");
            Vec::new()
        }
    }
}

/// What connects a session to this installation, gathered before it starts.
struct Wiring {
    /// This server over loopback, or why the session goes without MCP.
    origin: Result<String, String>,
    /// The MCP servers offered to the terminal.
    offered: Vec<sc_mcp::McpServerDef>,
    /// The framework's builder-agent prompt, in an application's directory.
    prompt: Option<String>,
    /// An Anthropic provider's key and base URL.
    api_key: Option<(String, Option<String>)>,
    /// Where the CLI keeps its login: the installation's, not the Unix user's.
    config_dir: Option<PathBuf>,
}

/// A program ready to start: what to run, where, with what, and what has to go
/// when it ends.
struct Launch {
    program: PathBuf,
    dir: PathBuf,
    args: Vec<std::ffi::OsString>,
    env: Vec<(String, String)>,
    /// Said in the terminal before the program starts.
    notices: Vec<String>,
    /// Removed from the session map when the launch is dropped.
    _token: SessionToken,
    /// Deleted when the launch is dropped.
    _mcp_config: Option<TempFile>,
}

impl Launch {
    /// The launch line of design §12.1. The MCP config, which holds the
    /// session's token, is written here to a private file outside the store.
    fn prepare(
        program: PathBuf,
        dir: PathBuf,
        wiring: Wiring,
        token: SessionToken,
    ) -> std::io::Result<Launch> {
        let mut args: Vec<std::ffi::OsString> = Vec::new();
        // Only when there is one: in a directory with no conversation yet, an
        // interactive `--continue` says so and exits.
        if wiring
            .config_dir
            .as_deref()
            .is_some_and(|config| has_conversation(config, &dir))
        {
            args.push("--continue".into());
        }
        let mut notices = Vec::new();
        let mcp_config = match &wiring.origin {
            Ok(origin) => {
                let entry = |url: String| {
                    json!({
                        "type": "http",
                        "url": url,
                        "headers": { "Authorization": format!("Bearer {}", token.secret) },
                    })
                };
                let mut servers = serde_json::Map::new();
                servers.insert(
                    MCP_SERVER_KEY.to_owned(),
                    entry(format!("{origin}{}", crate::mcp::MCP_ROUTE)),
                );
                for server in &wiring.offered {
                    if server.name == MCP_SERVER_KEY {
                        notices.push(format!(
                            "The MCP server `{MCP_SERVER_KEY}` is left out: this server's own \
                             tools have that name."
                        ));
                    } else if server.transport != sc_mcp::McpTransport::StreamableHttp {
                        notices.push(format!(
                            "The MCP server `{}` is left out: it uses SSE, which the terminal \
                             cannot reach through this server yet.",
                            server.name
                        ));
                    } else {
                        servers.insert(
                            server.name.clone(),
                            entry(crate::mcp_proxy::proxy_url(origin, &server.name)),
                        );
                    }
                }
                let config = json!({ "mcpServers": servers });
                let file = TempFile::private(
                    &format!("feldspar-terminal-{}.json", Uuid::new_v4().simple()),
                    config.to_string().as_bytes(),
                )?;
                args.push("--mcp-config".into());
                args.push(file.path.clone().into_os_string());
                Some(file)
            }
            Err(why) => {
                notices.push(format!(
                    "Claude Code is not connected to this server: {why}."
                ));
                None
            }
        };
        // Whatever the operator's own account has configured stays out: the
        // session sees this server and nothing else.
        args.push("--strict-mcp-config".into());
        if let Some(prompt) = wiring.prompt {
            args.push("--append-system-prompt".into());
            args.push(prompt.into());
        }

        let mut env = program_env();
        if let Some(path) = path_with_dir_of(&program, std::env::var_os("PATH").as_deref()) {
            env.retain(|(name, _)| name != "PATH");
            env.push(("PATH".to_owned(), path));
        }
        if let Some(dir) = wiring.config_dir {
            env.push(("CLAUDE_CONFIG_DIR".to_owned(), dir.display().to_string()));
        }
        if let Some((key, base)) = wiring.api_key {
            env.push(("ANTHROPIC_API_KEY".to_owned(), key));
            if let Some(base) = base {
                env.push(("ANTHROPIC_BASE_URL".to_owned(), base));
            }
        }
        Ok(Launch {
            program,
            dir,
            args,
            env,
            notices,
            _token: token,
            _mcp_config: mcp_config,
        })
    }
}

/// Whether Claude Code, keeping its state in `config_dir`, has a conversation
/// saved for `dir`.
///
/// It keeps them as `projects/<dir>/<session>.jsonl`, the directory's real path
/// with every character but a letter or a digit written as `-`. That layout is
/// the CLI's, not a promise: should it change, this answers no, and a session
/// starts a new conversation rather than failing.
fn has_conversation(config_dir: &Path, dir: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let key: String = dir
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    std::fs::read_dir(config_dir.join("projects").join(key)).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
    })
}

/// A file only this user can read, in the system's temporary directory,
/// deleted when dropped.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn private(name: &str, contents: &[u8]) -> std::io::Result<TempFile> {
        use std::io::Write;
        let path = std::env::temp_dir().join(name);
        let mut options = std::fs::OpenOptions::new();
        // `create_new`: never through a file or link someone else put there.
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut handle = options.open(&path)?;
        // Owned before it is written, so a failed write still deletes it.
        let file = TempFile { path };
        handle.write_all(contents)?;
        Ok(file)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The watch on a terminal's directory: what changes in it is reported to the
/// browser ([`crate::terminal_watch`]). None off Unix, where there is no
/// terminal to report for.
#[cfg(unix)]
type Watch = crate::terminal_watch::Changes;
#[cfg(not(unix))]
type Watch = std::convert::Infallible;

/// The watch on `place`'s directory, or `None` when it cannot be had. A
/// session without one still works; its editors just are not told.
#[cfg(unix)]
fn watch(store: &str, place: &TerminalPlace) -> Option<Watch> {
    crate::terminal_watch::Changes::watch(&place.root, &place.dir)
        .inspect_err(|e| {
            eprintln!("feldspar: terminal for {store}: not watching its directory: {e}")
        })
        .ok()
}

#[cfg(not(unix))]
fn watch(_store: &str, _place: &TerminalPlace) -> Option<Watch> {
    None
}

/// Run the launch on a pseudo-terminal, bridged to `socket` until either end
/// stops or `admin`'s session is lost, and send what `changes` reports.
/// `permit` is held for as long as the process lives, and the launch's session
/// token and MCP config for as long as the bridge runs.
#[cfg(unix)]
async fn bridge(
    mut socket: WebSocket,
    launch: Launch,
    store: String,
    mut changes: Option<Watch>,
    admin: AdminWatch,
    permit: OwnedSemaphorePermit,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for notice in &launch.notices {
        let line = format!("{notice}\r\n");
        let _ = socket.send(Message::Binary(line.into_bytes().into())).await;
    }
    let mut session = match pty::spawn(
        &launch.program,
        &launch.args,
        &launch.dir,
        &launch.env,
        DEFAULT_SIZE,
    ) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("feldspar: terminal for {store}: {e}");
            refuse(
                socket,
                format!("could not start {}: {e}", launch.program.display()),
            )
            .await;
            return;
        }
    };

    let (mut sender, mut receiver) = socket.split();
    let (mut from_pty, mut to_pty) = session.pty.split();
    // Whether the browser has asked for a pause: set by the loop that reads the
    // socket, waited on by the loop that reads the terminal.
    let (pause, mut paused) = tokio::sync::watch::channel(false);

    // Browser → program: keystrokes, and resizes on the side.
    let to_program = async {
        while let Some(Ok(message)) = receiver.next().await {
            match message {
                Message::Binary(bytes) => {
                    if to_pty.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Message::Text(text) => match control_message(&text) {
                    Some(Control::Resize(size)) => {
                        let _ = to_pty.resize(pty_process::Size::new(size.rows, size.cols));
                    }
                    Some(Control::Pause(on)) => {
                        pause.send_replace(on);
                    }
                    None => {}
                },
                Message::Close(_) => break,
                // Ping/Pong are answered by axum itself.
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    };

    // Program → browser. Ends when the program and everything it started have
    // closed the terminal, which is how a pty says "exited": Linux reports it
    // as `EIO` rather than end-of-file, so an error ends it the same way.
    let to_browser = async {
        let mut buf = vec![0_u8; 16 * 1024];
        loop {
            // The sender lives in the other loop, so neither wait can fail
            // while the bridge runs.
            let _ = paused.wait_for(|on| !*on).await;
            // A pause also cuts short a read already waiting, so nothing more
            // is sent once it has arrived. `biased`: when both are ready, the
            // pause wins; dropping the read loses nothing, as it is cancel-safe.
            let message = tokio::select! {
                biased;
                _ = paused.wait_for(|on| *on) => continue,
                read = from_pty.read(&mut buf) => match read {
                    Ok(0) | Err(_) => return true,
                    Ok(n) => Message::Binary(bytes::Bytes::copy_from_slice(&buf[..n])),
                },
                // Cancel-safe, like the read: what was gathered stays gathered.
                report = next_changes(&mut changes) => {
                    let frame = json!({ "changed": report.changed, "removed": report.removed });
                    Message::Text(frame.to_string().into())
                }
            };
            if sender.send(message).await.is_err() {
                return false;
            }
        }
    };

    // A sign-out, a disabled account or a demotion ends the session here, not
    // by dropping the bridge, so that the kill below still reaches everything
    // the program started.
    let end = tokio::select! {
        () = to_program => End::Browser,
        exited = to_browser => if exited { End::Program } else { End::Browser },
        lost = admin.lost() => End::Lost(lost),
    };

    let reason = match end {
        End::Browser => None,
        End::Lost(reason) => Some(reason),
        End::Program => {
            let status = tokio::time::timeout(EXIT_STATUS_WAIT, session.child.wait()).await;
            Some(match status {
                Ok(Ok(status)) => match status.code() {
                    Some(code) => format!("the program exited with status {code}"),
                    None => "the program was stopped by a signal".to_owned(),
                },
                _ => "the program closed its terminal".to_owned(),
            })
        }
    };
    if let Some(reason) = reason {
        let close = sender.send(Message::Close(Some(CloseFrame {
            code: close_code::NORMAL,
            reason: fit_close_reason(reason).into(),
        })));
        let _ = tokio::time::timeout(CLOSE_SEND_WAIT, close).await;
    }

    // One session per connection, and it goes when the connection does —
    // including whatever the program started.
    session.kill().await;
    drop(permit);
    // The token stops opening MCP, and the config file holding it goes.
    drop(launch);
}

/// Why a bridge stopped.
#[cfg(unix)]
enum End {
    /// The browser closed the socket, or it broke.
    Browser,
    /// The program and everything it started closed the terminal.
    Program,
    /// The admin session it was opened from is lost, said as a close frame
    /// says it.
    Lost(String),
}

/// The next changes `changes` reports; never, when there is no watch or it
/// has ended.
#[cfg(unix)]
async fn next_changes(changes: &mut Option<Watch>) -> crate::terminal_watch::Report {
    if let Some(watch) = changes.as_mut()
        && let Some(report) = watch.next().await
    {
        return report;
    }
    *changes = None;
    std::future::pending().await
}

/// No pseudo-terminal off Unix.
#[cfg(not(unix))]
async fn bridge(
    socket: WebSocket,
    _launch: Launch,
    _store: String,
    _changes: Option<Watch>,
    _admin: AdminWatch,
    _permit: OwnedSemaphorePermit,
) {
    refuse(socket, "the IDE terminal needs a Unix server".to_owned()).await;
}

#[cfg(unix)]
mod pty {
    //! A program on a pseudo-terminal, and how it is ended.

    use std::ffi::OsString;
    use std::path::Path;

    use tokio::process::Child;

    use super::TerminalSize;

    /// A running program and the terminal it is attached to.
    pub(super) struct Session {
        pub(super) pty: pty_process::Pty,
        pub(super) child: Child,
        /// The program's process id, which is also its session's id. Kept
        /// from the start, because `child` forgets it once the program has
        /// been waited for, and what it started may still be running then.
        pid: Option<libc::pid_t>,
    }

    /// Start `program` with `args` in `dir` with exactly `env`, on a new
    /// terminal of `size`.
    ///
    /// The program leads a new session with the terminal as its controlling one,
    /// which is what turns Ctrl-C into `SIGINT` and lets [`Session::kill`] find
    /// what it started.
    pub(super) fn spawn(
        program: &Path,
        args: &[OsString],
        dir: &Path,
        env: &[(String, String)],
        size: TerminalSize,
    ) -> pty_process::Result<Session> {
        let (pty, pts) = pty_process::open()?;
        pty.resize(pty_process::Size::new(size.rows, size.cols))?;
        // `spawn` takes the terminal's other end and drops the parent's copies,
        // so reading `pty` ends once the child and its descendants let go.
        let child = pty_process::Command::new(program)
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .kill_on_drop(true)
            .spawn(pts)?;
        let pid = child.id().and_then(|p| libc::pid_t::try_from(p).ok());
        Ok(Session { pty, child, pid })
    }

    impl Session {
        /// End the program and everything it started.
        ///
        /// "Everything it started" is not its process group. A shell with job
        /// control — which is what an interactive shell on a terminal is — puts
        /// each background job in a group of its own, and a program can call
        /// `setsid` for a child (Node's `detached: true` does), so `killpg` on
        /// the leader's group misses both. What is signalled instead is
        /// [`processes_to_end`]: the terminal's whole session, and every
        /// descendant of the program wherever it moved to.
        ///
        /// `SIGHUP` first, which is what closing a terminal sends and what a
        /// program expects on it; then `SIGKILL` for whatever ignored that, and
        /// a reap so no zombie is left behind.
        ///
        /// The program may already have exited and been waited for (it closed
        /// the terminal on its own); what it left running in its session is
        /// ended all the same.
        pub(super) async fn kill(mut self) {
            let Some(pid) = self.pid else {
                let _ = self.child.kill().await;
                return;
            };
            // Listed before anything is signalled: once the leader is gone its
            // children are re-parented, and the tree no longer leads to them.
            let first = processes_to_end(pid);
            signal_all(pid, &first, libc::SIGHUP);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), self.child.wait())
                .await;
            let mut last = processes_to_end(pid);
            last.extend(first);
            signal_all(pid, &last, libc::SIGKILL);
            let _ = self.child.kill().await;
        }
    }

    /// Send `signal` to the leader's group and to each of `pids`.
    fn signal_all(leader: libc::pid_t, pids: &[libc::pid_t], signal: libc::c_int) {
        // SAFETY: `killpg` and `kill` take plain integers and touch no memory.
        // `leader` leads its own group: `spawn` started it in a session of its own.
        unsafe {
            libc::killpg(leader, signal);
            for &pid in pids {
                libc::kill(pid, signal);
            }
        }
    }

    /// The processes in the session `leader` leads, and its descendants.
    ///
    /// Read from `/proc`, which is Linux's. Elsewhere this is empty and the
    /// leader's process group is all that is signalled.
    #[cfg(target_os = "linux")]
    fn processes_to_end(leader: libc::pid_t) -> Vec<libc::pid_t> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        // pid → (parent, session), for every process this user can read.
        let table: Vec<(libc::pid_t, libc::pid_t, libc::pid_t)> = entries
            .flatten()
            .filter_map(|entry| {
                let pid: libc::pid_t = entry.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
                let (parent, session) = parse_stat(&stat)?;
                Some((pid, parent, session))
            })
            .collect();
        let mut found: Vec<libc::pid_t> = table
            .iter()
            .filter(|(pid, _, session)| *session == leader && *pid != leader)
            .map(|(pid, _, _)| *pid)
            .collect();
        // The walk keeps its own visited set rather than consulting `found`: a
        // session member is already in `found`, and its children (which may
        // have left the session) must still be walked.
        let mut visited = vec![leader];
        let mut frontier = vec![leader];
        while let Some(parent) = frontier.pop() {
            for (pid, ppid, _) in &table {
                if *ppid == parent && !visited.contains(pid) {
                    visited.push(*pid);
                    frontier.push(*pid);
                    if !found.contains(pid) {
                        found.push(*pid);
                    }
                }
            }
        }
        found
    }

    #[cfg(not(target_os = "linux"))]
    fn processes_to_end(_leader: libc::pid_t) -> Vec<libc::pid_t> {
        Vec::new()
    }

    /// The parent and session ids out of a `/proc/<pid>/stat` line.
    ///
    /// The command name is in parentheses and may itself contain spaces and
    /// parentheses, so the fields are counted from the **last** `)`: state,
    /// parent, process group, session.
    #[cfg(target_os = "linux")]
    pub(super) fn parse_stat(stat: &str) -> Option<(libc::pid_t, libc::pid_t)> {
        let mut fields = stat.get(stat.rfind(')')? + 1..)?.split_whitespace();
        let _state = fields.next()?;
        let parent = fields.next()?.parse().ok()?;
        let _group = fields.next()?;
        let session = fields.next()?.parse().ok()?;
        Some((parent, session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resize_and_a_pause_are_the_only_control_messages() {
        assert_eq!(
            control_message(r#"{"resize":{"cols":120,"rows":40}}"#),
            Some(Control::Resize(TerminalSize {
                cols: 120,
                rows: 40
            }))
        );
        assert_eq!(control_message(r#"{"resize":{"cols":0,"rows":40}}"#), None);
        assert_eq!(
            control_message(r#"{"resize":{"cols":80,"rows":5000}}"#),
            None
        );
        assert_eq!(
            control_message(r#"{"pause":true}"#),
            Some(Control::Pause(true))
        );
        assert_eq!(
            control_message(r#"{"pause":false}"#),
            Some(Control::Pause(false))
        );
        assert_eq!(control_message(r#"{"pause":"yes"}"#), None);
        assert_eq!(control_message("ls -la"), None, "text is not typed");
        assert_eq!(control_message(r#"{"other":1}"#), None);
    }

    /// A fresh directory for one test's fake installs.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sc-find-claude-{tag}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An executable file at `path`, with its directories.
    fn install(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path.to_path_buf()
    }

    #[test]
    fn claude_is_looked_for_on_path_then_in_the_usual_places() {
        let root = scratch("order");
        let home = root.join("home");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths([&bin]).unwrap();

        // Nowhere: the refusal says where to give it.
        let err = find_claude_in(None, None, Some(&path), Some(&home)).unwrap_err();
        assert!(err.contains("claude is not installed"), "{err}");
        assert!(err.contains("Settings → Development"), "{err}");

        // A service manager's PATH misses ~/.local/bin; the usual places do not.
        let local = install(&home.join(".local/bin/claude"));
        assert_eq!(
            find_claude_in(None, None, Some(&path), Some(&home)).unwrap(),
            (local.clone(), ClaudeSource::Usual)
        );
        // On PATH comes first.
        let on_path = install(&bin.join("claude"));
        assert_eq!(
            find_claude_in(None, None, Some(&path), Some(&home)).unwrap(),
            (on_path, ClaudeSource::Path)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A path the admin named is the answer or the refusal, never swapped for
    /// another `claude` that happens to be installed.
    #[test]
    fn a_named_claude_comes_first_and_is_not_replaced() {
        let root = scratch("named");
        let bin = root.join("bin");
        install(&bin.join("claude"));
        let path = std::env::join_paths([&bin]).unwrap();
        let chosen = install(&root.join("opt/claude"));
        let option = install(&root.join("flag/claude"));

        assert_eq!(
            find_claude_in(None, Some(&chosen), Some(&path), None).unwrap(),
            (chosen.clone(), ClaudeSource::Setting)
        );
        assert_eq!(
            find_claude_in(Some(&option), Some(&chosen), Some(&path), None).unwrap(),
            (option, ClaudeSource::Option)
        );
        let missing = root.join("gone/claude");
        let err = find_claude_in(None, Some(&missing), Some(&path), None).unwrap_err();
        assert!(err.contains("claude is not installed"), "{err}");
        assert!(err.contains("Settings → Development names"), "{err}");
        let err = find_claude_in(Some(&missing), None, Some(&path), None).unwrap_err();
        assert!(err.contains("--claude-cli names"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_newest_node_under_nvm_is_the_one_used() {
        let root = scratch("nvm");
        let node = root.join(".nvm/versions/node");
        install(&node.join("v9.11.2/bin/claude"));
        let newest = install(&node.join("v22.12.0/bin/claude"));
        install(&node.join("v20.19.0/bin/claude"));
        // A newer Node without claude is passed over.
        std::fs::create_dir_all(node.join("v24.1.0/bin")).unwrap();
        assert_eq!(newest_nvm_claude(&root), Some(newest.clone()));
        assert_eq!(
            find_claude_in(None, None, None, Some(&root)).unwrap(),
            (newest, ClaudeSource::Usual)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// An npm-installed `claude` is `#!/usr/bin/env node`, and under nvm its
    /// `node` is beside it, not on a service manager's PATH.
    #[test]
    fn the_program_runs_with_its_own_directory_first_on_path() {
        let path =
            std::env::join_paths(["/usr/bin", "/home/me/.nvm/versions/node/v22/bin"]).unwrap();
        let joined = path_with_dir_of(
            Path::new("/home/me/.nvm/versions/node/v22/bin/claude"),
            Some(&path),
        )
        .unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(
            dirs,
            [
                PathBuf::from("/home/me/.nvm/versions/node/v22/bin"),
                PathBuf::from("/usr/bin"),
            ]
        );
        assert_eq!(path_with_dir_of(Path::new("claude"), Some(&path)), None);
    }

    #[test]
    fn a_bare_name_is_found_on_path_and_a_missing_one_is_not() {
        let path = std::env::var_os("PATH");
        let locate = |program: &str| locate_on(Path::new(program), path.as_deref());
        assert!(locate("sh").is_some());
        assert!(locate("definitely-not-installed-feldspar-cli").is_none());
        assert!(locate("/nonexistent/claude").is_none());
    }

    #[test]
    fn the_server_environment_is_not_inherited_wholesale() {
        let env = program_env();
        assert!(env.iter().all(|(k, _)| k != "DATABASE_URL"));
        assert!(
            env.iter()
                .any(|(k, v)| k == "TERM" && v == "xterm-256color")
        );
    }

    /// A command name may hold spaces and parentheses of its own.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stat_line_is_read_from_the_last_parenthesis() {
        let stat = "4242 (node (worker) x) S 4100 4242 4100 34817 4242 4194560";
        assert_eq!(pty::parse_stat(stat), Some((4100, 4100)));
        assert_eq!(pty::parse_stat("garbage"), None);
    }

    #[test]
    fn mcp_is_reached_over_loopback_or_not_at_all() {
        let url = |addr: &str| loopback_origin(addr.parse().unwrap());
        assert_eq!(url("0.0.0.0:3032"), Ok("http://127.0.0.1:3032".to_owned()));
        assert_eq!(
            url("127.0.0.1:3032"),
            Ok("http://127.0.0.1:3032".to_owned())
        );
        assert_eq!(url("[::]:3032"), Ok("http://[::1]:3032".to_owned()));
        assert!(url("192.168.1.5:3032").unwrap_err().contains("192.168.1.5"));
        assert!(url("0.0.0.0:0").is_err(), "an unknown port");
    }

    /// A session in a store of its own, for a test that needs a token.
    async fn admin() -> AdminSession {
        AdminSession::signed_in(&sc_auth::SessionStore::memory()).await
    }

    #[tokio::test]
    async fn a_session_token_lasts_as_long_as_its_session() {
        let tokens = SessionTokens::default();
        let admin = admin().await;
        let user = admin.user().id();
        let token = tokens.issue(admin);
        assert!(token.secret.starts_with(SESSION_TOKEN_PREFIX));
        assert_eq!(
            tokens.grant(&token.secret).map(|g| g.admin.user().id()),
            Some(user)
        );
        assert!(tokens.grant("fdterm_guessed").is_none());
        assert!(tokens.grant("fd_some_table_token").is_none());
        let secret = token.secret.clone();
        drop(token);
        assert!(tokens.grant(&secret).is_none());
    }

    fn wiring(origin: Result<String, String>) -> Wiring {
        Wiring {
            origin,
            offered: vec![
                sc_mcp::McpServerDef::new("crm", "https://crm.example.com/mcp")
                    .header("X-Api-Key", "crm-secret")
                    .offer_to_terminal(true),
                sc_mcp::McpServerDef::new("tickets", "https://tickets.example.com/sse")
                    .transport(sc_mcp::McpTransport::Sse)
                    .offer_to_terminal(true),
            ],
            prompt: Some("You build the shop.".to_owned()),
            api_key: Some(("sk-ant-test".to_owned(), None)),
            config_dir: Some(PathBuf::from("/data/claude")),
        }
    }

    #[tokio::test]
    async fn the_launch_line_wires_the_session_to_this_server() {
        let tokens = SessionTokens::default();
        let launch = Launch::prepare(
            PathBuf::from("/usr/bin/claude"),
            PathBuf::from("/store/shop"),
            wiring(Ok("http://127.0.0.1:3032".to_owned())),
            tokens.issue(admin().await),
        )
        .unwrap();
        let args: Vec<String> = launch
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args.contains(&"--continue".to_owned()),
            "nothing to continue yet"
        );
        let config_at = args.iter().position(|a| a == "--mcp-config").unwrap() + 1;
        let config_path = PathBuf::from(&args[config_at]);
        assert!(!config_path.starts_with("/store"), "outside the store");
        assert!(args.contains(&"--strict-mcp-config".to_owned()));
        let prompt_at = args
            .iter()
            .position(|a| a == "--append-system-prompt")
            .unwrap()
            + 1;
        assert_eq!(args[prompt_at], "You build the shop.");

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let server = &config["mcpServers"][MCP_SERVER_KEY];
        assert_eq!(server["url"], "http://127.0.0.1:3032/mcp");
        let secret = server["headers"]["Authorization"]
            .as_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap()
            .to_owned();
        assert!(tokens.grant(&secret).is_some());
        // An offered server through the proxy, with the session's token and
        // never its own header; an SSE one left out, with a reason.
        let crm = &config["mcpServers"]["crm"];
        assert_eq!(crm["url"], "http://127.0.0.1:3032/ide/mcp-proxy/crm");
        assert_eq!(crm["headers"]["Authorization"], format!("Bearer {secret}"));
        assert!(!config.to_string().contains("crm-secret"));
        assert!(config["mcpServers"].get("tickets").is_none());
        assert!(
            launch
                .notices
                .iter()
                .any(|n| n.contains("`tickets`") && n.contains("SSE")),
            "{:?}",
            launch.notices
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&config_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "only this user reads the token");
        }

        let env: HashMap<_, _> = launch.env.iter().cloned().collect();
        assert_eq!(env["CLAUDE_CONFIG_DIR"], "/data/claude");
        assert_eq!(env["ANTHROPIC_API_KEY"], "sk-ant-test");
        assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!env.contains_key("DATABASE_URL"));

        // The session ends: the token stops working and the file goes.
        drop(launch);
        assert!(tokens.grant(&secret).is_none());
        assert!(!config_path.exists());
    }

    /// `--continue` only where the CLI has a conversation saved for the
    /// directory: with none, an interactive `--continue` exits.
    #[tokio::test]
    async fn a_session_continues_only_where_there_is_a_conversation() {
        let root = std::env::temp_dir().join(format!("sc-continue-{}", Uuid::new_v4().simple()));
        let config = root.join("claude");
        let dir = root.join("my_store.v2");
        std::fs::create_dir_all(&dir).unwrap();
        let admin = admin().await;
        let continues = |config_dir: &Path| {
            let mut wiring = wiring(Err("no MCP here".to_owned()));
            wiring.config_dir = Some(config_dir.to_path_buf());
            Launch::prepare(
                PathBuf::from("/usr/bin/claude"),
                dir.clone(),
                wiring,
                SessionTokens::default().issue(admin.clone()),
            )
            .unwrap()
            .args
            .iter()
            .any(|a| a == "--continue")
        };
        assert!(!continues(&config));

        let key: String = dir
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        assert!(key.ends_with("-my-store-v2"), "{key}");
        let saved = config.join("projects").join(&key);
        std::fs::create_dir_all(&saved).unwrap();
        assert!(!continues(&config), "an empty folder is no conversation");
        std::fs::write(saved.join("4f1c.jsonl"), "{}\n").unwrap();
        assert!(continues(&config));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn without_a_loopback_route_the_session_says_why_and_has_no_mcp() {
        let launch = Launch::prepare(
            PathBuf::from("/usr/bin/claude"),
            PathBuf::from("/store"),
            wiring(Err("this server listens on 10.0.0.5 only".to_owned())),
            SessionTokens::default().issue(admin().await),
        )
        .unwrap();
        assert!(!launch.args.iter().any(|a| a == "--mcp-config"));
        assert!(launch.args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(
            launch.notices[0].contains("10.0.0.5"),
            "{:?}",
            launch.notices
        );
    }

    #[tokio::test]
    async fn no_catalog_is_a_reason_not_a_panic() {
        let reason = terminal_dir(None, "app-source", None)
            .await
            .expect_err("no catalog");
        assert!(reason.contains("no catalog"), "{reason}");
    }
}

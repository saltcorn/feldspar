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
//! ## What is ours and what is not
//!
//! The pseudo-terminal itself — opening one, making it the child's controlling
//! terminal in a new session, resizing it — is `pty-process`'s. What is this
//! module's is what that crate does not know about: which directory, which
//! environment, and ending everything the program started when the socket
//! closes ([`pty::Session::kill`]). Off Unix the route refuses with a reason.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use futures::{SinkExt, StreamExt};
use sc_catalog::Catalog;
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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

/// The environment variables the program inherits from the server.
///
/// An allow-list, not the server's environment: that holds `DATABASE_URL` and
/// whatever else the operator started the server with, and none of it is the
/// terminal's business. An admin with a shell can still go looking, but the
/// server does not hand it over unasked.
const INHERITED_ENV: [&str; 9] = [
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TZ", "TMPDIR",
];

/// The bound on concurrent terminals, as the router holds it.
pub(crate) type TerminalSlots = Arc<Semaphore>;

/// A fresh bound, one per built router.
pub(crate) fn terminal_slots() -> TerminalSlots {
    Arc::new(Semaphore::new(MAX_TERMINALS))
}

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

/// Why this store cannot have a terminal, or the directory to start one in.
///
/// The whole policy, apart from the executable and the bound: split out for the
/// reason [`crate::lsp::language_server_root`] is.
pub(crate) async fn terminal_dir(
    catalog: Option<&Arc<Catalog>>,
    store: &str,
    app: Option<&str>,
) -> Result<PathBuf, String> {
    let Some(catalog) = catalog else {
        return Err("this server has no catalog, so it has no file stores".to_owned());
    };
    match sc_config::ide_terminal_enabled(catalog).await {
        Ok(true) => {}
        Ok(false) => {
            return Err("the IDE terminal is turned off (Settings → Development)".to_owned());
        }
        Err(e) => return Err(format!("could not read the settings: {e}")),
    }
    let handle = catalog
        .file_store(store)
        .map_err(|e| format!("file store {store}: {e}"))?
        .ok_or_else(|| format!("the file store {store} is not connected"))?;
    let dir = match app.map(str::trim).filter(|a| !a.is_empty()) {
        None => String::new(),
        Some(app) => app_source_dir(catalog, store, app).await?,
    };
    let root = handle
        .local_path(&dir)
        .map_err(|e| format!("file store {store}: {e}"))?
        .ok_or_else(|| format!("{store} is not on this server's disk, so it has no terminal"))?;
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

/// The directory, inside `store`, that application `app`'s source is in.
///
/// The application must be built from *this* store: the IDE has this store open,
/// and a terminal started in a directory the file tree does not show would edit
/// files the admin cannot see.
async fn app_source_dir(catalog: &Catalog, store: &str, app: &str) -> Result<String, String> {
    let apps = sc_app::list_applications(catalog)
        .await
        .map_err(|e| format!("could not read the applications: {e}"))?;
    let found = apps
        .iter()
        .find(|a| a.name == app)
        .ok_or_else(|| format!("there is no application named {app}"))?;
    let (source_store, dir) =
        sc_app::app_source_location(found).map_err(|_| format!("{app} has no source directory"))?;
    if source_store != store {
        return Err(format!("{app}'s source is in {source_store}, not {store}"));
    }
    Ok(dir)
}

/// The executable `program` names, if it is installed.
///
/// A bare name is looked up on `PATH` here rather than by the spawn, so that
/// "not installed" is a sentence before the socket opens rather than an
/// `ENOENT` after it, and so the child is started by absolute path.
pub(crate) fn locate_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return is_executable(program).then(|| program.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
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

/// Serve one terminal socket for `store`.
///
/// The caller has already established that the request is an admin's.
pub(crate) async fn terminal_upgrade(
    ws: WebSocketUpgrade,
    catalog: Option<&Arc<Catalog>>,
    slots: &TerminalSlots,
    program: Arc<PathBuf>,
    store: String,
    query: TerminalQuery,
) -> axum::response::Response {
    let refusal = match terminal_dir(catalog, &store, query.app.as_deref()).await {
        Err(reason) => reason,
        Ok(dir) => match locate_program(&program) {
            None => format!(
                "{} is not installed on this server",
                program.file_name().map_or_else(
                    || program.display().to_string(),
                    |name| name.to_string_lossy().into_owned()
                )
            ),
            Some(exe) => match Arc::clone(slots).try_acquire_owned() {
                Ok(permit) => {
                    return ws.on_upgrade(move |socket| bridge(socket, exe, dir, store, permit));
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

/// Run `program` in `dir` on a pseudo-terminal, bridged to `socket` until either
/// end stops. `permit` is held for as long as the process lives.
#[cfg(unix)]
async fn bridge(
    socket: WebSocket,
    program: PathBuf,
    dir: PathBuf,
    store: String,
    permit: OwnedSemaphorePermit,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut session = match pty::spawn(&program, &dir, &program_env(), DEFAULT_SIZE) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("feldspar: terminal for {store}: {e}");
            refuse(
                socket,
                format!("could not start {}: {e}", program.display()),
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
            let read = tokio::select! {
                biased;
                _ = paused.wait_for(|on| *on) => continue,
                read = from_pty.read(&mut buf) => read,
            };
            match read {
                Ok(0) | Err(_) => return true,
                Ok(n) => {
                    if sender
                        .send(Message::Binary(bytes::Bytes::copy_from_slice(&buf[..n])))
                        .await
                        .is_err()
                    {
                        return false;
                    }
                }
            }
        }
    };

    let program_ended = tokio::select! {
        () = to_program => false,
        ended = to_browser => ended,
    };

    if program_ended {
        let status = tokio::time::timeout(EXIT_STATUS_WAIT, session.child.wait()).await;
        let reason = match status {
            Ok(Ok(status)) => match status.code() {
                Some(code) => format!("the program exited with status {code}"),
                None => "the program was stopped by a signal".to_owned(),
            },
            _ => "the program closed its terminal".to_owned(),
        };
        let _ = sender
            .send(Message::Close(Some(CloseFrame {
                code: close_code::NORMAL,
                reason: fit_close_reason(reason).into(),
            })))
            .await;
    }

    // One session per connection, and it goes when the connection does —
    // including whatever the program started.
    session.kill().await;
    drop(permit);
}

/// No pseudo-terminal off Unix.
#[cfg(not(unix))]
async fn bridge(
    socket: WebSocket,
    _program: PathBuf,
    _dir: PathBuf,
    _store: String,
    _permit: OwnedSemaphorePermit,
) {
    refuse(socket, "the IDE terminal needs a Unix server".to_owned()).await;
}

#[cfg(unix)]
mod pty {
    //! A program on a pseudo-terminal, and how it is ended.

    use std::path::Path;

    use tokio::process::Child;

    use super::TerminalSize;

    /// A running program and the terminal it is attached to.
    pub(super) struct Session {
        pub(super) pty: pty_process::Pty,
        pub(super) child: Child,
    }

    /// Start `program` in `dir` with exactly `env`, on a new terminal of `size`.
    ///
    /// The program leads a new session with the terminal as its controlling one,
    /// which is what turns Ctrl-C into `SIGINT` and lets [`Session::kill`] find
    /// what it started.
    pub(super) fn spawn(
        program: &Path,
        dir: &Path,
        env: &[(String, String)],
        size: TerminalSize,
    ) -> pty_process::Result<Session> {
        let (pty, pts) = pty_process::open()?;
        pty.resize(pty_process::Size::new(size.rows, size.cols))?;
        // `spawn` takes the terminal's other end and drops the parent's copies,
        // so reading `pty` ends once the child and its descendants let go.
        let child = pty_process::Command::new(program)
            .current_dir(dir)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .kill_on_drop(true)
            .spawn(pts)?;
        Ok(Session { pty, child })
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
        pub(super) async fn kill(mut self) {
            let Some(pid) = self.child.id().and_then(|p| libc::pid_t::try_from(p).ok()) else {
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

    #[test]
    fn a_bare_name_is_found_on_path_and_a_missing_one_is_not() {
        assert!(locate_program(Path::new("sh")).is_some());
        assert!(locate_program(Path::new("definitely-not-installed-feldspar-cli")).is_none());
        assert!(locate_program(Path::new("/nonexistent/claude")).is_none());
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

    #[tokio::test]
    async fn no_catalog_is_a_reason_not_a_panic() {
        let reason = terminal_dir(None, "app-source", None)
            .await
            .expect_err("no catalog");
        assert!(reason.contains("no catalog"), "{reason}");
    }
}

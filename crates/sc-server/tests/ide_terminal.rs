//! The IDE terminal route (design §12.1, "Claude Code in a terminal tab";
//! TODO-claude-terminal CT1): who may open it, when it refuses, and what
//! crosses it.
//!
//! As `ide_language_server.rs`, a real listener and a real WebSocket client,
//! because the subject is what happens after the upgrade. `/bin/sh` stands in
//! for Claude Code: the bridge does not care which program is on the terminal,
//! and a shell can be asked about the terminal it is on (`stty size`, `pwd`,
//! `$$`).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderValue, StatusCode, header};
use futures::{SinkExt, StreamExt};
use sc_app::{Application, FrameworkRef};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, SessionStore, User};
use sc_catalog::Catalog;
use sc_config::{IDE_TERMINAL_ENABLED, set_config};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::{FileStore, LocalFileStore};
use sc_server::{
    AppMounts, MAX_TERMINALS, SESSION_COOKIE, ServerConfig, TERMINAL_ROUTE, build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use uuid::Uuid;

/// How long a test waits for output that should arrive promptly.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A fresh, unique temp directory, created on disk.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sc-terminal-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A running server with the store `code` connected at `root`.
struct Server {
    addr: std::net::SocketAddr,
    admin: String,
    public: String,
    catalog: Arc<Catalog>,
    root: PathBuf,
    _db: TestDb,
}

impl Server {
    async fn connect(
        &self,
        path_and_query: &str,
        session: Option<&str>,
    ) -> std::result::Result<Socket, WsError> {
        let mut request = format!("ws://{}{path_and_query}", self.addr)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{}", self.addr)).unwrap(),
        );
        if let Some(token) = session {
            request.headers_mut().insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={token}")).unwrap(),
            );
        }
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(stream, _)| stream)
    }

    /// An admin's terminal on `code`, which must open.
    async fn open(&self, query: &str) -> Socket {
        self.connect(&format!("/ide/terminal/code{query}"), Some(&self.admin))
            .await
            .expect("the handshake succeeds")
    }
}

/// A server running `program` as its Claude Code, with the terminal turned on
/// unless `enabled` is false.
async fn serve(program: &Path, enabled: bool) -> Result<Server> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    let root = temp_dir("store");
    catalog.connect_file_store(
        Arc::new(LocalFileStore::new("code", root.clone())?) as Arc<dyn FileStore>
    )?;
    if enabled {
        set_config(&catalog, IDE_TERMINAL_ENABLED, json!(true)).await?;
    }
    let sessions = Arc::new(SessionStore::default());
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        sc_server::HandlerRegistry::new(),
        sessions.clone(),
        &ServerConfig {
            claude_cli: program.to_path_buf(),
            ..ServerConfig::default()
        },
        Arc::new(AppMounts::new(Arc::clone(&catalog))),
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(Server {
        addr,
        admin: sessions
            .login(User::new(Uuid::new_v4(), ROLE_ADMIN)?)
            .await?,
        public: sessions
            .login(User::new(Uuid::new_v4(), ROLE_PUBLIC)?)
            .await?,
        catalog,
        root,
        _db: db,
    })
}

fn sh() -> PathBuf {
    PathBuf::from("/bin/sh")
}

/// Type a line into the terminal.
async fn type_line(socket: &mut Socket, line: &str) {
    socket
        .send(Message::binary(format!("{line}\n").into_bytes()))
        .await
        .expect("send");
}

/// Read terminal output until it contains `needle`; everything read so far.
async fn read_until(socket: &mut Socket, needle: &str) -> String {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;
    while !seen.contains(needle) {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; saw {seen:?}"))
            .expect("the socket ended")
            .expect("a socket error");
        match message {
            Message::Binary(bytes) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            Message::Close(frame) => panic!("closed while waiting for {needle:?}: {frame:?}"),
            _ => {}
        }
    }
    seen
}

/// The close frame's reason, skipping any output before it.
async fn close_reason(socket: &mut Socket) -> String {
    loop {
        let message = tokio::time::timeout(REPLY_TIMEOUT, socket.next())
            .await
            .expect("timed out waiting for the close frame")
            .expect("the socket ended without a close frame")
            .expect("a socket error");
        if let Message::Close(frame) = message {
            return frame.map(|f| f.reason.to_string()).unwrap_or_default();
        }
    }
}

/// Whether process `pid` still exists.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait up to a few seconds for `pid` to be gone.
async fn gone(pid: i32) -> bool {
    for _ in 0..50 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// The URL the IDE will build and the route this crate mounts are one contract.
#[test]
fn the_route_is_under_the_ide() {
    assert_eq!(TERMINAL_ROUTE, "/ide/terminal/{store}");
}

/// Only an admin, and the refusal is an HTTP status before the upgrade.
#[tokio::test]
async fn only_an_admin_may_open_a_terminal() -> Result<()> {
    let server = serve(&sh(), true).await?;
    let anonymous = server.connect("/ide/terminal/code", None).await;
    assert!(
        matches!(&anonymous, Err(WsError::Http(r)) if r.status() == StatusCode::UNAUTHORIZED),
        "anonymous: {:?}",
        anonymous.map(|_| "a socket")
    );
    let public = server
        .connect("/ide/terminal/code", Some(&server.public))
        .await;
    assert!(
        matches!(&public, Err(WsError::Http(r)) if r.status() == StatusCode::FORBIDDEN),
        "non-admin: {:?}",
        public.map(|_| "a socket")
    );
    Ok(())
}

/// Every other refusal is a close frame with a sentence in it.
#[tokio::test]
async fn each_refusal_says_why() -> Result<()> {
    let off = serve(&sh(), false).await?;
    let mut socket = off.open("").await;
    assert!(close_reason(&mut socket).await.contains("turned off"));

    let server = serve(&sh(), true).await?;
    let mut socket = server
        .connect("/ide/terminal/elsewhere", Some(&server.admin))
        .await
        .unwrap();
    assert!(close_reason(&mut socket).await.contains("not connected"));

    let mut socket = server.open("?app=nope").await;
    assert!(
        close_reason(&mut socket)
            .await
            .contains("no application named nope")
    );

    // An application built from a different store is not this store's.
    server.catalog.connect_file_store(
        Arc::new(LocalFileStore::new("other", temp_dir("other"))?) as Arc<dyn FileStore>,
    )?;
    let framework = FrameworkRef::new("none")
        .with("store", "other")
        .with("source", "web");
    sc_app::save_application(
        &server.catalog,
        &Application::new("site", "site", framework),
    )
    .await?;
    let mut socket = server.open("?app=site").await;
    assert!(close_reason(&mut socket).await.contains("is in other"));

    let missing = serve(Path::new("/nonexistent/claude"), true).await?;
    let mut socket = missing.open("").await;
    assert!(
        close_reason(&mut socket)
            .await
            .contains("claude is not installed")
    );
    Ok(())
}

/// Keystrokes reach the program and its output comes back; a resize reaches
/// the terminal; the program starts at the store's root.
#[tokio::test]
async fn bytes_and_resizes_cross_the_bridge() -> Result<()> {
    let server = serve(&sh(), true).await?;
    let mut socket = server.open("").await;

    // Computed by the shell, so seeing it proves a round trip rather than echo.
    type_line(&mut socket, "echo sum-$((40+2))").await;
    read_until(&mut socket, "sum-42").await;

    socket
        .send(Message::text(
            json!({ "resize": { "cols": 100, "rows": 33 } }).to_string(),
        ))
        .await
        .unwrap();
    type_line(&mut socket, "stty size").await;
    read_until(&mut socket, "33 100").await;

    type_line(&mut socket, "pwd").await;
    let root = server.root.canonicalize()?;
    read_until(&mut socket, &root.display().to_string()).await;
    Ok(())
}

/// While the browser has paused the terminal nothing reaches it; on resuming,
/// what the program wrote meanwhile does.
#[tokio::test]
async fn a_paused_terminal_holds_its_output_until_resumed() -> Result<()> {
    let server = serve(&sh(), true).await?;
    let mut socket = server.open("").await;
    type_line(&mut socket, "echo ready-$((1+1))").await;
    read_until(&mut socket, "ready-2").await;

    let pause = |on: bool| Message::text(json!({ "pause": on }).to_string());
    socket.send(pause(true)).await.unwrap();
    type_line(&mut socket, "echo held-$((40+2))").await;
    let mut while_paused = String::new();
    let quiet_until = tokio::time::Instant::now() + Duration::from_millis(600);
    while let Ok(Some(Ok(message))) = tokio::time::timeout_at(quiet_until, socket.next()).await {
        if let Message::Binary(bytes) = message {
            while_paused.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    assert!(
        !while_paused.contains("held-42"),
        "output arrived while paused: {while_paused:?}"
    );

    socket.send(pause(false)).await.unwrap();
    read_until(&mut socket, "held-42").await;
    Ok(())
}

/// With `?app=`, the program starts in that application's source directory.
#[tokio::test]
async fn an_application_terminal_starts_in_its_source_directory() -> Result<()> {
    let server = serve(&sh(), true).await?;
    std::fs::create_dir_all(server.root.join("web"))?;
    let framework = FrameworkRef::new("none")
        .with("store", "code")
        .with("source", "web");
    sc_app::save_application(
        &server.catalog,
        &Application::new("site", "site", framework),
    )
    .await?;

    let mut socket = server.open("?app=site").await;
    type_line(&mut socket, "pwd").await;
    let web = server.root.join("web").canonicalize()?;
    read_until(&mut socket, &web.display().to_string()).await;
    Ok(())
}

/// The program exiting closes the socket normally, with its status.
#[tokio::test]
async fn an_exit_closes_the_socket_with_the_status() -> Result<()> {
    let server = serve(&sh(), true).await?;
    let mut socket = server.open("").await;
    type_line(&mut socket, "exit 7").await;
    let reason = close_reason(&mut socket).await;
    assert!(reason.contains("status 7"), "{reason}");
    Ok(())
}

/// The children of `parent`, read from `/proc` (Linux only).
fn children_of(parent: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let ppid: i32 = stat
                .get(stat.rfind(')')? + 1..)?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            (ppid == parent).then_some(pid)
        })
        .collect()
}

/// Closing the socket ends the program **and what it started**, including the
/// two kinds of child its process group does not contain: a background job of
/// a shell with job control (a group of its own) and a child that left the
/// session with `setsid` (here `setsid -w`, which keeps its parent alive as a
/// program like Claude Code would).
#[tokio::test]
async fn closing_the_socket_ends_the_program_and_its_children() -> Result<()> {
    let server = serve(&sh(), true).await?;
    let mut socket = server.open("").await;
    // The terminal echoes what is typed, so the marker the test waits for is
    // spelled in octal on the way in (`\074`, `\076`) and only appears as `<`
    // and `>` in what the shell prints.
    type_line(&mut socket, r"sleep 1000 & printf '\074%s-%s\076\n' $$ $!").await;
    let output = read_until(&mut socket, ">").await;
    let pids = output
        .rsplit('<')
        .next()
        .and_then(|rest| rest.split('>').next())
        .expect("the shell printed its pids");
    let (shell, job) = pids.split_once('-').expect("two pids");
    let (shell, job): (i32, i32) = (shell.parse().unwrap(), job.parse().unwrap());

    type_line(
        &mut socket,
        r"setsid -w sleep 1001 & printf '\074%s\076\n' $!",
    )
    .await;
    let output = read_until(&mut socket, ">\r").await;
    let waiter: i32 = output
        .rsplit('<')
        .next()
        .and_then(|rest| rest.split('>').next())
        .and_then(|pid| pid.parse().ok())
        .expect("the shell printed the setsid pid");
    // `setsid` forks when it already leads a group, as a job does; the sleep
    // in a session of its own is that fork's child.
    let mut detached = Vec::new();
    for _ in 0..50 {
        detached = children_of(waiter);
        if !detached.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!detached.is_empty(), "setsid started no child");
    assert!(alive(shell) && alive(job) && alive(waiter));

    socket.close(None).await.unwrap();
    drop(socket);
    assert!(gone(shell).await, "the shell outlived its socket");
    assert!(gone(job).await, "a background job outlived the socket");
    assert!(gone(waiter).await, "setsid outlived the socket");
    for pid in detached {
        assert!(
            gone(pid).await,
            "a child in its own session outlived the socket: {:?}",
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
        );
    }
    Ok(())
}

/// The bound holds, and refuses with a reason.
#[tokio::test]
async fn the_number_of_terminals_is_bounded() -> Result<()> {
    let server = serve(&sh(), true).await?;
    let mut open = Vec::new();
    for n in 0..MAX_TERMINALS {
        let mut socket = server.open("").await;
        // Make sure each one really started before opening the next.
        type_line(&mut socket, &format!("echo up-{n}-ok")).await;
        read_until(&mut socket, &format!("up-{n}-ok")).await;
        open.push(socket);
    }
    let mut refused = server.open("").await;
    assert!(close_reason(&mut refused).await.contains("maximum"));

    // Closing one frees its slot.
    let mut first = open.remove(0);
    first.close(None).await.unwrap();
    drop(first);
    let mut again = None;
    for _ in 0..50 {
        let mut socket = server.open("").await;
        type_line(&mut socket, "echo again-ok").await;
        match tokio::time::timeout(Duration::from_secs(2), socket.next()).await {
            Ok(Some(Ok(Message::Close(_)))) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ => {
                again = Some(socket);
                break;
            }
        }
    }
    let mut again = again.expect("a slot came free");
    read_until(&mut again, "again-ok").await;
    Ok(())
}

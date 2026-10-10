//! The file-store IDE's language-server route (design §12.1, phase 4): who may
//! open it, which stores can have one, and what actually crosses it.
//!
//! Unlike every other route in this crate, this one cannot be tested with
//! `tower::ServiceExt::oneshot`: the whole subject is what happens *after* the
//! `101 Switching Protocols`, so these tests bind a real listener and connect a
//! real WebSocket client to it.
//!
//! Four properties, in the order they matter:
//!
//! 1. **Only an admin may open it.** The socket hands its holder a process on the
//!    server; anonymous and non-admin requests are refused before the upgrade.
//! 2. **A store with no local path is told why.** That refusal arrives as a close
//!    frame rather than an HTTP status, because a browser cannot read the body of
//!    a failed WebSocket handshake and a reason it cannot read is not a reason.
//! 3. **The bridge translates both framings and both URI spaces.** The workbench
//!    edits `/<store>`; the process sees a directory on disk. A stub language
//!    server in the fixture proves each direction independently.
//! 4. **A real `typescript-language-server` reports a real type error**, at the
//!    file and line it is on — the thing the whole phase exists for. Skipped when
//!    the tool is not installed, exactly as `ide_typecheck.rs` skips without a
//!    `tsc`.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, SessionStore, User};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_files::{Entry, FileMeta, FileStore, LocalFileStore};
use sc_server::{AppMounts, LSP_ROUTE, SESSION_COOKIE, ServerConfig, build_router_with_apps};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use uuid::Uuid;

/// How long a test waits for a message that should arrive promptly. Generous
/// enough for a cold tsserver on a loaded machine, short enough to fail rather
/// than hang CI.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

// --- a store with no local path ---------------------------------------------

/// An object-store-backed store, as far as this route is concerned: it holds
/// files, it just does not hold them anywhere a process can be pointed at.
///
/// Nothing here is called — the route asks one question,
/// [`FileStore::local_path`], and stops on the answer — so the rest of the
/// contract is implemented as the refusal it would be.
struct ObjectStore {
    name: String,
}

fn no_bytes<T>(what: &str) -> Result<T> {
    Err(Error::msg(format!(
        "the test object store does not implement {what}"
    )))
}

#[async_trait]
impl FileStore for ObjectStore {
    fn name(&self) -> &str {
        &self.name
    }
    async fn read(&self, _path: &str) -> Result<Bytes> {
        no_bytes("read")
    }
    async fn write(&self, _path: &str, _data: Bytes) -> Result<()> {
        no_bytes("write")
    }
    async fn list(&self, _dir: &str) -> Result<Vec<Entry>> {
        Ok(Vec::new())
    }
    async fn mkdir(&self, _path: &str) -> Result<()> {
        no_bytes("mkdir")
    }
    async fn delete(&self, _path: &str) -> Result<bool> {
        no_bytes("delete")
    }
    async fn rename(&self, _from: &str, _to: &str) -> Result<()> {
        no_bytes("rename")
    }
    fn is_git_repo(&self) -> bool {
        false
    }
    async fn get_meta(&self, _path: &str) -> Result<FileMeta> {
        Ok(FileMeta::default())
    }
    async fn set_meta(&self, _path: &str, _meta: &FileMeta) -> Result<()> {
        no_bytes("set_meta")
    }
}

// --- fixtures ----------------------------------------------------------------

/// A fresh, unique temp directory, created on disk.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sc-lsp-route-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A stand-in for `typescript-language-server`, installed where the real one
/// would be so that no test hook is needed to reach it: the bridge prefers the
/// project's own copy (as a build prefers the project's own bundler), and in this
/// fixture the project's own copy is this.
///
/// It speaks just enough LSP to answer the two questions the bridge is asked:
/// what did the process *receive* (echoed back in the `initialize` result), and
/// what does the browser *see* of what the process sends (a diagnostic addressed
/// with the process's own absolute path).
///
/// The echo is deliberately **not** a URI — it is a URI with a word in front of
/// it — because a URI would be translated back on its way out and the test would
/// assert nothing: it would watch a round trip and call it a delivery.
const STUB_SERVER: &str = r#"#!/usr/bin/env node
let buffer = Buffer.alloc(0);

function send(message) {
  const body = Buffer.from(JSON.stringify(message), "utf8");
  process.stdout.write(`Content-Length: ${body.length}\r\n\r\n`);
  process.stdout.write(body);
}

function handle(message) {
  if (message.method !== "initialize") return;
  send({
    jsonrpc: "2.0",
    id: message.id,
    result: {
      capabilities: {},
      seenRootUri: `saw ${message.params.rootUri}`,
      seenRootPath: `saw ${message.params.rootPath}`,
      cwd: process.cwd(),
    },
  });
  send({
    jsonrpc: "2.0",
    method: "textDocument/publishDiagnostics",
    params: {
      uri: `${message.params.rootUri}/src/App.ts`,
      diagnostics: [{
        range: { start: { line: 3, character: 6 }, end: { line: 3, character: 7 } },
        severity: 1,
        message: "stub diagnostic",
      }],
    },
  });
}

process.stdin.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    const end = buffer.indexOf("\r\n\r\n");
    if (end === -1) return;
    const match = /Content-Length: (\d+)/i.exec(buffer.subarray(0, end).toString());
    if (match === null) return;
    const start = end + 4;
    const length = Number(match[1]);
    if (buffer.length < start + length) return;
    const body = buffer.subarray(start, start + length).toString();
    buffer = buffer.subarray(start + length);
    handle(JSON.parse(body));
  }
});
"#;

/// Install an executable at `<root>/node_modules/.bin/typescript-language-server`.
fn install_server_bin(root: &Path, script: &str) {
    let bin = root.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("typescript-language-server");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// --- the server under test ---------------------------------------------------

/// A running server, its address, and the session tokens the tests connect with.
struct Server {
    addr: std::net::SocketAddr,
    admin: String,
    public: String,
    _db: Option<TestDb>,
}

impl Server {
    /// Open the language-server socket for `store` as the given session.
    async fn connect(
        &self,
        store: &str,
        session: Option<&str>,
    ) -> std::result::Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        WsError,
    > {
        let mut request = format!("ws://{}/ide/lsp/{store}", self.addr)
            .into_client_request()
            .unwrap();
        // A browser sends its page's origin on every handshake, and the server
        // accepts an upgrade only from its own (TODO.md "Live updates" §3).
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
}

/// Bind a router on an ephemeral port and serve it for the rest of the test.
async fn serve(router: Router, sessions: &SessionStore, db: Option<TestDb>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Server {
        addr,
        admin: sessions
            .login(User::new(Uuid::new_v4(), ROLE_ADMIN).unwrap())
            .await
            .unwrap(),
        public: sessions
            .login(User::new(Uuid::new_v4(), ROLE_PUBLIC).unwrap())
            .await
            .unwrap(),
        _db: db,
    }
}

/// A server whose catalog has `stores` connected, over a real database.
async fn serve_with_stores(stores: Vec<Arc<dyn FileStore>>) -> Result<Server> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    for store in stores {
        catalog.connect_file_store(store)?;
    }
    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        sc_server::HandlerRegistry::new(),
        sessions.clone(),
        &ServerConfig::default(),
        apps,
    )?;
    Ok(serve(router, &sessions, Some(db)).await)
}

/// The next text message on the socket, or a failed test.
async fn next_message(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    loop {
        let message = tokio::time::timeout(REPLY_TIMEOUT, socket.next())
            .await
            .expect("timed out waiting for a message")
            .expect("the socket closed")
            .expect("a socket error");
        match message {
            Message::Text(text) => {
                return serde_json::from_str(&text).expect("the server sent non-JSON");
            }
            Message::Close(frame) => panic!("the server closed the socket: {frame:?}"),
            _ => {}
        }
    }
}

/// The reply to request `id`, skipping the notifications a language server sends
/// while it is starting up (logs, progress, an empty first diagnostic pass).
async fn reply_to(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: i64,
) -> Value {
    loop {
        let message = next_message(socket).await;
        if message["id"] == json!(id) && message.get("method").is_none() {
            return message;
        }
    }
}

/// Send one JSON-RPC message as the IDE's client does: bare JSON, no framing.
async fn send(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    message: Value,
) {
    socket
        .send(Message::text(message.to_string()))
        .await
        .expect("send");
}

// --- 0. the route the IDE opens ----------------------------------------------

/// The URL `ui/ide` builds (`languageClient.ts`) and the route this crate mounts
/// are one contract with two spellings, and nothing else in the build would
/// notice them diverging: a page that connects to the wrong path gets a 404 and
/// no semantics, which looks exactly like a store that cannot have any.
#[test]
fn the_route_is_the_path_the_ide_connects_to() {
    assert_eq!(LSP_ROUTE, "/ide/lsp/{store}");
    let ide = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/ide/src/languageServer.ts");
    let source = std::fs::read_to_string(&ide).expect("read languageServer.ts");
    assert!(
        source.contains("/ide/lsp/"),
        "{} must connect to {LSP_ROUTE}",
        ide.display()
    );
}

// --- 1. who may open it ------------------------------------------------------

/// The route is admin-only, and says so *before* the upgrade — the one refusal an
/// HTTP status is the right answer to.
#[tokio::test]
async fn only_an_admin_may_open_the_language_server_socket() {
    let sessions = Arc::new(SessionStore::default());
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        sc_server::HandlerRegistry::new(),
        sessions.clone(),
        &ServerConfig::default(),
        Arc::new(AppMounts::none()),
    )
    .expect("build router");
    let server = serve(router, &sessions, None).await;

    let anonymous = server.connect("app-source", None).await;
    assert!(
        matches!(&anonymous, Err(WsError::Http(response)) if response.status() == StatusCode::UNAUTHORIZED),
        "an anonymous socket must be refused, got {:?}",
        anonymous.map(|_| "a socket")
    );

    let public = server.connect("app-source", Some(&server.public)).await;
    assert!(
        matches!(&public, Err(WsError::Http(response)) if response.status() == StatusCode::FORBIDDEN),
        "a non-admin socket must be refused, got {:?}",
        public.map(|_| "a socket")
    );
}

// --- 2. which stores can have one -------------------------------------------

/// A store with no local path — an object store — cannot host a language server,
/// and the admin is told that instead of being left with an editor that silently
/// has no semantics.
#[tokio::test]
async fn a_store_with_no_local_path_is_closed_with_the_reason() -> Result<()> {
    let object = Arc::new(ObjectStore {
        name: "assets".to_owned(),
    }) as Arc<dyn FileStore>;
    let server = serve_with_stores(vec![object]).await?;

    let mut socket = server
        .connect("assets", Some(&server.admin))
        .await
        .expect("the handshake itself succeeds: the refusal is on the socket");

    let message = tokio::time::timeout(REPLY_TIMEOUT, socket.next())
        .await
        .expect("timed out")
        .expect("the socket ended without a close frame")
        .expect("a socket error");
    let Message::Close(Some(frame)) = message else {
        panic!("expected a close frame, got {message:?}");
    };
    assert!(
        frame.reason.contains("no local path"),
        "the reason must say why: {:?}",
        frame.reason
    );

    // And a store that is not connected at all is a different, equally spelled-out
    // reason — not a hang and not a stack trace.
    let mut socket = server
        .connect("nowhere", Some(&server.admin))
        .await
        .expect("handshake");
    let message = tokio::time::timeout(REPLY_TIMEOUT, socket.next())
        .await
        .expect("timed out")
        .expect("the socket ended without a close frame")
        .expect("a socket error");
    let Message::Close(Some(frame)) = message else {
        panic!("expected a close frame, got {message:?}");
    };
    assert!(frame.reason.contains("not connected"), "{:?}", frame.reason);
    Ok(())
}

// --- 3. what crosses the bridge ----------------------------------------------

/// The handshake reaches a real process, and both URI spaces are translated on
/// the way.
///
/// The stub echoes what it received, so the first half of this asserts what the
/// *server* saw: the store's real directory, not the workbench's `/<store>`. The
/// diagnostic it then sends is addressed with that same real path, so the second
/// half asserts what the *browser* sees: `/<store>` again. Neither end ever hears
/// of the other's URIs, which is the property the bridge exists to keep.
#[tokio::test]
async fn the_bridge_carries_a_handshake_and_translates_both_uri_spaces() -> Result<()> {
    let root = temp_dir("bridge");
    std::fs::write(root.join("package.json"), r#"{"name":"fixture"}"#).unwrap();
    install_server_bin(&root, STUB_SERVER);

    let store = Arc::new(LocalFileStore::new("app-source", root.clone())?) as Arc<dyn FileStore>;
    let server = serve_with_stores(vec![store]).await?;
    let mut socket = server
        .connect("app-source", Some(&server.admin))
        .await
        .expect("handshake");

    send(
        &mut socket,
        json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "processId": null,
                "rootPath": "/app-source",
                "rootUri": "file:///app-source",
                "workspaceFolders": [{ "uri": "file:///app-source", "name": "app-source" }],
                "capabilities": {}
            }
        }),
    )
    .await;

    let reply = reply_to(&mut socket, 0).await;
    let on_disk = root.display().to_string();
    assert_eq!(
        reply["result"]["seenRootUri"],
        json!(format!("saw file://{on_disk}")),
        "the language server must be told where the store really is"
    );
    assert_eq!(
        reply["result"]["seenRootPath"],
        json!(format!("saw {on_disk}")),
        "including through `initialize`'s deprecated plain-path field"
    );
    assert_eq!(
        reply["result"]["cwd"],
        json!(on_disk),
        "and it must be running in the store's directory"
    );

    let published = next_message(&mut socket).await;
    assert_eq!(
        published["method"],
        json!("textDocument/publishDiagnostics")
    );
    assert_eq!(
        published["params"]["uri"],
        json!("file:///app-source/src/App.ts"),
        "the workbench only ever sees its own workspace folder"
    );
    assert_eq!(
        published["params"]["diagnostics"][0]["message"],
        json!("stub diagnostic")
    );

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

// --- 4. a real type error ----------------------------------------------------

/// The `typescript-language-server` this checkout installed, if it did.
///
/// Like `ide_typecheck.rs`, the test that needs it **skips** rather than fails
/// when the Node toolchain has not been installed under `ui/ide`, so a Rust-only
/// checkout stays green.
fn installed_language_server() -> Option<PathBuf> {
    let modules = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/ide/node_modules");
    modules
        .join(".bin/typescript-language-server")
        .exists()
        .then(|| modules.canonicalize().ok())
        .flatten()
}

/// Give the fixture the two packages a language server needs, and **only** those.
///
/// Symlinking `ui/ide/node_modules` whole would be one line, and it is the wrong
/// one: tsserver indexes a project's dependencies for auto-imports, and that
/// directory is the entire VS Code workbench — hundreds of packages whose index
/// costs a gigabyte and enough memory pressure to take the *other* test binaries
/// down with it (this workspace links a static V8 into most of them). What the
/// server actually needs is a `typescript` to run and itself to be run.
fn borrow_language_server(modules: &Path, root: &Path) {
    let fixture = root.join("node_modules");
    std::fs::create_dir_all(fixture.join(".bin")).unwrap();
    for package in ["typescript", "typescript-language-server"] {
        std::os::unix::fs::symlink(modules.join(package), fixture.join(package)).unwrap();
    }
    std::os::unix::fs::symlink(
        modules.join(".bin/typescript-language-server"),
        fixture.join(".bin/typescript-language-server"),
    )
    .unwrap();
}

/// The point of the whole phase: a deliberate type error comes back as a
/// diagnostic, on the right file and the right line.
#[tokio::test]
async fn typescript_reports_a_type_error_at_its_line() -> Result<()> {
    let Some(modules) = installed_language_server() else {
        eprintln!("skipping: no typescript-language-server. Run `npm install` in ui/ide first.");
        return Ok(());
    };

    let root = temp_dir("typecheck");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"fixture","version":"0.0.0"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions":{"strict":true,"target":"es2022","module":"esnext","moduleResolution":"bundler","noEmit":true},"include":["src"]}"#,
    )
    .unwrap();
    // Line 3 (1-based), column 7: the identifier `wrong`.
    let source = "export function f(): number {\n  return 1;\n}\nconst wrong: string = f();\n";
    std::fs::write(root.join("src/App.ts"), source).unwrap();
    // The project's dependencies, borrowed rather than installed.
    borrow_language_server(&modules, &root);

    let store = Arc::new(LocalFileStore::new("app-source", root.clone())?) as Arc<dyn FileStore>;
    let server = serve_with_stores(vec![store]).await?;
    let mut socket = server
        .connect("app-source", Some(&server.admin))
        .await
        .expect("handshake");

    send(
        &mut socket,
        json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "processId": null,
                "rootUri": "file:///app-source",
                "workspaceFolders": [{ "uri": "file:///app-source", "name": "app-source" }],
                "capabilities": {
                    "textDocument": { "publishDiagnostics": { "relatedInformation": false } }
                }
            }
        }),
    )
    .await;
    let reply = reply_to(&mut socket, 0).await;
    assert!(
        reply["result"]["capabilities"]["completionProvider"].is_object(),
        "a real language server offers completions: {reply}"
    );

    send(
        &mut socket,
        json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    )
    .await;
    send(
        &mut socket,
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": "file:///app-source/src/App.ts",
                    "languageId": "typescript",
                    "version": 1,
                    "text": source
                }
            }
        }),
    )
    .await;

    // Diagnostics arrive when tsserver has loaded the project, after any number
    // of other notifications (logs, progress, an empty first pass).
    let diagnostic = loop {
        let message = next_message(&mut socket).await;
        if message["method"] != json!("textDocument/publishDiagnostics") {
            continue;
        }
        assert_eq!(
            message["params"]["uri"],
            json!("file:///app-source/src/App.ts"),
            "addressed in the workbench's own URI space"
        );
        if let Some(first) = message["params"]["diagnostics"].get(0) {
            break first.clone();
        }
    };

    assert_eq!(
        diagnostic["range"]["start"]["line"],
        json!(3),
        "the error is on the fourth line (zero-based 3): {diagnostic}"
    );
    assert!(
        diagnostic["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not assignable"),
        "the type error itself: {diagnostic}"
    );

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

//! The file-store IDE's TypeScript language server (design §12.1, phase 4).
//!
//! Semantics — errors, completions, go-to-definition — come from a **real
//! `typescript-language-server` process on the server**, not from tsserver in a
//! web worker. The reason is where `node_modules` is: the project's dependencies
//! and its `tsconfig.json` are already on the server's disk, and type-checking
//! against them there costs one process, while type-checking against them in the
//! browser would mean dragging the whole type surface across HTTP.
//!
//! That makes this the one IDE capability gated on
//! [`FileStore::local_path`](sc_files::FileStore::local_path), exactly as a
//! framework's build step is (§13.3). A store with no local path gets editing,
//! formatting, grammars and syntax errors — and is *told* it gets no semantics,
//! which is what the close frame below carries.
//!
//! ## The shape of the bridge
//!
//! One WebSocket, one process. The socket carries **bare JSON-RPC messages**
//! (what `vscode-ws-jsonrpc` speaks); the process speaks LSP's `Content-Length`
//! framing over stdio. So each direction is a translation of the framing, plus one
//! translation of *URIs*:
//!
//! The workbench edits the store as the workspace folder `/<store>` (§12.1's
//! filesystem provider), while the language server sees the store's real directory
//! on disk. Both halves are `file://` URIs and neither can be talked out of its
//! own, so the bridge — the one place that knows both — rewrites them as messages
//! pass. Doing it here rather than in the browser keeps the server's directory
//! layout out of the page, and keeps the workspace folder the same `/<store>` for
//! every backend rather than only for the ones with a local path.
//!
//! ## Refusals
//!
//! Everything decidable before the upgrade is decided before it, but only an
//! **authentication** failure is answered with an HTTP status: a browser cannot
//! read the body of a failed WebSocket handshake, so a reason sent that way would
//! be lost. Every other refusal — no such store, no local path, no installed
//! dependencies, too many servers already running — upgrades the socket and then
//! closes it with the reason in the close frame, which the IDE shows to the admin
//! once (`languageClient.ts`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use futures::{SinkExt, StreamExt};
use sc_catalog::Catalog;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The route the IDE opens its language-server socket on. `{store}` is the store
/// the workbench has open, which is also the workspace folder's name.
pub const LSP_ROUTE: &str = "/ide/lsp/{store}";

/// How many language servers may run at once, across every admin and every store.
///
/// A `typescript-language-server` is a Node process with a tsserver inside it and
/// a whole project's type graph in memory: a handful is a normal working day, a
/// hundred is a page that reconnects in a loop or an operator with a problem. The
/// bound turns the second into a refusal with a reason rather than into a machine
/// that stops answering.
pub const MAX_LANGUAGE_SERVERS: usize = 8;

/// The executable, as `npx`-style resolution would find it.
pub(crate) const SERVER_BIN: &str = "typescript-language-server";

/// A close frame's reason is capped by the protocol (125 bytes for the whole
/// payload, two of which are the code), so reasons are written to fit and are
/// truncated rather than dropped if one ever does not.
const MAX_CLOSE_REASON: usize = 120;

/// The bound on concurrent language servers, as the router holds it.
pub(crate) type ServerSlots = Arc<Semaphore>;

/// A fresh bound, one per built router.
pub(crate) fn server_slots() -> ServerSlots {
    Arc::new(Semaphore::new(MAX_LANGUAGE_SERVERS))
}

/// Why this store cannot have a language server, or the directory to run one in.
///
/// Split out from the handler because it is the whole of the policy and none of
/// the plumbing: the tests state it directly, and the handler only has to decide
/// what to do with the answer.
pub(crate) fn language_server_root(
    catalog: Option<&Arc<Catalog>>,
    store: &str,
) -> Result<PathBuf, String> {
    let Some(catalog) = catalog else {
        return Err("this server has no catalog, so it has no file stores".to_owned());
    };
    let handle = catalog
        .file_store(store)
        .map_err(|e| format!("file store {store}: {e}"))?
        .ok_or_else(|| format!("the file store {store} is not connected"))?;
    let root = handle
        .local_path("")
        .map_err(|e| format!("file store {store}: {e}"))?
        .ok_or_else(|| {
            format!("{store} has no local path, so it can be edited but not type-checked")
        })?;
    if !root.is_dir() {
        return Err(format!("{store}'s directory is not there any more"));
    }
    if let Some(project) = uninstalled_project(&root) {
        return Err(format!(
            "no TypeScript semantics: {project} has no node_modules — press Build to install"
        ));
    }
    Ok(root)
}

/// A project in the store whose dependencies have never been installed, if the
/// store holds one.
///
/// Without `node_modules` tsserver resolves no import at all, so every file in the
/// project is a wall of "cannot find module" — thousands of errors that say one
/// thing. Saying that one thing once, and running no server, is the honest answer
/// (the build installs dependencies on its first run, §13.3, so the fix is a
/// button the admin already has).
///
/// A project is a directory holding a `package.json`, looked for at the store root
/// and one level below it — where a scaffolded app's source sits (§13.3) — and a
/// store holding no `package.json` at all is not a node project and is left alone:
/// plain TypeScript files type-check perfectly well without dependencies.
fn uninstalled_project(root: &Path) -> Option<String> {
    let mut projects = Vec::new();
    if root.join("package.json").is_file() {
        projects.push((String::from("this store"), root.to_path_buf()));
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("package.json").is_file() {
                projects.push((entry.file_name().to_string_lossy().into_owned(), path));
            }
        }
    }
    if projects.is_empty() {
        return None;
    }
    // One installed project is enough: the workspace is usable, and which of
    // several the admin is editing is not something this can know.
    if projects
        .iter()
        .any(|(_, dir)| dir.join("node_modules").is_dir())
    {
        return None;
    }
    projects.first().map(|(name, _)| name.clone())
}

/// Serve one language-server socket for `store`.
///
/// The caller has already established that the request is an admin's; everything
/// this decides is about the store and the machine.
pub(crate) async fn language_server_upgrade(
    ws: WebSocketUpgrade,
    catalog: Option<&Arc<Catalog>>,
    slots: &ServerSlots,
    program: Arc<PathBuf>,
    store: String,
) -> axum::response::Response {
    // Both questions are answered before the upgrade, so a refusal costs no
    // process and no socket beyond the one it is reported on.
    let refusal = match language_server_root(catalog, &store) {
        Err(reason) => reason,
        Ok(root) => match Arc::clone(slots).try_acquire_owned() {
            Ok(permit) => {
                return ws.on_upgrade(move |socket| bridge(socket, program, root, store, permit));
            }
            Err(_) => format!(
                "the server is already running its maximum of {MAX_LANGUAGE_SERVERS} language servers"
            ),
        },
    };
    ws.on_upgrade(move |socket| refuse(socket, refusal))
}

/// Close a socket, saying why. The IDE turns this into one notification.
async fn refuse(mut socket: WebSocket, reason: String) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: close_code::POLICY,
            reason: fit_close_reason(reason).into(),
        })))
        .await;
}

/// A reason cut to what a close frame can carry, on a character boundary.
///
/// The boundary is not a nicety: these sentences contain em dashes, and cutting
/// a `String` mid-character panics — inside the task serving a socket, which is
/// the worst place to find out that a store's name was long.
fn fit_close_reason(reason: String) -> String {
    if reason.len() <= MAX_CLOSE_REASON {
        return reason;
    }
    let mut cut = MAX_CLOSE_REASON;
    while cut > 0 && !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = reason;
    text.truncate(cut);
    text
}

/// Run a language server for `root`, bridging it to `socket` until either end
/// stops.
///
/// `permit` is held for exactly as long as the process lives — dropping it at the
/// end of this function is what returns the slot, so a socket that closes (a
/// reload, a closed tab, a lost network) frees the server it was using without
/// anything having to notice separately.
async fn bridge(
    socket: WebSocket,
    program: Arc<PathBuf>,
    root: PathBuf,
    store: String,
    permit: OwnedSemaphorePermit,
) {
    let mut child = match spawn_language_server(&program, &root) {
        Ok(child) => child,
        Err(e) => {
            eprintln!("feldspar: language server for {store}: {e}");
            refuse(socket, format!("could not start {SERVER_BIN}: {e}")).await;
            return;
        }
    };

    // Every message crossing the bridge is rewritten between these two URI
    // spaces: the workbench's workspace folder and the process's real directory.
    let client_root = format!("file:///{store}");
    let server_root = format!("file://{}", root.display());

    let (mut stdin, stdout, stderr) =
        match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(i), Some(o), Some(e)) => (i, o, e),
            _ => {
                eprintln!("feldspar: language server for {store}: no pipes on the child process");
                refuse(
                    socket,
                    "the language server started without pipes".to_owned(),
                )
                .await;
                return;
            }
        };

    // The server's own complaints (a missing tsserver, a crash) would otherwise
    // vanish: they are not LSP messages, so nothing else would ever read them.
    let store_for_log = store.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("feldspar: {SERVER_BIN} [{store_for_log}]: {line}");
        }
    });

    let (mut sender, mut receiver) = socket.split();

    // Browser → process: bare JSON in, `Content-Length`-framed LSP out.
    let to_server = async {
        while let Some(Ok(message)) = receiver.next().await {
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                    Ok(text) => text,
                    Err(_) => continue,
                },
                Message::Close(_) => break,
                // Ping/Pong are answered by axum itself.
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            let body = remap_message(&text, &client_root, &server_root);
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            if stdin.write_all(header.as_bytes()).await.is_err()
                || stdin.write_all(body.as_bytes()).await.is_err()
                || stdin.flush().await.is_err()
            {
                break;
            }
        }
    };

    // Process → browser: framed LSP in, bare JSON out.
    let to_client = async {
        let mut reader = BufReader::new(stdout);
        while let Some(body) = read_lsp_message(&mut reader).await {
            let text = remap_message(&body, &server_root, &client_root);
            if sender.send(Message::text(text)).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        () = to_server => {}
        () = to_client => {}
    }

    // One process per connection, and it goes when the connection does: an
    // orphaned tsserver holding a project's type graph is the cost of getting
    // this wrong.
    let _ = child.kill().await;
    drop(permit);
}

/// Start the server's own language server (`program`) in `root`.
///
/// Never the store's own `node_modules/.bin` copy, though it would match the
/// project's TypeScript more closely: whoever may edit the store may write that
/// file, and it would run as the server's operating-system user the next time
/// an admin opened the IDE (security review #16).
fn spawn_language_server(program: &Path, root: &Path) -> std::io::Result<Child> {
    let mut command = Command::new(program);
    command
        .arg("--stdio")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A belt to the braces of the explicit kill above: if this task is
        // cancelled rather than run to its end, the child still goes.
        .kill_on_drop(true);
    command.spawn()
}

/// Read one `Content-Length`-framed LSP message body, or `None` at end of stream.
///
/// Headers are `\r\n`-terminated and end with a blank line; only `Content-Length`
/// is acted on, because it is the only one the protocol requires and the only one
/// tsserver sends. A header block that does not carry one is unreadable — there is
/// no way to know where the message ends — so the stream is abandoned rather than
/// guessed at.
async fn read_lsp_message<R>(reader: &mut BufReader<R>) -> Option<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok();
            }
        }
    }
    let mut body = vec![0_u8; length?];
    reader.read_exact(&mut body).await.ok()?;
    String::from_utf8(body).ok()
}

/// Rewrite every `file://` URI in one JSON-RPC message from one root to another.
///
/// Structural rather than textual: a message carries a document's whole text on
/// `didOpen`, and a blind search-and-replace over that would edit the admin's
/// source code. Walking the value touches only strings, and only those that are
/// URIs under the root being left behind.
///
/// A message that is not JSON, or one no URI in which is under the root, is
/// returned exactly as it arrived. That is the deliberate failure mode: a URI this
/// cannot map (a definition in a file outside the store — TypeScript's own bundled
/// `lib.d.ts`, say) passes through unchanged, and the workbench reports it cannot
/// open that file, rather than the bridge inventing a path that does not exist.
fn remap_message(text: &str, from: &str, to: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(text) else {
        return text.to_owned();
    };
    remap_value(&mut value, from, to);
    serde_json::to_string(&value).unwrap_or_else(|_| text.to_owned())
}

/// The recursive half of [`remap_message`].
fn remap_value(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(text) => {
            if let Some(mapped) = remap_uri(text, from, to) {
                *text = mapped;
            }
        }
        Value::Array(items) => {
            for item in items {
                remap_value(item, from, to);
            }
        }
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                // `rootPath` is `initialize`'s deprecated plain-path twin of
                // `rootUri`, and a language server that reads it would otherwise
                // be told the workspace is at `/<store>`.
                if key == "rootPath" {
                    if let Value::String(text) = field {
                        if let Some(mapped) = remap_uri(text, strip_scheme(from), strip_scheme(to))
                        {
                            *text = mapped;
                        }
                    }
                    continue;
                }
                remap_value(field, from, to);
            }
        }
        _ => {}
    }
}

/// `file:///store/src/App.tsx` under one root, as the same file under another.
///
/// The match is on a whole path segment, so a store `app` never rewrites a URI
/// under `app-source`, and `None` — leave it alone — is the answer for everything
/// that is not under `from`.
fn remap_uri(uri: &str, from: &str, to: &str) -> Option<String> {
    let rest = uri.strip_prefix(from)?;
    if rest.is_empty() || rest.starts_with('/') {
        Some(format!("{to}{rest}"))
    } else {
        None
    }
}

/// The path of a `file://` URI, for comparing against a plain path.
fn strip_scheme(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory holding whatever the test needs, cleaned up by the caller.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sc-lsp-{}-{tag}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn a_uri_is_remapped_only_under_its_own_root() {
        assert_eq!(
            remap_uri("file:///app/src/App.tsx", "file:///app", "file:///srv/app"),
            Some("file:///srv/app/src/App.tsx".to_owned())
        );
        // The folder itself.
        assert_eq!(
            remap_uri("file:///app", "file:///app", "file:///srv/app"),
            Some("file:///srv/app".to_owned())
        );
        // A different store that merely starts with the same letters.
        assert_eq!(
            remap_uri("file:///app-source/x.ts", "file:///app", "file:///srv/app"),
            None
        );
        // Somewhere else entirely.
        assert_eq!(
            remap_uri("file:///etc/passwd", "file:///app", "file:///srv/app"),
            None
        );
    }

    #[test]
    fn a_message_is_remapped_structurally_and_a_documents_text_is_not() {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": "file:///app/src/App.tsx",
                    // The trap textual replacement falls into: source code that
                    // happens to mention the workspace path.
                    "text": "// see file:///app/src/App.tsx\nexport const a = 1;\n"
                }
            }
        })
        .to_string();

        let out = remap_message(&message, "file:///app", "file:///srv/code");
        let value: Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(
            value["params"]["textDocument"]["uri"],
            serde_json::json!("file:///srv/code/src/App.tsx")
        );
        assert_eq!(
            value["params"]["textDocument"]["text"],
            serde_json::json!("// see file:///app/src/App.tsx\nexport const a = 1;\n"),
            "the document's own text is content, not a URI"
        );
    }

    #[test]
    fn initialize_carries_both_the_uri_and_the_deprecated_plain_path() {
        let message = serde_json::json!({
            "id": 0,
            "method": "initialize",
            "params": {
                "rootPath": "/app",
                "rootUri": "file:///app",
                "workspaceFolders": [{ "uri": "file:///app", "name": "app" }]
            }
        })
        .to_string();

        let out = remap_message(&message, "file:///app", "file:///srv/code");
        let value: Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(
            value["params"]["rootUri"],
            serde_json::json!("file:///srv/code")
        );
        assert_eq!(value["params"]["rootPath"], serde_json::json!("/srv/code"));
        assert_eq!(
            value["params"]["workspaceFolders"][0]["uri"],
            serde_json::json!("file:///srv/code")
        );
    }

    /// Something that is not JSON is not a message this may rewrite.
    #[test]
    fn a_non_json_message_passes_through() {
        assert_eq!(
            remap_message("not json", "file:///a", "file:///b"),
            "not json"
        );
    }

    #[tokio::test]
    async fn lsp_framing_round_trips() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let stream = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n{body}\
             Content-Length: 2\r\n\r\n{{}}",
            body.len()
        );
        let mut reader = BufReader::new(std::io::Cursor::new(stream.into_bytes()));
        assert_eq!(read_lsp_message(&mut reader).await.as_deref(), Some(body));
        assert_eq!(read_lsp_message(&mut reader).await.as_deref(), Some("{}"));
        assert_eq!(read_lsp_message(&mut reader).await, None);
    }

    /// A project that has never been installed is refused with a reason, and the
    /// same project with `node_modules` is not.
    #[test]
    fn a_project_with_no_dependencies_is_named() {
        let root = temp_dir("uninstalled");
        assert_eq!(
            uninstalled_project(&root),
            None,
            "no package.json: not a node project"
        );

        std::fs::write(root.join("package.json"), "{}").expect("write package.json");
        assert_eq!(uninstalled_project(&root).as_deref(), Some("this store"));

        std::fs::create_dir_all(root.join("node_modules")).expect("create node_modules");
        assert_eq!(uninstalled_project(&root), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The scaffolded shape: the app is a directory *inside* the store.
    #[test]
    fn a_project_one_level_down_is_found_too() {
        let root = temp_dir("nested");
        std::fs::create_dir_all(root.join("app")).expect("create app dir");
        std::fs::write(root.join("app/package.json"), "{}").expect("write package.json");
        assert_eq!(uninstalled_project(&root).as_deref(), Some("app"));

        std::fs::create_dir_all(root.join("app/node_modules")).expect("create node_modules");
        assert_eq!(uninstalled_project(&root), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A reason too long for a close frame is cut, and cut where a `String` may
    /// be cut — these sentences have em dashes in them, and the naive truncation
    /// panics on one inside the task serving the socket.
    #[test]
    fn a_long_reason_is_cut_on_a_character_boundary() {
        let short = "assets has no local path".to_owned();
        assert_eq!(fit_close_reason(short.clone()), short);

        // Padded so the cut lands inside the em dash's three bytes.
        let long = format!("{}— and more", "x".repeat(MAX_CLOSE_REASON - 1));
        let cut = fit_close_reason(long);
        assert!(cut.len() <= MAX_CLOSE_REASON, "{} bytes", cut.len());
        assert_eq!(cut, "x".repeat(MAX_CLOSE_REASON - 1));
    }

    /// Without a catalog there are no stores, and the reason says so rather than
    /// failing as though the store were at fault.
    #[test]
    fn no_catalog_is_a_reason_not_a_panic() {
        let reason = language_server_root(None, "app-source").expect_err("no catalog");
        assert!(reason.contains("no catalog"), "{reason}");
    }
}

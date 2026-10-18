//! The generated `live-doc.ts` (TODO.md "Live updates" L5.6), **run** under
//! Node against a fake socket, with the real `yjs` and `y-protocols` — and
//! type-checked, with `useDocument`, against the generated client.
//!
//! What is claimed:
//!
//! - opening a document subscribes on the row's topic; `doc_sync` fills the
//!   `Y.Doc`, and the page answers with nothing when it had nothing new;
//! - a local edit goes out as one `doc_update`, and a remote one is applied
//!   and **not** sent back;
//! - **edits made while disconnected arrive after the reconnect**: the page
//!   answers the new `doc_sync` with what the server lacks;
//! - a read-only document sends nothing;
//! - the page's awareness is its presence state (base64 of
//!   `encodeAwarenessUpdate`), another member's state reaches the awareness,
//!   and a member who leaves is removed from it;
//! - the root is the stream's: `field` and `root` name it;
//! - `helper.ts` and `live-react.ts` never import `yjs`.
//!
//! Needs `node`, and `yjs` in `ui/admin/node_modules` (a dev dependency there);
//! skips without them.

use std::path::{Path, PathBuf};
use std::process::Command;

use sc_api::{StreamExport, StreamTopics, TypeSchema};

fn admin_node_modules() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/admin/node_modules");
    let ready = dir.join("yjs").is_dir()
        && dir.join("y-protocols").is_dir()
        && Command::new("node").arg("--version").output().is_ok();
    ready.then_some(dir)
}

fn note_body(kind: &str) -> StreamExport {
    StreamExport {
        name: "note_body".to_owned(),
        path: "/api/live".to_owned(),
        value: TypeSchema::json(),
        topics: StreamTopics::Document {
            root: "default".to_owned(),
            root_kind: kind.to_owned(),
        },
        presence: false,
        client_publish: false,
    }
}

/// A scratch directory with `node_modules` linked to the admin UI's.
fn project(tag: &str, node_modules: &Path) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("sc-app-live-doc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::os::unix::fs::symlink(node_modules, dir.join("node_modules"))?;
    Ok(dir)
}

const DRIVER_TS: &str = r#"
import * as Y from "yjs";
import { Awareness, encodeAwarenessUpdate } from "y-protocols/awareness";
import { createClient } from "./client.ts";
import { openDocument } from "./live-doc.ts";

const log: unknown[] = [];
const say = (...entry: unknown[]) => log.push(entry);
const b64 = (u: Uint8Array) => Buffer.from(u).toString("base64");
const unb64 = (s: string) => new Uint8Array(Buffer.from(s, "base64"));

class FakeSocket {
  static all: FakeSocket[] = [];
  url: string;
  readyState = 0;
  sent: any[] = [];
  onopen: ((e: unknown) => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: ((e: unknown) => void) | null = null;
  onerror: ((e: unknown) => void) | null = null;
  constructor(url: string) {
    this.url = url;
    FakeSocket.all.push(this);
  }
  send(data: string) { this.sent.push(JSON.parse(data)); }
  close() { this.readyState = 3; this.onclose?.({}); }
  open() { this.readyState = 1; this.onopen?.({}); }
  frame(frame: unknown) { this.onmessage?.({ data: JSON.stringify(frame) }); }
  drop() { this.readyState = 3; this.onclose?.({}); }
  take(type: string) {
    const found = this.sent.filter((f) => f.type === type);
    this.sent = this.sent.filter((f) => f.type !== type);
    return found;
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// The server's copy of the document, which already says "Hello".
const server = new Y.Doc();
server.getText("default").insert(0, "Hello");
const sync = (socket: FakeSocket, readOnly = false) =>
  socket.frame({ type: "doc_sync", sub: "s1", update: b64(Y.encodeStateAsUpdate(server)), read_only: readOnly });
const absorb = (socket: FakeSocket) => {
  const updates = socket.take("doc_update");
  for (const u of updates) Y.applyUpdate(server, unb64(u.update));
  return updates.length;
};

const api = createClient({
  baseUrl: "https://notes.example.com",
  live: { WebSocket: FakeSocket as unknown as typeof WebSocket, minDelay: 2, maxDelay: 8, presenceInterval: 0 },
});
const page = openDocument(api.live.note_body, 7);
say("field", page.field, "root is a Y.Text", page.root instanceof Y.Text);
const first = FakeSocket.all[0];
first.open();
say("subscribed", first.take("subscribe"));
first.frame({ type: "ready", sub: "s1", stream: "note_body", replayed: 0, can_publish: false });
first.frame({ type: "presence", sub: "s1", connection: "me", members: [{ connection: "me", user_id: "a", state: null }] });
sync(first);
say("after sync", page.status, page.readOnly, page.root.toString(), "answered", absorb(first));

// A local edit goes out once; a remote one comes in and is not sent back.
page.root.insert(5, " world");
say("local edit sent", absorb(first), server.getText("default").toString());
const other = new Y.Doc();
Y.applyUpdate(other, Y.encodeStateAsUpdate(server));
const remote: Uint8Array[] = [];
other.on("update", (u: Uint8Array) => remote.push(u));
other.getText("default").insert(0, ">> ");
Y.applyUpdate(server, remote[0]);
first.frame({ type: "doc_update", sub: "s1", update: b64(remote[0]) });
say("remote edit applied", page.root.toString(), "echoed", first.take("doc_update").length);

// Awareness: this page's goes out as its presence state…
page.awareness.setLocalStateField("user", { name: "Alice" });
await sleep(5);
const states = first.take("presence");
const mine = states[states.length - 1];
say("own awareness is a string state", typeof mine.state === "string" && mine.state.length > 0);
// …another member's comes in, and goes when they leave.
const theirs = new Awareness(other);
theirs.setLocalStateField("user", { name: "Bob" });
const bobState = b64(encodeAwarenessUpdate(theirs, [other.clientID]));
first.frame({ type: "presence_diff", sub: "s1", joined: [{ connection: "bob", user_id: "b", state: bobState }] });
say("bob's cursor arrived", page.awareness.getStates().get(other.clientID)?.user?.name);
first.frame({ type: "presence_diff", sub: "s1", left: ["bob"] });
say("bob's cursor left", page.awareness.getStates().has(other.clientID));

// The socket drops; an edit made meanwhile is kept…
first.drop();
page.root.insert(page.root.length, "!");
say("offline edit kept", page.root.toString(), page.status);
await sleep(30);
const second = FakeSocket.all[1];
second.open();
second.frame({ type: "ready", sub: "s1", stream: "note_body", replayed: 0, can_publish: false });
sync(second);
// …and is what the page answers the new sync with.
say("after reconnect", page.status, "answered", absorb(second), server.getText("default").toString());

// Read-only: nothing is sent.
sync(second, true);
page.root.insert(0, "x");
say("read-only sends nothing", page.readOnly, absorb(second));

page.close();
say("closed", second.take("unsubscribe").length >= 0);
theirs.destroy();
console.log(JSON.stringify(log));
"#;

#[test]
fn a_document_syncs_edits_reconnects_and_carries_awareness() -> std::io::Result<()> {
    let Some(node_modules) = admin_node_modules() else {
        eprintln!("skipping: needs `node` and `yjs` in ui/admin/node_modules");
        return Ok(());
    };
    let dir = project("run", &node_modules)?;
    let streams = [note_body("text")];
    let client = sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &streams)
        .replace("from \"./helper\"", "from \"./helper.ts\"");
    let doc =
        sc_app::live_doc_ts(&streams, false).replace("from \"./helper\"", "from \"./helper.ts\"");
    std::fs::write(dir.join("client.ts"), client)?;
    for (name, text) in sc_api::client_runtime(false) {
        std::fs::write(dir.join(name), text)?;
    }
    std::fs::write(dir.join("live-doc.ts"), doc)?;
    std::fs::write(dir.join("driver.ts"), DRIVER_TS)?;
    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "running live-doc.ts failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("the driver prints its log");
    // Compared as values, not strings: whether an object's keys print sorted
    // depends on serde_json's `preserve_order`, which feature unification
    // turns on in a workspace build and leaves off in `-p sc-app`.
    let expected: Vec<serde_json::Value> = [
        r#"["field","default","root is a Y.Text",true]"#,
        r#"["subscribed",[{"stream":"note_body","sub":"s1","topic":"7","type":"subscribe"}]]"#,
        r#"["after sync","synced",false,"Hello","answered",0]"#,
        r#"["local edit sent",1,"Hello world"]"#,
        r#"["remote edit applied",">> Hello world","echoed",0]"#,
        r#"["own awareness is a string state",true]"#,
        r#"["bob's cursor arrived","Bob"]"#,
        r#"["bob's cursor left",false]"#,
        r#"["offline edit kept",">> Hello world!","reconnecting"]"#,
        r#"["after reconnect","synced","answered",1,">> Hello world!"]"#,
        r#"["read-only sends nothing",true,0]"#,
        r#"["closed",true]"#,
    ]
    .iter()
    .map(|entry| serde_json::from_str(entry).expect("an expected entry is JSON"))
    .collect();
    assert_eq!(log, expected, "{log:#?}");
    Ok(())
}

/// A component using `useDocument` type-checks against the generated runtime,
/// for both root kinds, under the scaffold's strictness.
#[test]
fn use_document_type_checks_with_the_generated_client() -> std::io::Result<()> {
    let Some(node_modules) = admin_node_modules() else {
        eprintln!("skipping: needs `node` and `yjs` in ui/admin/node_modules");
        return Ok(());
    };
    let tsc = std::env::var("SC_TSC")
        .map(PathBuf::from)
        .unwrap_or_else(|_| node_modules.join(".bin/tsc"));
    if !tsc.exists() || !node_modules.join("@types/react").is_dir() {
        eprintln!("skipping: no tsc or @types/react in ui/admin/node_modules");
        return Ok(());
    }
    let dir = project("tsc", &node_modules)?;
    let mut text = note_body("text");
    text.name = "code_body".to_owned();
    let streams = [note_body("xml"), text];
    std::fs::write(
        dir.join("client.ts"),
        sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &streams),
    )?;
    for (name, text) in sc_api::client_runtime(false) {
        std::fs::write(dir.join(name), text)?;
    }
    std::fs::write(
        dir.join("hooks.ts"),
        "import { createClient } from \"./client\";\nexport const api = createClient();\n",
    )?;
    std::fs::write(dir.join("live-doc.ts"), sc_app::live_doc_ts(&streams, true))?;
    std::fs::write(
        dir.join("Note.tsx"),
        r#"import * as Y from "yjs";
import { docs, useDocument } from "./live-doc";

export function Note({ id }: { id: number }) {
  const { doc, awareness, field, root, readOnly, status, error } = useDocument(docs.note_body, id);
  // An xml root is a fragment, as TipTap's Collaboration binds it.
  const fragment: Y.XmlFragment | undefined = root;
  if (error) return <p>{error.message}</p>;
  if (!doc || !awareness) return <p>{status}</p>;
  return <div data-field={field} data-readonly={readOnly}>{fragment?.length}</div>;
}

export function Code({ id }: { id: number }) {
  const { root } = useDocument(docs.code_body, id);
  const text: Y.Text | undefined = root;
  return <pre>{text?.toString()}</pre>;
}

export function outside(): string {
  const open = docs.note_body.open(1);
  const fragment: Y.XmlFragment = open.root;
  open.close();
  return fragment.toString();
}
"#,
    )?;
    let output = Command::new(&tsc)
        .current_dir(&dir)
        .args([
            "--noEmit",
            "--strict",
            "--noUnusedLocals",
            "--skipLibCheck",
            "--jsx",
            "react-jsx",
            "--target",
            "es2020",
            "--lib",
            "es2020,dom",
            "--module",
            "esnext",
            "--moduleResolution",
            "bundler",
            "Note.tsx",
        ])
        .output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "tsc refused the document runtime:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// `yjs` is imported by `live-doc.ts` alone: never by the helper (which the
/// admin and IDE UIs carry a copy of) and never by `live-react.ts`.
#[test]
fn only_live_doc_imports_yjs() {
    assert!(!sc_api::client_helper().contains("from \"yjs\""));
    let streams = [note_body("xml")];
    let client = sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &streams);
    assert!(!client.contains("yjs"), "the client imports no yjs");
    assert!(
        client.contains("connection.documentStream<\"xml\">(\"note_body\", \"default\", \"xml\")")
    );
    let doc = sc_app::live_doc_ts(&streams, true);
    assert!(doc.contains("from \"yjs\""));
    assert!(doc.contains("note_body: documentStream(api.live.note_body)"));
}

/// A client whose API has one stream query, `openNote` (TODO F7), beside the
/// `note_body` document stream.
fn query_client(streams: &[StreamExport]) -> String {
    use sc_api::{Endpoint, EndpointSet, Method, PathSpec, StructField};
    let mut set = EndpointSet::new();
    set.register(
        Endpoint::new(
            "openNote",
            Method::Post,
            PathSpec::root().lit("/api").lit("/open-note"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "token",
            TypeSchema::text(),
        )]))
        .output(TypeSchema::json())
        .open_args()
        .stream_output(),
    );
    sc_api::generate_client_with_streams(&set, streams)
}

const QUERY_DRIVER_TS: &str = r#"
import * as Y from "yjs";
import { createClient } from "./client.ts";
import { openQueryDocument } from "./live-doc.ts";

const log: unknown[] = [];
const say = (...entry: unknown[]) => log.push(entry);
const b64 = (u: Uint8Array) => Buffer.from(u).toString("base64");
const unb64 = (s: string) => new Uint8Array(Buffer.from(s, "base64"));

class FakeSocket {
  static all: FakeSocket[] = [];
  url: string;
  readyState = 0;
  sent: any[] = [];
  onopen: ((e: unknown) => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: ((e: unknown) => void) | null = null;
  onerror: ((e: unknown) => void) | null = null;
  constructor(url: string) {
    this.url = url;
    FakeSocket.all.push(this);
  }
  send(data: string) { this.sent.push(JSON.parse(data)); }
  close() { this.readyState = 3; this.onclose?.({}); }
  open() { this.readyState = 1; this.onopen?.({}); }
  frame(frame: unknown) { this.onmessage?.({ data: JSON.stringify(frame) }); }
  drop() { this.readyState = 3; this.onclose?.({}); }
  take(type: string) {
    const found = this.sent.filter((f) => f.type === type);
    this.sent = this.sent.filter((f) => f.type !== type);
    return found;
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const server = new Y.Doc();
server.getText("body").insert(0, "Hello");
const ready = (socket: FakeSocket, sub: string) => socket.frame({
  type: "ready", sub, stream: "note_body", replayed: 0, can_publish: false,
  opened: { stream: "note_body", kind: "document", key: "7", root: "body", root_kind: "text" },
});

const api = createClient({
  baseUrl: "https://notes.example.com",
  live: { WebSocket: FakeSocket as unknown as typeof WebSocket, minDelay: 2, maxDelay: 8, presenceInterval: 0 },
});
const page = openQueryDocument((h) => api.openNote({ token: "t1" }, h), api.live.note_body.connection);
const first = FakeSocket.all[0];
first.open();
say("opened with", first.take("open"));
say("no root before ready", page.root === undefined, page.field === undefined);
ready(first, "s1");
say("root after ready", page.field, page.root instanceof Y.Text, page.opened?.key);
first.frame({ type: "doc_sync", sub: "s1", update: b64(Y.encodeStateAsUpdate(server)), read_only: false });
say("synced", page.status, page.readOnly, String(page.root));
(page.root as Y.Text).insert(5, " world");
const edits = first.take("doc_update");
for (const u of edits) Y.applyUpdate(server, unb64(u.update));
say("edit sent", edits.length, server.getText("body").toString());

// A reconnect opens the query again, with the same parameters.
first.drop();
await sleep(30);
const second = FakeSocket.all[1];
second.open();
say("reopened", second.take("open"));

// A link that opens nothing is the page's error.
const refused = openQueryDocument((h) => api.openNote({ token: "nope" }, h));
second.frame({ type: "error", sub: "s2", code: "unavailable", message: "nothing called `openNote` here" });
say("refused", refused.error?.code);
page.close();
refused.close();
console.log(JSON.stringify(log));
"#;

#[test]
fn a_stream_query_opens_a_document_whose_root_the_server_names() -> std::io::Result<()> {
    let Some(node_modules) = admin_node_modules() else {
        eprintln!("skipping: needs `node` and `yjs` in ui/admin/node_modules");
        return Ok(());
    };
    let dir = project("query", &node_modules)?;
    let streams = [note_body("xml")];
    let fix = |text: String| text.replace("from \"./helper\"", "from \"./helper.ts\"");
    std::fs::write(dir.join("client.ts"), fix(query_client(&streams)))?;
    for (name, text) in sc_api::client_runtime(false) {
        std::fs::write(dir.join(name), text)?;
    }
    std::fs::write(
        dir.join("live-doc.ts"),
        fix(sc_app::live_doc_ts(&streams, false)),
    )?;
    std::fs::write(dir.join("driver.ts"), QUERY_DRIVER_TS)?;
    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "running live-doc.ts failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("the driver prints its log");
    let expected: Vec<serde_json::Value> = [
        r#"["opened with",[{"params":{"token":"t1"},"query":"openNote","sub":"s1","type":"open"}]]"#,
        r#"["no root before ready",true,true]"#,
        r#"["root after ready","body",true,"7"]"#,
        r#"["synced","synced",false,"Hello"]"#,
        r#"["edit sent",1,"Hello world"]"#,
        r#"["reopened",[{"params":{"token":"t1"},"query":"openNote","sub":"s1","type":"open"}]]"#,
        r#"["refused","unavailable"]"#,
    ]
    .iter()
    .map(|entry| serde_json::from_str(entry).expect("an expected entry is JSON"))
    .collect();
    assert_eq!(log, expected, "{log:#?}");
    Ok(())
}

/// `useQueryDocument(api.openNote, { token })` type-checks against the
/// generated client, and a stream query's method is typed as a session.
#[test]
fn use_query_document_type_checks_with_the_generated_client() -> std::io::Result<()> {
    let Some(node_modules) = admin_node_modules() else {
        eprintln!("skipping: needs `node` and `yjs` in ui/admin/node_modules");
        return Ok(());
    };
    let tsc = std::env::var("SC_TSC")
        .map(PathBuf::from)
        .unwrap_or_else(|_| node_modules.join(".bin/tsc"));
    if !tsc.exists() || !node_modules.join("@types/react").is_dir() {
        eprintln!("skipping: no tsc or @types/react in ui/admin/node_modules");
        return Ok(());
    }
    let dir = project("query-tsc", &node_modules)?;
    let streams = [note_body("xml")];
    let client = query_client(&streams);
    assert!(client.contains(
        "openNote(body: OpenNoteRequest, handlers?: LiveOpenHandlers): LiveOpenedSession;"
    ));
    std::fs::write(dir.join("client.ts"), client)?;
    for (name, text) in sc_api::client_runtime(false) {
        std::fs::write(dir.join(name), text)?;
    }
    std::fs::write(
        dir.join("hooks.ts"),
        "import { createClient } from \"./client\";\nexport const api = createClient();\n",
    )?;
    std::fs::write(dir.join("live-doc.ts"), sc_app::live_doc_ts(&streams, true))?;
    std::fs::write(
        dir.join("Shared.tsx"),
        r#"import * as Y from "yjs";
import { api } from "./hooks";
import { useQueryDocument } from "./live-doc";

export function Shared({ token }: { token: string }) {
  const { doc, root, field, opened, readOnly, status, error } = useQueryDocument(api.openNote, { token });
  if (error) return <p>{error.code}</p>;
  if (!doc || !root) return <p>{status}</p>;
  const fragment: Y.XmlFragment | undefined = root instanceof Y.XmlFragment ? root : undefined;
  return <div data-field={field} data-readonly={readOnly} data-key={opened?.key}>{fragment?.length}</div>;
}

export function rowTopic(token: string): () => void {
  const session = api.openNote({ token }, {
    ready: (info) => console.log(info.opened.kind),
    element: (envelope) => console.log(envelope.value),
  });
  if (session.canPublish) session.publish({ at: 1 });
  return () => session.close();
}
"#,
    )?;
    let output = Command::new(&tsc)
        .current_dir(&dir)
        .args([
            "--noEmit",
            "--strict",
            "--noUnusedLocals",
            "--skipLibCheck",
            "--jsx",
            "react-jsx",
            "--target",
            "es2020",
            "--lib",
            "es2020,dom",
            "--module",
            "esnext",
            "--moduleResolution",
            "bundler",
            "Shared.tsx",
        ])
        .output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "tsc refused the stream query runtime:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

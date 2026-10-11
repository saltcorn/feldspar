//! The generated client's `LiveConnection` (TODO.md "Live updates" L1.9),
//! **run** rather than read: Node executes the generated TypeScript by
//! stripping its types, and the driver below hands the client a fake
//! `WebSocket` it can open, feed and drop at will. `generated_client_csrf`'s
//! arrangement, for its reason — asserting on the emitted text would pass just
//! as happily on code that never runs. Skips when `node` is absent.
//!
//! What it claims, in the order a page lives through it:
//!
//! - nothing connects until the first `subscribe`, and every stream shares one
//!   socket, at `wss://…/api/live` for an `https` base URL;
//! - subscribing on an open socket sends at once, and before it opens is sent
//!   on open;
//! - frames reach the subscription they name, and a frame of a type the client
//!   does not know, or text that is not JSON, is ignored rather than thrown on;
//! - a subscription refused before `ready` is ended and is **not** asked for
//!   again after a reconnect;
//! - a dropped socket reconnects by itself, resubscribes what is left, and
//!   tells each of those subscribers `resync`;
//! - `revoked` ends a subscription; closing the last one closes the socket.

use std::process::Command;

use sc_api::{StreamExport, TypeSchema};

const DRIVER_TS: &str = r#"
import { createClient } from "./client.ts";

const log: unknown[] = [];
const say = (...entry: unknown[]) => log.push(entry);

class FakeSocket {
  static all: FakeSocket[] = [];
  url: string;
  readyState = 0;
  sent: unknown[] = [];
  closedByClient = false;
  onopen: ((e: unknown) => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: ((e: unknown) => void) | null = null;
  onerror: ((e: unknown) => void) | null = null;
  constructor(url: string) {
    this.url = url;
    FakeSocket.all.push(this);
  }
  send(data: string) {
    this.sent.push(JSON.parse(data));
  }
  close() {
    this.closedByClient = true;
    this.readyState = 3;
    this.onclose?.({});
  }
  // The server's side.
  open() {
    this.readyState = 1;
    this.onopen?.({});
  }
  frame(frame: unknown) {
    this.onmessage?.({ data: typeof frame === "string" ? frame : JSON.stringify(frame) });
  }
  drop() {
    this.readyState = 3;
    this.onclose?.({});
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const api = createClient({
  baseUrl: "https://blog.example.com",
  live: { WebSocket: FakeSocket as unknown as typeof WebSocket, minDelay: 2, maxDelay: 8 },
});
const connection = api.live.boiler.connection;
connection.onStatus((s) => say("status", s));
say("sockets before subscribing", FakeSocket.all.length);

const handlers = (name: string) => ({
  ready: (info: { replayed: number }) => say(name, "ready", info.replayed),
  element: (e: { value: unknown }) => say(name, "element", e.value),
  lagged: (n: number) => say(name, "lagged", n),
  resync: () => say(name, "resync"),
  error: (e: { code: string }) => say(name, "error", e.code),
});

const boiler = api.live.boiler.subscribe(handlers("boiler"));
const first = FakeSocket.all[0];
say("url", first.url);
first.open();
first.frame({ type: "ready", sub: "s1", stream: "boiler", replayed: 2 });
first.frame({ type: "element", sub: "s1", envelope: { value: { temperature: 31.2 } } });
first.frame({ type: "lagged", sub: "s1", dropped: 3 });
first.frame({ type: "a_frame_from_the_future", sub: "s1" });
first.frame("not json at all");

// Subscribed on an open socket: sent at once. Refused before `ready`.
api.live.pulse.subscribe(handlers("pulse"));
first.frame({ type: "error", sub: "s2", code: "unavailable", message: "no" });
say("sent on the first socket", [...first.sent]);
say("sockets after two subscriptions", FakeSocket.all.length);

// The server goes away; the client comes back by itself.
first.drop();
await sleep(50);
const second = FakeSocket.all[1];
second.open();
say("sent on the second socket", [...second.sent]);

// Access re-checked and refused: the subscription is over.
second.frame({ type: "revoked", sub: "s1" });
second.frame({ type: "element", sub: "s1", envelope: { value: { temperature: 99 } } });

// The last subscription to close closes the socket.
const third = api.live.pulse.subscribe(handlers("third"));
third.close();
boiler.close();
say("second socket closed by the client", second.closedByClient);
say("sockets in all", FakeSocket.all.length);
say("final status", connection.status);

console.log(JSON.stringify(log));
"#;

#[test]
fn the_live_connection_reconnects_resubscribes_and_lets_go() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    let streams = ["boiler", "pulse"].map(|name| StreamExport {
        name: name.to_owned(),
        path: "/api/live".to_owned(),
        value: TypeSchema::json(),
        topics: sc_api::StreamTopics::Single,
        presence: false,
        client_publish: false,
    });
    let client_ts = sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &streams)
        .replace("from \"./helper\"", "from \"./helper.ts\"");

    let dir = std::env::temp_dir().join(format!("sc-api-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), client_ts)?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER_TS)?;
    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "running the generated client failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("the driver prints its log");
    let log: Vec<String> = log.iter().map(|entry| entry.to_string()).collect();
    let expected = [
        // Lazy: nothing until the first subscription.
        r#"["sockets before subscribing",0]"#,
        r#"["url","wss://blog.example.com/api/live"]"#,
        r#"["status","open"]"#,
        r#"["boiler","ready",2]"#,
        r#"["boiler","element",{"temperature":31.2}]"#,
        r#"["boiler","lagged",3]"#,
        // The unknown frame and the non-JSON text left no trace.
        r#"["pulse","error","unavailable"]"#,
        concat!(
            r#"["sent on the first socket",[{"type":"subscribe","sub":"s1","stream":"boiler"},"#,
            r#"{"type":"subscribe","sub":"s2","stream":"pulse"}]]"#
        ),
        r#"["sockets after two subscriptions",1]"#,
        r#"["status","reconnecting"]"#,
        r#"["status","open"]"#,
        // Only what was not refused is asked for again, and told to resync.
        r#"["boiler","resync"]"#,
        r#"["sent on the second socket",[{"type":"subscribe","sub":"s1","stream":"boiler"}]]"#,
        r#"["boiler","error","revoked"]"#,
        // The element after `revoked` reached nobody, and the subscription
        // made and closed on the second socket was sent and withdrawn (checked
        // below); the last close lets the socket go.
        r#"["status","connecting"]"#,
        r#"["second socket closed by the client",true]"#,
        r#"["sockets in all",2]"#,
        r#"["final status","connecting"]"#,
    ];
    assert_eq!(log, expected, "{log:#?}");
    Ok(())
}

/// A subscription made on an open socket is sent at once, and closing it sends
/// the `unsubscribe` while the socket stays for the others.
#[test]
fn closing_one_subscription_unsubscribes_it_and_keeps_the_socket() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    const DRIVER: &str = r#"
import { createClient } from "./client.ts";
const sockets: any[] = [];
class FakeSocket {
  readyState = 0; sent: unknown[] = []; closed = false;
  onopen: any = null; onmessage: any = null; onclose: any = null; onerror: any = null;
  url: string;
  constructor(url: string) { this.url = url; sockets.push(this); }
  send(d: string) { this.sent.push(JSON.parse(d)); }
  close() { this.closed = true; }
}
const api = createClient({ live: { WebSocket: FakeSocket as unknown as typeof WebSocket } });
const a = api.live.boiler.subscribe({ element() {} });
sockets[0].readyState = 1;
sockets[0].onopen({});
const b = api.live.boiler.subscribe({ element() {} });
b.close();
b.close();
console.log(JSON.stringify({ sent: sockets[0].sent, closed: sockets[0].closed, sockets: sockets.length }));
void a;
"#;
    let streams = [StreamExport {
        name: "boiler".to_owned(),
        path: "/api/live".to_owned(),
        value: TypeSchema::json(),
        topics: sc_api::StreamTopics::Single,
        presence: false,
        client_publish: false,
    }];
    let client_ts = sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &streams)
        .replace("from \"./helper\"", "from \"./helper.ts\"");
    let dir = std::env::temp_dir().join(format!("sc-api-live-close-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), client_ts)?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER)?;
    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        seen,
        serde_json::json!({
            "sent": [
                { "type": "subscribe", "sub": "s1", "stream": "boiler" },
                { "type": "subscribe", "sub": "s2", "stream": "boiler" },
                // Once, however many times `close` is called.
                { "type": "unsubscribe", "sub": "s2" },
            ],
            "closed": false,
            "sockets": 1,
        })
    );
    Ok(())
}

/// A row stream's subscription names its row on the wire — as text, whether the
/// page wrote `7` or `"7"` — and asks for the same row again after a
/// reconnect; a per-user stream's names nothing (TODO.md "Live updates" L2.10).
#[test]
fn a_row_topic_is_sent_with_its_subscription_and_again_after_a_reconnect() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    const DRIVER: &str = r#"
import { createClient } from "./client.ts";
const sockets: any[] = [];
class FakeSocket {
  readyState = 0; sent: unknown[] = []; closed = false;
  onopen: any = null; onmessage: any = null; onclose: any = null; onerror: any = null;
  url: string;
  constructor(url: string) { this.url = url; sockets.push(this); }
  send(d: string) { this.sent.push(JSON.parse(d)); }
  close() { this.closed = true; }
}
const api = createClient({ live: { WebSocket: FakeSocket as unknown as typeof WebSocket, minDelay: 1, maxDelay: 2 } });
api.live.board_activity.subscribe(7, { element() {} });
api.live.board_activity.topic("8").subscribe({ element() {} });
api.live.job_status.subscribe({ element() {} });
const same = api.live.board_activity.topic(7) === api.live.board_activity.topic("7");
sockets[0].readyState = 1;
sockets[0].onopen({});
const first = [...sockets[0].sent];
sockets[0].readyState = 3;
sockets[0].onclose({});
await new Promise((r) => setTimeout(r, 30));
sockets[1].readyState = 1;
sockets[1].onopen({});
console.log(JSON.stringify({ first, second: sockets[1].sent, same }));
"#;
    let message = TypeSchema::json();
    let streams = [
        StreamExport {
            name: "board_activity".to_owned(),
            path: "/api/live".to_owned(),
            value: message.clone(),
            topics: sc_api::StreamTopics::Row,
            presence: false,
            client_publish: false,
        },
        StreamExport {
            name: "job_status".to_owned(),
            path: "/api/live".to_owned(),
            value: message,
            topics: sc_api::StreamTopics::User,
            presence: false,
            client_publish: false,
        },
    ];
    let client_ts = sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &streams)
        .replace("from \"./helper\"", "from \"./helper.ts\"");
    let dir = std::env::temp_dir().join(format!("sc-api-live-topics-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), client_ts)?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER)?;
    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let frames = serde_json::json!([
        { "type": "subscribe", "sub": "s1", "stream": "board_activity", "topic": "7" },
        { "type": "subscribe", "sub": "s2", "stream": "board_activity", "topic": "8" },
        { "type": "subscribe", "sub": "s3", "stream": "job_status" },
    ]);
    assert_eq!(seen["first"], frames);
    assert_eq!(seen["second"], frames, "the same rows asked for again");
    assert_eq!(seen["same"], serde_json::json!(true));
    Ok(())
}

/// A subscription to a table's changes is sent with the read's filter, lowered
/// to the socket's filter object, and sent again after a reconnect; the
/// table's own object carries the same stream as `changes` (TODO.md "Live
/// updates" L3.10).
#[test]
fn a_change_subscription_is_sent_with_its_filter_and_again_after_a_reconnect() -> std::io::Result<()>
{
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    const DRIVER: &str = r#"
import { createClient } from "./client.ts";
const sockets: any[] = [];
class FakeSocket {
  readyState = 0; sent: unknown[] = []; closed = false;
  onopen: any = null; onmessage: any = null; onclose: any = null; onerror: any = null;
  url: string;
  constructor(url: string) { this.url = url; sockets.push(this); }
  send(d: string) { this.sent.push(JSON.parse(d)); }
  close() { this.closed = true; }
}
const api = createClient({ live: { WebSocket: FakeSocket as unknown as typeof WebSocket, minDelay: 1, maxDelay: 2 } });
const seen: unknown[] = [];
api.live.posts_live.subscribe(
  { where: { author: "eq.7", title: "in.(a,\"b,c\")", published: "is_null.false" } },
  { element: (e) => seen.push(e.value) },
);
api.posts.changes.subscribe({}, { element() {} });
const same = api.posts.changes === api.live.posts_live;
sockets[0].readyState = 1;
sockets[0].onopen({});
sockets[0].onmessage({ data: JSON.stringify({ type: "element", sub: "s1", envelope: { value: { op: "leave", key: 3 } } }) });
const first = [...sockets[0].sent];
sockets[0].readyState = 3;
sockets[0].onclose({});
await new Promise((r) => setTimeout(r, 30));
sockets[1].readyState = 1;
sockets[1].onopen({});
console.log(JSON.stringify({ first, second: sockets[1].sent, same, seen, key: api.posts.changes.key, table: api.posts.changes.table }));
"#;
    let provider = sc_api::RestProvider::project("/api", &[posts()]);
    let streams = [StreamExport {
        name: "posts_live".to_owned(),
        path: "/api/live".to_owned(),
        value: TypeSchema::json(),
        topics: sc_api::StreamTopics::Changes {
            table: "posts".to_owned(),
        },
        presence: false,
        client_publish: false,
    }];
    let client_ts =
        sc_api::generate_client_with_streams(sc_api::ApiProvider::endpoints(&provider), &streams)
            .replace("from \"./helper\"", "from \"./helper.ts\"");
    let dir = std::env::temp_dir().join(format!("sc-api-live-changes-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), client_ts)?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER)?;
    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let frames = serde_json::json!([
        {
            "type": "subscribe", "sub": "s1", "stream": "posts_live",
            "filter": {
                "author": { "eq": "7" },
                "title": { "in": ["a", "b,c"] },
                "published": { "is_null": false },
            },
        },
        // No `where` is every row: the frame carries no filter.
        { "type": "subscribe", "sub": "s2", "stream": "posts_live" },
    ]);
    assert_eq!(seen["first"], frames);
    assert_eq!(seen["second"], frames, "the same filters asked for again");
    assert_eq!(seen["same"], serde_json::json!(true));
    assert_eq!(
        seen["seen"],
        serde_json::json!([{ "op": "leave", "key": 3 }])
    );
    assert_eq!(seen["key"], serde_json::json!("id"));
    assert_eq!(seen["table"], serde_json::json!("posts"));
    Ok(())
}

/// A small `posts` table with an identity key, for a client that reads it.
fn posts() -> sc_catalog::Table {
    use sc_catalog::{AccessRules, DataField, DbId, Table, TableId, TableSource};
    use sc_types::{BasicType, TypeRef};
    Table {
        id: TableId("posts".to_owned()),
        name: "posts".to_owned(),
        database: DbId::primary(),
        source: TableSource::Database,
        fields: vec![
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .primary_key()
                .required()
                .generated(sc_db::ColumnGenerator::Identity),
            DataField::plain("title", TypeRef::Basic(BasicType::Text)).required(),
            DataField::plain("author", TypeRef::Basic(BasicType::Int)),
            DataField::plain("published", TypeRef::Basic(BasicType::Date)),
        ],
        primary_key: vec!["id".to_owned()],
        label: "posts".to_owned(),
        description: String::new(),
        access: AccessRules::default(),
        attributes: Default::default(),
        overlay: None,
        ownership: None,
        ownership_error: None,
        rls_enabled: false,
        constraints: Vec::new(),
    }
}

/// L4.4: joining a row's topic keeps its member list — `presence` replaces it,
/// `presence_diff` patches it — this page's state is throttled and sent again
/// after a reconnect, a publish before `ready` waits for it, `echo` is asked
/// for only when wanted, and publishing without a join opens a subscription
/// for the value and closes it once sent.
#[test]
fn joining_keeps_the_members_and_sends_state_and_publishes() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    const DRIVER: &str = r#"
import { createClient, applyPresence } from "./client.ts";
const sockets: any[] = [];
class FakeSocket {
  readyState = 0; sent: any[] = []; closed = false;
  onopen: any = null; onmessage: any = null; onclose: any = null; onerror: any = null;
  url: string;
  constructor(url: string) { this.url = url; sockets.push(this); }
  send(d: string) { this.sent.push(JSON.parse(d)); }
  close() { this.closed = true; }
  frame(f: unknown) { this.onmessage({ data: JSON.stringify(f) }); }
}
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const api = createClient({
  live: { WebSocket: FakeSocket as unknown as typeof WebSocket, minDelay: 1, maxDelay: 2, presenceInterval: 40 },
});
const seen: unknown[] = [];
const room = api.live.board_presence.join(7, {
  presence: (members) => seen.push(members.map((m) => [m.connection, m.state])),
});
// Published before the socket is open: held until `ready`.
room.publish({ dragging: 1 });
const s = sockets[0];
s.readyState = 1;
s.onopen({});
s.frame({ type: "ready", sub: "s1", stream: "board_presence", replayed: 0, can_publish: true });
s.frame({ type: "presence", sub: "s1", connection: "me", members: [
  { connection: "me", user_id: "u1", state: null },
  { connection: "bob", user_id: "u2", name: "Bob", state: null },
] });
s.frame({ type: "presence_diff", sub: "s1",
  joined: [{ connection: "carol", user_id: null, state: null }],
  updated: [{ connection: "bob", state: { dragging: 4 } }] });
s.frame({ type: "presence_diff", sub: "s1", left: ["carol"] });
// Three states at once: the first is sent, the last after the interval.
room.setState({ dragging: 1 });
room.setState({ dragging: 2 });
room.setState({ dragging: 3 });
const beforeWait = s.sent.filter((f: any) => f.type === "presence").length;
await sleep(80);
const afterWait = s.sent.filter((f: any) => f.type === "presence").map((f: any) => f.state);
const state = { me: room.me, canPublish: room.canPublish, members: room.members.map((m) => m.connection) };
// A reconnect: the state is sent again once the new list does not carry it.
s.readyState = 3;
s.onclose({});
await sleep(20);
const t = sockets[1];
t.readyState = 1;
t.onopen({});
t.frame({ type: "ready", sub: "s1", stream: "board_presence", replayed: 0, can_publish: true });
t.frame({ type: "presence", sub: "s1", connection: "me2", members: [{ connection: "me2", user_id: "u1", state: null }] });
// Publishing on a topic this page has not joined: a subscription for it.
api.live.board_presence.publish(8, { dragging: 9 });
const opened = t.sent.at(-1);
t.frame({ type: "ready", sub: opened.sub, stream: "board_presence", replayed: 0, can_publish: true });
// Echo is asked for only when wanted.
api.live.board_presence.join(9, { echo: true });
console.log(JSON.stringify({
  first: s.sent, beforeWait, afterWait, state, seen, second: t.sent,
  pure: applyPresence([{ connection: "a", user_id: null, state: null }],
    { type: "presence_diff", left: ["a"], joined: [{ connection: "a", user_id: null, state: 1 }] }),
}));
room.leave();
"#;
    let streams = [StreamExport {
        name: "board_presence".to_owned(),
        path: "/api/live".to_owned(),
        value: TypeSchema::json(),
        topics: sc_api::StreamTopics::Row,
        presence: true,
        client_publish: true,
    }];
    let client_ts = sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &streams)
        .replace("from \"./helper\"", "from \"./helper.ts\"")
        // The pure reducer is the helper's; the driver reaches it through the
        // client module like everything else.
        + "export { applyPresence } from \"./helper.ts\";\n";
    let dir = std::env::temp_dir().join(format!("sc-api-live-presence-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), client_ts)?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER)?;
    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    use serde_json::json;
    assert_eq!(
        seen["first"],
        json!([
            { "type": "subscribe", "sub": "s1", "stream": "board_presence", "topic": "7" },
            // Held until `ready`, then sent.
            { "type": "publish", "sub": "s1", "value": { "dragging": 1 } },
            { "type": "presence", "sub": "s1", "state": { "dragging": 1 } },
            // The two in between were coalesced into the latest.
            { "type": "presence", "sub": "s1", "state": { "dragging": 3 } },
        ]),
        "{seen:#}"
    );
    assert_eq!(seen["beforeWait"], json!(1));
    assert_eq!(
        seen["afterWait"],
        json!([{ "dragging": 1 }, { "dragging": 3 }])
    );
    assert_eq!(
        seen["state"],
        json!({ "me": "me", "canPublish": true, "members": ["me", "bob"] })
    );
    // The list as the page was told it: whole, patched, patched, and then
    // this page's own state shown at once, each time it was set.
    assert_eq!(seen["seen"][0], json!([["me", null], ["bob", null]]));
    assert_eq!(
        seen["seen"][1],
        json!([["me", null], ["bob", { "dragging": 4 }], ["carol", null]])
    );
    assert_eq!(
        seen["seen"][2],
        json!([["me", null], ["bob", { "dragging": 4 }]])
    );
    assert_eq!(
        seen["second"],
        json!([
            { "type": "subscribe", "sub": "s1", "stream": "board_presence", "topic": "7" },
            // The fresh list did not carry this page's state: sent again.
            { "type": "presence", "sub": "s1", "state": { "dragging": 3 } },
            { "type": "subscribe", "sub": "s2", "stream": "board_presence", "topic": "8" },
            { "type": "publish", "sub": "s2", "value": { "dragging": 9 } },
            { "type": "unsubscribe", "sub": "s2" },
            { "type": "subscribe", "sub": "s3", "stream": "board_presence", "topic": "9", "echo": true },
        ]),
        "{seen:#}"
    );
    // A diff is applied joined, then updated, then left.
    assert_eq!(seen["pure"], json!([]));
    Ok(())
}

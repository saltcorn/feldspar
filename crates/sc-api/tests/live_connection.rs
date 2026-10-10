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

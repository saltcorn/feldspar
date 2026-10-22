//! Offline sync S4: the generated client keeps a copy of the synced tables,
//! works on it without a connection, and syncs it (design §14.11).
//!
//! The client is **run**, not read: Node executes the generated TypeScript, and
//! its `fetch` hands every request to this test over stdin/stdout, which answers
//! it with the real REST provider on a real database. So the copy is checked
//! against the server it will meet, not against a stub of it. Skips when `node`
//! is absent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Stdio;
use std::sync::Arc;

use sc_api::{ApiProvider, ApiRequest, Method, RestProvider, schema_edit};
use sc_auth::{User, bootstrap, create_user};
use sc_catalog::{Catalog, bootstrap_change_log, bootstrap_field_meta, bootstrap_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const APP: uuid::Uuid = uuid::Uuid::from_u128(0x5eed);
const SERVER_NOTE: &str = "00000000-0000-4000-8000-000000000001";

/// The device's side. `fetch` and `sql` are questions to the test; each answer
/// comes back on stdin under the question's id.
const DRIVER_TS: &str = r#"
import { createClient } from "./client.ts";
import { memoryStorage } from "./offline-backends.ts";
import * as readline from "node:readline";

const [, , serverNote, annId] = process.argv;
const waiting = new Map<number, (answer: any) => void>();
readline.createInterface({ input: process.stdin }).on("line", (line) => {
  const answer = JSON.parse(line);
  waiting.get(answer.id)?.(answer);
  waiting.delete(answer.id);
});
let asked = 0;
const ask = (question: object): Promise<any> => {
  const id = ++asked;
  process.stdout.write(JSON.stringify({ ...question, id }) + "\n");
  return new Promise((resolve) => waiting.set(id, resolve));
};
const sql = (statement: string) => ask({ kind: "sql", sql: statement });

let offline = false;
const calls: string[] = [];
const fetch = async (url: any, init: any) => {
  if (offline) throw new TypeError("fetch failed");
  calls.push(String(url));
  const answer = await ask({
    kind: "fetch",
    method: init?.method ?? "GET",
    url: String(url),
    body: init?.body ? JSON.parse(init.body) : null,
  });
  return new Response(JSON.stringify(answer.body), {
    status: answer.status,
    headers: { "content-type": "application/json" },
  });
};

const out: Record<string, unknown> = {};
const storage = memoryStorage();
const api = createClient({ fetch, offline: { storage, auto: false } });
const sync = api.sync!;

// The first sync brings the server's rows.
await sync.now();
out.first = (await api.notes.list()).map((n) => n.title);

// Offline: every read and write works on the copy.
offline = true;
const mine = await api.notes.create({ title: "Written offline", owner: annId });
await api.items.create({ note: mine.id, label: "child of an offline note" });
await api.notes.update(serverNote, { title: "Server note, edited offline" });
const doomed = await api.notes.create({ title: null as any, owner: annId });
out.offlineNotes = (await api.notes.list({ order: "title" })).map((n) => n.title);
out.filtered = (await api.notes.list({ filter: { title: "ilike.%OFFLINE%" }, order: "title.desc" })).map((n) => n.title);
out.embedded = await api.items.list({ select: "label,parent:note(title)" });
out.got = (await api.notes.get(mine.id))?.title;
out.pendingOffline = sync.status().pending;
out.offlineSync = await sync.now().then(() => "synced", (e) => String(e));
out.offlineStatus = { online: sync.status().online, error: Boolean(sync.status().error) };

// Meanwhile, on the server.
await sql(`INSERT INTO notes (id, title, owner) VALUES ('00000000-0000-4000-8000-000000000002', 'Written on the web', '${annId}')`);

// Online again: up goes the outbox, down come the server's changes.
offline = false;
await sync.now();
out.afterSync = (await api.notes.list({ order: "title" })).map((n) => n.title);
out.refused = sync.refused().map((r) => ({ op: r.op, key: r.key === doomed.id, error: r.error }));
out.pendingAfter = sync.status().pending;
out.status = { online: sync.status().online, error: sync.status().error ?? null };

// A second client over the same storage starts from it, as the same device.
const before = calls.length;
const again = createClient({ fetch, offline: { storage, auto: false } });
await again.sync!.ready;
out.reloaded = (await again.notes.list({ order: "title" })).map((n) => n.title);
offline = true;
await again.notes.delete(serverNote);
out.afterDelete = (await again.notes.list()).length;
offline = false;
await again.sync!.now();
out.reloadCalls = calls.slice(before).map((url) => url.replace(/^.*\/sync\//, ""));
out.finalNotes = (await again.notes.list({ order: "title" })).map((n) => n.title);
out.offlineNote = mine.id;

process.stdout.write(JSON.stringify({ kind: "done", result: out }) + "\n");
process.exit(0);
"#;

struct Fixture {
    cat: Arc<Catalog>,
    api: RestProvider,
    ann: User,
}

/// `notes` belong to their `owner`; `items` hang off a note. Both synced.
async fn fixture(db: &TestDb) -> Result<Fixture> {
    fixture_with(db, Some("owner === user.id")).await
}

/// [`fixture`], with `notes` read through `formula`, or readable by every user
/// when `None`.
async fn fixture_with(db: &TestDb, formula: Option<&str>) -> Result<Fixture> {
    sql(
        db,
        "DO $$ DECLARE r record; BEGIN \
           FOR r IN SELECT table_schema FROM information_schema.tables \
           WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
             EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
           END LOOP; END $$;
         CREATE TABLE notes (id uuid PRIMARY KEY, title text NOT NULL, owner uuid);
         CREATE TABLE items (id uuid PRIMARY KEY, note uuid REFERENCES notes(id) ON DELETE CASCADE,
                             label text, rank integer, done boolean);",
    )
    .await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap(&cat).await?;
    bootstrap_table_meta(&cat).await?;
    bootstrap_field_meta(&cat).await?;
    bootstrap_change_log(&cat).await?;
    cat.reload().await?;

    let synced = |read: u8, formula: Option<&str>| schema_edit::TableSettings {
        min_role_read: Some(read),
        min_role_write: Some(read),
        ownership_formula: formula.map(Into::into),
        sync_enabled: Some(true),
        ..Default::default()
    };
    schema_edit::apply(
        &cat,
        &[
            schema_edit::Operation::AlterTable {
                table: "notes".into(),
                settings: match formula {
                    Some(formula) => synced(1, Some(formula)),
                    None => synced(80, None),
                },
            },
            schema_edit::Operation::AlterTable {
                table: "items".into(),
                settings: synced(80, None),
            },
        ],
        &schema_edit::ApplyOptions::default(),
    )
    .await?;

    sc_auth::save_role(&cat, &sc_auth::Role::new(80, "User")).await?;
    let ann = create_user(&cat, "ann@example.com", "ann-password", 80).await?;
    let api = RestProvider::project("/api", &[cat.require("notes")?, cat.require("items")?])
        .with_application(APP)
        .with_evaluator(Arc::new(DenoEvaluator::new()) as Arc<dyn JsEvaluator>);
    Ok(Fixture {
        cat: Arc::new(cat),
        api,
        ann,
    })
}

async fn sql(db: &TestDb, statements: &str) -> Result<()> {
    db.client()
        .await?
        .batch_execute(statements)
        .await
        .map_err(|e| Error::database(format!("{e:?}")))
}

async fn titles(db: &TestDb) -> Result<Vec<String>> {
    let rows = db
        .client()
        .await?
        .query("SELECT title FROM notes ORDER BY title", &[])
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

/// `%41` and `+` decoded, as a server reads a query string.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 2;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

/// One request from the client, answered by the provider as `ann`.
async fn answer(f: &Fixture, question: &Json) -> Result<Json> {
    let url = question["url"].as_str().unwrap();
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let method = match question["method"].as_str().unwrap() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "PATCH" => Method::Patch,
        "DELETE" => Method::Delete,
        other => panic!("unexpected method {other}"),
    };
    let mut request = ApiRequest::new(method, decode(path)).body(question["body"].clone());
    request.query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect();
    // A refused request is an error status with the reason, as the server
    // answers it over HTTP.
    match f.api.handle(request, &f.cat, Some(&f.ann)).await {
        Ok(response) => Ok(json!({ "status": response.status, "body": response.body })),
        Err(e) => Ok(json!({ "status": 400, "body": { "error": e.to_string() } })),
    }
}

/// Run `driver` in Node beside the generated client and its runtime, answering
/// its `fetch` with the real provider (as `ann`) and its `sql` on the database,
/// until it reports `done`. Returns what it reported.
async fn run_bridged(
    db: &TestDb,
    f: &Fixture,
    tag: &str,
    driver: &str,
    args: &[&str],
) -> Result<Json> {
    let client_ts = sc_api::generate_app_client(ApiProvider::endpoints(&f.api), &[], &[]);
    let dir = std::env::temp_dir().join(format!("sc-api-sync-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("client.ts"), runnable(&client_ts)).unwrap();
    write_runtime(&dir);
    std::fs::write(dir.join("driver.ts"), driver).unwrap();

    let mut node = tokio::process::Command::new("node")
        .arg(dir.join("driver.ts"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = node.stdin.take().unwrap();
    let mut lines = BufReader::new(node.stdout.take().unwrap()).lines();
    let mut result = None;
    while let Some(line) = lines.next_line().await.unwrap() {
        let question: Json = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("the driver said something that is not JSON: {line}"));
        let reply = match question["kind"].as_str() {
            Some("fetch") => answer(f, &question).await?,
            Some("sql") => {
                sql(db, question["sql"].as_str().unwrap()).await?;
                json!({ "status": 200, "body": null })
            }
            Some("done") => {
                result = Some(question["result"].clone());
                break;
            }
            _ => panic!("unexpected question {question}"),
        };
        let mut reply = reply;
        reply["id"] = question["id"].clone();
        stdin
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
    }
    let output = node.wait_with_output().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let Some(r) = result else {
        panic!(
            "the driver stopped before it was done:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    Ok(r)
}

#[tokio::test]
async fn a_device_works_on_its_copy_offline_and_syncs_it() -> Result<()> {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let f = fixture(&db).await?;
    sql(
        &db,
        &format!(
            "INSERT INTO notes (id, title, owner) VALUES ('{SERVER_NOTE}', 'Server note', '{}')",
            f.ann.id
        ),
    )
    .await?;

    let client_ts = sc_api::generate_app_client(ApiProvider::endpoints(&f.api), &[], &[]);
    assert!(
        client_ts.contains("offline.table(\"notes\""),
        "a synced table is kept offline:\n{client_ts}"
    );
    let r = run_bridged(
        &db,
        &f,
        "client",
        DRIVER_TS,
        &[SERVER_NOTE, &f.ann.id.to_string()],
    )
    .await?;

    assert_eq!(r["first"], json!(["Server note"]), "{r:#}");
    // Offline, the copy answers reads, ordered and filtered like the server.
    assert_eq!(
        r["offlineNotes"],
        // Nulls last, as Postgres sorts ascending.
        json!(["Server note, edited offline", "Written offline", null]),
        "{r:#}"
    );
    assert_eq!(
        r["filtered"],
        json!(["Written offline", "Server note, edited offline"]),
        "{r:#}"
    );
    assert_eq!(
        r["embedded"],
        json!([{ "label": "child of an offline note", "parent": { "title": "Written offline" } }]),
        "{r:#}"
    );
    assert_eq!(r["got"], json!("Written offline"), "{r:#}");
    assert_eq!(r["pendingOffline"], json!(4), "{r:#}");
    assert_eq!(r["offlineSync"], json!("TypeError: fetch failed"), "{r:#}");
    assert_eq!(
        r["offlineStatus"],
        json!({ "online": false, "error": true }),
        "{r:#}"
    );

    // Online: the refused insert is gone and reported, the rest is applied, and
    // what the web wrote meanwhile has arrived.
    assert_eq!(
        r["afterSync"],
        json!([
            "Server note, edited offline",
            "Written offline",
            "Written on the web"
        ]),
        "{r:#}"
    );
    let refused = r["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 1, "{r:#}");
    assert_eq!(refused[0]["op"], json!("insert"));
    assert_eq!(refused[0]["key"], json!(true));
    assert!(
        refused[0]["error"].as_str().unwrap().contains("title"),
        "{r:#}"
    );
    assert_eq!(r["pendingAfter"], json!(0), "{r:#}");
    assert_eq!(
        r["status"],
        json!({ "online": true, "error": null }),
        "{r:#}"
    );

    // The second client read the copy from storage and is the same device: it
    // pushed and pulled without registering again.
    assert_eq!(r["reloaded"], r["afterSync"], "{r:#}");
    assert_eq!(r["afterDelete"], json!(2), "{r:#}");
    assert_eq!(r["reloadCalls"], json!(["push", "pull"]), "{r:#}");
    assert_eq!(
        r["finalNotes"],
        json!(["Written offline", "Written on the web"]),
        "{r:#}"
    );

    // And the server agrees.
    assert_eq!(
        titles(&db).await?,
        vec!["Written offline", "Written on the web"]
    );
    let items = db
        .client()
        .await?
        .query("SELECT note::text FROM items", &[])
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].get::<_, String>(0),
        r["offlineNote"].as_str().unwrap()
    );
    Ok(())
}

/// Background sync (S4.2), against a stub server: the store syncs by itself
/// shortly after a local write, when a live stream says a synced table changed,
/// and when the browser says it is online again; `stop` ends all of it.
const AUTO_DRIVER_TS: &str = r#"
import type { SyncTransport } from "./offline-types.ts";
import { memoryStorage } from "./offline-backends.ts";
import { OfflineCopy } from "./offline-copy.ts";

const posts: string[] = [];
const transport: SyncTransport = {
  async post(endpoint, body: any) {
    posts.push(endpoint);
    if (endpoint === "register") return { client_id: "device-1" };
    if (endpoint === "push") return { results: body.changes.map((c: any) => ({ mutation: c.mutation, status: "applied" })) };
    return { watermark: "1:1:", tables: [{ table: "notes", reset: true, rows: [], removed: [] }] };
  },
};
const listeners = new Map<string, () => void>();
(globalThis as any).addEventListener = (type: string, fn: () => void) => listeners.set(type, fn);
(globalThis as any).removeEventListener = (type: string) => listeners.delete(type);

let element: (() => void) | undefined;
let closed = false;
const online = {
  changes: {
    subscribe(_query: object, handlers: { element(): void }) {
      element = handlers.element;
      return { close: () => { closed = true; } };
    },
  },
};
const settle = () => new Promise((resolve) => setTimeout(resolve, 600));
const store = new OfflineCopy(
  { storage: memoryStorage(), interval: 60_000 },
  transport,
  { notes: { key: "id", keyKind: "uuid", columns: { id: "text", title: "text" }, rels: {} } },
);
const notes = store.table("notes", online) as any;
const out: Record<string, unknown> = {};

await settle();
out.atStart = [...posts];
posts.length = 0;
await notes.create({ title: "a" });
await notes.create({ title: "b" });
await settle();
out.afterWrites = [...posts];
posts.length = 0;
element?.();
await settle();
out.afterLiveChange = [...posts];
posts.length = 0;
listeners.get("online")?.();
await settle();
out.afterOnline = [...posts];
store.stop();
out.stopped = { closed, listening: listeners.has("online") };
posts.length = 0;
element?.();
await settle();
out.afterStop = [...posts];
console.log(JSON.stringify(out));
"#;

/// `text` with its imports of the client's files spelled the way Node resolves
/// them: with the `.ts` a bundler adds by itself.
fn runnable(text: &str) -> String {
    [
        "helper",
        "offline-types",
        "offline-backends",
        "offline-copy",
    ]
    .iter()
    .fold(text.to_owned(), |text, name| {
        text.replace(
            &format!("from \"./{name}\""),
            &format!("from \"./{name}.ts\""),
        )
    })
}

/// The files the generated client imports, written into `dir` for Node.
fn write_runtime(dir: &std::path::Path) {
    for (name, text) in sc_api::client_runtime(true) {
        std::fs::write(dir.join(name), runnable(&text)).unwrap();
    }
}

/// Run `driver` beside the generated helper in `node` and read the JSON it
/// prints. `None` when there is no `node` to run it.
fn run_stub_driver(name: &str, driver: &str) -> Option<Json> {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return None;
    }
    let dir = std::env::temp_dir().join(format!("sc-api-sync-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_runtime(&dir);
    std::fs::write(dir.join("driver.ts"), driver).unwrap();
    let output = std::process::Command::new("node")
        .arg(dir.join("driver.ts"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(serde_json::from_slice(&output.stdout).unwrap())
}

#[test]
fn the_copy_syncs_by_itself_until_stopped() {
    let Some(r) = run_stub_driver("auto", AUTO_DRIVER_TS) else {
        return;
    };
    assert_eq!(r["atStart"], json!(["register", "pull"]), "{r:#}");
    // Two writes in quick succession go up in one upload.
    assert_eq!(r["afterWrites"], json!(["push", "pull"]), "{r:#}");
    assert_eq!(r["afterLiveChange"], json!(["pull"]), "{r:#}");
    assert_eq!(r["afterOnline"], json!(["pull"]), "{r:#}");
    assert_eq!(
        r["stopped"],
        json!({ "closed": true, "listening": false }),
        "{r:#}"
    );
    assert_eq!(r["afterStop"], json!([]), "{r:#}");
}

/// A create on an integer-key table goes to the server, which gives out the
/// key: offline it fails with a message that says so, while an error the server
/// answered is passed on as it is.
const INTEGER_KEY_DRIVER_TS: &str = r#"
import type { SyncTransport } from "./offline-types.ts";
import { memoryStorage } from "./offline-backends.ts";
import { OfflineCopy } from "./offline-copy.ts";

const transport: SyncTransport = {
  async post() {
    throw new TypeError("fetch failed");
  },
};
let reachable = false;
const online = {
  async create() {
    if (!reachable) throw new TypeError("fetch failed");
    throw new Error("create failed: 400 value is required");
  },
};
const store = new OfflineCopy(
  { storage: memoryStorage(), auto: false },
  transport,
  { data: { key: "id", keyKind: "number", columns: { id: "number", value: "text" }, rels: {} } },
);
const data = store.table("data", online) as any;
const message = (p: Promise<unknown>) => p.then(() => "created", (e: Error) => e.message);
const out: Record<string, unknown> = {};
out.offline = await message(data.create({ value: "a" }));
reachable = true;
out.refused = await message(data.create({ value: "a" }));
out.pending = store.status().pending;
console.log(JSON.stringify(out));
"#;

#[test]
fn an_integer_key_create_offline_says_why_it_failed() {
    let Some(r) = run_stub_driver("integer-key", INTEGER_KEY_DRIVER_TS) else {
        return;
    };
    assert_eq!(
        r["offline"],
        "Adding to \"data\" while offline is not supported yet (it is planned). Please try again when you are back online.",
        "{r:#}"
    );
    assert_eq!(
        r["refused"], "create failed: 400 value is required",
        "{r:#}"
    );
    assert_eq!(r["pending"], 0, "{r:#}");
}

const NOTE_A: &str = "00000000-0000-4000-8000-00000000000a";
const NOTE_B: &str = "00000000-0000-4000-8000-00000000000b";
const NOTE_HIDDEN: &str = "00000000-0000-4000-8000-00000000000c";

/// Every read twice: once by a plain client against the server, once by an
/// offline client from its copy. The two answers must be the same, and a read
/// one side refuses the other must refuse too.
const PARITY_DRIVER_TS: &str = r#"
import { createClient } from "./client.ts";
import { memoryStorage } from "./offline-backends.ts";
import * as readline from "node:readline";

const [, , noteA] = process.argv;
const waiting = new Map<number, (answer: any) => void>();
readline.createInterface({ input: process.stdin }).on("line", (line) => {
  const answer = JSON.parse(line);
  waiting.get(answer.id)?.(answer);
  waiting.delete(answer.id);
});
let asked = 0;
const ask = (question: object): Promise<any> => {
  const id = ++asked;
  process.stdout.write(JSON.stringify({ ...question, id }) + "\n");
  return new Promise((resolve) => waiting.set(id, resolve));
};
const serverFetch = async (url: any, init: any) => {
  const answer = await ask({
    kind: "fetch",
    method: init?.method ?? "GET",
    url: String(url),
    body: init?.body ? JSON.parse(init.body) : null,
  });
  return new Response(JSON.stringify(answer.body), {
    status: answer.status,
    headers: { "content-type": "application/json" },
  });
};
let offline = false;
const deviceFetch = (url: any, init: any) =>
  offline ? Promise.reject(new TypeError("fetch failed")) : serverFetch(url, init);

const server = createClient({ fetch: serverFetch }) as any;
const device = createClient({ fetch: deviceFetch, offline: { storage: memoryStorage(), auto: false } }) as any;
await device.sync.now();
offline = true;

const cases: Array<[string, Record<string, unknown>]> = [
  ["items", {}],
  ["notes", { order: "title" }],
  ["items", { order: "rank" }],
  ["items", { order: "rank.desc" }],
  ["items", { order: "label" }],
  ["items", { order: "label.desc" }],
  ["items", { order: "done,rank" }],
  ["items", { select: "label,rank", order: "rank" }],
  ["items", { select: "l:label, r:rank", order: "rank" }],
  ["items", { select: "label,note(title)", order: "rank" }],
  ["items", { select: "label,parent:note(title,owner)", order: "rank" }],
  ["items", { select: "label,note(id,title)", filter: { note: "is_null.false" }, order: "rank" }],
  ["items", { filter: { rank: "eq.2" } }],
  ["items", { filter: { rank: "ne.2" }, order: "rank" }],
  ["items", { filter: { rank: "gt.2" }, order: "rank" }],
  ["items", { filter: { rank: "gte.2" }, order: "rank" }],
  ["items", { filter: { rank: "lt.3" }, order: "rank" }],
  ["items", { filter: { rank: "lte.3" }, order: "rank" }],
  ["items", { filter: { rank: "in.(1,3)" }, order: "rank" }],
  ["items", { filter: { rank: "nin.(1,3)" }, order: "rank" }],
  ["items", { filter: { label: "is_null.true" } }],
  ["items", { filter: { label: "like.%an%" }, order: "rank" }],
  ["items", { filter: { label: "ilike.%AN%" }, order: "rank" }],
  ["items", { filter: { label: "like.a_b%" }, order: "rank" }],
  ["items", { filter: { label: "eq.apple" } }],
  ["items", { filter: { note: `eq.${noteA}` }, order: "rank" }],
  // Not `done: "eq.true"`: the server refuses a boolean filter today (its
  // `Bool` coercion takes no text), so there is nothing to compare yet.
  ["items", { filter: { rank: "gte.2", label: "like.%a%" }, order: "rank" }],
  ["items", { order: "rank", limit: 2 }],
  ["items", { order: "rank", limit: 2, offset: 1 }],
  ["items", { order: "rank", offset: 10 }],
  ["notes", { select: "title", filter: { title: "ilike.%A%" }, order: "title.desc" }],
  // Refused on both sides.
  ["items", { select: "nope" }],
  ["items", { select: "label," }],
  ["items", { select: "note(" }],
  ["items", { select: "label(title)" }],
  ["items", { select: "note(nope)" }],
  ["items", { select: "...label" }],
  ["items", { select: "note!inner(title)" }],
  ["items", { select: "label::text" }],
  ["items", { select: "*" }],
  ["items", { select: "notes(title)" }],
  ["items", { filter: { nope: "eq.1" } }],
  ["items", { filter: { rank: "zz.1" } }],
  ["items", { filter: { rank: "eq" } }],
  ["items", { filter: { label: "is_null.maybe" } }],
  ["items", { filter: { rank: "in.1" } }],
  ["items", { order: "nope" }],
];

const sortedText = (rows: unknown[]) =>
  rows.map((r) => JSON.stringify(r)).sort().map((r) => JSON.parse(r));
const run = async (api: any, table: string, query: Record<string, unknown>) => {
  try {
    const rows = await api[table].list(query);
    return { rows: query.order ? rows : sortedText(rows) };
  } catch (e) {
    return { error: e instanceof Error ? e.message : String(e) };
  }
};
const results = [];
for (const [table, query] of cases) {
  results.push({ table, query, server: await run(server, table, query), device: await run(device, table, query) });
}
process.stdout.write(JSON.stringify({ kind: "done", result: results }) + "\n");
process.exit(0);
"#;

#[tokio::test]
async fn the_copy_answers_every_read_like_the_server() -> Result<()> {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    let db = TestDb::new().await?;
    // `notes` readable without a formula: the server does not follow a key into
    // a table read through one, and the copy would (see the TODO).
    let f = fixture_with(&db, None).await?;
    let ann = f.ann.id;
    sql(
        &db,
        &format!(
            "INSERT INTO notes (id, title, owner) VALUES
               ('{NOTE_A}', 'alpha', '{ann}'),
               ('{NOTE_B}', 'beta', '{ann}'),
               ('{NOTE_HIDDEN}', 'hidden', '00000000-0000-4000-8000-0000000000ff');
             INSERT INTO items (id, note, label, rank, done) VALUES
               ('00000000-0000-4000-8000-000000000101', '{NOTE_A}', 'apple', 3, true),
               ('00000000-0000-4000-8000-000000000102', '{NOTE_B}', 'banana', 1, false),
               ('00000000-0000-4000-8000-000000000103', '{NOTE_A}', NULL, 2, NULL),
               ('00000000-0000-4000-8000-000000000104', NULL, 'cherry', NULL, true),
               ('00000000-0000-4000-8000-000000000105', '{NOTE_HIDDEN}', 'date', 5, false),
               ('00000000-0000-4000-8000-000000000106', '{NOTE_B}', 'a_b%c', 4, true);"
        ),
    )
    .await?;

    let r = run_bridged(&db, &f, "parity", PARITY_DRIVER_TS, &[NOTE_A]).await?;
    let mut differences = Vec::new();
    for case in r.as_array().unwrap() {
        let (server, device) = (&case["server"], &case["device"]);
        let same = match (server.get("error"), device.get("error")) {
            (Some(_), Some(_)) => true,
            (None, None) => server["rows"] == device["rows"],
            _ => false,
        };
        if !same {
            differences.push(format!(
                "{} {}\n  server: {server}\n  device: {device}",
                case["table"], case["query"]
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "the copy answered differently from the server:\n{}",
        differences.join("\n")
    );
    Ok(())
}

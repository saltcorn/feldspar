//! The generated client must satisfy the server's CSRF contract (design §16).
//!
//! A browser client is authenticated by its session cookie, so the server refuses
//! any state-changing request that does not echo the non-`HttpOnly` `sc_csrf`
//! cookie in the `x-csrf-token` header — `login` included, it being a `POST` like
//! any other. A client generated from an endpoint set is *the* client of that
//! endpoint set, so it has to carry the header itself; a consumer left to
//! rediscover the convention gets a `403 CSRF token missing or invalid` from a
//! request it never thought of as an API request.
//!
//! This **runs** the generated module rather than reading it: Node executes
//! TypeScript directly (type stripping), so the test stubs `document.cookie` and
//! the global `fetch`, calls two endpoints, and inspects the headers that came
//! out. Asserting on the emitted text would pass just as happily on code that
//! never runs. Skips when `node` is absent, like the `tsc` test beside it.

use std::process::Command;

use sc_api::{ApiProvider, RestProvider};
use sc_catalog::{AccessRules, DataField, DbId, Table, TableSource};
use sc_types::{BasicType, TypeRef};

/// Stub a browser, call a mutating and a safe endpoint, and print what `fetch`
/// was handed.
const DRIVER_TS: &str = r#"
import { createClient } from "./client.ts";

type Call = { url: string; method: string; headers: Record<string, string> };
const calls: Call[] = [];

// A browser holding the cookie the server minted, among others. The value is
// URL-encoded, as a cookie value may be.
(globalThis as any).document = { cookie: "sc_session=opaque; sc_csrf=tok%2F123" };
(globalThis as any).fetch = async (url: string, init: any) => {
  calls.push({ url: String(url), method: init.method, headers: init.headers });
  return new Response("{}", {
    status: 200,
    headers: { "content-type": "application/json" },
  });
};

const api = createClient();
await api.login({ email: "a@b.c", password: "pw" });
await api.posts.list();

console.log(JSON.stringify(calls));
"#;

#[test]
fn the_generated_client_echoes_the_csrf_cookie_on_mutations() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }

    let posts = Table {
        name: "posts".to_owned(),
        database: DbId::primary(),
        source: TableSource::Database,
        fields: vec![
            DataField::plain("id", TypeRef::Basic(BasicType::Int)).primary_key(),
            DataField::plain("title", TypeRef::Basic(BasicType::Text)),
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
    };
    let provider = RestProvider::project("/api", &[posts]);
    let client_ts = sc_api::generate_client(ApiProvider::endpoints(&provider));

    let dir = std::env::temp_dir().join(format!("sc-api-csrf-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), for_node(&client_ts))?;
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

    let calls: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the driver prints the captured calls");
    let login = &calls[0];
    let list = &calls[1];

    // The mutation carries the decoded cookie value in the header the server
    // checks, under the names `sc-api` declares.
    assert_eq!(login["url"], serde_json::json!("/api/login"));
    assert_eq!(
        login["headers"][sc_api::auth::CSRF_HEADER],
        serde_json::json!("tok/123"),
        "login must echo the {} cookie: {calls}",
        sc_api::auth::CSRF_COOKIE
    );
    assert_eq!(
        login["headers"]["content-type"],
        serde_json::json!("application/json")
    );

    // A safe method needs no token, and is not given one: the header is only
    // meaningful on a request that changes something.
    assert_eq!(list["method"], serde_json::json!("GET"));
    assert!(
        list["headers"][sc_api::auth::CSRF_HEADER].is_null(),
        "a GET should carry no CSRF header: {calls}"
    );
    // A browser does not claim to be a native app: its session keeps ending with
    // the browser.
    assert!(
        login["headers"][sc_api::auth::CLIENT_KIND_HEADER].is_null(),
        "{calls}"
    );
    Ok(())
}

#[test]
fn a_client_outside_a_browser_still_works() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }

    // No `document` at all — a node script, a test, an SSR pass. There is no
    // cookie to echo, so the request goes out without the header rather than
    // throwing on a global that isn't there.
    const DRIVER: &str = r#"
import { createClient } from "./client.ts";
let seen: any = null;
(globalThis as any).fetch = async (_url: string, init: any) => {
  seen = init.headers;
  return new Response("{}", { status: 200, headers: { "content-type": "application/json" } });
};
await createClient().login({ email: "a@b.c", password: "pw" });
console.log(JSON.stringify(seen));
"#;

    let provider = RestProvider::project("/api", &[]);
    let client_ts = sc_api::generate_client(ApiProvider::endpoints(&provider));
    let dir = std::env::temp_dir().join(format!("sc-api-csrf-node-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), for_node(&client_ts))?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER)?;

    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "the generated client must run without a browser:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let headers: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(headers[sc_api::auth::CSRF_HEADER].is_null(), "{headers}");
    Ok(())
}

/// A native app — React Native on a phone — has no `document`, and its cookie
/// store is not visible to its JavaScript. The server names the token in every
/// response's `x-csrf-token` header, and the client remembers it: the `whoami` an
/// app makes at startup arms the `login` that follows.
#[test]
fn a_client_with_no_document_echoes_the_token_the_server_named() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    const DRIVER: &str = r#"
import { createClient } from "./client.ts";
const sent: any[] = [];
(globalThis as any).fetch = async (_url: string, init: any) => {
  sent.push(init.headers);
  // Every response names the token, as the server's CSRF middleware does.
  return new Response('{"id":"u","email":"a@b.c","role":1}', {
    status: 200,
    headers: { "content-type": "application/json", "x-csrf-token": "native-tok" },
  });
};
const api = createClient({ baseUrl: "http://todo.10.0.2.2.nip.io:3032" });
await api.whoami();
await api.login({ email: "a@b.c", password: "pw" });
console.log(JSON.stringify(sent));
"#;

    let provider = RestProvider::project("/api", &[]);
    let client_ts = sc_api::generate_client(ApiProvider::endpoints(&provider));
    let dir = std::env::temp_dir().join(format!("sc-api-csrf-native-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), for_node(&client_ts))?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER)?;

    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "running the generated client failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sent: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // The first request had nothing to echo; the login echoes what it was told.
    assert!(sent[0][sc_api::auth::CSRF_HEADER].is_null(), "{sent}");
    // And every request says it is a native app, so the login lasts.
    for request in sent.as_array().unwrap() {
        assert_eq!(
            request[sc_api::auth::CLIENT_KIND_HEADER],
            serde_json::json!(sc_api::auth::NATIVE_CLIENT),
            "{sent}"
        );
    }
    assert_eq!(
        sent[1][sc_api::auth::CSRF_HEADER],
        serde_json::json!("native-tok"),
        "{sent}"
    );
    Ok(())
}

/// The generated client, with its one relative import made resolvable by Node.
///
/// A TypeScript project must import `"./helper"`: an explicit `.ts` extension is
/// an error unless `allowImportingTsExtensions` is on, and no consumer should be
/// made to turn that on. Node, running the same file through its own type
/// stripping, resolves no extension at all. The generated text is the one every
/// project gets; this rewrite is the only difference between what is executed
/// here and what is shipped.
fn for_node(client_ts: &str) -> String {
    client_ts.replace("from \"./helper\"", "from \"./helper.ts\"")
}

/// A native app reopened while still signed in has its session cookie but not
/// the token it remembered. Its first write is refused `403`, naming a fresh
/// token, and the client sends that write once more with it — once, so a server
/// that keeps refusing is reported rather than retried forever.
#[test]
fn a_write_refused_for_a_stale_token_is_retried_once_with_the_named_one() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    const DRIVER: &str = r#"
import { createClient } from "./client.ts";
const sent: string[] = [];
let refuse = 1;
(globalThis as any).fetch = async (_url: string, init: any) => {
  sent.push(new Headers(init.headers).get("x-csrf-token") ?? "none");
  const status = refuse-- > 0 ? 403 : 200;
  return new Response('{"id":"u","email":"a@b.c","role":1}', {
    status,
    headers: { "content-type": "application/json", "x-csrf-token": "fresh" },
  });
};
const api = createClient();
await api.login({ email: "a@b.c", password: "pw" });
// A server that refuses even the named token is answered once, not looped on.
refuse = 5;
let failed = false;
try { await api.login({ email: "a@b.c", password: "pw" }); } catch { failed = true; }
console.log(JSON.stringify({ sent, failed }));
"#;

    let provider = RestProvider::project("/api", &[]);
    let client_ts = sc_api::generate_client(ApiProvider::endpoints(&provider));
    let dir = std::env::temp_dir().join(format!("sc-api-csrf-retry-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("client.ts"), for_node(&client_ts))?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER)?;

    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "running the generated client failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // First login: refused without a token, retried with the one it was given.
    // Second: sent with that token, refused, and not retried.
    assert_eq!(
        out["sent"],
        serde_json::json!(["none", "fresh", "fresh"]),
        "{out}"
    );
    assert_eq!(out["failed"], serde_json::json!(true), "{out}");
    Ok(())
}

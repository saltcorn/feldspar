//! A code query's generated method always takes its arguments (TODO F1.1).
//!
//! A JavaScript or Python custom query reads `body` and `query` whole, declared
//! or not, so a code query that declares no parameters must still have a
//! client method that sends what the caller passes. Before, an empty input
//! struct counted as "no body" and the method dropped it, and agents wrote
//! `fetch` by hand around the generated client.
//!
//! This **runs** the generated module under Node with a stubbed `fetch`, like
//! `generated_client_csrf.rs`. Skips when `node` is absent.

use std::process::Command;

use sc_api::{
    ApiProvider, CustomParam, CustomQuery, Method, QueryLanguage, RestProvider, ValueType,
};

/// Four code queries: a POST and a GET with nothing declared, and a POST and a
/// GET with one declared parameter each.
fn provider() -> RestProvider {
    let js = |name: &str, method: Method, path: &str| {
        CustomQuery::new(name, method, path, "return body;").language(QueryLanguage::Javascript)
    };
    RestProvider::project("/api", &[])
        .with_queries(vec![
            js("echo", Method::Post, "/echo"),
            js("peek", Method::Get, "/peek"),
            js("tagged", Method::Post, "/tagged")
                .params([CustomParam::new("tag", ValueType::Text)]),
            js("lookup", Method::Get, "/lookup")
                .params([CustomParam::new("q", ValueType::Text).optional()]),
        ])
        .expect("the queries are valid")
}

const DRIVER_TS: &str = r#"
import { createClient } from "./client.ts";

type Call = { url: string; method: string; body: string | null };
const calls: Call[] = [];
(globalThis as any).fetch = async (url: string, init: any) => {
  calls.push({ url: String(url), method: init.method, body: init.body ?? null });
  return new Response("{}", { status: 200, headers: { "content-type": "application/json" } });
};

const api = createClient();
await api.echo({ anything: 1, nested: { b: "é" } });
await api.echo();
await api.peek({ page: 2, q: "rock & roll", skip: null });
await api.peek();
await api.tagged({ tag: "x", extra: true });
await api.lookup({ q: "a", more: "b" });
console.log(JSON.stringify(calls));
"#;

#[test]
fn a_code_query_with_no_params_sends_its_body_and_the_query_sees_it() -> std::io::Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping: no `node` on PATH to run the generated client");
        return Ok(());
    }
    let provider = provider();
    let client_ts = sc_api::generate_client(ApiProvider::endpoints(&provider));

    let dir = std::env::temp_dir().join(format!("sc-api-code-query-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("client.ts"),
        client_ts.replace("from \"./helper\"", "from \"./helper.ts\""),
    )?;
    std::fs::write(
        dir.join(sc_api::CLIENT_HELPER_FILE),
        sc_api::client_helper(),
    )?;
    std::fs::write(dir.join("driver.ts"), DRIVER_TS)?;
    let output = Command::new("node").arg(dir.join("driver.ts")).output()?;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "running the generated client failed:\n{}\n--- client.ts ---\n{client_ts}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_str(calls[i]["body"].as_str().expect("a JSON body")).unwrap()
    };

    assert_eq!(calls[0]["url"], "/api/echo");
    assert_eq!(
        body(0),
        serde_json::json!({ "anything": 1, "nested": { "b": "é" } })
    );
    assert_eq!(
        body(1),
        serde_json::json!({}),
        "an omitted body is sent as {{}}"
    );
    assert_eq!(
        calls[2]["url"], "/api/peek?page=2&q=rock+%26+roll",
        "every set key is a query parameter; null is left out"
    );
    assert!(calls[2]["body"].is_null(), "a GET carries no body");
    assert_eq!(calls[3]["url"], "/api/peek");
    assert_eq!(body(4), serde_json::json!({ "tag": "x", "extra": true }));
    assert_eq!(calls[5]["url"], "/api/lookup?q=a&more=b");

    // What the server makes of it — that the code's `body` is this object —
    // is `custom.rs`'s `a_code_query_with_no_params_sees_the_whole_body`.
    Ok(())
}

#[test]
fn the_generated_signatures_take_open_arguments() {
    let client_ts = sc_api::generate_client(ApiProvider::endpoints(&provider()));
    for expected in [
        "export type EchoRequest = Record<string, unknown>;",
        "export type PeekQuery = Record<string, unknown>;",
        "export type TaggedRequest = { tag: string } & Record<string, unknown>;",
        "export type LookupQuery = { q?: string } & Record<string, unknown>;",
        "echo(body?: EchoRequest): Promise<EchoResponse>",
        "peek(query?: PeekQuery): Promise<PeekResponse>",
        "tagged(body: TaggedRequest): Promise<TaggedResponse>",
        "lookup(query?: LookupQuery): Promise<LookupResponse>",
    ] {
        assert!(
            client_ts.contains(expected),
            "missing `{expected}` in:\n{client_ts}"
        );
    }
}

#[test]
fn a_save_of_a_code_query_with_no_params_carries_a_note() {
    let bare = CustomQuery::new("echo", Method::Post, "/echo", "return body;")
        .language(QueryLanguage::Javascript);
    let note = sc_api::undeclared_params_note(&bare).expect("a note");
    assert!(note.contains("`echo`") && note.contains("body"), "{note}");

    let declared = bare
        .clone()
        .params([CustomParam::new("tag", ValueType::Text)]);
    assert!(sc_api::undeclared_params_note(&declared).is_none());
    let sql = CustomQuery::new("n", Method::Get, "/n", "SELECT 1 AS n");
    assert!(
        sc_api::undeclared_params_note(&sql).is_none(),
        "SQL declares what it uses"
    );
}

//! The traits that reach outside the server (TODO-agent-skills AS9), each
//! against a local stand-in, so nothing here touches the network:
//!
//! - `mcp_server`: a stub MCP server's tools offered as `<server>__<tool>`,
//!   narrowed by the allow-list, withheld and refused in `plan` unless marked
//!   read-only, called with text and images back, an `isError` result read as
//!   an error — and, in a whole run, nothing recorded for the server's cost;
//! - `web_search`: a URL template with its header, Tavily and Firecrawl, each
//!   answered as title, URL and snippet, with the domain lists applied;
//! - `http` `cookies`: a jar kept in the run state across requests and
//!   redirects, sent back only to the host that set each cookie;
//! - `generate_js`: model-written code run with its grants as the caller,
//!   with the follow-up prompt handed back and the source ledgered;
//! - `subagent`: the handover prompt before the briefing, the handoff prompt
//!   after the report.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::common::{Env, as_user, config};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{
    Agent, AgentRegistry, AgentTrait, EnabledTrait, ProviderConnector, RunMode, Runner,
    ToolsContext, save_agent,
};
use sc_catalog::Catalog;
use sc_core_traits::{
    ARG_TASK, CFG_AGENT, CFG_ALLOW_FETCH, CFG_ALLOWED_TOOLS, CFG_API_KEY, CFG_EXCLUDE_DOMAINS,
    CFG_FIELDS, CFG_FOLLOW_UP_PROMPT, CFG_GEN_TOOL_NAME, CFG_HANDOFF_PROMPT, CFG_HANDOVER_PROMPT,
    CFG_HEADER, CFG_INCLUDE_DOMAINS, CFG_PROVIDER, CFG_READ_ONLY_TOOLS, CFG_SEARCH_PRIVATE_NETWORK,
    CFG_SERVER, CFG_TABLE, CFG_TABLE_ACCESS, CFG_URL_TEMPLATE, WebSearch, tool_names,
};
use sc_error::{Error, Result};
use sc_llm::{ConnectedModel, LlmMessage, LlmProvider};
use sc_types::Attrs;
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// --- a local server ----------------------------------------------------------

/// One request a stub saw.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

impl Seen {
    fn json(&self) -> Json {
        serde_json::from_str(&self.body).unwrap_or(Json::Null)
    }
}

type Route = Arc<dyn Fn(&Seen) -> (String, String, String) + Send + Sync>;

/// A loopback server answering every request through `route`, which returns
/// the status line, extra header lines and the body; every request is kept.
async fn serve(route: Route) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            let route = Arc::clone(&route);
            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else {
                    return;
                };
                let (status, extra, body) = route(&request);
                log.lock().unwrap().push(request);
                let reply = format!(
                    "HTTP/1.1 {status}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

async fn read_request(socket: &mut TcpStream) -> Option<Seen> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let path = first.next()?.to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(Seen {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&buf[head_end..]).into_owned(),
    })
}

fn ok_json(body: Json) -> (String, String, String) {
    (
        "200 OK".to_owned(),
        "content-type: application/json\r\n".to_owned(),
        body.to_string(),
    )
}

/// Call `trait_`'s tool `tool` as Ada, in `mode`, over the trait state
/// `state`; the result, and the images the call attached.
async fn call(
    env: &Env,
    trait_: &Arc<dyn AgentTrait>,
    config: &Attrs,
    tool: &str,
    args: Json,
    mode: RunMode,
    state: &mut Json,
) -> (Result<Json>, usize) {
    let caller = as_user("ada@example.com");
    let mut ctx = env.context(&caller, state);
    ctx.mode = mode;
    let result = trait_.call(config, tool, &args, &mut ctx).await;
    (result, ctx.images.len())
}

fn tools_in(env: &Env, trait_: &Arc<dyn AgentTrait>, config: &Attrs, mode: RunMode) -> Vec<String> {
    let caps = sc_llm::ModelCapabilities::built_in("", "");
    let cx = ToolsContext::new(&env.catalog, mode, &caps);
    trait_
        .tools(&cx, config)
        .into_iter()
        .map(|t| t.name)
        .collect()
}

// --- mcp_server --------------------------------------------------------------

/// A streamable-HTTP MCP server answering in plain JSON, with a read-only
/// search, a destructive delete and a tool that fails.
async fn mcp_stub() -> (String, Arc<Mutex<Vec<Seen>>>) {
    serve(Arc::new(|request: &Seen| {
        if request.method != "POST" {
            return ("200 OK".into(), String::new(), String::new());
        }
        let message = request.json();
        let id = message["id"].clone();
        if id.is_null() {
            return ("202 Accepted".into(), String::new(), String::new());
        }
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "stub", "version": "1" },
            }),
            "tools/list" => json!({ "tools": [
                { "name": "search", "description": "Search the docs",
                  "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } } },
                  "annotations": { "readOnlyHint": true } },
                { "name": "delete_all", "description": "Delete every document",
                  "inputSchema": { "type": "object" } },
                { "name": "broken", "description": "Always fails",
                  "inputSchema": { "type": "object" } },
            ]}),
            "tools/call" => match message["params"]["name"].as_str().unwrap_or_default() {
                "search" => json!({ "content": [
                    { "type": "text", "text": format!("Found: {}", message["params"]["arguments"]["q"]) },
                    { "type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo=" },
                ]}),
                "delete_all" => json!({ "content": [{ "type": "text", "text": "Deleted." }] }),
                _ => json!({ "content": [{ "type": "text", "text": "the index is offline" }],
                             "isError": true }),
            },
            other => {
                return ok_json(json!({ "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": format!("no {other}") } }));
            }
        };
        ok_json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }))
    .await
}

/// An env with the MCP servers table and a server called `docs` at `url`.
async fn mcp_env(url: &str) -> Result<Env> {
    let env = Env::new().await?;
    sc_mcp::bootstrap_mcp_servers(&env.catalog).await?;
    let def = sc_mcp::McpServerDef::new("docs", format!("{url}/mcp")).private_network(true);
    sc_mcp::save_mcp_server(&env.catalog, &def).await?;
    Ok(env)
}

#[tokio::test]
async fn mcp_server_offers_the_remote_tools_by_server_and_name() -> Result<()> {
    let (url, _) = mcp_stub().await;
    let env = mcp_env(&url).await?;
    let trait_ = env.registry.require("mcp_server")?.clone();

    // An unknown server is refused on save, naming the ones there are.
    let err = env
        .check("mcp_server", &config(&[(CFG_SERVER, json!("nope"))]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("`docs`"), "{err}");

    // Saving reads the listing, so the tools are known straight away.
    let all = config(&[(CFG_SERVER, json!("docs"))]);
    env.check("mcp_server", &all).await?;
    assert_eq!(
        tools_in(&env, &trait_, &all, RunMode::Act),
        ["docs__search", "docs__delete_all", "docs__broken"]
    );
    assert_eq!(tool_names::mcp_server("docs", "search"), "docs__search");

    // The allow-list narrows them.
    let some = config(&[
        (CFG_SERVER, json!("docs")),
        (CFG_ALLOWED_TOOLS, json!("search\ndelete_all")),
    ]);
    assert_eq!(
        tools_in(&env, &trait_, &some, RunMode::Act),
        ["docs__search", "docs__delete_all"]
    );
    let mut state = Json::Null;
    let (refused, _) = call(
        &env,
        &trait_,
        &some,
        "docs__broken",
        json!({}),
        RunMode::Act,
        &mut state,
    )
    .await;
    let refused = refused.unwrap_err().to_string();
    assert!(refused.contains("no tool `docs__broken`"), "{refused}");

    // Planning: only what is marked read-only, by the server or the admin.
    assert_eq!(
        tools_in(&env, &trait_, &some, RunMode::Plan),
        ["docs__search"]
    );
    let (refused, _) = call(
        &env,
        &trait_,
        &some,
        "docs__delete_all",
        json!({}),
        RunMode::Plan,
        &mut state,
    )
    .await;
    let refused = refused.unwrap_err().to_string();
    assert!(
        refused.contains("`docs__delete_all` is not marked read-only"),
        "{refused}"
    );
    let marked = config(&[
        (CFG_SERVER, json!("docs")),
        (CFG_READ_ONLY_TOOLS, json!("delete_all")),
    ]);
    assert!(
        tools_in(&env, &trait_, &marked, RunMode::Explore).contains(&"docs__delete_all".to_owned())
    );

    // A call: text back, the image attached to the result.
    let (found, images) = call(
        &env,
        &trait_,
        &all,
        "docs__search",
        json!({"q": "retries"}),
        RunMode::Act,
        &mut state,
    )
    .await;
    assert_eq!(found?, json!("Found: \"retries\""));
    assert_eq!(images, 1);
    // An `isError` result is an error the model reads.
    let (broken, _) = call(
        &env,
        &trait_,
        &all,
        "docs__broken",
        json!({}),
        RunMode::Act,
        &mut state,
    )
    .await;
    let broken = broken.unwrap_err().to_string();
    assert!(broken.contains("the index is offline"), "{broken}");
    Ok(())
}

#[tokio::test]
async fn mcp_server_tools_are_called_in_a_run_and_cost_nothing_extra() -> Result<()> {
    let (url, seen) = mcp_stub().await;
    let env = mcp_env(&url).await?;
    let agent = Agent::new("librarian", "main")
        .system_prompt("You answer from the docs.")
        .with_trait(EnabledTrait::new("mcp_server").config(CFG_SERVER, "docs"));
    save_agent(&env.catalog, &env.registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::calls("docs__search", json!({"q": "retries"})),
        Reply::says("Retries are documented."),
    ]));
    let runner = Runner::new(
        &env.catalog,
        &env.registry,
        &agent,
        ConnectedModel::unconfigured(provider.clone()),
        as_user("ada@example.com"),
    );
    let (run, conclusion) = runner.start("how do retries work?").await?;
    assert_eq!(conclusion.answer(), Some("Retries are documented."));
    assert!(
        provider.requests()[0]
            .tools
            .iter()
            .any(|t| t.name == "docs__search"),
        "the remote tool is offered"
    );
    let calls = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.json()["method"] == json!("tools/call"))
        .count();
    assert_eq!(calls, 1);

    // The run's ledger holds its two model calls and nothing for the server.
    let state = sc_agent::load_run(&env.catalog, run.id)
        .await?
        .expect("the run row")
        .agent_loop()?;
    assert_eq!(state.ledger().steps().len(), 2);
    let result = state
        .messages()
        .iter()
        .find_map(|m| match m {
            LlmMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("a tool result");
    assert!(result.contains("Found: \\\"retries\\\"") || result.contains("Found: \"retries\""));
    Ok(())
}

// --- web_search --------------------------------------------------------------

#[tokio::test]
async fn web_search_answers_with_titles_urls_and_snippets_from_each_provider() -> Result<()> {
    let env = Env::new().await?;
    let (base, seen) = serve(Arc::new(|request: &Seen| {
        let results = json!([
            { "title": "Serde", "url": "https://docs.rs/serde", "content": "Serialization  framework" },
            { "title": "Spam", "url": "https://spam.example/serde", "content": "buy now" },
        ]);
        match request.path.split('?').next().unwrap_or_default() {
            "/brave" => ok_json(json!({ "web": { "results": [
                { "title": "Serde", "url": "https://docs.rs/serde", "description": "Ser" },
                { "title": "Elsewhere", "url": "https://example.com/serde", "description": "x" },
            ]}})),
            "/tavily" => ok_json(json!({ "results": results })),
            "/firecrawl" => ok_json(json!({ "success": true, "data": { "web": results } })),
            _ => (
                "200 OK".into(),
                "content-type: text/html\r\n".into(),
                "<html><body><main><h1>Results</h1><p>One page of hits.</p></main></body></html>"
                    .into(),
            ),
        }
    }))
    .await;
    let trait_: Arc<dyn AgentTrait> = Arc::new(WebSearch::with_endpoints(
        &format!("{base}/tavily"),
        &format!("{base}/firecrawl"),
    )?);
    let mut state = Json::Null;

    // A URL template: the query encoded into it, with the operators the
    // domain list adds, and the header sent.
    let template = config(&[
        (CFG_URL_TEMPLATE, json!(format!("{base}/brave?q={{{{q}}}}"))),
        (CFG_HEADER, json!("X-Subscription-Token: brave-key")),
        (CFG_SEARCH_PRIVATE_NETWORK, json!(true)),
        (CFG_INCLUDE_DOMAINS, json!("docs.rs")),
    ]);
    let tool = tools_in(&env, &trait_, &template, RunMode::Act)[0].clone();
    assert_eq!(tool, "web_search");
    let (found, _) = call(
        &env,
        &trait_,
        &template,
        &tool,
        json!({"query": "serde json"}),
        RunMode::Act,
        &mut state,
    )
    .await;
    assert_eq!(
        found?,
        json!({ "query": "serde json", "results": [
            { "title": "Serde", "url": "https://docs.rs/serde", "snippet": "Ser" },
        ]})
    );
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.path, "/brave?q=serde%20json%20%28site%3Adocs.rs%29");
    assert_eq!(last.headers["x-subscription-token"], "brave-key");

    // Without the private network the template's loopback host is refused.
    let mut closed = template.clone();
    closed.remove(CFG_SEARCH_PRIVATE_NETWORK);
    let (refused, _) = call(
        &env,
        &trait_,
        &closed,
        &tool,
        json!({"query": "x"}),
        RunMode::Act,
        &mut state,
    )
    .await;
    assert!(refused.is_err());

    // An answer that is not a result list is shown as a page.
    let page = config(&[
        (CFG_URL_TEMPLATE, json!(format!("{base}/html?q={{{{q}}}}"))),
        (CFG_SEARCH_PRIVATE_NETWORK, json!(true)),
    ]);
    let (text, _) = call(
        &env,
        &trait_,
        &page,
        &tool,
        json!({"query": "x"}),
        RunMode::Act,
        &mut state,
    )
    .await;
    assert!(text?.as_str().unwrap().contains("One page of hits"));

    // Tavily: the key as a bearer token, the domain lists as parameters, and
    // the excluded domain filtered out of what came back regardless.
    let tavily = config(&[
        (CFG_PROVIDER, json!("tavily")),
        (CFG_API_KEY, json!("tvly-key")),
        (CFG_EXCLUDE_DOMAINS, json!("spam.example")),
    ]);
    let (found, _) = call(
        &env,
        &trait_,
        &tavily,
        &tool,
        json!({"query": "serde"}),
        RunMode::Act,
        &mut state,
    )
    .await;
    let found = found?;
    assert_eq!(found["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        found["results"][0]["snippet"],
        json!("Serialization framework")
    );
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.path, "/tavily");
    assert_eq!(last.headers["authorization"], "Bearer tvly-key");
    assert_eq!(last.json()["exclude_domains"], json!(["spam.example"]));

    // Firecrawl: the domain list as operators in the query.
    let firecrawl = config(&[
        (CFG_PROVIDER, json!("firecrawl")),
        (CFG_API_KEY, json!("fc-key")),
        (CFG_EXCLUDE_DOMAINS, json!("spam.example")),
    ]);
    let (found, _) = call(
        &env,
        &trait_,
        &firecrawl,
        &tool,
        json!({"query": "serde"}),
        RunMode::Act,
        &mut state,
    )
    .await;
    assert_eq!(found?["results"][0]["url"], json!("https://docs.rs/serde"));
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.json()["query"], json!("serde -site:spam.example"));
    assert_eq!(last.headers["authorization"], "Bearer fc-key");
    Ok(())
}

// --- http cookies ------------------------------------------------------------

#[tokio::test]
async fn http_cookies_are_kept_for_the_run_and_sent_only_to_their_host() -> Result<()> {
    let env = Env::new().await?;
    let (base, seen) = serve(Arc::new(|request: &Seen| {
        match request.path.as_str() {
            // A login that sets a session and redirects to the page behind it.
            "/login" => (
                "302 Found".into(),
                "set-cookie: sid=s3cret; Path=/; HttpOnly\r\nlocation: /home\r\n".into(),
                String::new(),
            ),
            _ => {
                let cookie = request.headers.get("cookie").cloned().unwrap_or_default();
                (
                    "200 OK".into(),
                    "content-type: text/plain\r\n".into(),
                    format!("cookie=[{cookie}]"),
                )
            }
        }
    }))
    .await;
    let trait_ = env.registry.require("http")?.clone();
    let with_jar = config(&[
        (sc_core_traits::http::CFG_PRIVATE_NETWORK, json!(true)),
        (sc_core_traits::http::CFG_COOKIES, json!(true)),
    ]);
    let tool = tools_in(&env, &trait_, &with_jar, RunMode::Act)[0].clone();
    let mut state = Json::Null;
    // The redirect after the login already carries the cookie it set.
    let home = fetch_text(
        &env,
        &trait_,
        &tool,
        format!("{base}/login"),
        with_jar.clone(),
        &mut state,
    )
    .await?;
    assert!(home.contains("cookie=[sid=s3cret]"), "{home}");
    // The jar is in the run state, and the next request sends it.
    assert!(state["cookies"].is_object(), "{state}");
    let page = fetch_text(
        &env,
        &trait_,
        &tool,
        format!("{base}/account"),
        with_jar.clone(),
        &mut state,
    )
    .await?;
    assert!(page.contains("cookie=[sid=s3cret]"), "{page}");
    // Another host on the same machine is not sent it.
    let other = base.replace("127.0.0.1", "localhost");
    let elsewhere = fetch_text(
        &env,
        &trait_,
        &tool,
        format!("{other}/account"),
        with_jar.clone(),
        &mut state,
    )
    .await?;
    assert!(elsewhere.contains("cookie=[]"), "{elsewhere}");
    // An instance without `cookies` keeps nothing and sends nothing.
    let mut without = with_jar.clone();
    without.remove(sc_core_traits::http::CFG_COOKIES);
    let mut fresh = Json::Null;
    let plain = fetch_text(
        &env,
        &trait_,
        &tool,
        format!("{base}/account"),
        without,
        &mut fresh,
    )
    .await?;
    assert!(plain.contains("cookie=[]"), "{plain}");
    assert!(fresh.is_null());
    assert!(seen.lock().unwrap().len() >= 5);
    Ok(())
}

/// The text `http`'s tool answers `url` with, over the run state `state`.
async fn fetch_text(
    env: &Env,
    trait_: &Arc<dyn AgentTrait>,
    tool: &str,
    url: String,
    config: Attrs,
    state: &mut Json,
) -> Result<String> {
    let (result, _) = call(
        env,
        trait_,
        &config,
        tool,
        json!({ "url": url }),
        RunMode::Act,
        state,
    )
    .await;
    Ok(result?.as_str().unwrap_or_default().to_owned())
}

// --- generate_js -------------------------------------------------------------

#[tokio::test]
async fn generate_js_runs_the_models_code_with_its_grants_as_the_caller() -> Result<()> {
    let env = Env::new().await?.with_engine();
    env.own("books", "owner === user.email").await?;
    let trait_ = env.registry.require("generate_js")?.clone();
    let cfg = config(&[
        (CFG_GEN_TOOL_NAME, json!("compute")),
        (CFG_TABLE_ACCESS, json!("read")),
        (CFG_FOLLOW_UP_PROMPT, json!("Now explain the number.")),
    ]);
    env.check("generate_js", &cfg).await?;
    let tools = env.tools("generate_js", &cfg);
    assert_eq!(tools[0].name, "compute");
    assert!(
        tools[0].description.contains("read-only"),
        "{}",
        tools[0].description
    );
    // A table that does not exist is refused on save.
    let err = env
        .check(
            "generate_js",
            &config(&[
                (CFG_GEN_TOOL_NAME, json!("compute")),
                (CFG_TABLE_ACCESS, json!("read")),
                (sc_core_traits::CFG_TABLES, json!("nope")),
            ]),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no table `nope`"), "{err}");

    // Ada's own books, read as Ada, with the follow-up handed back.
    let caller = as_user("ada@example.com");
    let mut state = Json::Null;
    let mut ctx = env.context(&caller, &mut state);
    let out = trait_
        .call(
            &cfg,
            "compute",
            &json!({"code": "```js\nconst rows = await db.books.orderBy('id').rows();\nreturn rows.map(r => r.title)\n```"}),
            &mut ctx,
        )
        .await?;
    assert_eq!(
        out,
        json!({ "result": ["Dune", "Ilium"], "next": "Now explain the number." })
    );
    // A write under a read grant, and fetch without one, are refused.
    let err = trait_
        .call(
            &cfg,
            "compute",
            &json!({"code": "await db.books.insert({ id: 9, title: 'X' }); return 1"}),
            &mut ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("read-only"), "{err}");
    let err = trait_
        .call(
            &cfg,
            "compute",
            &json!({"code": "return (await fetch('https://example.com')).status"}),
            &mut ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("fetch"), "{err}");
    // Every evaluation is in the ledger, with its source.
    assert_eq!(ctx.extras.code.len(), 3);
    assert_eq!(ctx.extras.code[0].tool, "compute");
    assert!(ctx.extras.code[0].source.starts_with("const rows"));
    assert!(ctx.extras.code[0].ok && !ctx.extras.code[1].ok);

    // With fetch granted it is in scope (and still under the guard: a
    // loopback address is refused, not reached).
    let fetching = config(&[
        (CFG_GEN_TOOL_NAME, json!("compute")),
        (CFG_ALLOW_FETCH, json!(true)),
    ]);
    let err = trait_
        .call(
            &fetching,
            "compute",
            &json!({"code": "return (await fetch('http://127.0.0.1:9/')).status"}),
            &mut ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(!err.contains("ReferenceError"), "{err}");
    Ok(())
}

// --- subagent ----------------------------------------------------------------

/// A connector handing each agent its own script.
struct Scripts(Mutex<BTreeMap<String, Arc<FakeProvider>>>);

#[async_trait::async_trait]
impl ProviderConnector for Scripts {
    async fn connect(
        &self,
        _catalog: &Catalog,
        agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> Result<ConnectedModel> {
        match self.0.lock().unwrap().get(&agent.name) {
            Some(p) => Ok(ConnectedModel::unconfigured(
                Arc::clone(p) as Arc<dyn LlmProvider>
            )),
            None => Err(Error::config(format!("no script for `{}`", agent.name))),
        }
    }
}

#[tokio::test]
async fn subagent_puts_the_handover_before_the_briefing_and_the_handoff_after_the_report()
-> Result<()> {
    let env = Env::new().await?.with_engine();
    let researcher = Agent::new("researcher", "main")
        .system_prompt("You look things up.")
        .with_trait(
            EnabledTrait::new("query_table")
                .config(CFG_TABLE, "books")
                .config(CFG_FIELDS, json!(["id", "title"])),
        );
    let librarian = Agent::new("librarian", "main")
        .system_prompt("You answer questions.")
        .with_trait(
            EnabledTrait::new("subagent")
                .config(CFG_AGENT, "researcher")
                .config(CFG_HANDOVER_PROMPT, "Answer in one line, citing ids.")
                .config(CFG_HANDOFF_PROMPT, "Check the ids before you answer."),
        );
    let registry: &AgentRegistry = &env.registry;
    save_agent(&env.catalog, registry, &researcher).await?;
    save_agent(&env.catalog, registry, &librarian).await?;

    let parent = Arc::new(FakeProvider::new([
        Reply::calls(
            tool_names::subagent("researcher"),
            json!({ ARG_TASK: "Count the books" }),
        ),
        Reply::says("There are four."),
    ]));
    let child = Arc::new(FakeProvider::new([Reply::says("4 books (ids 1-4).")]));
    let scripts = Arc::new(Scripts(Mutex::new(BTreeMap::from([
        ("librarian".to_owned(), Arc::clone(&parent)),
        ("researcher".to_owned(), Arc::clone(&child)),
    ]))));
    let connector = scripts as Arc<dyn ProviderConnector>;
    let executor = connector
        .connect(&env.catalog, &librarian, sc_agent::ModelRole::Executor)
        .await?;
    let caller = sc_agent::RunCaller::system();
    let runner = Runner::new(&env.catalog, registry, &librarian, executor, caller)
        .with_connector(&connector)
        .with_evaluator(env.evaluator.as_ref().unwrap());
    let (run, conclusion) = runner.start("how many books?").await?;
    assert_eq!(conclusion.answer(), Some("There are four."));

    // The sub-agent's first message: the handover, then the briefing.
    let briefing = format!("{:?}", child.requests()[0].messages);
    let handover = briefing.find("Answer in one line").expect("the handover");
    let task = briefing.find("Count the books").expect("the task");
    assert!(handover < task, "{briefing}");

    // The parent's tool result: the answer verbatim, then the handoff.
    let result = run
        .agent_loop()?
        .messages()
        .iter()
        .find_map(|m| match m {
            LlmMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("the delegation's result");
    let report: Json = serde_json::from_str(&result).unwrap();
    assert_eq!(report["answer"], json!("4 books (ids 1-4)."));
    assert_eq!(report["handoff"], json!("Check the ids before you answer."));
    Ok(())
}

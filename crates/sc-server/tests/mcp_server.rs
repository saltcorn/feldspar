//! Phase 4 integration test: the administration MCP server (design §13.6).
//!
//! Driven through the assembled router against a real Postgres database, the
//! way an external coding agent drives it — a bearer token, a JSON-RPC body, no
//! cookie anywhere.
//!
//! What is asserted here and cannot be asserted anywhere else, because each one
//! spans the route, the credential and the tool set:
//!
//! 1. **The four gates come in the right order.** A disabled server is a `404`
//!    that never reads the token table; an `Origin` header is a refusal; a
//!    non-local peer is refused while `mcp_loopback_only` is on; and a session
//!    cookie with a valid CSRF token — and no bearer — gets nowhere.
//! 2. **A token's six flags are the tool list.** `allow_triggers` off takes the
//!    trigger tools out of `tools/list` altogether, both the hand-written four
//!    and the tagged endpoints in that half.
//! 3. **A refusal is a result, not a transport error.** An ungranted operation
//!    comes back as `isError: true` with the sentence the model reads; an unknown
//!    *tool* is a JSON-RPC error, because there is nothing there for a model to
//!    correct.
//! 4. **A tier-2 tool really dispatches through the handler registry**, as the
//!    token's user, and comes back with what the endpoint returns.
//! 5. **The reload/rebuild loop closes** (Phase 5): an `edit_schema` batch made
//!    over MCP re-projects the mounted applications that serve the tables it
//!    touched, rewrites their generated clients on disk, and *names them in the
//!    result* — with the ones that have a build marked as wanting one, because a
//!    re-projection deliberately runs no bundler.
//! 6. **A batch is refused whole.** An ungranted `drop_table` third in a list of
//!    three leaves the first two tables uncreated, and says which operation and
//!    which flag.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use sc_app::{
    ApiConfig, Application, AssetBundle, BuildSpec, CodeFramework, FrameworkRef, save_application,
};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_config::{MCP_ENABLED, MCP_LOOPBACK_ONLY, set_config};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MCP_PROTOCOL_VERSION, MCP_ROUTE, MountedApp, ServerConfig,
    admin_handlers, build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The cookie-jar admin client the other server suites use, for the half of this
/// test that mints the token through the ordinary admin API.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET"
            && method != "HEAD"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(ref b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

/// One MCP request, built the way a client builds it: a bearer credential, a
/// JSON-RPC body, a peer address and **no cookie**.
struct McpRequest {
    token: Option<String>,
    origin: Option<&'static str>,
    peer: Option<&'static str>,
    cookies: Option<String>,
    csrf: Option<String>,
    body: Value,
}

impl McpRequest {
    /// The ordinary case: this token, this JSON-RPC message, from localhost.
    fn new(token: &str, body: Value) -> McpRequest {
        McpRequest {
            token: Some(token.to_owned()),
            origin: None,
            peer: Some("127.0.0.1:51234"),
            cookies: None,
            csrf: None,
            body,
        }
    }

    async fn send(self, router: &Router) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(MCP_ROUTE)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = &self.token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(origin) = self.origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        if let Some(cookies) = &self.cookies {
            builder = builder.header(header::COOKIE, cookies);
        }
        if let Some(csrf) = &self.csrf {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let mut request = builder
            .body(Body::from(serde_json::to_vec(&self.body).unwrap()))
            .unwrap();
        // The peer address arrives as a request extension in production because
        // the listener is bound with connect info; `oneshot` has no listener, so
        // the test puts it there itself. Leaving it out is the "unknown peer"
        // case, which the loopback check treats as remote.
        if let Some(peer) = self.peer {
            let addr: SocketAddr = peer.parse().unwrap();
            request.extensions_mut().insert(ConnectInfo(addr));
        }
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

/// Everything one test needs to drive the server: the admin's cookie-jar client,
/// the router an MCP request goes to, the catalog behind both, and the live
/// mount registry — which the Phase 5 tests read to see what re-projected.
struct Harness {
    client: Client,
    router: Router,
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    _db: TestDb,
}

/// A router over a real database with the platform tables bootstrapped, an admin
/// signed in and the MCP server switched **on**.
async fn setup() -> sc_error::Result<Harness> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    let agents = sc_server::install_agents(&catalog).await?;
    // One provider for an agent to name. Nothing calls it: no turn is run here.
    sc_llm::save_llm_provider(
        &catalog,
        &sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-test"),
    )
    .await?;
    // The model an agent naming no model calls: the provider's default row.
    let provider = sc_llm::require_llm_provider(&catalog, "house").await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let triggers = sc_server::install_triggers(
        &catalog,
        sc_server::default_js_evaluator(),
        &agents,
        &models,
    )
    .await?;

    // The built-in stream providers, so `edit_streams` saves through a
    // supervisor that reloads, as on a running server.
    let streams =
        sc_server::install_streams(&catalog, &triggers, sc_stream::StreamConfig::default()).await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_agents(agents)
            .with_triggers(triggers)
            .with_streams(streams),
    );
    // `build_router_with_apps` rather than `build_router`, because the MCP route
    // reaches the catalog through the same `AppMounts` the app subdomains do —
    // and that is the call the real server makes (see `serve`). A router built
    // with no mounts has no catalog and therefore no MCP surface, which is the
    // same answer as the switch being off.
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        sessions,
        &ServerConfig::default(),
        apps.clone(),
    )?;

    let mut client = Client {
        router: router.clone(),
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    Ok(Harness {
        client,
        router,
        catalog,
        apps,
        _db: db,
    })
}

/// Turn the server on, with the loopback switch as given.
async fn enable_mcp(catalog: &Catalog, loopback_only: bool) -> sc_error::Result<()> {
    set_config(catalog, MCP_ENABLED, json!(true)).await?;
    set_config(catalog, MCP_LOOPBACK_ONLY, json!(loopback_only)).await
}

/// Mint a token through the admin API, as an administrator does.
async fn mint(client: &mut Client, label: &str, grants: Value) -> String {
    let (status, minted) = client
        .send(
            "POST",
            "/api/api-tokens",
            Some(json!({ "label": label, "grants": grants })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    minted["secret"]
        .as_str()
        .expect("the one plaintext")
        .to_owned()
}

/// The tool names a `tools/list` answer carries.
fn tool_names(listed: &Value) -> Vec<String> {
    listed["result"]["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

// --- the gates --------------------------------------------------------------

/// Off by default, and the difference between off and absent is deliberately
/// invisible. The token is minted first so the assertion is about the *switch*
/// and not about the credential.
#[tokio::test]
async fn a_disabled_server_is_a_404_and_a_valid_token_does_not_change_that() -> sc_error::Result<()>
{
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    let token = mint(&mut client, "claude-code on my laptop", json!({})).await;

    let (status, body) = McpRequest::new(
        &token,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // Not a word about tokens: a disabled feature should not be distinguishable
    // from one that was never built.
    assert!(!body.to_string().contains("token"), "{body}");

    // And the same body reaches the same route once it is switched on, with no
    // restart in between.
    enable_mcp(&catalog, true).await?;
    let (status, body) = McpRequest::new(
        &token,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

/// The confused-deputy story, asserted: a browser that is *logged in as this
/// very admin*, with a CSRF token the SPA would send, reaches nothing.
#[tokio::test]
async fn a_session_cookie_and_a_csrf_token_are_not_a_credential_here() -> sc_error::Result<()> {
    let Harness {
        client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    // The admin's live session and CSRF token, exactly as the SPA holds them.
    let session = client
        .cookies
        .get("sc_session")
        .cloned()
        .expect("a session");
    let csrf = client
        .cookies
        .get(CSRF_COOKIE)
        .cloned()
        .expect("a csrf token");

    let request = McpRequest {
        token: None,
        origin: None,
        peer: Some("127.0.0.1:51234"),
        cookies: Some(format!("sc_session={session}; {CSRF_COOKIE}={csrf}")),
        csrf: Some(csrf.clone()),
        body: json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    };
    let (status, body) = request.send(&router).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("Bearer"), "{body}");
    // The CSRF middleware did not refuse it — the route did. If the middleware
    // had, the status would be 403 and the message would be about CSRF, and the
    // exemption would be silently doing nothing.
    assert!(!message.contains("CSRF"), "{body}");
    Ok(())
}

/// A bearer request carries neither the CSRF cookie nor the header, so without
/// the exemption of §13.6 every POST here would be a `403` before the route ran.
#[tokio::test]
async fn a_bearer_request_is_exempt_from_the_csrf_check() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let token = mint(&mut client, "laptop", json!({})).await;

    let (status, body) = McpRequest::new(
        &token,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"], json!({}));
    Ok(())
}

/// Per the specification's DNS-rebinding guidance: refused outright, not
/// validated against a list.
#[tokio::test]
async fn a_request_carrying_an_origin_is_refused() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let token = mint(&mut client, "laptop", json!({})).await;

    let mut request = McpRequest::new(
        &token,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
    );
    request.origin = Some("https://evil.example.com");
    let (status, body) = request.send(&router).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"].as_str().unwrap().contains("Origin"), "{body}");
    Ok(())
}

/// The loopback switch, and what an unknown peer counts as.
#[tokio::test]
async fn a_remote_peer_is_refused_while_the_loopback_switch_is_on() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let token = mint(&mut client, "laptop", json!({})).await;
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });

    let mut request = McpRequest::new(&token, ping.clone());
    request.peer = Some("203.0.113.7:40000");
    let (status, body) = request.send(&router).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"].as_str().unwrap().contains("local"), "{body}");

    // A peer nobody can name is remote, because the safer answer is the one to
    // fall back to.
    let mut request = McpRequest::new(&token, ping.clone());
    request.peer = None;
    let (status, _) = request.send(&router).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // …and with the switch off, the same remote peer is served.
    enable_mcp(&catalog, false).await?;
    let mut request = McpRequest::new(&token, ping);
    request.peer = Some("203.0.113.7:40000");
    let (status, body) = request.send(&router).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

/// Revocation is what taking a token back means, and it takes effect on the very
/// next call because the lookup reads the row rather than a cached copy.
#[tokio::test]
async fn a_revoked_token_stops_working_on_the_next_call() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let token = mint(&mut client, "the laptop I lost", json!({})).await;
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });

    let (status, _) = McpRequest::new(&token, ping.clone()).send(&router).await;
    assert_eq!(status, StatusCode::OK);

    let (_, list) = client.send("GET", "/api/api-tokens", None).await;
    let id = list[0]["id"].as_str().unwrap().to_owned();
    let (status, _) = client
        .send("POST", &format!("/api/api-tokens/{id}/revoke"), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = McpRequest::new(&token, ping).send(&router).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    // Which of revoked, expired, unknown and demoted this was — the four are
    // different things to tell somebody.
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("revoked"),
        "{body}"
    );

    // An invented token is refused too, and by a different sentence.
    let (status, body) = McpRequest::new(
        "fspk_notarealtoken",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not recognised"),
        "{body}"
    );
    Ok(())
}

// --- the protocol -----------------------------------------------------------

/// The handshake and the pinned revision.
#[tokio::test]
async fn initialize_answers_with_the_one_revision_this_server_speaks() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let token = mint(&mut client, "laptop", json!({})).await;

    let (status, body) = McpRequest::new(
        &token,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "claude-code", "version": "1.0.0" },
            },
        }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["result"]["protocolVersion"],
        json!(MCP_PROTOCOL_VERSION)
    );
    assert!(
        body["result"]["capabilities"]["tools"].is_object(),
        "{body}"
    );
    assert!(body["result"]["serverInfo"]["name"].is_string(), "{body}");

    // A client asking for a **later** revision is answered with this server's
    // own, not refused: that is the specification's lifecycle, and it is what
    // keeps a server usable by clients newer than its constant — every real one
    // is. What must never happen is the client's revision being echoed back,
    // which would be agreeing to a protocol nobody here implements.
    let (status, body) = McpRequest::new(
        &token,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": { "protocolVersion": "2999-01-01" },
        }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("error").is_none(), "{body}");
    assert_eq!(
        body["result"]["protocolVersion"],
        json!(MCP_PROTOCOL_VERSION),
        "{body}"
    );

    // ...and the tools work under it, which is the whole reason the handshake
    // must not be a refusal.
    let (status, body) = McpRequest::new(
        &token,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body["result"]["tools"].as_array().unwrap().is_empty(),
        "{body}"
    );

    // The notification that follows takes no answer at all.
    let (status, _) = McpRequest::new(
        &token,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    Ok(())
}

/// The six flags **are** the tool list: an area that is off takes its tools out
/// of the listing rather than leaving them to be refused, and it does so for
/// both tiers, because a checkbox that meant two things would not be one
/// vocabulary.
#[tokio::test]
async fn a_token_without_the_trigger_area_is_offered_no_trigger_tools() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;

    let full = mint(&mut client, "everything", json!({})).await;
    let narrow = mint(
        &mut client,
        "schema only",
        json!({ "allow_triggers": false, "allow_applications": false }),
    )
    .await;
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });

    let (_, body) = McpRequest::new(&full, list.clone()).send(&router).await;
    let offered = tool_names(&body);
    // Tier 1: the ten composite tools.
    for name in [
        "describe_schema",
        "edit_schema",
        "describe_triggers",
        "describe_action",
        "save_trigger",
        "delete_trigger",
        "describe_code_api",
        "describe_applications",
        "save_api_query",
        "delete_api_query",
    ] {
        assert!(offered.contains(&name.to_owned()), "{offered:?}");
    }
    // Tier 2: the tagged endpoints, under the endpoint's own name.
    for name in [
        "listFieldTypes",
        "listAgents",
        "createAgent",
        "listRuns",
        "getRun",
        "listTriggers",
        "listActions",
        "saveWorkflow",
        "listApplications",
        "buildApplication",
    ] {
        assert!(offered.contains(&name.to_owned()), "{offered:?}");
    }
    // Tier 3 is absent — the number of tools is the design constraint.
    // (`listUsers` left tier 3 with TODO F1.9; `createUser` has not.)
    for absent in ["listRows", "writeFile", "createUser", "createApiToken"] {
        assert!(!offered.contains(&absent.to_owned()), "{offered:?}");
    }

    let (_, body) = McpRequest::new(&narrow, list).send(&router).await;
    let offered = tool_names(&body);
    for gone in [
        // The hand-written four…
        "describe_triggers",
        "save_trigger",
        "delete_trigger",
        "describe_action",
        // …and the tagged endpoints in the same half.
        "listTriggers",
        "listActions",
        "getWorkflow",
        "saveWorkflow",
        "listWorkflowRuns",
        // …and the applications half, likewise.
        "describe_applications",
        "listApplications",
        "buildApplication",
    ] {
        assert!(!offered.contains(&gone.to_owned()), "{offered:?}");
    }
    // The schema half has no area, so it is always there.
    assert!(
        offered.contains(&"describe_schema".to_owned()),
        "{offered:?}"
    );
    assert!(
        offered.contains(&"listFieldTypes".to_owned()),
        "{offered:?}"
    );

    // Every listed tool declares an object `inputSchema`, which is what an MCP
    // client requires.
    for tool in body["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], json!("object"), "{tool}");
        assert!(
            !tool["description"].as_str().unwrap_or_default().is_empty(),
            "{tool}"
        );
    }
    Ok(())
}

/// A tier-2 tool is a real request through the real registry: the answer is what
/// the endpoint's handler returns, for the token's user.
#[tokio::test]
async fn a_tagged_endpoint_is_dispatched_through_the_handler_registry() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let token = mint(&mut client, "laptop", json!({})).await;

    // A read with no arguments at all.
    let (status, body) = McpRequest::new(
        &token,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "listFieldTypes", "arguments": {} },
        }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], json!(false), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    // The type vocabulary `edit_schema` writes a field in.
    assert!(
        text.contains("\"text\"") && text.contains("\"int\""),
        "{text}"
    );

    // A write, whose effect is visible through the ordinary admin API — which is
    // the point of one authorization model: the row an agent made is the row the
    // screen shows.
    let (status, body) = McpRequest::new(
        &token,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "createAgent",
                "arguments": {
                    "name": "helper",
                    "description": "made over MCP",
                    "provider": "house",
                    "model": null,
                    "system_prompt": "Be helpful.",
                    "min_role": 1,
                    "traits": [],
                    "attributes": {},
                },
            },
        }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], json!(false), "{body}");

    let (_, agents) = client.send("GET", "/api/agents", None).await;
    assert_eq!(agents.as_array().unwrap().len(), 1, "{agents}");
    assert_eq!(agents[0]["name"], json!("helper"));

    // ...and reading them back is the shape that matters: an endpoint answering
    // a **list** — which most of tier 2 does — carries its result as text and
    // **no `structuredContent` key at all**, because that field is defined as an
    // object. A null there is a malformed result to a client that validates it,
    // and a client that validates it refuses the call: `listAgents` was
    // unusable from a real one until this was true.
    let (status, body) = McpRequest::new(
        &token,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "listAgents", "arguments": {} },
        }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], json!(false), "{body}");
    assert!(
        body["result"].get("structuredContent").is_none(),
        "a list must not carry a structuredContent: {body}"
    );
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("helper"), "{text}");
    Ok(())
}

/// The two shapes a failure can take, and which is which.
#[tokio::test]
async fn an_unknown_tool_is_a_protocol_error_and_a_refused_grant_is_a_result()
-> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    // Create and edit, not drop — the milestone's own example.
    let token = mint(
        &mut client,
        "claude-code on my laptop",
        json!({ "allow_create": true, "allow_edit": true, "allow_drop": false }),
    )
    .await;

    // An unknown tool is wrong with the *call*: there is nothing inside a tool
    // that does not exist for a model to correct.
    let (status, body) = McpRequest::new(
        &token,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "drop_everything", "arguments": {} },
        }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], json!(-32601), "{body}");
    assert!(body["result"].is_null(), "{body}");

    // An ungranted operation is a **result**: the model reads it, and it names
    // the operation and the flag that would allow it.
    let (status, body) = McpRequest::new(
        &token,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "edit_schema",
                "arguments": { "operations": [{ "op": "drop_table", "table": "invoices" }] },
            },
        }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["isError"], json!(true), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("allow_drop"), "{text}");

    // And the same flag governs the tier-2 tool that destroys something, so the
    // six flags mean one thing whichever tier a tool came from.
    let (_, body) = McpRequest::new(
        &token,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "deleteAgent",
                "arguments": { "id": "11111111-2222-3333-4444-555555555555" },
            },
        }),
    )
    .send(&router)
    .await;
    assert_eq!(body["result"]["isError"], json!(true), "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("allow_drop"), "{text}");

    // A method this server does not implement is a protocol error too.
    let (_, body) = McpRequest::new(
        &token,
        json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" }),
    )
    .send(&router)
    .await;
    assert_eq!(body["error"]["code"], json!(-32601), "{body}");
    Ok(())
}

// --- reload and rebuild -----------------------------------------------------

/// A directory a mounted application's source and generated client live in.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-mcp-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Mount an application over `tables`, served from a **built bundle** — which is
/// what makes it one that `wants_build` when the schema under it moves.
async fn mount_shop(
    catalog: &Arc<Catalog>,
    apps: &AppMounts,
    tables: &[&str],
) -> sc_error::Result<Application> {
    let framework_ref = FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts");
    let mut app = Application::new("Shop", "shop", framework_ref)
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"));
    for table in tables {
        app = app.with_table(TableId((*table).to_owned()));
    }
    save_application(catalog, &app).await?;
    // A framework with a build step, because that is the whole question the
    // report answers: re-projection kept this bundle and did not rebuild it.
    let framework = Arc::new(CodeFramework::new("code", AssetBundle::new()).with_build(
        BuildSpec {
            command: "sh".to_owned(),
            args: vec!["build.sh".to_owned()],
            source_dir: "web".to_owned(),
            output_dir: "web/dist".to_owned(),
            install: None,
            server_entry: None,
        },
    ));
    apps.remount(MountedApp::new(app.clone(), framework, catalog)?);
    Ok(app)
}

/// One `tools/call edit_schema` over MCP, as the result the model reads.
async fn edit_schema(router: &Router, token: &str, operations: Value) -> Value {
    let (status, body) = McpRequest::new(
        token,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "edit_schema", "arguments": { "operations": operations } },
        }),
    )
    .send(router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["result"].clone()
}

/// The generated client re-emitted after a re-projection is written by a spawned
/// task (it is file I/O on somebody's admin request), so read it with a bound
/// rather than assuming it has landed.
async fn await_client(path: &std::path::Path, needle: &str) -> String {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.contains(needle)
        {
            return text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let found = std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>"));
    panic!("the generated client never came to mention `{needle}`; it says:\n{found}");
}

/// The whole loop of §13.6's "reload and rebuild": a batch made over MCP moves
/// the schema, the catalog reloads **once** at the end of it, the applications
/// serving the tables it touched re-project against the reloaded catalog, their
/// generated clients are rewritten on disk — and the result *names* them, saying
/// which have a build and are therefore now serving a bundle that is behind.
#[tokio::test]
async fn an_edit_schema_batch_reprojects_the_mounted_app_and_names_it() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        apps,
        _db,
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let dir = TempDir::new("reload");
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &dir.0)?))?;
    // Everything, including the access rules: the batch below tightens one, and
    // the tightening is what proves the providers were rebuilt rather than kept.
    let token = mint(
        &mut client,
        "claude-code on my laptop",
        json!({ "allow_access_changes": true }),
    )
    .await;

    // Two connected tables in **one** batch — the milestone's own example.
    let result = edit_schema(
        &router,
        &token,
        json!([
            {
                "op": "create_table",
                "table": "customers",
                "fields": [
                    { "name": "id", "type": "int", "primary_key": true },
                    { "name": "name", "type": "text" },
                ],
            },
            {
                "op": "create_table",
                "table": "orders",
                "fields": [
                    { "name": "id", "type": "int", "primary_key": true },
                    { "name": "customer", "type": "int", "references": "customers" },
                ],
            },
        ]),
    )
    .await;
    assert_eq!(result["isError"], json!(false), "{result}");
    let applied = &result["structuredContent"];
    assert_eq!(applied["tables_created"], json!(["customers", "orders"]));
    // Nothing is mounted yet, so the report names nothing. An application list
    // that were merely decorative would say something here.
    assert_eq!(applied["applications"], json!([]), "{applied}");
    catalog.reload().await?;
    assert!(catalog.get("customers")?.is_some() && catalog.get("orders")?.is_some());

    // Now an application serving both of them, from a built bundle.
    let app = mount_shop(&catalog, &apps, &["customers", "orders"]).await?;
    let client_file = dir.0.join("web/src/client.ts");

    // A second batch touching **both** tables: a field on one, a tightened role
    // floor on the other.
    let result = edit_schema(
        &router,
        &token,
        json!([
            { "op": "add_field", "table": "orders", "type": "bool", "field": "shipped" },
            { "op": "alter_table", "table": "customers", "min_role_read": 80 },
        ]),
    )
    .await;
    assert_eq!(result["isError"], json!(false), "{result}");
    let applied = &result["structuredContent"];

    // The report: one application, named once though two of its tables moved,
    // carrying the id `buildApplication` takes and the fact that it wants one.
    assert_eq!(
        applied["applications"],
        json!([{
            "id": app.id.0.to_string(),
            "subdomain": "shop",
            "wants_build": true,
        }]),
        "{applied}"
    );
    // …and the sentence that says what to do about it, which is the half a
    // structured list cannot carry.
    let notes = applied["notes"].to_string();
    assert!(notes.contains("buildApplication"), "{notes}");
    assert!(notes.contains("shop"), "{notes}");

    // The providers really were rebuilt, against the **reloaded** catalog: the
    // tightened floor is the endpoint's auth requirement now, not at the next
    // restart. This is the assertion that the reload happened before the
    // notification rather than after it.
    let mounted = apps.get("shop").expect("the app is still mounted");
    let endpoints = mounted
        .providers
        .first()
        .expect("the rest provider")
        .endpoints();
    let listing = endpoints
        .find("listCustomers")
        .expect("the customers listing");
    assert_eq!(
        listing.auth,
        sc_api::AuthRequirement::MinRole(80),
        "the re-projection read the new access rules"
    );

    // And the generated client on disk describes the same API the projection
    // does — including the field this batch added.
    let text = await_client(&client_file, "shipped").await;
    assert!(text.contains("listOrders"), "{text}");
    assert!(text.contains("listCustomers"), "{text}");
    Ok(())
}

/// A batch is all or nothing, and the refusal is a **result** the model reads:
/// an ungranted `drop_table` third in a list of three leaves the first two
/// tables uncreated, and names both the operation and the flag.
#[tokio::test]
async fn an_ungranted_drop_refuses_the_whole_batch_and_names_it() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    // Create and edit, not drop — the milestone's own token.
    let token = mint(
        &mut client,
        "claude-code on my laptop",
        json!({ "allow_create": true, "allow_edit": true, "allow_drop": false }),
    )
    .await;

    // A table for the third operation to aim at, so the refusal is about the
    // grant and not about a table that was never there.
    let result = edit_schema(
        &router,
        &token,
        json!([{
            "op": "create_table",
            "table": "invoices",
            "fields": [{ "name": "id", "type": "int", "primary_key": true }],
        }]),
    )
    .await;
    assert_eq!(result["isError"], json!(false), "{result}");

    let result = edit_schema(
        &router,
        &token,
        json!([
            {
                "op": "create_table",
                "table": "suppliers",
                "fields": [{ "name": "id", "type": "int", "primary_key": true }],
            },
            {
                "op": "create_table",
                "table": "deliveries",
                "fields": [{ "name": "id", "type": "int", "primary_key": true }],
            },
            { "op": "drop_table", "table": "invoices" },
        ]),
    )
    .await;
    // A refusal is the result, not a transport error: the model is the one who
    // has to act on it.
    assert_eq!(result["isError"], json!(true), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    // Which operation, by index and by name…
    assert!(text.contains("operation 2"), "{text}");
    assert!(text.contains("drop_table"), "{text}");
    assert!(text.contains("invoices"), "{text}");
    // …and the flag that would have allowed it.
    assert!(text.contains("allow_drop"), "{text}");

    // Whole, not partly: the two tables before it do not exist, and the one it
    // aimed at still does.
    catalog.reload().await?;
    assert!(catalog.get("suppliers")?.is_none(), "the batch was applied");
    assert!(
        catalog.get("deliveries")?.is_none(),
        "the batch was applied"
    );
    assert!(catalog.get("invoices")?.is_some());
    Ok(())
}

/// A build that did not compile is the **result**, not a refusal: `built: false`
/// with the tools' own output and the file/line/message diagnostics parsed out of
/// it — the same decision `build_application` made for the chat copilot, because
/// a model told only "the build failed" cannot fix anything.
#[tokio::test]
async fn a_failed_build_comes_back_as_a_result_with_its_diagnostics() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        apps,
        _db,
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let dir = TempDir::new("build");
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &dir.0)?))?;
    let token = mint(&mut client, "claude-code on my laptop", json!({})).await;

    // A source tree whose "bundler" reports a type error and fails, which is the
    // interesting half of building.
    std::fs::create_dir_all(dir.0.join("web/src"))?;
    std::fs::write(
        dir.0.join("web/build.sh"),
        "echo \"src/App.tsx(12,5): error TS2322: Type 'number' is not assignable to type 'string'.\"\nexit 2\n",
    )?;
    let app = mount_shop(&catalog, &apps, &[]).await?;

    let (status, body) = McpRequest::new(
        &token,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "buildApplication",
                "arguments": { "id": app.id.0.to_string() },
            },
        }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Not `isError`: nothing is wrong with the call, and the model is being told
    // what the build tools said.
    assert_eq!(body["result"]["isError"], json!(false), "{body}");
    let built = &body["result"]["structuredContent"];
    assert_eq!(built["built"], json!(false), "{built}");
    assert!(
        built["log"].as_str().unwrap_or_default().contains("TS2322"),
        "{built}"
    );
    assert_eq!(built["diagnostics"][0]["file"], json!("src/App.tsx"));
    assert_eq!(built["diagnostics"][0]["line"], json!(12));
    // The application that was serving before is still the one mounted: a failed
    // build changes nothing.
    assert!(apps.get("shop").is_some());
    Ok(())
}

// --- roles and accounts (TODO F1.9) ------------------------------------------

/// One `tools/call`, as `(isError, text or structured result)`.
async fn call_tool(router: &Router, token: &str, name: &str, arguments: Value) -> (bool, Value) {
    let (status, body) = McpRequest::new(
        token,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }),
    )
    .send(router)
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["error"].is_null(), "a protocol error: {body}");
    let result = &body["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    let value = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
    (result["isError"] == json!(true), value)
}

/// The role and account tools are offered, each refusal names the grant it
/// lacks, deleting needs `allow_drop` as well as `allow_access_changes`, and
/// the admin the token runs as cannot disable or delete themselves — the last
/// admin there is.
#[tokio::test]
async fn roles_and_accounts_are_tools_behind_the_access_and_drop_grants() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let plain = mint(
        &mut client,
        "no access",
        json!({ "allow_access_changes": false, "allow_drop": false }),
    )
    .await;
    let access = mint(
        &mut client,
        "access",
        json!({ "allow_access_changes": true, "allow_drop": false }),
    )
    .await;
    let both = mint(
        &mut client,
        "access and drop",
        json!({ "allow_access_changes": true, "allow_drop": true }),
    )
    .await;

    let (status, listed) = McpRequest::new(
        &plain,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .send(&router)
    .await;
    assert_eq!(status, StatusCode::OK);
    let names = tool_names(&listed);
    for tool in [
        "listRoles",
        "createRole",
        "deleteRole",
        "listUsers",
        "setUserDisabled",
        "deleteUser",
    ] {
        assert!(
            names.contains(&tool.to_owned()),
            "`{tool}` is not offered: {names:?}"
        );
    }

    // Reads need no grant.
    let (failed, roles) = call_tool(&router, &plain, "listRoles", json!({})).await;
    assert!(!failed, "{roles}");
    assert!(roles.to_string().contains("\"role\":1"), "{roles}");
    let (failed, users) = call_tool(&router, &plain, "listUsers", json!({})).await;
    assert!(!failed, "{users}");
    let admin_id = users[0]["id"].as_str().expect("the admin's id").to_owned();
    assert!(!users.to_string().contains("password_hash"), "{users}");

    // Creating a role is an access change.
    let editor = json!({ "role": 40, "name": "Editor", "description": "Edits" });
    let (failed, text) = call_tool(&router, &plain, "createRole", editor.clone()).await;
    assert!(
        failed && text.to_string().contains("allow_access_changes"),
        "{text}"
    );
    let (failed, created) = call_tool(&router, &access, "createRole", editor).await;
    assert!(!failed, "{created}");
    assert!(sc_auth::load_role(&catalog, 40).await?.is_some());

    // Deleting one is an access change *and* a deletion, and each refusal names
    // the flag that is missing.
    let (failed, text) = call_tool(&router, &plain, "deleteRole", json!({ "role": 40 })).await;
    assert!(
        failed && text.to_string().contains("allow_access_changes"),
        "{text}"
    );
    let (failed, text) = call_tool(&router, &access, "deleteRole", json!({ "role": 40 })).await;
    assert!(failed && text.to_string().contains("allow_drop"), "{text}");

    // An account to act on.
    let (status, made) = client
        .send(
            "POST",
            "/api/users",
            Some(json!({ "email": "ed@example.com", "password": "pw-12345", "role": 40 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let ed = made["user"]["id"].as_str().unwrap().to_owned();

    let (failed, text) = call_tool(
        &router,
        &plain,
        "setUserDisabled",
        json!({ "id": ed, "disabled": true }),
    )
    .await;
    assert!(
        failed && text.to_string().contains("allow_access_changes"),
        "{text}"
    );
    let (failed, row) = call_tool(
        &router,
        &access,
        "setUserDisabled",
        json!({ "id": ed, "disabled": true }),
    )
    .await;
    assert!(!failed, "{row}");
    assert!(
        sc_auth::authenticate(&catalog, "ed@example.com", "pw-12345")
            .await?
            .is_none(),
        "disabled"
    );

    let (failed, text) = call_tool(&router, &access, "deleteUser", json!({ "id": ed })).await;
    assert!(failed && text.to_string().contains("allow_drop"), "{text}");
    let (failed, text) = call_tool(&router, &both, "deleteUser", json!({ "id": ed })).await;
    assert!(!failed, "{text}");
    let (failed, text) = call_tool(&router, &both, "deleteRole", json!({ "role": 40 })).await;
    assert!(!failed, "{text}");

    // The only admin — the one this token runs as — cannot be disabled or
    // deleted through it.
    let (failed, text) = call_tool(
        &router,
        &both,
        "setUserDisabled",
        json!({ "id": admin_id, "disabled": true }),
    )
    .await;
    assert!(
        failed && text.to_string().contains("cannot disable"),
        "{text}"
    );
    let (failed, text) = call_tool(&router, &both, "deleteUser", json!({ "id": admin_id })).await;
    assert!(
        failed && text.to_string().contains("cannot delete"),
        "{text}"
    );
    assert!(
        sc_auth::authenticate(&catalog, "admin@example.com", "hunter2pass")
            .await?
            .is_some()
    );
    Ok(())
}

/// The live-sync loop an external agent needs: a `collab_doc` field made with
/// `edit_schema`, a `document` stream on it made with `edit_streams` — flowing
/// at once, because the save goes through the admin's own `saveStream` — and
/// the stream exposed on the app with `update_application`. Setting who may
/// observe it needs `allow_access_changes`; deleting it needs `allow_drop` and
/// is refused while an application still exposes it.
#[tokio::test]
async fn a_document_stream_is_created_exposed_and_deleted_over_mcp() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        apps,
        _db,
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let dir = TempDir::new("streams");
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &dir.0)?))?;
    let builder = mint(
        &mut client,
        "builder",
        json!({ "allow_access_changes": false, "allow_drop": false }),
    )
    .await;
    let admin = mint(
        &mut client,
        "admin",
        json!({ "allow_access_changes": true, "allow_drop": true }),
    )
    .await;

    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let (_, listed) = McpRequest::new(&builder, list).send(&router).await;
    assert!(tool_names(&listed).contains(&"edit_streams".to_owned()));

    edit_schema(
        &router,
        &builder,
        json!([{
            "op": "create_table",
            "table": "notes",
            "fields": [
                { "name": "id", "type": "int", "primary_key": true },
                { "name": "body", "type": "collab_doc" },
            ],
        }]),
    )
    .await;
    mount_shop(&catalog, &apps, &[]).await?;

    // No arguments: the listing, with each provider's settings.
    let (error, listing) = call_tool(&router, &builder, "edit_streams", json!({})).await;
    assert!(!error, "{listing}");
    assert_eq!(listing["streams"], json!([]), "{listing}");
    let document = listing["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == json!("document"))
        .expect("the document provider");
    assert!(
        document["settings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == json!("field")),
        "{document}"
    );

    let note_body = json!({ "save": [{
        "name": "note_body",
        "provider": "document",
        "configuration": { "table": "notes", "field": "body" },
        "min_role": 80,
    }] });
    // `min_role` is an access rule: refused whole without the grant.
    let (error, refused) = call_tool(&router, &builder, "edit_streams", note_body.clone()).await;
    assert!(error, "{refused}");
    assert!(
        refused.to_string().contains("allow_access_changes"),
        "{refused}"
    );
    assert!(
        sc_stream::load_stream_by_name(&catalog, "note_body")
            .await?
            .is_none()
    );

    let (error, created) = call_tool(&router, &admin, "edit_streams", note_body).await;
    assert!(!error, "{created}");
    assert_eq!(created["created"], json!(["note_body"]), "{created}");
    assert_eq!(created["streams"][0]["min_role"], json!(80), "{created}");
    assert_eq!(
        created["streams"][0]["topics"],
        json!({ "kind": "row", "table": "notes" }),
        "{created}"
    );
    // Flowing already: the save reloaded the supervisor.
    let stored = sc_stream::require_stream(&catalog, "note_body").await?;
    assert!(
        apps.streams()
            .unwrap()
            .supervisor()
            .get(stored.id)
            .is_some()
    );

    // Exposing a document stream needs its table connected.
    let (error, refused) = call_tool(
        &router,
        &builder,
        "update_application",
        json!({ "application": "shop", "streams": { "add": ["note_body"] } }),
    )
    .await;
    assert!(error, "{refused}");
    assert!(refused.to_string().contains("notes"), "{refused}");
    let (error, exposed) = call_tool(
        &router,
        &builder,
        "update_application",
        json!({
            "application": "shop",
            "tables": { "add": ["notes"] },
            "streams": { "add": ["note_body"] },
        }),
    )
    .await;
    assert!(!error, "{exposed}");
    assert_eq!(exposed["streams"], json!(["note_body"]), "{exposed}");
    assert_eq!(exposed["live_socket"], json!("/api/live"), "{exposed}");

    // A change names only what changes; the rest is kept.
    let (error, changed) = call_tool(
        &router,
        &builder,
        "edit_streams",
        json!({ "save": [{ "name": "note_body", "configuration": { "root_kind": "text" } }] }),
    )
    .await;
    assert!(!error, "{changed}");
    assert_eq!(changed["changed"], json!(["note_body"]), "{changed}");
    let stream = &changed["streams"][0];
    assert_eq!(
        stream["configuration"]["root_kind"],
        json!("text"),
        "{changed}"
    );
    assert_eq!(stream["configuration"]["field"], json!("body"), "{changed}");
    assert_eq!(stream["min_role"], json!(80), "{changed}");
    assert_eq!(stream["exposed_by"], json!(["shop"]), "{changed}");

    // Deleting: needs `allow_drop`, and refused while the app exposes it.
    let delete = json!({ "delete": ["note_body"] });
    let (error, refused) = call_tool(&router, &builder, "edit_streams", delete.clone()).await;
    assert!(
        error && refused.to_string().contains("allow_drop"),
        "{refused}"
    );
    let (error, refused) = call_tool(&router, &admin, "edit_streams", delete.clone()).await;
    assert!(
        error && refused.to_string().contains("application `shop`"),
        "{refused}"
    );
    let (error, removed) = call_tool(
        &router,
        &builder,
        "update_application",
        json!({ "application": "shop", "streams": { "remove": ["note_body"] } }),
    )
    .await;
    assert!(!error, "{removed}");
    let (error, deleted) = call_tool(&router, &admin, "edit_streams", delete).await;
    assert!(!error, "{deleted}");
    assert_eq!(deleted["deleted"], json!(["note_body"]), "{deleted}");
    assert_eq!(deleted["streams"], json!([]), "{deleted}");
    assert!(
        apps.streams()
            .unwrap()
            .supervisor()
            .get(stored.id)
            .is_none()
    );
    Ok(())
}

/// A token gets every flag unless the mint says otherwise — modules, drops and
/// access changes included. The module tools are offered only to a token with
/// `allow_modules`; installing also needs `allow_create`, and changing a
/// module's settings (`updateModule`) `allow_edit`.
#[tokio::test]
async fn the_module_tools_are_offered_only_to_a_token_given_allow_modules() -> sc_error::Result<()>
{
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;

    let without = mint(&mut client, "no modules", json!({ "allow_modules": false })).await;
    let modules = mint(&mut client, "defaults", json!({})).await;
    let read_only = mint(
        &mut client,
        "modules, no create",
        json!({ "allow_modules": true, "allow_create": false }),
    )
    .await;
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });

    let (_, body) = McpRequest::new(&without, list.clone()).send(&router).await;
    let offered = tool_names(&body);
    for absent in ["listModules", "installModule", "updateModule"] {
        assert!(!offered.contains(&absent.to_owned()), "{offered:?}");
    }
    let (_, body) = McpRequest::new(&modules, list).send(&router).await;
    let offered = tool_names(&body);
    for present in ["listModules", "installModule", "updateModule"] {
        assert!(offered.contains(&present.to_owned()), "{offered:?}");
    }
    let install = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("installModule"))
        .unwrap();
    assert!(
        install["description"].as_str().unwrap().contains("pypi"),
        "the description should say how to install a PyPI library: {install}"
    );

    // The call reaches the `listModules` handler, which on this harness (built
    // without module services) answers with its own error.
    let (is_error, answer) = call_tool(&router, &modules, "listModules", json!({})).await;
    assert!(is_error, "{answer}");
    assert!(answer.to_string().contains("no module support"), "{answer}");

    // Installing without `allow_create` is refused, naming it, before anything
    // is installed.
    let (is_error, answer) = call_tool(
        &router,
        &read_only,
        "installModule",
        json!({ "language": "python", "source": "pypi", "location": "requests" }),
    )
    .await;
    assert!(is_error, "{answer}");
    assert!(answer.to_string().contains("allow_create"), "{answer}");

    // And a token without the area cannot call it by name either.
    let (_, body) = McpRequest::new(
        &without,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "installModule",
                "arguments": { "language": "python", "source": "pypi", "location": "requests" },
            },
        }),
    )
    .send(&router)
    .await;
    assert!(
        !body["error"].is_null() || body["result"]["isError"] == json!(true),
        "{body}"
    );
    Ok(())
}

/// Row history's tools are offered only to a token given `allow_history`, and
/// one called reaches the history: here a row's versions (TODO-history H2.4),
/// and an undo of the last of them (H3.3).
#[tokio::test]
async fn the_history_tools_are_offered_only_to_a_token_given_allow_history() -> sc_error::Result<()>
{
    let Harness {
        mut client,
        router,
        catalog,
        _db,
        ..
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    sc_catalog::bootstrap_change_log(&catalog).await?;
    _db.client()
        .await?
        .batch_execute("CREATE TABLE notes (id bigint primary key, title text)")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    catalog.reload().await?;
    sc_api::schema_edit::apply(
        &catalog,
        &[sc_api::schema_edit::Operation::AlterTable {
            table: "notes".to_owned(),
            settings: sc_api::schema_edit::TableSettings {
                history_enabled: Some(true),
                ..Default::default()
            },
        }],
        &sc_api::schema_edit::ApplyOptions::default(),
    )
    .await?;
    // Two transactions, so two `sql` changesets and two versions.
    for statement in [
        "INSERT INTO notes VALUES (1, 'milk')",
        "UPDATE notes SET title = 'oat milk'",
    ] {
        _db.client()
            .await?
            .batch_execute(statement)
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))?;
    }

    let tools = [
        "getRowHistory",
        "listDeletedRows",
        "getRowAsOf",
        "listChangesets",
        "getChangeset",
        "restoreRow",
        "revertRow",
        "undoChangeset",
    ];
    let without = mint(&mut client, "no history", json!({ "allow_history": false })).await;
    let history = mint(&mut client, "defaults", json!({})).await;
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let (_, body) = McpRequest::new(&without, list.clone()).send(&router).await;
    let offered = tool_names(&body);
    for absent in tools {
        assert!(!offered.contains(&absent.to_owned()), "{offered:?}");
    }
    let (_, body) = McpRequest::new(&history, list).send(&router).await;
    let offered = tool_names(&body);
    for present in tools {
        assert!(offered.contains(&present.to_owned()), "{offered:?}");
    }

    let (is_error, answer) = call_tool(
        &router,
        &history,
        "getRowHistory",
        json!({ "table": "notes", "id": "1" }),
    )
    .await;
    assert!(!is_error, "{answer}");
    let versions: Vec<&str> = answer
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["op"].as_str().unwrap())
        .collect();
    assert_eq!(versions, ["u", "i"], "{answer}");
    assert_eq!(answer[0]["changeset"]["source"], json!("sql"));

    let update = answer[0]["changeset"]["id"].as_str().unwrap().to_owned();
    let (is_error, undone) =
        call_tool(&router, &history, "undoChangeset", json!({ "id": update })).await;
    assert!(!is_error, "{undone}");
    assert_eq!(undone["undone"], json!(true), "{undone}");
    let (_, answer) = call_tool(
        &router,
        &history,
        "getRowHistory",
        json!({ "table": "notes", "id": "1" }),
    )
    .await;
    assert_eq!(answer[0]["changeset"]["source"], json!("undo"), "{answer}");
    assert_eq!(answer[0]["changes"][0]["new"], json!("milk"), "{answer}");
    Ok(())
}

/// The whole loop an agent needs to call a module's action from its app: a
/// `none` trigger made with `save_trigger`, exposed with `update_application`'s
/// `triggers` section, and `api.run<Name>()` in the generated client at once —
/// with no one ticking a box. A trigger with an event, or one that does not
/// exist, is refused by name; a public one needs `allow_access_changes`; and
/// removing one takes the method back out of the client.
#[tokio::test]
async fn a_trigger_is_exposed_on_an_app_and_removed_over_mcp() -> sc_error::Result<()> {
    let Harness {
        mut client,
        router,
        catalog,
        apps,
        _db,
    } = setup().await?;
    enable_mcp(&catalog, true).await?;
    let dir = TempDir::new("triggers");
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &dir.0)?))?;
    let builder = mint(
        &mut client,
        "builder",
        json!({ "allow_access_changes": false, "allow_drop": false }),
    )
    .await;
    let admin = mint(
        &mut client,
        "admin",
        json!({ "allow_access_changes": true }),
    )
    .await;

    edit_schema(
        &router,
        &builder,
        json!([{
            "op": "create_table",
            "table": "audit",
            "fields": [
                { "name": "id", "type": "int", "primary_key": true },
                { "name": "title", "type": "text" },
            ],
        }]),
    )
    .await;
    mount_shop(&catalog, &apps, &["audit"]).await?;

    // The section is advertised, not only accepted: an agent reads the schema.
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let (_, listed) = McpRequest::new(&builder, list).send(&router).await;
    let update = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("update_application"))
        .expect("update_application is offered");
    assert!(
        update["inputSchema"]["properties"]["triggers"]["properties"]["add"].is_object(),
        "{update}"
    );
    assert!(
        update["description"]
            .as_str()
            .unwrap()
            .contains("`triggers` — the **triggers** it exposes"),
        "{update}"
    );

    let insert = json!({ "table": "audit", "values": { "title": "payload.what" } });
    for (name, when, min_role) in [
        ("log_visit", "none", Some(80)),
        ("unset_role", "none", None),
        ("open_door", "none", Some(100)),
        ("on_audit", "insert", None),
    ] {
        let mut trigger = json!({
            "name": name,
            "when": when,
            "action": "insert_row",
            "configuration": insert,
        });
        if when == "insert" {
            trigger["table"] = json!("audit");
        }
        if let Some(role) = min_role {
            trigger["min_role"] = json!(role);
        }
        let (error, saved) = call_tool(&router, &admin, "save_trigger", trigger).await;
        assert!(!error, "{saved}");
    }

    let (error, exposed) = call_tool(
        &router,
        &builder,
        "update_application",
        json!({ "application": "shop", "triggers": { "add": ["log_visit", "unset_role"] } }),
    )
    .await;
    assert!(!error, "{exposed}");
    assert_eq!(exposed["was"]["triggers"], json!([]), "{exposed}");
    let first = &exposed["triggers"][0];
    assert_eq!(first["name"], json!("log_visit"), "{exposed}");
    assert_eq!(first["action"], json!("insert_row"), "{exposed}");
    assert_eq!(first["min_role"], json!(80), "{exposed}");
    assert_eq!(first["method"], json!("runLogVisit"), "{exposed}");
    assert_eq!(
        first["route"],
        json!("POST /api/actions/log_visit"),
        "{exposed}"
    );
    assert!(first.get("note").is_none(), "{exposed}");
    // Left admin-only: said so, since that is an easy mistake to make.
    assert!(
        exposed["triggers"][1]["note"]
            .to_string()
            .contains("only admins"),
        "{exposed}"
    );
    // Stored (the admin's tick box reads this), mounted, and in the client.
    let stored = sc_app::load_application_by_subdomain(&catalog, "shop")
        .await?
        .unwrap();
    assert!(stored.exposes_trigger("log_visit") && stored.exposes_trigger("unset_role"));
    assert!(apps.get("shop").unwrap().app.exposes_trigger("log_visit"));
    let client_ts = dir.0.join("web/src/client.ts");
    await_client(&client_ts, "runLogVisit").await;
    let (error, described) = call_tool(&router, &builder, "describe_applications", json!({})).await;
    assert!(!error, "{described}");
    assert!(described.to_string().contains("log_visit"), "{described}");

    // Refused by name, and nothing saved: an event trigger, a missing one.
    for (name, says) in [("on_audit", "insert"), ("no_such", "no_such")] {
        let (error, refused) = call_tool(
            &router,
            &builder,
            "update_application",
            json!({ "application": "shop", "triggers": { "add": [name] } }),
        )
        .await;
        assert!(error, "{refused}");
        let text = refused.to_string();
        assert!(
            text.contains(&format!("`{name}`")) && text.contains(says),
            "{text}"
        );
    }
    // A public trigger opens a route to signed-out callers.
    let public = json!({ "application": "shop", "triggers": { "add": ["open_door"] } });
    let (error, refused) = call_tool(&router, &builder, "update_application", public.clone()).await;
    assert!(
        error && refused.to_string().contains("allow_access_changes"),
        "{refused}"
    );
    let stored = sc_app::load_application_by_subdomain(&catalog, "shop")
        .await?
        .unwrap();
    assert_eq!(stored.triggers.len(), 2);
    let (error, opened) = call_tool(&router, &admin, "update_application", public).await;
    assert!(!error, "{opened}");
    assert_eq!(opened["triggers"][2]["min_role"], json!(100), "{opened}");

    // Removing it takes the method out of the client and the mount.
    let (error, removed) = call_tool(
        &router,
        &builder,
        "update_application",
        json!({ "application": "shop", "triggers": { "remove": ["log_visit"] } }),
    )
    .await;
    assert!(!error, "{removed}");
    assert_eq!(
        removed["was"]["triggers"],
        json!(["log_visit", "unset_role", "open_door"]),
        "{removed}"
    );
    assert!(!apps.get("shop").unwrap().app.exposes_trigger("log_visit"));
    let mut gone = false;
    for _ in 0..100 {
        let text = std::fs::read_to_string(&client_ts).unwrap_or_default();
        if text.contains("runOpenDoor") && !text.contains("runLogVisit") {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(gone, "the client still has runLogVisit");
    Ok(())
}

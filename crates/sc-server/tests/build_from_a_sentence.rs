//! "Build me a to-do list", over the administration MCP server (§13.6): an
//! external coding agent with nothing but a token creates the application —
//! its new local file store, its scaffolded React project and its builder agent
//! — learns the project directory to carry on in, creates the tables, connects
//! them, and finds them in the regenerated client.
//!
//! The chat copilot calls the same tools through the same seam (the catalog's
//! `AdminHost`, installed when the router is built), so this is its path too.
//!
//! Its own binary, because new local stores are made under the data directory,
//! and the variable that moves it out of the developer's real one is
//! process-wide.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_config::{MCP_ENABLED, set_config};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MCP_ROUTE, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The data directory every new local store lands under, set once for the
/// process (see the module docs).
fn data_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("sc-build-sentence-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var(sc_files::DATA_DIR_ENV, &dir) };
        dir
    })
}

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
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }
}

/// One `tools/call` over MCP, from localhost with a bearer token; the tool's
/// structured result, or a panic carrying the refusal.
async fn call(router: &Router, token: &str, tool: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    let mut request = Request::builder()
        .method("POST")
        .uri(MCP_ROUTE)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let addr: SocketAddr = "127.0.0.1:51234".parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(addr));
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["result"]["isError"], json!(false), "{tool}: {value}");
    value["result"]["structuredContent"].clone()
}

/// A server with agents, a provider for the builder agent to name, MCP on, an
/// admin signed in and a token minted.
async fn setup() -> sc_error::Result<(Router, Arc<Catalog>, String, TestDb)> {
    setup_with(ServerConfig::default(), json!({})).await
}

/// [`setup`] with a server configuration and the token's grants.
async fn setup_with(
    config: ServerConfig,
    grants: Value,
) -> sc_error::Result<(Router, Arc<Catalog>, String, TestDb)> {
    data_dir();
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
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    let agents = sc_server::install_agents(&catalog).await?;
    sc_llm::save_llm_provider(
        &catalog,
        &sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-test"),
    )
    .await?;
    let provider = sc_llm::require_llm_provider(&catalog, "house").await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;

    let apps = Arc::new(AppMounts::new(catalog.clone()).with_agents(agents));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps,
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
    set_config(&catalog, MCP_ENABLED, json!(true)).await?;
    let (status, minted) = client
        .send(
            "POST",
            "/api/api-tokens",
            Some(json!({ "label": "claude-code", "grants": grants })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    let token = minted["secret"].as_str().unwrap().to_owned();
    Ok((router, catalog, token, db))
}

#[tokio::test]
async fn an_external_agent_builds_an_application_from_a_sentence() -> sc_error::Result<()> {
    let (router, catalog, token, _db) = setup().await?;

    // Nothing but a display name: React, a new local store, a subdomain made
    // from the name, a builder agent.
    let created = call(
        &router,
        &token,
        "create_application",
        json!({ "name": "Todo list", "description": "Things to do" }),
    )
    .await;
    assert_eq!(created["created"], json!("todo-list"), "{created}");
    assert_eq!(created["framework"], json!("react"), "{created}");
    assert_eq!(
        created["created_file_stores"],
        json!(["todo-list"]),
        "{created}"
    );
    assert_eq!(
        created["builder_agent"],
        json!("build-todo-list"),
        "{created}"
    );
    assert!(created.get("scaffold_error").is_none(), "{created}");

    // The directory to carry on in: under the data directory, scaffolded, with
    // the generated client.
    let project = PathBuf::from(created["project_dir"].as_str().expect("a project_dir"));
    assert!(project.starts_with(data_dir()), "{}", project.display());
    assert!(project.join("package.json").is_file());
    assert!(project.join("AGENTS.md").is_file());
    let client_ts = project.join("src/feldspar/client.ts");
    assert!(client_ts.is_file());

    // Tables first…
    call(
        &router,
        &token,
        "edit_schema",
        json!({ "operations": [{
            "op": "create_table",
            "table": "todos",
            "fields": [
                { "name": "id", "type": "int", "primary_key": true },
                { "name": "title", "type": "text", "required": true },
                { "name": "done", "type": "bool" },
            ],
        }] }),
    )
    .await;
    // …then connected, which regenerates the client with them in it.
    let before = std::fs::read_to_string(&client_ts).unwrap();
    assert!(!before.contains("todos"), "not connected yet");
    let connected = call(
        &router,
        &token,
        "update_application",
        json!({ "application": "todo-list", "tables": { "add": ["todos"] } }),
    )
    .await;
    assert_eq!(connected["tables"], json!(["todos"]), "{connected}");
    let after = std::fs::read_to_string(&client_ts).unwrap();
    assert!(after.contains("todos"), "the client is typed for the table");
    let stored = sc_app::load_application_by_subdomain(&catalog, "todo-list")
        .await?
        .unwrap();
    assert_eq!(stored.tables.len(), 1);

    // A table that does not exist is refused, and nothing changes.
    let body = json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "update_application",
                    "arguments": { "application": "todo-list",
                                   "tables": { "add": ["nope"] } } },
    });
    let mut request = Request::builder()
        .method("POST")
        .uri(MCP_ROUTE)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1".parse::<SocketAddr>().unwrap()));
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let refused: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(refused["result"]["isError"], json!(true), "{refused}");

    // The application is listed with where its code is and who builds it, and
    // the builder agent really exists, pointed at it.
    let described = call(&router, &token, "describe_applications", json!({})).await;
    let app = &described["applications"][0];
    assert_eq!(app["project_dir"], created["project_dir"], "{described}");
    assert_eq!(app["builder_agent"], json!("build-todo-list"));
    let agent = sc_agent::load_agent_by_name(&catalog, "build-todo-list")
        .await?
        .expect("the builder agent");
    assert!(agent.traits.iter().any(|t| t.trait_ == "coding"));

    // A second "todo list" gets the next free subdomain rather than a refusal.
    let again = call(
        &router,
        &token,
        "create_application",
        json!({ "name": "Todo list" }),
    )
    .await;
    assert_eq!(again["created"], json!("todo-list2"), "{again}");
    Ok(())
}

/// A local store made on its own, for an application created afterwards.
#[tokio::test]
async fn a_file_store_is_created_and_named_by_the_application() -> sc_error::Result<()> {
    let (router, catalog, token, _db) = setup().await?;
    let store = call(
        &router,
        &token,
        "create_file_store",
        json!({ "name": "notes-code" }),
    )
    .await;
    assert_eq!(store["created"], json!("notes-code"), "{store}");
    let dir = PathBuf::from(store["directory"].as_str().unwrap());
    assert!(dir.is_dir() && dir.starts_with(data_dir()), "{store}");
    assert!(catalog.file_store("notes-code")?.is_some());

    let created = call(
        &router,
        &token,
        "create_application",
        json!({ "name": "Notes", "file_store": "notes-code" }),
    )
    .await;
    assert!(created.get("created_file_stores").is_none(), "{created}");
    assert_eq!(
        PathBuf::from(created["project_dir"].as_str().unwrap()),
        dir,
        "{created}"
    );
    assert!(dir.join("package.json").is_file());
    Ok(())
}

/// What the external agent building a presentation had to leave for a person:
/// serving a store's folder on a URL of the app, and widening the app's CSP so
/// another app may frame it. Both are sections of `update_application` now, and
/// both change what the **running** app serves at once — it goes through the
/// admin's own update, which refreshes the mount, rather than writing a record
/// nothing reads.
#[tokio::test]
async fn an_agent_serves_a_store_folder_and_widens_the_csp_of_a_running_app() -> sc_error::Result<()>
{
    let config = ServerConfig {
        base_domain: Some("example.com".to_owned()),
        ..ServerConfig::default()
    };
    let (router, catalog, token, _db) =
        setup_with(config, json!({ "allow_access_changes": true })).await?;

    let store = call(
        &router,
        &token,
        "create_file_store",
        json!({ "name": "media" }),
    )
    .await;
    let dir = PathBuf::from(store["directory"].as_str().unwrap());
    std::fs::create_dir_all(dir.join("slides")).unwrap();
    std::fs::write(dir.join("slides/hero.png"), b"\x89PNG\r\n\x1a\nhero").unwrap();

    let created = call(
        &router,
        &token,
        "create_application",
        json!({ "name": "Show", "framework": "none" }),
    )
    .await;
    assert_eq!(created["created"], json!("show"), "{created}");

    let get = |path: &str| {
        let router = router.clone();
        let request = Request::get(path)
            .header(header::HOST, "show.example.com")
            .body(Body::empty())
            .unwrap();
        async move { router.oneshot(request).await.unwrap() }
    };
    assert_eq!(get("/media/hero.png").await.status(), StatusCode::NOT_FOUND);

    let served = call(
        &router,
        &token,
        "update_application",
        json!({
            "application": "show",
            "static_dirs": {
                "add": [{ "mount": "media", "file_store": "media", "path": "/slides/" }],
            },
        }),
    )
    .await;
    assert_eq!(
        served["static_dirs"],
        json!([{ "mount": "/media", "file_store": "media", "path": "slides" }]),
        "{served}"
    );
    assert_eq!(
        served["connected_file_stores"],
        json!(["media"]),
        "{served}"
    );
    assert!(served.get("mount_error").is_none(), "{served}");
    let stored = sc_app::load_application_by_subdomain(&catalog, "show")
        .await?
        .unwrap();
    assert!(stored.file_stores.iter().any(|s| s.0 == "media"));

    // Served by the running app, with no restart.
    let response = get("/media/hero.png").await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert!(bytes.ends_with(b"hero"));

    let widened = call(
        &router,
        &token,
        "update_application",
        json!({
            "application": "show",
            "csp": {
                "frame-ancestors": ["'self'", "https://admin-app.example.com"],
                "style-src": ["'self'", "'unsafe-inline'"],
            },
        }),
    )
    .await;
    let header_value = widened["csp_header"].as_str().unwrap().to_owned();
    assert!(
        header_value.contains("frame-ancestors 'self' https://admin-app.example.com"),
        "{widened}"
    );
    let response = get("/media/hero.png").await;
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("https://admin-app.example.com"), "{csp}");
    assert!(csp.contains("style-src 'self' 'unsafe-inline'"), "{csp}");
    // The static directory survived the CSP's update: a section not named is
    // left as it was.
    assert_eq!(response.status(), StatusCode::OK);

    // Two sections in one call, saved as one.
    let removed = call(
        &router,
        &token,
        "update_application",
        json!({
            "application": "show",
            "static_dirs": { "remove": ["/media"] },
            "csp": { "style-src": null },
        }),
    )
    .await;
    assert_eq!(removed["static_dirs"], json!([]), "{removed}");
    assert!(removed["csp"].get("style-src").is_none(), "{removed}");
    let response = get("/media/hero.png").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(!csp.contains("unsafe-inline"), "{csp}");
    Ok(())
}

//! The MCP client (TODO-agent-skills AS4) against **this server's own MCP
//! endpoint**, served on a real socket: what a remote server is to an agent,
//! with the admin API that defines it.
//!
//! What is asserted here: a definition is saved with a credential header that
//! never comes back; *Test* connects, initialises and lists the tools, with the
//! stored header standing in for the sentinel the form sends; a tool is called
//! and answers; a wrong credential and a private address without the
//! definition's permission are refusals the admin can read. (That a server an
//! agent's `mcp_server` trait names cannot be deleted is tested with the trait,
//! AS9.1.)
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_config::{MCP_ENABLED, MCP_LOOPBACK_ONLY, set_config};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MCP_ROUTE, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use sc_types::SECRET_SENTINEL;
use serde_json::{Value, json};
use tower::ServiceExt;

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

/// An admin's client over a router whose MCP server is on and listening on a
/// loopback port, whose URL comes back with it.
async fn setup() -> sc_error::Result<(Client, Arc<Catalog>, String, TestDb)> {
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
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_mcp::bootstrap_mcp_servers(&catalog).await?;
    sc_agent::bootstrap_agents(&catalog).await?;
    set_config(&catalog, MCP_ENABLED, json!(true)).await?;
    set_config(&catalog, MCP_LOOPBACK_ONLY, json!(true)).await?;

    // Agents installed, so one can name a server (AS9.1).
    let agents = sc_server::install_agents(&catalog).await?;
    let apps = Arc::new(AppMounts::new(catalog.clone()).with_agents(agents));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            served.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let mut client = Client {
        router,
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
    Ok((client, catalog, format!("http://{addr}{MCP_ROUTE}"), db))
}

/// Mint an MCP token through the admin API.
async fn mint(client: &mut Client) -> String {
    let (status, minted) = client
        .send(
            "POST",
            "/api/api-tokens",
            Some(json!({ "label": "the agent's", "grants": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    minted["secret"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn our_own_mcp_server_is_defined_tested_listed_and_called() -> sc_error::Result<()> {
    let (mut client, catalog, url, _db) = setup().await?;
    let token = mint(&mut client).await;

    let (status, created) = client
        .send(
            "POST",
            "/api/mcp-servers",
            Some(json!({
                "name": "self", "description": "This server", "url": url,
                "transport": "streamable_http",
                "headers": [{ "name": "Authorization", "value": format!("Bearer {token}") }],
                "allow_private_network": true, "timeout_secs": 20,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    // The credential never comes back.
    assert_eq!(created["headers"][0]["value"], json!(SECRET_SENTINEL));
    assert!(!created.to_string().contains(&token));
    let id = created["id"].as_str().unwrap().to_owned();
    let (_, listed) = client.send("GET", "/api/mcp-servers", None).await;
    assert!(!listed.to_string().contains(&token), "{listed}");

    // *Test* with the form as it stands: the sentinel resolves to the key.
    let mut form = created.clone();
    form["id"] = json!(id);
    let (status, tested) = client
        .send("POST", "/api/mcp-server-test", Some(form.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["ok"], json!(true), "{tested}");
    assert_eq!(tested["server"]["protocol_version"], json!("2025-06-18"));
    let tools: Vec<&str> = tested["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"describe_schema"), "{tools:?}");

    // The saved server's tools, from the cache the test filled.
    let (status, cached) = client
        .send("GET", &format!("/api/mcp-servers/{id}/tools"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{cached}");
    assert_eq!(cached["tools"].as_array().unwrap().len(), tools.len());

    // A call, as the `mcp_server` trait will make it.
    let def = sc_mcp::require_mcp_server(&catalog, "self").await?;
    let result = sc_mcp::call_tool(&def, "describe_schema", json!({})).await?;
    assert!(!result.is_error, "{}", result.text);
    assert!(!result.text.is_empty());
    // An unknown tool is a call that did not happen.
    let err = sc_mcp::call_tool(&def, "no_such_tool", json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("refused `tools/call`"), "{err}");

    // A wrong credential is the server's refusal, readable.
    form["headers"] = json!([{ "name": "Authorization", "value": "Bearer wrong" }]);
    let (_, refused) = client
        .send("POST", "/api/mcp-server-test", Some(form.clone()))
        .await;
    assert_eq!(refused["ok"], json!(false), "{refused}");
    let message = refused["message"].as_str().unwrap();
    assert!(
        message.contains("401") && message.contains("credential"),
        "{message}"
    );

    // Loopback without the definition's permission is refused before sending.
    form["headers"] = json!([{ "name": "Authorization", "value": SECRET_SENTINEL }]);
    form["allow_private_network"] = json!(false);
    let (_, refused) = client
        .send("POST", "/api/mcp-server-test", Some(form))
        .await;
    assert_eq!(refused["ok"], json!(false), "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("private or reserved"),
        "{refused}"
    );

    // A save that sends the sentinel back keeps the key.
    let (status, updated) = client
        .send(
            "PUT",
            &format!("/api/mcp-servers/{id}"),
            Some(json!({
                "name": "self", "description": "Renamed description", "url": url,
                "headers": [{ "name": "Authorization", "value": SECRET_SENTINEL }],
                "allow_private_network": true, "timeout_secs": 20,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let stored = sc_mcp::require_mcp_server(&catalog, "self").await?;
    assert_eq!(stored.headers["Authorization"], format!("Bearer {token}"));
    assert_eq!(stored.description, "Renamed description");
    Ok(())
}

/// A server an agent's `mcp_server` trait names (in its `server` setting) is
/// not deleted out from under the agent, and the refusal names the agent; once
/// the agent no longer names it, it goes (TODO-agent-skills AS9.1).
#[tokio::test]
async fn a_server_an_agents_mcp_server_trait_names_is_not_deleted() -> sc_error::Result<()> {
    let (mut client, catalog, url, _db) = setup().await?;
    let token = mint(&mut client).await;
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

    let (status, created) = client
        .send(
            "POST",
            "/api/mcp-servers",
            Some(json!({
                "name": "self", "url": url, "transport": "streamable_http",
                "headers": [{ "name": "Authorization", "value": format!("Bearer {token}") }],
                "allow_private_network": true, "timeout_secs": 20,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();

    let agent = |traits: Value| {
        json!({
            "name": "librarian", "description": "", "provider": "house", "model": null,
            "system_prompt": "You help.", "traits": traits, "min_role": null, "attributes": {},
        })
    };
    let (status, saved) = client
        .send(
            "POST",
            "/api/agents",
            Some(agent(json!([
                { "trait": "mcp_server", "config": { "server": "self" } },
            ]))),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    assert_eq!(saved["error"], Value::Null, "{saved}");
    let agent_id = saved["id"].as_str().unwrap().to_owned();

    let (status, refused) = client
        .send("DELETE", &format!("/api/mcp-servers/{id}"), None)
        .await;
    assert_ne!(status, StatusCode::OK, "{refused}");
    assert!(
        refused.to_string().contains("agent `librarian`"),
        "{refused}"
    );
    assert!(
        sc_mcp::load_mcp_server_by_name(&catalog, "self")
            .await?
            .is_some()
    );

    // Once the agent no longer names it, it can go.
    let (status, edited) = client
        .send(
            "PUT",
            &format!("/api/agents/{agent_id}"),
            Some(agent(json!([]))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    let (status, deleted) = client
        .send("DELETE", &format!("/api/mcp-servers/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], json!(true));
    Ok(())
}

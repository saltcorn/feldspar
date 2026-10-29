//! Phase 1 integration test: the LLM-provider **configuration** API, driven
//! through the assembled router as a browser-based admin would.
//!
//! Most of it is the file-store configuration API's story with a different noun,
//! and is asserted here rather than trusted because "the same shape" is a claim
//! about code that was written twice. The part that is genuinely new is the
//! **secret**: a provider's API key must reach the browser as a sentinel and
//! never as itself, and a save that hands the sentinel back must leave the
//! stored key alone. That claim spans the declaration, the handler and the row,
//! so the only place it can be pinned is here — at the HTTP boundary, which is
//! the boundary the key must not cross.
//!
//! A provider's **models** are rows of their own (TODO §3a), with their own
//! endpoints: listed per provider, at most one default, refused on delete while
//! an agent calls them, discovered from the host's listing, and tested one at a
//! time.
//!
//! `testLlmModel` and `fetchLlmModels` are exercised against a **stub HTTP
//! server**, never a vendor: no test may require an API key or spend a token
//! (decision 7).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_llm::{CFG_API_KEY, CFG_BASE_URL, list_llm_providers};
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use sc_types::SECRET_SENTINEL;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

/// A cookie-jar-carrying client over the router (CSRF + session), mirroring the
/// helper in `admin_api.rs`.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router) -> Client {
        Client {
            router,
            cookies: HashMap::new(),
        }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let cookie_header = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie_header);
        }
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
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
            if let Ok(text) = raw.to_str() {
                let pair = text.split(';').next().unwrap_or("");
                if let Some((name, value)) = pair.split_once('=') {
                    if value.is_empty() {
                        self.cookies.remove(name);
                    } else {
                        self.cookies.insert(name.to_owned(), value.to_owned());
                    }
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
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

/// A stub endpoint replaying `body` with `status` to every request, so *Test*
/// and *Fetch models* have something to talk to that is not a vendor.
async fn stub_provider(status: u16, content_type: &'static str, body: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let head = head.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });
    format!("http://{addr}")
}

/// An Anthropic SSE body whose whole answer is `text`.
fn anthropic_answer(text: &str) -> String {
    let events = [
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_1", "role": "assistant", "content": [],
                "model": "claude-sonnet-4-5", "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 8, "output_tokens": 0,
                          "cache_read_input_tokens": null, "cache_creation_input_tokens": null},
            }
        }),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"output_tokens": 2}}),
    ];
    events
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<Vec<_>>()
        .join("")
}

/// A router over a real database with the platform tables bootstrapped, and an
/// admin logged in.
async fn setup() -> sc_error::Result<(Client, Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    // Neutralise any `users` table inherited from the template database before
    // bootstrap introspects, exactly as the other server tests do.
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
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_agent::bootstrap_agents(&catalog).await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps),
        sessions,
        &ServerConfig::default(),
    )?;

    let mut client = Client::new(router);
    client.send("GET", "/api/auth/status", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    Ok((client, catalog, db))
}

#[tokio::test]
async fn a_provider_is_created_listed_edited_and_deleted() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    let (status, body) = client.send("GET", "/api/llm-providers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());

    let (status, created) = client
        .send(
            "POST",
            "/api/llm-providers",
            Some(json!({
                "name": "house",
                "description": "The house Anthropic key",
                "backend": "anthropic",
                "config": { "api_key": "sk-ant-real" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["name"], json!("house"));
    assert!(created["id"].is_string());
    let id = created["id"].as_str().unwrap().to_owned();

    // Editing the description leaves everything else alone.
    let (status, updated) = client
        .send(
            "PUT",
            &format!("/api/llm-providers/{id}"),
            Some(json!({
                "name": "house",
                "description": "Renamed",
                "backend": "anthropic",
                "config": { "api_key": SECRET_SENTINEL, "base_url": "https://proxy.internal" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["description"], json!("Renamed"));
    assert_eq!(
        updated["config"][CFG_BASE_URL],
        json!("https://proxy.internal")
    );

    let (_, listed) = client.send("GET", "/api/llm-providers", None).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let (status, deleted) = client
        .send("DELETE", &format!("/api/llm-providers/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["deleted"], json!(true));
    assert!(list_llm_providers(&catalog).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn the_api_key_never_crosses_the_wire_and_a_save_does_not_destroy_it() -> sc_error::Result<()>
{
    let (mut client, catalog, _db) = setup().await?;

    let (status, created) = client
        .send(
            "POST",
            "/api/llm-providers",
            Some(json!({
                "name": "house",
                "description": "",
                "backend": "anthropic",
                "config": { "api_key": "sk-ant-real-secret" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();

    // Even the response to the *create* — the request that carried the key —
    // reads it back redacted, because redaction happens where the record is
    // serialised rather than in a screen.
    assert_eq!(created["config"][CFG_API_KEY], json!(SECRET_SENTINEL));
    assert!(
        !created.to_string().contains("sk-ant"),
        "not even a prefix may leave: {created}"
    );

    // And so does the listing, which is the endpoint a second reader would use.
    let (_, listed) = client.send("GET", "/api/llm-providers", None).await;
    assert!(
        !listed.to_string().contains("sk-ant"),
        "the listing leaked the key: {listed}"
    );
    assert_eq!(listed[0]["config"][CFG_API_KEY], json!(SECRET_SENTINEL));

    // The stored key, meanwhile, is the real one — redaction is on the way out,
    // not on the way in.
    let stored = list_llm_providers(&catalog).await?;
    assert_eq!(stored[0].setting(CFG_API_KEY), Some("sk-ant-real-secret"));

    // The admin edits the name and saves, handing back exactly what they were
    // shown. This is the failure the sentinel contract exists to prevent: a
    // masked field written over the key.
    let (status, updated) = client
        .send(
            "PUT",
            &format!("/api/llm-providers/{id}"),
            Some(json!({
                "name": "house-anthropic",
                "description": "",
                "backend": "anthropic",
                "config": { "api_key": SECRET_SENTINEL },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");

    let stored = list_llm_providers(&catalog).await?;
    assert_eq!(stored[0].name, "house-anthropic");
    assert_eq!(
        stored[0].setting(CFG_API_KEY),
        Some("sk-ant-real-secret"),
        "the sentinel must not be saved over the key"
    );

    // Replacing the key deliberately still works.
    let (status, _) = client
        .send(
            "PUT",
            &format!("/api/llm-providers/{id}"),
            Some(json!({
                "name": "house-anthropic",
                "description": "",
                "backend": "anthropic",
                "config": { "api_key": "sk-ant-rotated" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let stored = list_llm_providers(&catalog).await?;
    assert_eq!(stored[0].setting(CFG_API_KEY), Some("sk-ant-rotated"));
    Ok(())
}

#[tokio::test]
async fn the_backends_endpoint_declares_the_key_as_a_secret() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    let (status, body) = client.send("GET", "/api/llm-provider-backends", None).await;
    assert_eq!(status, StatusCode::OK);
    let backends = body.as_array().unwrap();
    assert_eq!(backends.len(), 3, "{body}");

    for backend in backends {
        let spec = backend["config_spec"].as_array().unwrap();
        let key = spec
            .iter()
            .find(|f| f["name"] == json!(CFG_API_KEY))
            .unwrap_or_else(|| panic!("{} declares no api_key", backend["name"]));
        // This flag is the whole of what tells the form to render a password
        // input; without it the key would be typed in the clear.
        assert_eq!(key["secret"], json!(true), "{backend}");
        // The model is a row of its own, never a provider setting.
        assert!(
            spec.iter().all(|f| f["name"] != json!("model")),
            "{backend}"
        );
        let base = spec
            .iter()
            .find(|f| f["name"] == json!(CFG_BASE_URL))
            .unwrap();
        if backend["name"] == json!("openai_chat") {
            // No one host, and a local one takes no key.
            assert_eq!(base["required"], json!(true), "{backend}");
            assert_eq!(key["required"], json!(false), "{backend}");
        } else {
            // Defaulted rather than required, which is what makes an
            // OpenAI-compatible endpoint a value the admin types.
            assert_eq!(key["required"], json!(true), "{backend}");
            assert_eq!(base["required"], json!(false));
            assert!(base["default"].is_string());
        }
    }
    Ok(())
}

/// Create a provider through the API, answering its id.
async fn create_provider(client: &mut Client, name: &str, config: Value) -> String {
    let (status, created) = client
        .send(
            "POST",
            "/api/llm-providers",
            Some(json!({ "name": name, "description": "", "backend": "anthropic", "config": config })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    created["id"].as_str().unwrap().to_owned()
}

/// Create a model under `provider` through the API, answering the response.
async fn create_model(client: &mut Client, provider: &str, body: Value) -> Value {
    let (status, created) = client
        .send(
            "POST",
            &format!("/api/llm-providers/{provider}/models"),
            Some(body),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    created
}

#[tokio::test]
async fn models_are_created_listed_edited_and_deleted_under_their_provider() -> sc_error::Result<()>
{
    let (mut client, _catalog, _db) = setup().await?;
    let provider = create_provider(&mut client, "house", json!({ "api_key": "sk-ant-x" })).await;

    let (status, spec) = client
        .send(
            "GET",
            "/api/llm-provider-backends/anthropic/model-settings",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{spec}");
    assert!(
        spec.as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == json!("price_input")),
        "{spec}"
    );

    // Blank settings are dropped, and what they resolve to comes back beside them.
    let sonnet = create_model(
        &mut client,
        &provider,
        json!({ "name": "claude-sonnet-5", "description": "", "is_default": true,
                "config": { "price_input": 3.0, "price_output": 15.0, "vision": "" } }),
    )
    .await;
    assert_eq!(
        sonnet["config"],
        json!({ "price_input": 3.0, "price_output": 15.0 })
    );
    assert_eq!(
        sonnet["capabilities"]["context_window"],
        json!(1_000_000),
        "{sonnet}"
    );
    assert_eq!(sonnet["prices"]["input"], json!(3.0), "{sonnet}");
    assert_eq!(
        sonnet["prices"]["cached_input"],
        Value::Null,
        "unknown, not zero: {sonnet}"
    );

    let opus = create_model(
        &mut client,
        &provider,
        json!({ "name": "claude-opus-5", "description": "", "is_default": false, "config": {} }),
    )
    .await;

    // Making opus the default takes the flag from sonnet.
    let opus_id = opus["id"].as_str().unwrap();
    let (status, updated) = client
        .send(
            "PUT",
            &format!("/api/llm-models/{opus_id}"),
            Some(
                json!({ "name": "claude-opus-5", "description": "the big one",
                         "is_default": true, "config": { "edit_format": "whole_file" } }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["capabilities"]["edit_format"], json!("whole_file"));

    let (_, listed) = client
        .send(
            "GET",
            &format!("/api/llm-providers/{provider}/models"),
            None,
        )
        .await;
    let defaults: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["is_default"] == json!(true))
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(defaults, ["claude-opus-5"], "{listed}");

    // A setting the backend does not declare is refused, naming it.
    let (status, err) = client
        .send(
            "POST",
            &format!("/api/llm-providers/{provider}/models"),
            Some(json!({ "name": "x", "description": "", "is_default": false,
                         "config": { "native_apply_patch": "yes" } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.to_string().contains("native_apply_patch"), "{err}");

    // A second row of one name under one provider is refused.
    let (status, err) = client
        .send(
            "POST",
            &format!("/api/llm-providers/{provider}/models"),
            Some(json!({ "name": "claude-opus-5", "description": "", "is_default": false, "config": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");

    let sonnet_id = sonnet["id"].as_str().unwrap();
    let (status, deleted) = client
        .send("DELETE", &format!("/api/llm-models/{sonnet_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["deleted"], json!(true));

    // Deleting the provider takes its remaining model with it.
    let (status, _) = client
        .send("DELETE", &format!("/api/llm-providers/{provider}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = client
        .send(
            "GET",
            &format!("/api/llm-providers/{provider}/models"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn a_model_an_agent_calls_is_not_deleted_or_renamed_out_from_under_it() -> sc_error::Result<()>
{
    let (mut client, catalog, _db) = setup().await?;
    let provider = create_provider(&mut client, "house", json!({ "api_key": "sk-ant-x" })).await;
    let sonnet = create_model(
        &mut client,
        &provider,
        json!({ "name": "claude-sonnet-5", "description": "", "is_default": true, "config": {} }),
    )
    .await;
    let opus = create_model(
        &mut client,
        &provider,
        json!({ "name": "claude-opus-5", "description": "", "is_default": false, "config": {} }),
    )
    .await;

    // One agent on the default, one naming opus.
    let registry = sc_agent::AgentRegistry::new();
    sc_agent::save_agent(
        &catalog,
        &registry,
        &sc_agent::Agent::new("helper", "house"),
    )
    .await?;
    sc_agent::save_agent(
        &catalog,
        &registry,
        &sc_agent::Agent::new("thinker", "house").model("claude-opus-5"),
    )
    .await?;

    for (model, agent) in [(&sonnet, "helper"), (&opus, "thinker")] {
        let id = model["id"].as_str().unwrap();
        let (status, err) = client
            .send("DELETE", &format!("/api/llm-models/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
        assert!(err.to_string().contains(agent), "{err}");
    }

    // Renaming opus would break the agent naming it.
    let opus_id = opus["id"].as_str().unwrap();
    let (status, err) = client
        .send(
            "PUT",
            &format!("/api/llm-models/{opus_id}"),
            Some(
                json!({ "name": "renamed", "description": "", "is_default": false, "config": {} }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.to_string().contains("thinker"), "{err}");

    // Taking the default flag from sonnet would break the agent naming no model.
    let sonnet_id = sonnet["id"].as_str().unwrap();
    let (status, err) = client
        .send(
            "PUT",
            &format!("/api/llm-models/{sonnet_id}"),
            Some(json!({ "name": "claude-sonnet-5", "description": "", "is_default": false, "config": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.to_string().contains("helper"), "{err}");

    // Opus may become the default: the agent on the default follows it, and the
    // agent naming opus still finds it.
    let (status, body) = client
        .send(
            "PUT",
            &format!("/api/llm-models/{opus_id}"),
            Some(
                json!({ "name": "claude-opus-5", "description": "now the default",
                         "is_default": true, "config": {} }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Sonnet, no longer the default and named by nobody, may now go.
    let (status, body) = client
        .send("DELETE", &format!("/api/llm-models/{sonnet_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The provider, likewise, names the agents.
    let (status, err) = client
        .send("DELETE", &format!("/api/llm-providers/{provider}"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.to_string().contains("helper"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_model_only_a_role_names_is_not_deleted_out_from_under_it() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let house = create_provider(&mut client, "house", json!({ "api_key": "sk-ant-x" })).await;
    create_model(
        &mut client,
        &house,
        json!({ "name": "claude-sonnet-5", "description": "", "is_default": true, "config": {} }),
    )
    .await;
    let gateway = create_provider(&mut client, "gateway", json!({ "api_key": "sk-ant-y" })).await;
    let opus = create_model(
        &mut client,
        &gateway,
        json!({ "name": "claude-opus-5", "description": "", "is_default": false, "config": {} }),
    )
    .await;

    // The agent itself calls `house`; only its strong role reaches `gateway`.
    sc_agent::save_agent(
        &catalog,
        &sc_agent::AgentRegistry::new(),
        &sc_agent::Agent::new("planner", "house").role(
            sc_agent::ModelRole::Strong,
            sc_agent::ModelRef::new("gateway", Some("claude-opus-5")),
        ),
    )
    .await?;

    let opus_id = opus["id"].as_str().unwrap();
    let (status, err) = client
        .send("DELETE", &format!("/api/llm-models/{opus_id}"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.to_string().contains("planner"), "{err}");

    let (status, err) = client
        .send("DELETE", &format!("/api/llm-providers/{gateway}"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.to_string().contains("planner"), "{err}");
    Ok(())
}

#[tokio::test]
async fn fetch_models_offers_the_names_that_have_no_row() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let base = stub_provider(
        200,
        "application/json",
        r#"{"data":[{"id":"claude-sonnet-5"},{"id":"claude-opus-5"},{"id":"claude-haiku-4-5"}]}"#
            .to_owned(),
    )
    .await;
    let provider = create_provider(
        &mut client,
        "house",
        json!({ "api_key": "sk-ant-x", "base_url": base }),
    )
    .await;
    create_model(
        &mut client,
        &provider,
        json!({ "name": "claude-sonnet-5", "description": "", "is_default": true, "config": {} }),
    )
    .await;

    let (status, body) = client
        .send(
            "POST",
            &format!("/api/llm-providers/{provider}/fetch-models"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true), "{body}");
    assert_eq!(
        body["names"],
        json!(["claude-haiku-4-5", "claude-opus-5"]),
        "{body}"
    );

    // A host with no listing is told so: an answer, not a broken request.
    let nowhere = stub_provider(404, "text/html", "not here".to_owned()).await;
    let other = create_provider(
        &mut client,
        "other",
        json!({ "api_key": "sk-ant-x", "base_url": nowhere }),
    )
    .await;
    let (status, body) = client
        .send(
            "POST",
            &format!("/api/llm-providers/{other}/fetch-models"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(false), "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("type the model name"),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn testing_a_model_reports_what_it_said_and_what_it_resolved_to() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let base = stub_provider(200, "text/event-stream", anthropic_answer("ok")).await;

    let (status, body) = client
        .send(
            "POST",
            "/api/llm-model-test",
            Some(json!({
                "backend": "anthropic",
                "config": { "api_key": "sk-ant-x", "base_url": base },
                "name": "claude-sonnet-4-5",
                "model_config": { "price_input": 3.0, "price_output": 15.0, "vision": "no" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true), "{body}");
    // The model's own reply, so an admin pointed at the wrong endpoint sees a
    // wrong answer rather than a green tick.
    assert_eq!(body["message"], json!("ok"));
    assert_eq!(body["model"], json!("claude-sonnet-4-5"));
    assert_eq!(body["capabilities"]["vision"], json!(false), "{body}");
    assert_eq!(
        body["capabilities"]["prompt_caching"],
        json!("explicit"),
        "{body}"
    );
    assert_eq!(body["prices"]["output"], json!(15.0), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_rejected_key_is_a_reported_failure_not_a_broken_request() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let base = stub_provider(
        401,
        "application/json",
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
            .to_owned(),
    )
    .await;

    let (status, body) = client
        .send(
            "POST",
            "/api/llm-model-test",
            Some(json!({
                "backend": "anthropic",
                "config": { "api_key": "sk-ant-wrong", "base_url": base },
                "name": "claude-sonnet-4-5",
            })),
        )
        .await;

    // 200, because the provider refusing *is* the answer to "does this work?".
    // An error status would make the UI render it as a broken request rather
    // than as the diagnostic the admin asked for.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(false), "{body}");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("invalid x-api-key") || message.contains("401"),
        "the provider's own words are the actionable part: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn testing_a_model_of_a_saved_provider_does_not_require_retyping_its_key()
-> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let base = stub_provider(200, "text/event-stream", anthropic_answer("ok")).await;

    let (_, created) = client
        .send(
            "POST",
            "/api/llm-providers",
            Some(json!({
                "name": "house",
                "description": "",
                "backend": "anthropic",
                "config": { "api_key": "sk-ant-real", "base_url": base },
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    // The form sends back exactly what it was shown — the sentinel — plus the
    // provider's id. Without the id there is nothing to resolve it against,
    // which is why the test body carries one.
    let (status, body) = client
        .send(
            "POST",
            "/api/llm-model-test",
            Some(json!({
                "provider_id": id,
                "backend": "anthropic",
                "config": created["config"],
                "name": "claude-sonnet-4-5",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_structurally_wrong_provider_is_refused_where_the_admin_can_fix_it()
-> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    // No API key.
    let (status, body) = client
        .send(
            "POST",
            "/api/llm-providers",
            Some(json!({
                "name": "house", "description": "", "backend": "anthropic",
                "config": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains(CFG_API_KEY), "{body}");

    // A backend nothing implements. A *configuration* error rather than an
    // invalid one — 422, as an unknown file-store backend is — because the
    // request was well-formed and the system cannot serve it, and the error
    // names what there is.
    let (status, body) = client
        .send(
            "POST",
            "/api/llm-providers",
            Some(json!({
                "name": "house", "description": "", "backend": "bedrock",
                "config": { "api_key": "x" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("anthropic"), "{body}");

    // A setting the backend never declared, which is a typo rather than
    // something to accept and ignore.
    let (status, body) = client
        .send(
            "POST",
            "/api/llm-providers",
            Some(json!({
                "name": "house", "description": "", "backend": "anthropic",
                "config": { "api_key": "x", "temperture": 0.5 },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("temperture"), "{body}");

    // Nothing was written by any of them.
    assert!(list_llm_providers(&catalog).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn two_providers_cannot_share_a_name() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    let body = json!({
        "name": "house", "description": "", "backend": "anthropic",
        "config": { "api_key": "sk-1" },
    });
    let (status, _) = client
        .send("POST", "/api/llm-providers", Some(body.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, err) = client.send("POST", "/api/llm-providers", Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.to_string().contains("house"), "{err}");
    assert_eq!(list_llm_providers(&catalog).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn every_provider_endpoint_requires_a_session() -> sc_error::Result<()> {
    let (admin, _catalog, _db) = setup().await?;

    // A client with no session. The admin API's `login` authenticates *admins*
    // only, so "a logged-in non-admin" is not a state this surface can be in —
    // anonymous is the whole of what an unauthorised caller looks like here, and
    // it is what these endpoints have to refuse.
    let mut user = Client::new(admin.router.clone());
    user.send("GET", "/api/auth/status", None).await;

    // A provider holds an API key; nothing below admin may read, write or test
    // one. Asserted per endpoint rather than for one, because each carries its
    // own `AuthRequirement` and a missing one would not show up anywhere else.
    for (method, path, body) in [
        ("GET", "/api/llm-providers".to_owned(), None),
        (
            "POST",
            "/api/llm-providers".to_owned(),
            Some(
                json!({"name": "x", "description": "", "backend": "anthropic",
                        "config": {"api_key": "k"}}),
            ),
        ),
        ("GET", "/api/llm-provider-backends".to_owned(), None),
        (
            "GET",
            "/api/llm-provider-backends/anthropic/model-settings".to_owned(),
            None,
        ),
        (
            "GET",
            format!("/api/llm-providers/{}/models", uuid::Uuid::new_v4()),
            None,
        ),
        (
            "POST",
            format!("/api/llm-providers/{}/models", uuid::Uuid::new_v4()),
            Some(json!({"name": "m", "description": "", "is_default": true, "config": {}})),
        ),
        (
            "PUT",
            format!("/api/llm-models/{}", uuid::Uuid::new_v4()),
            Some(json!({"name": "m", "description": "", "is_default": true, "config": {}})),
        ),
        (
            "DELETE",
            format!("/api/llm-models/{}", uuid::Uuid::new_v4()),
            None,
        ),
        (
            "POST",
            format!("/api/llm-providers/{}/fetch-models", uuid::Uuid::new_v4()),
            None,
        ),
        (
            "POST",
            "/api/llm-model-test".to_owned(),
            Some(json!({"backend": "anthropic", "config": {"api_key": "k"}, "name": "m"})),
        ),
    ] {
        let (status, _) = user.send(method, &path, body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} was reachable without a session"
        );
    }
    Ok(())
}

/// The provider the server's configuration file supplies: listed beside the
/// admin's, read-only, the default until the admin picks another — and its key
/// never leaves the server, not even through *Test*.
#[tokio::test]
async fn the_configuration_files_provider_is_usable_but_not_editable() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    sc_config::bootstrap(&catalog).await?;
    let host_endpoint = stub_provider(200, "text/event-stream", anthropic_answer("ok")).await;
    // Where an admin would like the key sent. It answers differently, so a
    // test that reached it would say so.
    let elsewhere = stub_provider(200, "text/event-stream", anthropic_answer("leaked")).await;

    let def = sc_llm::LlmProviderDef::new("hosted", sc_llm::ANTHROPIC_BACKEND)
        .with(CFG_API_KEY, "sk-operator")
        .with(CFG_BASE_URL, host_endpoint.clone());
    let host = sc_llm::HostLlmProvider::new(def, vec![], "claude-sonnet-5")?;
    sc_llm::set_host_llm_provider(&catalog, Some(host)).await?;

    // Listed, marked, the default, and redacted like any other.
    let (status, listed) = client.send("GET", "/api/llm-providers", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let hosted = listed[0].clone();
    assert_eq!(hosted["name"], "hosted");
    assert_eq!(hosted["from_config_file"], json!(true));
    assert_eq!(hosted["is_default"], json!(true));
    assert_eq!(hosted["config"]["api_key"], json!(SECRET_SENTINEL));
    let id = hosted["id"].as_str().unwrap().to_owned();

    // Neither it nor its models can be changed.
    let (status, body) = client
        .send(
            "PUT",
            &format!("/api/llm-providers/{id}"),
            Some(json!({
                "name": "hosted",
                "description": "",
                "backend": "anthropic",
                "config": { "api_key": SECRET_SENTINEL, "base_url": elsewhere },
            })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    let (status, body) = client
        .send("DELETE", &format!("/api/llm-providers/{id}"), None)
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    let (status, body) = client
        .send(
            "POST",
            &format!("/api/llm-providers/{id}/models"),
            Some(json!({ "name": "claude-opus-5-5", "description": "", "is_default": false, "config": {} })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");

    // *Test* uses the file's endpoint, whatever the form sends…
    let (status, body) = client
        .send(
            "POST",
            "/api/llm-model-test",
            Some(json!({
                "provider_id": id,
                "backend": "anthropic",
                "config": { "api_key": SECRET_SENTINEL, "base_url": elsewhere },
                "name": "claude-sonnet-5",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message"], json!("ok"), "{body}");
    // …and only the models the file lists.
    let (status, body) = client
        .send(
            "POST",
            "/api/llm-model-test",
            Some(json!({
                "provider_id": id,
                "backend": "anthropic",
                "config": hosted["config"],
                "name": "claude-opus-5-5",
            })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");

    // The admin adds their own and makes it the default.
    let mine = create_provider(
        &mut client,
        "mine",
        json!({ "api_key": "sk-admin", "base_url": host_endpoint }),
    )
    .await;
    let (status, body) = client
        .send("PUT", &format!("/api/llm-providers/{mine}/default"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_default"], json!(true));
    let (_, listed) = client.send("GET", "/api/llm-providers", None).await;
    let defaults: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["is_default"] == json!(true))
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(defaults, ["mine"]);
    assert_eq!(
        sc_llm::default_llm_provider(&catalog)
            .await?
            .map(|d| d.name),
        Some("mine".to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn models_have_a_kind_with_its_own_settings_default_and_test() -> sc_error::Result<()> {
    // TODO-agent-skills AS3.1.
    let (mut client, _catalog, _db) = setup().await?;
    let base = stub_provider(
        200,
        "application/json",
        r#"{"data":[{"index":0,"embedding":[0.1,0.2,0.3]}],"usage":{"prompt_tokens":1}}"#
            .to_owned(),
    )
    .await;
    let (status, created) = client
        .send(
            "POST",
            "/api/llm-providers",
            Some(
                json!({ "name": "local", "description": "", "backend": "openai_chat",
                         "config": { "base_url": base } }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let provider = created["id"].as_str().unwrap().to_owned();

    // Each kind declares its own settings; Anthropic serves chat only.
    let (status, spec) = client
        .send(
            "GET",
            "/api/llm-provider-backends/openai_chat/model-settings?kind=embedding",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{spec}");
    let names: Vec<&str> = spec
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(names, ["price_input"]);
    let (status, refused) = client
        .send(
            "GET",
            "/api/llm-provider-backends/anthropic/model-settings?kind=image",
            None,
        )
        .await;
    assert!(status.is_client_error(), "{status} {refused}");

    // A chat default and an embedding default live side by side.
    let chat = create_model(
        &mut client,
        &provider,
        json!({ "name": "llama3.2", "description": "", "is_default": true, "config": {} }),
    )
    .await;
    assert_eq!(chat["kind"], json!("chat"));
    let embedding = create_model(
        &mut client,
        &provider,
        json!({ "name": "nomic-embed-text", "description": "", "kind": "embedding",
                "is_default": true, "config": { "price_input": 0.01 } }),
    )
    .await;
    assert_eq!(embedding["kind"], json!("embedding"));
    assert_eq!(embedding["capabilities"], Value::Null, "{embedding}");
    let (_, listed) = client
        .send(
            "GET",
            &format!("/api/llm-providers/{provider}/models"),
            None,
        )
        .await;
    let defaults = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["is_default"] == json!(true))
        .count();
    assert_eq!(defaults, 2, "{listed}");

    // An edit that leaves the kind out keeps it; one that changes it is refused.
    let id = embedding["id"].as_str().unwrap();
    let (status, kept) = client
        .send(
            "PUT",
            &format!("/api/llm-models/{id}"),
            Some(
                json!({ "name": "nomic-embed-text", "description": "Local embeddings",
                         "is_default": true, "config": { "price_input": 0.01 } }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(kept["kind"], json!("embedding"));
    let (status, refused) = client
        .send(
            "PUT",
            &format!("/api/llm-models/{id}"),
            Some(
                json!({ "name": "nomic-embed-text", "description": "", "kind": "chat",
                         "is_default": true, "config": {} }),
            ),
        )
        .await;
    assert!(status.is_client_error(), "{status} {refused}");
    assert!(
        refused.to_string().contains("cannot become a chat model"),
        "{refused}"
    );

    // *Test* embeds a word and says what came back.
    let (status, tested) = client
        .send(
            "POST",
            "/api/llm-model-test",
            Some(json!({
                "provider_id": provider,
                "backend": "openai_chat",
                "config": { "base_url": base },
                "name": "nomic-embed-text",
                "kind": "embedding",
                "model_config": { "price_input": 0.01 },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["ok"], json!(true), "{tested}");
    assert_eq!(tested["message"], json!("a vector of 3 dimensions"));
    Ok(())
}

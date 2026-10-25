//! The admin UI's home page: `listEntities` against a real Postgres.
//!
//! What is asserted is what the page is built on: every kind of entity is in
//! the one list, each **save** stamps its `updated_at` (so the most recently
//! edited sort first, and editing an old one moves it back to the top), a
//! table — whose `_fd_tables` row is optional — is stamped by the schema
//! editor, and only a deep listing carries the definitions a deep search reads.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator, install_triggers,
};
use sc_stream::testing::ScriptedProvider;
use sc_stream::{ElementField, ElementType, StreamConfig, StreamProvider, StreamRegistry};
use sc_test_harness::TestDb;
use sc_types::BasicType;
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
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }
}

async fn setup() -> sc_error::Result<(Client, TestDb)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    // A scripted stream provider, so a stream can be saved without a broker.
    let mut registry = StreamRegistry::new();
    let provider = ScriptedProvider::new(
        "scripted",
        ElementType::json([ElementField::new("temperature", BasicType::Float)]),
    );
    registry.register(Arc::new(provider) as Arc<dyn StreamProvider>)?;
    let streams = sc_server::install_streams_with(
        &catalog,
        &dispatcher,
        Arc::new(registry),
        StreamConfig::default(),
    )
    .await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_triggers(dispatcher)
            .with_agents(agents)
            .with_models(models)
            .with_streams(streams),
    );
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;
    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    client
        .ok(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    Ok((client, db))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-entities-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn trigger_body(code: &str) -> Value {
    json!({
        "name": "greet", "description": "says hello", "when": "none", "channel": null,
        "only_if": null, "action": "run_js_code", "configuration": { "code": code },
        "min_role": null, "enabled": true,
    })
}

/// The entity of `kind` named `name` in a listing.
fn entity<'a>(list: &'a Value, kind: &str, name: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == json!(kind) && e["name"] == json!(name))
        .unwrap_or_else(|| panic!("no {kind} `{name}` in {list}"))
}

fn updated_at(list: &Value, kind: &str, name: &str) -> chrono::DateTime<chrono::Utc> {
    let at = entity(list, kind, name)["updated_at"]
        .as_str()
        .unwrap_or_else(|| panic!("{kind} `{name}` has no updated_at"));
    chrono::DateTime::parse_from_rfc3339(at).unwrap().into()
}

#[tokio::test]
async fn every_entity_is_listed_with_when_it_was_last_saved() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let pause = || tokio::time::sleep(Duration::from_millis(20));

    // A table, through the schema editor: no `_fd_tables` row is written for it.
    client
        .ok("POST", "/api/tables", Some(json!({ "name": "books" })))
        .await;
    pause().await;
    let trigger = client
        .ok(
            "POST",
            "/api/triggers",
            Some(trigger_body("return 'hello';")),
        )
        .await;
    let trigger_id = trigger["id"].as_str().unwrap().to_owned();
    pause().await;
    let dir = temp_dir("store");
    client
        .ok(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "docs", "description": "shared documents", "backend": "local",
                "config": { "path": dir.to_string_lossy() }, "min_role": null,
            })),
        )
        .await;

    let list = client.ok("GET", "/api/entities", None).await;
    let books = entity(&list, "table", "books");
    assert_eq!(books["id"], json!("books"));
    assert_eq!(books["runnable"], json!(true));
    let greet = entity(&list, "trigger", "greet");
    assert_eq!(greet["id"], json!(trigger_id));
    assert_eq!(greet["description"], json!("says hello"));
    assert_eq!(greet["details"], json!(["none", "run_js_code"]));
    // No floor on a trigger is admin-only, named as the role is named.
    assert_eq!(greet["access"], json!("Admin"));
    assert_eq!(greet["runnable"], json!(true));
    let docs = entity(&list, "file_store", "docs");
    assert_eq!(docs["details"][0], json!("local"));
    // A shallow listing carries no definitions.
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .all(|e| e["definition"].is_null())
    );

    // Saved in this order, so stamped in this order.
    assert!(updated_at(&list, "table", "books") < updated_at(&list, "trigger", "greet"));
    assert!(updated_at(&list, "trigger", "greet") < updated_at(&list, "file_store", "docs"));

    // Editing the oldest trigger makes it the newest edit.
    pause().await;
    client
        .ok(
            "PUT",
            &format!("/api/triggers/{trigger_id}"),
            Some(trigger_body("return 'hello again';")),
        )
        .await;
    // And changing the table's schema stamps the table, which still has no
    // overlay row.
    pause().await;
    client
        .ok(
            "POST",
            "/api/tables/books/fields",
            Some(json!({ "name": "title", "type": "text" })),
        )
        .await;
    let list = client.ok("GET", "/api/entities", None).await;
    assert!(updated_at(&list, "file_store", "docs") < updated_at(&list, "trigger", "greet"));
    assert!(updated_at(&list, "trigger", "greet") < updated_at(&list, "table", "books"));
    assert_eq!(
        entity(&list, "table", "books")["details"],
        json!(["1 field"])
    );

    // A deep listing carries the definition, which is what a deep search reads.
    let deep = client.ok("GET", "/api/entities?deep=true", None).await;
    let definition = entity(&deep, "trigger", "greet")["definition"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(definition.contains("hello again"), "{definition}");
    assert!(
        entity(&deep, "table", "books")["definition"]
            .as_str()
            .unwrap()
            .contains("title")
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn streams_are_listed_and_triggers_name_their_channel() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    client
        .ok("POST", "/api/tables", Some(json!({ "name": "readings" })))
        .await;
    client
        .ok(
            "POST",
            "/api/streams",
            Some(json!({
                "name": "boiler", "description": "the boiler's thermometer",
                "provider": "scripted", "configuration": {}, "min_role": null,
                "enabled": false,
            })),
        )
        .await;
    let on = |name: &str, when: &str, channel: &str| {
        json!({
            "name": name, "description": "", "when": when, "channel": channel,
            "only_if": null, "action": "run_js_code", "configuration": { "code": "1" },
            "min_role": null, "enabled": true,
        })
    };
    client
        .ok("POST", "/api/triggers", Some(on("on_reading", "stream", "boiler")))
        .await;
    client
        .ok("POST", "/api/triggers", Some(on("audit", "insert", "readings")))
        .await;

    let list = client.ok("GET", "/api/entities", None).await;
    let boiler = entity(&list, "stream", "boiler");
    assert_eq!(boiler["description"], json!("the boiler's thermometer"));
    assert_eq!(boiler["details"], json!(["scripted", "off"]));
    assert_eq!(boiler["access"], json!("Admin"));
    // Switched off, so there is nothing to observe.
    assert_eq!(boiler["runnable"], json!(false));
    assert!(boiler["updated_at"].is_string(), "{boiler}");
    assert!(boiler["channel"].is_null());

    // A trigger's channel is what the diagram joins it to.
    assert_eq!(entity(&list, "trigger", "on_reading")["channel"], json!("boiler"));
    assert_eq!(entity(&list, "trigger", "audit")["channel"], json!("readings"));
    assert!(entity(&list, "table", "readings")["channel"].is_null());

    let deep = client.ok("GET", "/api/entities?deep=true", None).await;
    assert!(
        entity(&deep, "stream", "boiler")["definition"]
            .as_str()
            .unwrap()
            .contains("scripted")
    );
    Ok(())
}

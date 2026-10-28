//! Phase 1.4 integration test: the file-store **configuration** API, driven
//! through the assembled router as a browser-based admin would.
//!
//! `file_manager.rs` covers browsing and editing a store's *contents*; this
//! covers managing the stores themselves — create, edit, delete, and the two
//! things that only this layer can get right:
//!
//! 1. the live registry is kept in step with the definitions (a renamed store's
//!    old handle is disconnected, a deleted store's handle too), and
//! 2. the delete reference check composes both halves — `sc-catalog` can see
//!    `File` fields, only this layer can see applications.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
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

/// A fresh, unique temp directory (created on disk) for one store.
fn temp_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "sc-store-api-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// A router over a real database with the platform tables bootstrapped, and an
/// admin logged in. No file store is connected — these tests create them through
/// the API, which is the point.
async fn setup() -> sc_error::Result<(Client, Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    // Neutralise any `users` table inherited from the template database before
    // bootstrap introspects: a stray one in another schema would be found and
    // `sc_auth::bootstrap` would skip creating the real one. A no-op against a
    // clean template, which is what the configured test template should be.
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
async fn a_store_is_created_listed_and_connected_through_the_api() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let dir = temp_dir("create");

    // Nothing configured yet.
    let (status, body) = client.send("GET", "/api/file-stores", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());

    let (status, created) = client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "docs",
                "description": "Shared documents",
                "backend": "local",
                "config": { "path": dir.to_string_lossy() },
                "min_role": 40,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["name"], json!("docs"));
    assert_eq!(created["min_role"], json!(40));
    // Creating connects it, so the admin learns immediately whether the
    // directory is actually reachable rather than at next boot.
    assert_eq!(created["connected"], json!(true));
    assert_eq!(created["error"], Value::Null);
    assert_eq!(created["is_git_repo"], json!(false));
    assert!(created["id"].is_string());

    // It is connected in the live registry, so the file manager can reach it
    // with no restart.
    assert!(catalog.file_store("docs")?.is_some());

    // And it is listed with its definition intact.
    let (_, body) = client.send("GET", "/api/file-stores", None).await;
    let stores = body.as_array().unwrap();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["description"], json!("Shared documents"));
    assert_eq!(stores[0]["config"]["path"], json!(dir.to_string_lossy()));

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// A store pointing somewhere unreachable still saves, and reports why it is not
/// connected. This is the §1.2 split arriving at the UI: the definition is
/// structurally fine, so it must be storable and editable — editing it is the
/// repair — while the reachability failure is reported rather than thrown away.
#[tokio::test]
async fn an_unreachable_store_is_saved_and_reports_why_it_is_not_connected() -> sc_error::Result<()>
{
    let (mut client, _catalog, _db) = setup().await?;

    let (status, created) = client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "gone",
                "description": "",
                "backend": "local",
                "config": { "path": "/definitely/not/here" },
                "min_role": Value::Null,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "saving must still succeed");
    assert_eq!(created["connected"], json!(false));
    let error = created["error"]
        .as_str()
        .expect("a reason must be reported");
    assert!(error.contains("gone"), "{error}");
    // No instance to ask, so no answer invented.
    assert_eq!(created["is_git_repo"], Value::Null);

    // It stays listed, which is what makes it fixable.
    let (_, body) = client.send("GET", "/api/file-stores", None).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    Ok(())
}

/// A structurally broken definition is refused outright — the §1.2 distinction
/// between "you mis-configured this" and "this is not currently reachable".
#[tokio::test]
async fn a_misconfigured_store_is_rejected() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    // `local` requires a `path`.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "docs", "description": "", "backend": "local",
                "config": {}, "min_role": Value::Null,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // An unknown backend.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "docs", "description": "", "backend": "s3",
                "config": { "bucket": "things" }, "min_role": Value::Null,
            })),
        )
        .await;
    assert_ne!(status, StatusCode::CREATED);

    // A role outside the scale is rejected rather than clamped: clamping would
    // quietly change who can reach the store.
    let dir = temp_dir("roles");
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "docs", "description": "", "backend": "local",
                "config": { "path": dir.to_string_lossy() }, "min_role": 999,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, body) = client.send("GET", "/api/file-stores", None).await;
    assert!(body.as_array().unwrap().is_empty(), "nothing was stored");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// Editing a store's path takes effect with no restart, and **renaming it
/// disconnects the old handle** — the composition step §1.3 could not perform,
/// because saving deliberately does not touch the registry.
#[tokio::test]
async fn editing_repoints_live_and_renaming_disconnects_the_old_name() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let old_dir = temp_dir("old");
    let new_dir = temp_dir("new");
    std::fs::write(old_dir.join("marker.txt"), b"old").unwrap();
    std::fs::write(new_dir.join("marker.txt"), b"new").unwrap();

    let (_, created) = client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "docs", "description": "", "backend": "local",
                "config": { "path": old_dir.to_string_lossy() }, "min_role": Value::Null,
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let before = catalog
        .require_file_store("docs")?
        .read("marker.txt")
        .await?;
    assert_eq!(&before[..], b"old");

    // Rename *and* repoint in one edit.
    let (status, updated) = client
        .send(
            "PUT",
            &format!("/api/file-stores/{id}"),
            Some(json!({
                "name": "handbook", "description": "", "backend": "local",
                "config": { "path": new_dir.to_string_lossy() }, "min_role": Value::Null,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["name"], json!("handbook"));
    assert_eq!(updated["connected"], json!(true));
    // The id is the path's, not the body's.
    assert_eq!(updated["id"], json!(id));

    // The new name serves the new directory, immediately.
    let after = catalog
        .require_file_store("handbook")?
        .read("marker.txt")
        .await?;
    assert_eq!(&after[..], b"new");

    // And the old name is gone from the registry. Without the explicit
    // disconnect the old handle would still be serving the old directory under
    // a name that no longer has a definition.
    assert!(
        catalog.file_store("docs")?.is_none(),
        "a rename must disconnect the old name"
    );

    let (_, body) = client.send("GET", "/api/file-stores", None).await;
    assert_eq!(
        body.as_array().unwrap().len(),
        1,
        "still one store, renamed"
    );

    std::fs::remove_dir_all(&old_dir).ok();
    std::fs::remove_dir_all(&new_dir).ok();
    Ok(())
}

/// Deleting removes the row **and** disconnects the handle. Both are needed:
/// §1.3 proved the row can be gone while the file manager happily goes on
/// browsing the store.
#[tokio::test]
async fn deleting_removes_the_definition_and_stops_it_serving() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let dir = temp_dir("delete");

    let (_, created) = client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "scratch", "description": "", "backend": "local",
                "config": { "path": dir.to_string_lossy() }, "min_role": Value::Null,
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(catalog.file_store("scratch")?.is_some());

    let (status, body) = client
        .send("DELETE", &format!("/api/file-stores/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(true));

    assert!(
        catalog.file_store("scratch")?.is_none(),
        "a delete must disconnect the handle, not just drop the row"
    );
    let (_, body) = client.send("GET", "/api/file-stores", None).await;
    assert!(body.as_array().unwrap().is_empty());

    // The bytes are untouched — a store definition points at data that exists
    // independently of Saltcorn (§1.1).
    assert!(dir.is_dir());

    // Deleting again is a 404, not a silent success.
    let (status, _) = client
        .send("DELETE", &format!("/api/file-stores/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// The delete reference check composes both halves. `sc-catalog` cannot see
/// applications — they live a crate above it — so this layer collects them. A
/// store still holding an application's source must not be deletable.
#[tokio::test]
async fn a_store_used_by_an_application_cannot_be_deleted() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let dir = temp_dir("inuse");

    let (_, created) = client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "apps", "description": "", "backend": "local",
                "config": { "path": dir.to_string_lossy() }, "min_role": Value::Null,
            })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    // An application whose code framework builds out of that store.
    let app = sc_app::Application::new(
        "My Blog",
        "blog",
        sc_app::FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "npm run build"),
    );
    sc_app::save_application(&catalog, &app).await?;

    let (status, body) = client
        .send("DELETE", &format!("/api/file-stores/{id}"), None)
        .await;
    // A refused delete is an `Invalid` — the admin's to fix by removing the
    // reference — which the router maps to 400. (422 is reserved for
    // Application-kind errors, such as a build carrying bundler diagnostics.)
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("My Blog"),
        "the error must name what still uses the store: {body}"
    );

    // Refused, not half-done: still defined and still serving.
    assert!(catalog.file_store("apps")?.is_some());
    let (_, body) = client.send("GET", "/api/file-stores", None).await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// The backends registry reaches the UI as data, so the create/edit form can
/// render controls for a backend it knows nothing about.
#[tokio::test]
async fn backends_are_listed_with_their_settings_spec() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    let (status, body) = client.send("GET", "/api/file-store-backends", None).await;
    assert_eq!(status, StatusCode::OK);
    let backends = body.as_array().unwrap();
    // Every registered backend, in registry order. `git_store_api.rs` covers
    // what the git one declares; this is about the shape they all share.
    let names: Vec<&str> = backends
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["local", "git"]);

    let spec = backends[0]["config_spec"].as_array().unwrap();
    let names: Vec<&str> = spec.iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["path", "create"]);

    // Everything the form needs to render a control, and nothing the UI has to
    // know about `local` specifically.
    let path = &spec[0];
    assert_eq!(path["label"], json!("Directory"));
    assert_eq!(path["type"], json!("text"));
    assert_eq!(path["required"], json!(true));
    let create = &spec[1];
    assert_eq!(create["type"], json!("bool"));
    assert_eq!(create["required"], json!(false));
    assert_eq!(create["default"], json!(false));

    Ok(())
}

/// A store connected by `--file-store` has no row, and must still be visible —
/// otherwise a developer running with the flag sees an empty screen and cannot
/// reach the store they just connected. Its null id says "nothing to edit here".
#[tokio::test]
async fn a_flag_connected_store_is_listed_but_has_no_id() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let dir = temp_dir("ephemeral");

    // What `--file-store scratch=DIR` does at boot: connect, without a row.
    catalog.connect_file_store(sc_files::connect_from_def(&sc_files::FileStoreDef::local(
        "scratch",
        dir.to_string_lossy(),
    ))?)?;

    let (status, body) = client.send("GET", "/api/file-stores", None).await;
    assert_eq!(status, StatusCode::OK);
    let stores = body.as_array().unwrap();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["name"], json!("scratch"));
    assert_eq!(stores[0]["connected"], json!(true));
    assert_eq!(
        stores[0]["id"],
        Value::Null,
        "a null id is what tells the UI this store cannot be edited or deleted"
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// §1.6: the `store` setting reaches the UI as a **resolved** pick-list.
///
/// The server evaluates the server-query source before handing the spec over, so
/// the admin SPA renders a select without needing a query evaluator of its own —
/// which is why this did not have to wait for `ui/form-runtime`.
#[tokio::test]
async fn the_store_setting_arrives_as_a_resolved_pick_list() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let dir = temp_dir("picklist");

    // With no stores configured, the pick-list is empty but still a pick-list.
    let (status, frameworks) = client.send("GET", "/api/frameworks", None).await;
    assert_eq!(status, StatusCode::OK);
    let store_field = |body: &Value| -> Value {
        body.as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == json!("code"))
            .unwrap()["config_spec"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == json!("store"))
            .unwrap()
            .clone()
    };
    assert_eq!(store_field(&frameworks)["options"], json!([]));

    // Configure two stores …
    for name in ["apps", "uploads"] {
        client
            .send(
                "POST",
                "/api/file-stores",
                Some(json!({
                    "name": name, "description": "", "backend": "local",
                    "config": { "path": dir.to_string_lossy() }, "min_role": Value::Null,
                })),
            )
            .await;
    }

    // … and they are what the setting now offers, in order.
    let (_, frameworks) = client.send("GET", "/api/frameworks", None).await;
    let field = store_field(&frameworks);
    assert_eq!(field["options"], json!(["apps", "uploads"]));
    // Still described the same way otherwise — the UI renders it as a select
    // purely because `options` is non-empty, with no special case for stores.
    assert_eq!(field["type"], json!("text"));
    assert_eq!(field["required"], json!(true));

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

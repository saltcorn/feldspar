//! Phase 7 integration test: the schema-editing endpoints, and a **mounted
//! application re-projecting** when the schema moves underneath it (§3.3, §13.2).
//!
//! Two things are asserted here that no unit test can:
//!
//! - `dropTable` and `deleteField` exist over the same `sc_api::schema_edit`
//!   module the agent's `edit_schema` calls, refuse the same things by name, and
//!   take the overlay rows with them. An agent must not be able to do something
//!   the admin UI cannot, and these endpoints are what makes that true.
//! - The [`SchemaObserver`](sc_catalog::SchemaObserver) seam works end to end: a
//!   field dropped through the schema editor removes the file endpoints a mounted
//!   app's REST projection carried for it, and a role floor tightened through it
//!   is enforced on the next request — with **no** `refresh_table` call in any
//!   handler, because there are none left.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_app::{ApiConfig, Application, CodeFramework, FrameworkRef, bootstrap, save_application};
use sc_auth::{Role, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "blog.example.com";

/// A cookie-jar client over the router (session + CSRF), as a browser SPA would.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.host);
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
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        let value = match bytes.is_empty() {
            true => Value::Null,
            false => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        };
        (status, value)
    }
}

struct Harness {
    client: Client,
    router: Router,
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    _db: TestDb,
    _dir: TempDir,
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-schema-edit-{}-{tag}-{:?}",
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

/// An admin router with a logged-in admin, a `posts` table, and an `apps` file
/// store — plus the schema observer installed on the catalog, exactly as the
/// server installs it at boot.
async fn setup(tag: &str) -> sc_error::Result<Harness> {
    let db = TestDb::new().await?;
    let dir = TempDir::new(tag);
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
    bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &dir.0)?))?;

    // No `set_schema_observer` here on purpose: `admin_handlers` installs it,
    // so this test also asserts that a server which assembles the admin API gets
    // live re-projection without remembering to ask for it.
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;
    let mut client = Client {
        router: router.clone(),
        host: BASE_DOMAIN.to_owned(),
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    client
        .send("POST", "/api/tables", Some(json!({ "name": "posts" })))
        .await;
    // The key, declared like any other field — nothing invents one (GOALS) —
    // and needed here because `comments.post` points at it.
    client
        .send(
            "POST",
            "/api/tables/posts/fields",
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    client
        .send(
            "POST",
            "/api/tables/posts/fields",
            Some(json!({ "name": "title", "type": "text" })),
        )
        .await;
    Ok(Harness {
        client,
        router,
        catalog,
        apps,
        _db: db,
        _dir: dir,
    })
}

/// Mount an app over `posts` with a REST API at `/api`, so the test has a live
/// projection to watch move.
async fn mount_blog(h: &Harness) -> sc_error::Result<()> {
    let framework_ref = FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts");
    let app = Application::new("Blog", "blog", framework_ref)
        .with_table(TableId("posts".to_owned()))
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"));
    save_application(&h.catalog, &app).await?;
    let framework = Arc::new(CodeFramework::new("code", sc_app::AssetBundle::new()));
    h.apps.remount(MountedApp::new(app, framework, &h.catalog)?);
    Ok(())
}

/// GET `path` on the app's host, anonymously.
async fn app_get(router: &Router, path: &str) -> StatusCode {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::HOST, APP_HOST)
        .body(Body::empty())
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn drop_table_and_delete_field_over_the_endpoints() -> sc_error::Result<()> {
    let mut h = setup("endpoints").await?;

    // Two connected tables, so the refusals have something to refuse.
    h.client
        .send("POST", "/api/tables", Some(json!({ "name": "comments" })))
        .await;
    let (status, body) = h
        .client
        .send(
            "POST",
            "/api/tables/comments/fields",
            Some(json!({
                "name": "post",
                "type": "int8",
                "kind": { "type": "key", "target_table": "posts", "target_field": "id" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // A table another table's key points at is refused **by name**, listing the
    // field to remove first.
    let (status, body) = h.client.send("DELETE", "/api/tables/posts", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("comments.post"),
        "{body}"
    );
    assert!(h.catalog.get("posts")?.is_some());

    // A primary key is refused too.
    let (status, body) = h
        .client
        .send("DELETE", "/api/tables/posts/fields/id", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("primary key"),
        "{body}"
    );

    // Drop the key, then the table it pointed at — and the overlay row the key
    // had goes with the column.
    let (status, body) = h
        .client
        .send("DELETE", "/api/tables/comments/fields/post", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["dropped"], json!("post"));
    assert!(h.catalog.require("comments")?.field("post").is_none());
    assert!(
        sc_catalog::load_field_meta_by_field(&h.catalog, "comments", "post")
            .await?
            .is_none()
    );

    let (status, body) = h.client.send("DELETE", "/api/tables/posts", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["dropped"], json!("posts"));
    assert!(h.catalog.get("posts")?.is_none());
    // It really is gone from the database, not merely from the settings.
    let (_, tables) = h.client.send("GET", "/api/tables", None).await;
    let names: Vec<&str> = tables
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(!names.contains(&"posts"), "{names:?}");
    Ok(())
}

#[tokio::test]
async fn a_mounted_app_reprojects_when_the_schema_moves_under_it() -> sc_error::Result<()> {
    let mut h = setup("reproject").await?;
    // A `File` field contributes download/upload endpoints to an app's REST
    // projection (§4), so it is the field whose *loss* is visible as a route.
    let (status, body) = h
        .client
        .send(
            "POST",
            "/api/tables/posts/fields",
            Some(json!({
                "name": "cover",
                "type": "text",
                "kind": { "type": "file", "store": "apps", "folder": "covers" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // A reader who can see the table, so the role-floor half below has a caller
    // whose answer *changes* rather than one refused at the door either way.
    save_role(&h.catalog, &Role::new(80, "Reader")).await?;
    create_user(&h.catalog, "reader@example.com", "reader-pw", 80).await?;
    h.client
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "", "description": "",
                "min_role_read": 80, "min_role_write": 1,
                "ownership_formula": "", "rls_enabled": false, "sync_enabled": false,
            })),
        )
        .await;
    mount_blog(&h).await?;

    // The download route is mounted: an *anonymous* probe is refused by its auth
    // requirement (401), which is the endpoint being there. When it is gone the
    // app's own framework answers the path instead, with 404.
    assert_eq!(
        app_get(&h.router, "/api/posts/1/cover").await,
        StatusCode::UNAUTHORIZED,
        "expected the download endpoint to exist and gate the caller"
    );

    // Drop the field through the schema editor. No handler calls
    // `refresh_table` any more: the catalog's observer is what re-projects.
    let (status, body) = h
        .client
        .send("DELETE", "/api/tables/posts/fields/cover", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        app_get(&h.router, "/api/posts/1/cover").await,
        StatusCode::NOT_FOUND,
        "the endpoint went with the field it was projected from"
    );

    // …and a role floor tightened through the same module is enforced on the
    // next request, not after a restart.
    let mut reader = Client {
        router: h.router.clone(),
        host: APP_HOST.to_owned(),
        cookies: HashMap::new(),
    };
    reader.send("GET", "/", None).await;
    let (status, body) = reader
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "reader@example.com", "password": "reader-pw" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK, "role 80 meets a floor of 80");

    h.client
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "", "description": "",
                "min_role_read": 1, "min_role_write": 1,
                "ownership_formula": "", "rls_enabled": false, "sync_enabled": false,
            })),
        )
        .await;
    let (status, _) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the tightened floor took effect with no restart"
    );
    Ok(())
}

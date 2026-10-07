//! **Clear all**, driven through the assembled router as the Development tab's
//! dialog drives it: the preview it opens with, then the clear it confirms.
//!
//! The claims: every table the admin made in the primary database is dropped —
//! including two that point at each other and one `users` points at, which the
//! schema editor will only drop in the right order — every file store's
//! definition goes, only the ticked stores leave the disk, and every account,
//! role and setting goes too: the admin who pressed the button is signed out,
//! the status says no user exists (the admin UI's create-first-user screen),
//! and creating the first user works again.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator, install_agents, install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// A cookie-jar-carrying client over the router (CSRF + session), as the other
/// admin-API tests use — plus the two calls whose bodies are not JSON.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, bytes, _) = self
            .raw(
                method,
                path,
                body.map(|b| (serde_json::to_vec(&b).unwrap(), "application/json")),
            )
            .await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// The bytes of a response, and its headers — what a download has to be read
    /// through.
    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Option<(Vec<u8>, &str)>,
    ) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
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
        if method != "GET"
            && method != "HEAD"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some((bytes, content_type)) => builder
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(bytes))
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
        let headers = response.headers().clone();
        // Generous: a backup of a handful of rows and one small file, plus the
        // zip's own overhead.
        let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        (status, bytes, headers)
    }
}

struct Server {
    client: Client,
    catalog: Arc<Catalog>,
    _db: TestDb,
}

async fn setup() -> sc_error::Result<Server> {
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
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_viewpattern::bootstrap(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_agents(agents)
            .with_triggers(dispatcher),
    );
    let router = build_router_with_apps(
        &admin_endpoints(),
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
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    Ok(Server {
        client,
        catalog,
        _db: db,
    })
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "sc-clear-all-{name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

async fn ok(client: &mut Client, method: &str, path: &str, body: Value) -> Value {
    let (status, out) = client.send(method, path, Some(body)).await;
    assert!(status.is_success(), "{method} {path}: {status} {out}");
    out
}

#[tokio::test]
async fn clear_all_empties_the_installation_and_removes_only_ticked_stores() -> sc_error::Result<()>
{
    let mut server = setup().await?;
    let c = &mut server.client;

    // Two tables that reference each other, and a `users` column that points
    // at one of them.
    for table in ["authors", "books"] {
        ok(c, "POST", "/api/tables", json!({ "name": table })).await;
        ok(
            c,
            "POST",
            &format!("/api/tables/{table}/fields"),
            json!({ "name": "id", "type": "int", "primary_key": true }),
        )
        .await;
    }
    for (table, field, target) in [
        ("books", "author", "authors"),
        ("authors", "favourite", "books"),
        ("users", "author", "authors"),
    ] {
        ok(
            c,
            "POST",
            &format!("/api/tables/{table}/fields"),
            json!({ "name": field, "kind": { "type": "key", "target_table": target, "target_field": "id" } }),
        )
        .await;
    }

    // An extra role and a stored setting, which go too.
    ok(
        c,
        "POST",
        "/api/roles",
        json!({ "role": 40, "name": "Staff", "description": "" }),
    )
    .await;
    sc_config::set_config(&server.catalog, sc_config::SMTP_PORT, json!(2525)).await?;
    // The TLS settings and the ACME cache, which stay: they are how this host
    // serves, and clearing them would take it off the port its proxy forwards
    // to at the next restart.
    sc_config::set_config_many(
        &server.catalog,
        &json!({
            sc_config::SSL_MODE: sc_config::MODE_LETSENCRYPT,
            sc_config::ACME_CONTACT_EMAIL: "ops@example.com",
            sc_config::REDIRECT_HTTP_TO_HTTPS: false,
        })
        .as_object()
        .unwrap()
        .clone(),
    )
    .await?;
    let acme = sc_config::AcmeCache::new(server.catalog.clone());
    acme.store("account", b"the account key").await?;

    // Two file stores with a file each; only one will be ticked.
    let gone = temp_dir("gone");
    let kept = temp_dir("kept");
    for (name, dir) in [("gone", &gone), ("kept", &kept)] {
        ok(
            c,
            "POST",
            "/api/file-stores",
            json!({ "name": name, "description": "", "backend": "local",
                    "config": { "path": dir.to_string_lossy() }, "min_role": null }),
        )
        .await;
        ok(
            c,
            "POST",
            &format!("/api/file-stores/{name}/write"),
            json!({ "path": "a.txt", "text": "hello" }),
        )
        .await;
    }

    // The preview lists both stores with where they are on disk.
    let (status, preview) = c.send("GET", "/api/clear-all", None).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let stores = preview["file_stores"].as_array().unwrap();
    assert_eq!(stores.len(), 2, "{preview}");
    assert!(
        stores
            .iter()
            .any(|s| s["name"] == "gone" && s["directory"] == json!(gone.to_string_lossy())),
        "{preview}"
    );

    let report = ok(
        c,
        "POST",
        "/api/clear-all",
        json!({ "delete_from_disk": ["gone"] }),
    )
    .await;
    assert_eq!(report["warnings"], json!([]), "{report}");

    // The tables are gone.
    assert!(server.catalog.get("books")?.is_none());
    assert!(server.catalog.get("authors")?.is_none());
    let users = server
        .catalog
        .get("users")?
        .expect("the users table is the system's");
    assert!(
        users.field("author").is_none(),
        "a column the admin added to users goes"
    );
    assert!(
        sc_config::stored_config(&server.catalog, sc_config::SMTP_PORT)
            .await?
            .is_none(),
        "settings go"
    );
    for (key, value) in [
        (sc_config::SSL_MODE, json!(sc_config::MODE_LETSENCRYPT)),
        (sc_config::ACME_CONTACT_EMAIL, json!("ops@example.com")),
        (sc_config::REDIRECT_HTTP_TO_HTTPS, json!(false)),
    ] {
        assert_eq!(
            sc_config::stored_config(&server.catalog, key).await?,
            Some(value),
            "the TLS setting `{key}` stays"
        );
    }
    assert_eq!(
        acme.load("account").await?.as_deref(),
        Some(&b"the account key"[..]),
        "the ACME cache stays"
    );
    let roles = sc_auth::list_roles(&server.catalog).await?;
    assert_eq!(
        roles.iter().map(|r| r.role).collect::<Vec<_>>(),
        vec![sc_auth::ROLE_ADMIN, sc_auth::ROLE_PUBLIC],
        "only the two built-in roles, seeded again"
    );

    // No file stores, the ticked one is off the disk, the other is untouched.
    assert!(
        sc_catalog::list_file_stores(&server.catalog)
            .await?
            .is_empty()
    );
    assert!(!gone.exists(), "the ticked store's directory is removed");
    assert!(
        kept.join("a.txt").exists(),
        "an unticked store keeps its files"
    );
    std::fs::remove_dir_all(&kept).ok();

    // Nobody is left: the caller is signed out, and the status is the one the
    // admin UI answers with the create-first-user screen.
    let (_, status_body) = server.client.send("GET", "/api/auth/status", None).await;
    assert_eq!(
        status_body["any_user_exists"],
        json!(false),
        "{status_body}"
    );
    assert_eq!(status_body["current_user"], Value::Null, "{status_body}");
    let (status, _) = server.client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Which works: a new first admin, signed straight in.
    ok(
        &mut server.client,
        "POST",
        "/api/first-user",
        json!({ "email": "new@example.com", "password": PASSWORD }),
    )
    .await;

    // Clearing an installation that is already empty is not an error.
    let report = ok(
        &mut server.client,
        "POST",
        "/api/clear-all",
        json!({ "delete_from_disk": [] }),
    )
    .await;
    assert_eq!(report["warnings"], json!([]), "{report}");
    Ok(())
}

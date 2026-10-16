//! Phase 1.4 end-to-end integration test: a table's stored access rules are the
//! ones an application enforces, and an admin's change to them takes effect on a
//! **mounted** app with no restart (design §7, §9, §13.2).
//!
//! §1.1–§1.3 made a table's read/write roles storable, merged and settable; this
//! is where they do observable work. What is asserted here is the whole loop, at
//! three roles, through the real router:
//!
//! - a table's rules govern an application's REST API — role 80 may read a
//!   `min_role_read: 80` table, role 100 may not;
//! - **read and write are separate** — opening reads to role 80 does not open
//!   writes to it;
//! - an admin's change through the admin API reaches the already-mounted app
//!   **live**, which is the seam this phase exists to close: a mounted app built
//!   its providers from the table as it was at mount time, so without a live
//!   re-projection the admin's change would silently do nothing until a restart.
//!
//! The admin (on the base domain) and the app (on its subdomain) are served by
//! one router, so one test can configure through the former and observe through
//! the latter — which is exactly the arrangement a real deployment has.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_app::{
    ApiConfig, Application, CodeFramework, FrameworkRef, app_source_from_config, build_application,
};
use sc_auth::{ROLE_ADMIN, Role, SessionStore, create_user, save_role};
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

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-access-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A cookie-carrying client addressing one host, echoing CSRF on mutations.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router, host: &str) -> Client {
        Client {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
        }
    }

    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Vec<u8>) {
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
        (status, bytes.to_vec())
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, bytes) = self.raw(method, path, body).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// Load the SPA (a safe GET) to obtain the CSRF cookie, then log in — the
    /// order a browser follows before it may mutate anything.
    async fn login(&mut self, email: &str, password: &str) -> StatusCode {
        self.raw("GET", "/", None).await;
        let (status, _) = self
            .send(
                "POST",
                "/api/login",
                Some(json!({ "email": email, "password": password })),
            )
            .await;
        status
    }
}

/// The app's source tree: a git repo whose stand-in bundler emits an SPA.
fn write_app_source(root: &Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let web = root.join("web");
    std::fs::create_dir_all(web.join("src")).unwrap();
    let script = web.join("build.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         set -e\n\
         test -f src/client.ts\n\
         mkdir -p dist/assets\n\
         printf '<!doctype html><div id=root></div>' > dist/index.html\n\
         cp src/client.ts dist/assets/client.js\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn code_framework() -> FrameworkRef {
    FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts")
}

fn blog_app() -> Application {
    Application::new("Blog", "blog", code_framework())
        .with_table(TableId("posts".to_owned()))
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"))
}

/// Build and mount the app, bootstrap every platform table, seed a first admin,
/// and return a router serving both the admin (base domain) and the app.
async fn setup(tmp: &TempDir) -> sc_error::Result<(Router, Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$; \
             CREATE TABLE posts (\
               id bigint generated by default as identity primary key, \
               title text not null)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    // The overlay table has to exist for the admin to configure through it, just
    // as `connect_catalog` bootstraps it on the real boot path.
    sc_catalog::bootstrap_table_meta(&catalog).await?;

    write_app_source(tmp.path());
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &blog_app(), &source, None).await?;
    let framework = Arc::new(CodeFramework::new("code", report.bundle));
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    apps.mount(MountedApp::new(blog_app(), framework, &catalog)?)?;

    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps,
    )?;
    Ok((router, catalog, db))
}

#[tokio::test]
async fn a_tables_rules_govern_the_app_api_and_an_admin_change_takes_effect_live()
-> sc_error::Result<()> {
    let tmp = TempDir::new("live");
    let (router, catalog, _db) = setup(&tmp).await?;

    // Three roles. 40 and 80 have to exist before anyone can hold them —
    // `users.role` is a foreign key onto `_fd_roles` now (§7.4).
    create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    save_role(&catalog, &Role::new(80, "Reader")).await?;
    save_role(&catalog, &Role::new(40, "Editor")).await?;
    create_user(&catalog, "reader@example.com", "reader-pw", 80).await?;
    create_user(&catalog, "editor@example.com", "editor-pw", 40).await?;

    // --- default: posts is admin-only, so neither reader nor editor can read ---
    let mut reader = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        reader.login("reader@example.com", "reader-pw").await,
        StatusCode::OK
    );
    let (status, _) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an unconfigured table is admin-only; role 80 is not admin"
    );

    // --- the admin opens reads to role 80, writes staying admin-only ----------
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let (status, saved) = admin
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "",
                "description": "",
                "min_role_read": 80,
                "min_role_write": 1,
                "ownership_formula": "",
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["min_role_read"], json!(80));

    // --- LIVE: the already-mounted app now serves reads to role 80 ------------
    // No restart, no rebuild, no fresh login. This is the whole point of the
    // phase: the running app picked up the new rule.
    let (status, body) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the read rule reached the mounted app live"
    );
    assert_eq!(body, json!([]));

    // Read and write are separate: role 80 reads, but writing is still admin's.
    let (status, _) = reader
        .send("POST", "/api/posts", Some(json!({ "title": "sneaky" })))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "min_role_write was left at admin"
    );

    // Role 100 (public, unauthenticated) still cannot read an 80 table.
    let mut anon = Client::new(router.clone(), APP_HOST);
    anon.raw("GET", "/", None).await;
    let (status, _) = anon.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // --- the admin opens writes to role 40 too --------------------------------
    let (status, _) = admin
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "",
                "description": "",
                "min_role_read": 80,
                "min_role_write": 40,
                "ownership_formula": "",
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // The editor (role 40) can now write — again with no new session.
    let mut editor = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        editor.login("editor@example.com", "editor-pw").await,
        StatusCode::OK
    );
    let (status, created) = editor
        .send("POST", "/api/posts", Some(json!({ "title": "hello" })))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "min_role_write 40 lets role 40 write"
    );
    assert_eq!(created["title"], json!("hello"));

    // And the reader (role 80) sees the editor's row but still cannot write:
    // 40 is more privileged than 80, so a write floor of 40 excludes 80.
    let (status, body) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(Vec::len), Some(1));
    let (status, _) = reader
        .send("POST", "/api/posts", Some(json!({ "title": "nope" })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    Ok(())
}

#[tokio::test]
async fn forgetting_a_tables_settings_re_closes_the_app_live() -> sc_error::Result<()> {
    let tmp = TempDir::new("forget");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    save_role(&catalog, &Role::new(80, "Reader")).await?;
    create_user(&catalog, "reader@example.com", "reader-pw", 80).await?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    admin.login("admin@example.com", "admin-pw").await;
    admin
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "",
                "description": "",
                "min_role_read": 80,
                "min_role_write": 80,
                "ownership_formula": "",
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;

    let mut reader = Client::new(router.clone(), APP_HOST);
    reader.login("reader@example.com", "reader-pw").await;
    let (status, _) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK, "opened to role 80");

    // Forget the settings: the table reverts to admin-only, and the mounted app
    // must re-close now — a delete that only took effect on the next restart
    // would leave data readable that the admin just re-secured.
    let (status, body) = admin
        .send("DELETE", "/api/tables/posts/settings", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(true));

    let (status, _) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "forgetting the settings re-closed the running app"
    );
    Ok(())
}

//! Phase 9 end-to-end integration test: an application built from a git-repo
//! file store is served by the Saltcorn process — its bundle over HTTP, its data
//! through its API, and nothing through the database (design §13.2/§13.3/§13.4).
//!
//! This is the Phase 9 acceptance story, driven through the assembled router
//! against a real Postgres:
//!
//! - the **build** step runs a bundler and the server serves what it emitted;
//! - an **API auth round-trip**: anonymous → `login` → the session identifies the
//!   caller on later requests;
//! - an **unauthorized request is rejected**, and changes nothing;
//! - each app is confined to its own subdomain, and the admin is still reachable.
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
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, SessionStore, create_user};
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

/// The domain apps are served under in this test.
const BASE_DOMAIN: &str = "example.com";
/// The app's host: subdomain `blog` under the base domain.
const APP_HOST: &str = "blog.example.com";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-app-{}-{tag}-{:?}",
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

/// A cookie-carrying client that addresses one host, echoing CSRF on mutations
/// exactly as a browser-based SPA would.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
    /// The `Content-Security-Policy` of the most recent response.
    csp: String,
}

impl Client {
    fn new(router: Router, host: &str) -> Client {
        Client {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
            csp: String::new(),
        }
    }

    /// Issue a request and return its status, content type, and raw body.
    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Vec<u8>) {
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
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        self.csp = response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();

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
        (status, content_type, bytes.to_vec())
    }

    /// Issue a request expecting a JSON reply.
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, _, bytes) = self.raw(method, path, body).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

/// The app's source tree: a git repo whose bundler emits an SPA into `web/dist`.
///
/// The stand-in bundler is a shell script — to the server a bundler is just what
/// produced the bytes it serves, and a real `npm run build` would make this test
/// need a Node toolchain.
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
         printf 'console.log(\"blog\")' > dist/assets/app.js\n\
         cp src/client.ts dist/assets/client.js\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The framework config an admin would have filled in; every build setting is
/// resolved from it rather than hand-built (§13.2).
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

/// Build the app, mount it, and return a router serving it plus the admin.
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
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;

    write_app_source(tmp.path());
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    // The real build path: emit the app's typed client, run the bundler, serve
    // what it produced.
    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &blog_app(), &source, None).await?;

    let framework = Arc::new(CodeFramework::new("code", report.bundle));
    let mounted = MountedApp::new(blog_app(), framework, &catalog)?;
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    apps.mount(mounted)?;

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
async fn the_built_react_app_is_served_and_its_api_round_trips() -> sc_error::Result<()> {
    let tmp = TempDir::new("story");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "editor@example.com", "correct-horse", ROLE_ADMIN).await?;

    let mut app = Client::new(router, APP_HOST);

    // --- the build's assets are served -------------------------------------
    let (status, ct, body) = app.raw("GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "text/html; charset=utf-8");
    assert_eq!(body, b"<!doctype html><div id=root></div>");
    // The app carries its own CSP (§13.2), not the admin's.
    assert_eq!(app.csp, "default-src 'self'");

    let (status, ct, body) = app.raw("GET", "/assets/app.js", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "text/javascript; charset=utf-8");
    assert_eq!(body, b"console.log(\"blog\")");

    // The generated client really reached the bundle the bundler emitted.
    let (status, _, body) = app.raw("GET", "/assets/client.js", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("createClient"));

    // A client-routed deep link resolves to the SPA entry point.
    let (status, ct, _) = app.raw("GET", "/posts/42", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "text/html; charset=utf-8");

    // --- an unauthorized request is rejected -------------------------------
    let (status, _) = app.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = app
        .send("POST", "/api/posts", Some(json!({"title": "sneaky"})))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // --- API auth round-trip ------------------------------------------------
    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            Some(json!({"email": "editor@example.com", "password": "wrong"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = app
        .send(
            "POST",
            "/api/login",
            Some(json!({"email": "editor@example.com", "password": "correct-horse"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["email"], json!("editor@example.com"));
    // The login set a session cookie: the client is now carrying it.
    assert!(app.cookies.contains_key(sc_server::SESSION_COOKIE));

    // The session identifies the caller on the very next request.
    let (status, body) = app.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["email"], json!("editor@example.com"));

    // The same request that was 401 now succeeds.
    let (status, body) = app.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]), "the rejected write must not have inserted");

    // Full CRUD through the app's own API.
    let (status, body) = app
        .send("POST", "/api/posts", Some(json!({"title": "hello"})))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["id"].as_i64().unwrap();

    let (status, body) = app.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(Vec::len), Some(1));

    let (status, body) = app
        .send(
            "PUT",
            &format!("/api/posts/{id}"),
            Some(json!({"title": "goodbye"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], json!("goodbye"));

    let (status, _) = app.send("DELETE", &format!("/api/posts/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);

    // --- logout drops the session ------------------------------------------
    let (status, _) = app.send("POST", "/api/logout", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app.send("GET", "/api/posts", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "logout must end the session"
    );
    Ok(())
}

#[tokio::test]
async fn a_logged_in_user_still_only_sees_what_its_role_allows() -> sc_error::Result<()> {
    let tmp = TempDir::new("authz");
    let (router, catalog, _db) = setup(&tmp).await?;
    // A real user at the least-privileged role.
    create_user(&catalog, "reader@example.com", "correct-horse", ROLE_PUBLIC).await?;

    let mut app = Client::new(router, APP_HOST);

    // Load the app first, as a browser does: the CSRF double-submit needs the
    // client to be holding the `sc_csrf` cookie before it may mutate anything,
    // and serving the SPA is what hands it over.
    let (status, _, _) = app.raw("GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            Some(json!({"email": "reader@example.com", "password": "correct-horse"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Authenticated — but `posts` is admin-only, so the session is not a grant.
    let (status, _) = app.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = app
        .send("POST", "/api/posts", Some(json!({"title": "x"})))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // It can still see who it is, and the app's UI is public.
    let (status, _) = app.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = app.raw("GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn apps_are_confined_to_their_subdomain_and_the_admin_is_still_reachable()
-> sc_error::Result<()> {
    let tmp = TempDir::new("hosts");
    let (router, _catalog, _db) = setup(&tmp).await?;

    // On the base domain the app does not exist. `/api/posts` is not an admin
    // route, so it falls through to the admin's SPA bootstrap document — the
    // point being that it is emphatically *not* the app's API answering.
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    let (status, _, body) = admin.raw("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, sc_server::BOOTSTRAP_HTML.as_bytes());

    // `/` is the admin's bootstrap, not the app's index.html.
    let (status, _, body) = admin.raw("GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, sc_server::BOOTSTRAP_HTML.as_bytes());

    // The admin API still answers on the base domain: mounting an app does not
    // take the admin away from the operator.
    let (status, body) = admin.send("GET", "/api/auth/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["any_user_exists"], json!(false));

    // An unknown subdomain is not an app; it reaches the admin rather than some
    // other app's data.
    let mut unknown = Client::new(router.clone(), "nope.example.com");
    let (_, _, body) = unknown.raw("GET", "/api/posts", None).await;
    assert_eq!(body, sc_server::BOOTSTRAP_HTML.as_bytes());

    // A host merely *ending* with the base domain's text is not ours: a request
    // cannot reach `blog` by claiming to be `blog.notexample.com`.
    let mut evil = Client::new(router, "blog.notexample.com");
    let (_, _, body) = evil.raw("GET", "/", None).await;
    assert_eq!(body, sc_server::BOOTSTRAP_HTML.as_bytes());
    Ok(())
}

/// The admin file and backup routes are the admin's host's alone: an admin's
/// session sent to an application's host — by a script on that host, which can
/// read the host's own CSRF cookie — reaches none of them (security review #15).
#[tokio::test]
async fn admin_file_and_backup_routes_are_nothing_on_an_application_host() -> sc_error::Result<()> {
    let tmp = TempDir::new("adminroutes");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "editor@example.com", "correct-horse", ROLE_ADMIN).await?;

    // The admin signs in to the application, as any user of it might.
    let mut app = Client::new(router.clone(), APP_HOST);
    let (status, _, _) = app.raw("GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            Some(json!({"email": "editor@example.com", "password": "correct-horse"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    for (method, path) in [
        ("GET", "/download/apps/web/build.sh"),
        ("POST", "/upload/apps/web/planted.js"),
        ("POST", "/backup/create"),
        ("POST", "/backup/upload"),
    ] {
        let (status, _, _) = app.raw(method, path, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {path} on the app host"
        );
    }
    assert!(!tmp.path().join("web/planted.js").exists());

    // The same session on the admin's host is an admin's: the refusal above is
    // the host, not the session.
    let mut admin = Client::new(router, BASE_DOMAIN);
    admin.cookies = app.cookies.clone();
    let (status, _, body) = admin.raw("GET", "/download/apps/web/build.sh", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("mkdir -p dist/assets"));
    Ok(())
}

#[tokio::test]
async fn mounting_an_app_without_a_base_domain_is_refused() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());

    let framework = Arc::new(CodeFramework::new("code", Default::default()));
    let app = Application::new("Blog", "blog", code_framework());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    apps.mount(MountedApp::new(app, framework, &catalog)?)?;

    // No base domain: no request could ever reach the app, so say so at boot
    // rather than serving an app nobody can address.
    let err = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog, apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )
    .expect_err("mounting an app with no base domain must fail");
    assert!(err.to_string().contains("base-domain"), "{err}");
    Ok(())
}

#[tokio::test]
async fn two_apps_cannot_claim_the_same_subdomain() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());

    let one = MountedApp::new(
        Application::new("Blog", "blog", code_framework()),
        Arc::new(CodeFramework::new("code", Default::default())),
        &catalog,
    )?;
    // A different app claiming the same subdomain.
    let two = MountedApp::new(
        Application::new("Other", "blog", code_framework()),
        Arc::new(CodeFramework::new("code", Default::default())),
        &catalog,
    )?;

    let apps = AppMounts::new(catalog);
    apps.mount(one)?;
    let err = apps
        .mount(two)
        .expect_err("a subdomain collision must fail");
    assert!(err.to_string().contains("blog"), "{err}");
    Ok(())
}

//! An application's REST API offers **sign-up** — `POST /api/signup` — only
//! where its API settings turn it on (design §7.2, §13.4).
//!
//! Driven through the real router against Postgres, as a React app's sign-up
//! form would drive it:
//!
//! - with `allow_signup` off (the default) there is no such route;
//! - with it on, an anonymous caller makes an account with the configured role,
//!   is signed in by the same response, and the session is the new user's;
//! - an address that already has an account is a `409`, and changes nothing;
//! - the account is an ordinary one: it can sign out and back in through
//!   `/api/login`.
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
use sc_auth::{Role, SessionStore, load_user_by_email, save_role};
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
            "sc-server-signup-{}-{tag}-{:?}",
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
    /// The socket address requests come from; none by default.
    peer: Option<std::net::SocketAddr>,
}

impl Client {
    fn new(router: Router, host: &str) -> Client {
        Client {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
            peer: None,
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
        let mut request = request;
        if let Some(peer) = self.peer {
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
        }
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

fn blog_app(api: ApiConfig) -> Application {
    Application::new("Blog", "blog", code_framework())
        .with_table(TableId("posts".to_owned()))
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_api(api)
}

/// Build and mount the app with `api` as its REST API, and return a router
/// serving it.
async fn setup(tmp: &TempDir, api: ApiConfig) -> sc_error::Result<(Router, Arc<Catalog>, TestDb)> {
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
    sc_catalog::bootstrap_table_meta(&catalog).await?;

    write_app_source(tmp.path());
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    let app = blog_app(api);
    // The settings go through the same check a save does.
    sc_app::validate_api_config(&app, &app.apis[0])?;
    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &app, &source, None).await?;
    let framework = Arc::new(CodeFramework::new("code", report.bundle));
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    apps.mount(MountedApp::new(app, framework, &catalog)?)?;

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

fn credentials(email: &str, password: &str) -> Option<Value> {
    Some(json!({ "email": email, "password": password }))
}

#[tokio::test]
async fn signup_is_absent_unless_the_api_settings_allow_it() -> sc_error::Result<()> {
    let tmp = TempDir::new("off");
    let (router, catalog, _db) = setup(&tmp, ApiConfig::new("rest", "/api")).await?;

    let mut app = Client::new(router, APP_HOST);
    app.raw("GET", "/", None).await;
    let (status, _) = app
        .send(
            "POST",
            "/api/signup",
            credentials("new@example.com", "pw-123"),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no route without the setting"
    );
    assert!(
        load_user_by_email(&catalog, "new@example.com")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn signup_makes_an_account_with_the_configured_role_and_signs_it_in() -> sc_error::Result<()>
{
    let tmp = TempDir::new("on");
    let api = ApiConfig::new("rest", "/api")
        .with(sc_api::REST_CFG_ALLOW_SIGNUP, true)
        .with(sc_api::REST_CFG_NEW_USER_ROLE, 80);
    let (router, catalog, _db) = setup(&tmp, api).await?;
    // Role 80 has to exist for an account to hold it (`users.role` is a key).
    save_role(&catalog, &Role::new(80, "Member")).await?;

    let mut app = Client::new(router, APP_HOST);
    // Load the SPA first: that is what hands a browser its CSRF cookie.
    app.raw("GET", "/", None).await;
    let (status, _) = app.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = app
        .send(
            "POST",
            "/api/signup",
            credentials(" new@example.com ", "pw-123"),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["email"], json!("new@example.com"));
    assert_eq!(body["role"], json!(80));
    assert!(body.get("password_hash").is_none());

    // Signed in by that same response.
    let (status, me) = app.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["email"], json!("new@example.com"));

    // An address that has an account is refused, and nothing changes.
    let mut other = Client::new(app.router.clone(), APP_HOST);
    other.raw("GET", "/", None).await;
    let (status, _) = other
        .send(
            "POST",
            "/api/signup",
            credentials("new@example.com", "other-pw"),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = other
        .send(
            "POST",
            "/api/login",
            credentials("new@example.com", "other-pw"),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the first password stands"
    );

    // A blank password is not an account.
    let (status, _) = other
        .send("POST", "/api/signup", credentials("blank@example.com", ""))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // It is an ordinary account: out, and back in through login.
    let (status, _) = app.send("POST", "/api/logout", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            credentials("new@example.com", "pw-123"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

/// One address makes a handful of accounts an hour, not a thousand (security
/// review #22).
#[tokio::test]
async fn signup_is_throttled_per_address() -> sc_error::Result<()> {
    let tmp = TempDir::new("throttle");
    let api = ApiConfig::new("rest", "/api")
        .with(sc_api::REST_CFG_ALLOW_SIGNUP, true)
        .with(sc_api::REST_CFG_NEW_USER_ROLE, 80);
    let (router, catalog, _db) = setup(&tmp, api).await?;
    save_role(&catalog, &Role::new(80, "Member")).await?;

    let peer: std::net::SocketAddr = "203.0.113.77:4000".parse().unwrap();
    for n in 0..6 {
        let mut client = Client::new(router.clone(), APP_HOST);
        client.peer = Some(peer);
        client.raw("GET", "/", None).await;
        let (status, body) = client
            .send(
                "POST",
                "/api/signup",
                credentials(&format!("bot{n}@example.com"), "pw-123"),
            )
            .await;
        let expected = if n < 5 {
            StatusCode::CREATED
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(status, expected, "signup {n}: {body}");
    }
    // Another address is counted apart.
    let mut client = Client::new(router.clone(), APP_HOST);
    client.peer = Some("203.0.113.78:4000".parse().unwrap());
    client.raw("GET", "/", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/signup",
            credentials("human@example.com", "pw-123"),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Ok(())
}

/// Anybody can sign up, so the settings may not make that anybody an admin.
#[test]
fn a_signup_role_of_admin_is_refused_on_save() {
    let app = blog_app(
        ApiConfig::new("rest", "/api")
            .with(sc_api::REST_CFG_ALLOW_SIGNUP, true)
            .with(sc_api::REST_CFG_NEW_USER_ROLE, 1),
    );
    let msg = sc_app::validate_api_config(&app, &app.apis[0])
        .unwrap_err()
        .to_string();
    assert!(msg.contains("new_user_role"), "{msg}");
}

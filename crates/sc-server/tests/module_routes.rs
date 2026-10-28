//! v1 plugin **routes** over HTTP: `/_feldspar/routes/<module>/<path>` on the
//! admin host, served as whoever calls — the session's user, an API token's,
//! or nobody — with v1's `noCsrf` and `apiToken` flags enforced by the server,
//! and the installation facts `@saltcorn/heartbeat-monitor` reads.
//!
//! Needs `npm`, which is what installs the fixture module, and skips without
//! it, as `module_seams.rs` does.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_action::TriggerDispatcher;
use sc_app::{Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ModuleServices, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_agents, install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sc-module/tests/fixtures")
        .join(name)
}

fn have_npm() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// One answer: the status, the headers and the body as text.
pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    headers: axum::http::HeaderMap,
    pub(crate) text: String,
}

impl Answer {
    pub(crate) fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
}

/// A caller: a browser that keeps its cookies (and sends their CSRF token), or
/// one that never has any.
pub(crate) struct Client {
    router: Router,
    cookies: HashMap<String, String>,
    keeps_cookies: bool,
}

impl Client {
    pub(crate) async fn send(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Answer {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
            if method != "GET"
                && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
            {
                builder = builder.header(CSRF_HEADER, csrf);
            }
        }
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let request = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        if self.keeps_cookies {
            for raw in headers.get_all(header::SET_COOKIE) {
                if let Some((name, value)) = raw
                    .to_str()
                    .ok()
                    .and_then(|t| t.split(';').next())
                    .and_then(|p| p.split_once('='))
                {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        Answer {
            status,
            headers,
            text: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }
}

pub(crate) struct Server {
    pub(crate) admin: Client,
    router: Router,
    pub(crate) catalog: Arc<Catalog>,
    dispatcher: Arc<TriggerDispatcher>,
    root: PathBuf,
    apps: Arc<AppMounts>,
    _modules: Arc<ModuleServices>,
    _db: TestDb,
    _registries: Option<tokio::sync::RwLockReadGuard<'static, ()>>,
}

impl Server {
    /// A caller with no cookies and no token.
    fn nobody(&self) -> Client {
        Client {
            router: self.router.clone(),
            cookies: HashMap::new(),
            keeps_cookies: false,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn setup() -> sc_error::Result<Server> {
    let registries = crate::module_registries().read().await;
    let mut server = setup_unlocked().await?;
    server._registries = Some(registries);
    Ok(server)
}

/// [`setup`] for a caller that holds [`crate::module_registries`] itself — for
/// writing, to keep the registries this server installs.
pub(crate) async fn setup_unlocked() -> sc_error::Result<Server> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    // One directory per test, not per process: the tests run side by side, and
    // each one's `Drop` removes its directory.
    let root = std::env::temp_dir().join(format!(
        "sc-module-routes-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.clone()),
        None,
        1,
        sc_server::default_python_adapter(),
        None,
    )
    .await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(default_js_evaluator())
            .with_triggers(dispatcher.clone())
            .with_agents(agents)
            .with_modules(modules.clone()),
    );
    // A base domain, so a request can name an application's host; a request
    // with no `Host` is the admin's.
    let config = ServerConfig {
        base_domain: Some("example.com".to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;
    let mut admin = Client {
        router: router.clone(),
        cookies: HashMap::new(),
        keeps_cookies: true,
    };
    admin.send("GET", "/api/auth/status", &[], None).await;
    let first = admin
        .send(
            "POST",
            "/api/first-user",
            &[],
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    Ok(Server {
        admin,
        router,
        catalog,
        dispatcher,
        root,
        apps,
        _modules: modules,
        _db: db,
        _registries: None,
    })
}

const BASE: &str = "/_feldspar/routes/routes";

#[tokio::test]
async fn a_modules_routes_are_served_on_the_admin_host_as_whoever_calls() -> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let mut server = setup().await?;
    let installed = server
        .admin
        .send(
            "POST",
            "/api/modules",
            &[],
            Some(json!({
                "source": "local",
                "location": fixture("route-module").display().to_string(),
            })),
        )
        .await;
    assert_eq!(installed.status, StatusCode::CREATED, "{}", installed.text);
    let card = installed.json();
    assert_eq!(card["loaded"], json!(true), "{card}");
    assert!(card["issues"].as_array().unwrap().is_empty(), "{card}");
    // The card lists each route at the path it is served at — the package's
    // name without its scope — and `routes` is no longer an unsupported key.
    let whoami = card["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["url"] == json!("/whoami"))
        .cloned()
        .unwrap();
    assert_eq!(whoami["method"], json!("GET"));
    assert_eq!(whoami["path"], json!(format!("{BASE}/whoami")));
    assert!(
        !card["unsupported"].to_string().contains("routes"),
        "{card}"
    );

    // The admin, by their session: v1's `req.user` with `role_id`, Express's
    // `(req, res)` pointing at each other, the path below the prefix, and no
    // cookie handed to the module.
    let mine = server
        .admin
        .send("GET", &format!("{BASE}/whoami?x=1"), &[], None)
        .await;
    assert_eq!(mine.status, StatusCode::OK, "{}", mine.text);
    assert_eq!(mine.headers[header::CACHE_CONTROL], "no-store");
    let mine = mine.json();
    assert_eq!(mine["role_id"], json!(1), "{mine}");
    assert_eq!(mine["email"], json!("admin@example.com"));
    assert_eq!(mine["query"], json!({ "x": "1" }));
    assert_eq!(mine["path"], json!("/whoami"));
    assert_eq!(mine["originalUrl"], json!(format!("{BASE}/whoami?x=1")));
    assert_eq!(mine["sameRequest"], json!(true));
    assert_eq!(mine["cookie"], Value::Null);

    // Nobody: no `req.user`, as v1's public has none.
    let mut nobody = server.nobody();
    let theirs = nobody
        .send("GET", &format!("{BASE}/whoami"), &[], None)
        .await;
    assert_eq!(theirs.status, StatusCode::OK, "{}", theirs.text);
    assert_eq!(theirs.json()["role_id"], Value::Null);

    // An application's host is the application's: nothing is served there.
    server.apps.mount(MountedApp::new_with(
        Application::new("Shop", "shop", FrameworkRef::new("code")),
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html>"),
        )),
        &server.catalog,
        server.apps.evaluator(),
        server.apps.triggers(),
    )?)?;
    let elsewhere = nobody
        .send(
            "GET",
            &format!("{BASE}/whoami"),
            &[("host", "shop.example.com")],
            None,
        )
        .await;
    assert_eq!(
        elsewhere.status,
        StatusCode::NOT_FOUND,
        "{}",
        elsewhere.text
    );

    // `noCsrf`: a cookie-less POST is served, as nobody, with its JSON body
    // parsed and its `:param` bound.
    let echo = nobody
        .send(
            "POST",
            &format!("{BASE}/echo/a%20b"),
            &[],
            Some(json!({ "n": 2 })),
        )
        .await;
    assert_eq!(echo.status, StatusCode::CREATED, "{}", echo.text);
    assert_eq!(
        echo.json(),
        json!({ "id": "a b", "body": { "n": 2 }, "user": null })
    );

    // Without `noCsrf`, a POST needs the token: refused for nobody, served for
    // the admin, who sends it.
    let forged = nobody
        .send("POST", &format!("{BASE}/guarded"), &[], None)
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN, "{}", forged.text);
    let posted = server
        .admin
        .send("POST", &format!("{BASE}/guarded"), &[], None)
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.text);
    assert_eq!(posted.text, "posted");
    assert!(
        posted.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );

    // `apiToken`: nobody is refused; a token's user is served.
    let refused = nobody
        .send("GET", &format!("{BASE}/token"), &[], None)
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{}", refused.text);
    let minted = server
        .admin
        .send(
            "POST",
            "/api/api-tokens",
            &[],
            Some(json!({ "label": "monitor", "grants": {}, "expires_in_days": 1 })),
        )
        .await;
    assert!(minted.status.is_success(), "{}", minted.text);
    let secret = minted.json()["secret"].as_str().unwrap().to_owned();
    let bearer = format!("Bearer {secret}");
    let by_token = nobody
        .send(
            "GET",
            &format!("{BASE}/token"),
            &[("authorization", &bearer)],
            None,
        )
        .await;
    assert_eq!(by_token.status, StatusCode::OK, "{}", by_token.text);
    assert_eq!(by_token.json()["email"], json!("admin@example.com"));
    let bad_token = nobody
        .send(
            "GET",
            &format!("{BASE}/whoami"),
            &[("authorization", "Bearer fspk_nonsense")],
            None,
        )
        .await;
    assert_eq!(
        bad_token.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        bad_token.text
    );

    // A callback that answers from a promise it did not return is waited for.
    let later = nobody
        .send("GET", &format!("{BASE}/later"), &[], None)
        .await;
    assert_eq!(later.status, StatusCode::OK, "{}", later.text);
    assert_eq!(later.text, "later");

    // A throw is a 500 naming it; an Express member this host lacks names itself.
    let fail = nobody.send("GET", &format!("{BASE}/fail"), &[], None).await;
    assert_eq!(fail.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(fail.text.contains("the route broke"), "{}", fail.text);
    let render = nobody
        .send("GET", &format!("{BASE}/render"), &[], None)
        .await;
    assert_eq!(render.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(render.text.contains("res.render"), "{}", render.text);

    // The wrong method is a 405 saying the right one; no route is a 404.
    let wrong = nobody
        .send("POST", &format!("{BASE}/later"), &[], Some(json!({})))
        .await;
    assert_eq!(
        wrong.status,
        StatusCode::FORBIDDEN,
        "CSRF first: {}",
        wrong.text
    );
    let wrong = server
        .admin
        .send("POST", &format!("{BASE}/later"), &[], None)
        .await;
    assert_eq!(
        wrong.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "{}",
        wrong.text
    );
    assert_eq!(wrong.headers[header::ALLOW], "GET");
    let missing = nobody.send("GET", &format!("{BASE}/nope"), &[], None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.text);
    Ok(())
}

#[tokio::test]
async fn a_route_reads_the_machine_the_startups_and_the_backups() -> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let mut server = setup().await?;
    let installed = server
        .admin
        .send(
            "POST",
            "/api/modules",
            &[],
            Some(json!({
                "source": "local",
                "location": fixture("route-module").display().to_string(),
            })),
        )
        .await;
    assert_eq!(installed.status, StatusCode::CREATED, "{}", installed.text);

    // Before anything has happened: no startups, no backups, none scheduled.
    let mut nobody = server.nobody();
    let before = nobody
        .send("GET", &format!("{BASE}/facts"), &[], None)
        .await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.text);
    let before = before.json();
    assert_eq!(before["disk"], json!(true), "{before}");
    assert_eq!(before["mem"], json!(true), "{before}");
    assert_eq!(before["load"], json!(true), "{before}");
    assert_eq!(before["startups"], json!(0));
    assert_eq!(before["lastBackup"], Value::Null);
    assert_eq!(before["frequency"], json!("Never"));

    // A startup is recorded; a weekly backup and a daily one are scheduled,
    // and one of them has succeeded.
    sc_server::fire_startup(&server.catalog, &server.dispatcher).await;
    let schedule = |frequency: &str| {
        json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "destination": { "kind": "local", "directory": "/srv/backups" },
            "frequency": frequency,
            "retention_days": 7,
        })
    };
    let (weekly, daily) = (schedule("weekly"), schedule("daily"));
    sc_config::set_config(
        &server.catalog,
        sc_config::BACKUP_SCHEDULES,
        json!([weekly, daily]),
    )
    .await?;
    sc_config::set_config(
        &server.catalog,
        sc_config::BACKUP_SCHEDULE_STATUS,
        json!({
            weekly["id"].as_str().unwrap(): {
                "last_attempt_at": "2026-10-26T02:00:00+00:00",
                "last_success_at": "2026-10-26T02:00:00+00:00",
                "last_file": "/srv/backups/feldspar-backup-2026-10-26-020000.zip",
            },
            daily["id"].as_str().unwrap(): {
                "last_attempt_at": "2026-10-27T02:00:00+00:00",
                "last_error": "the disk is full",
            },
        }),
    )
    .await?;

    let after = nobody
        .send("GET", &format!("{BASE}/facts"), &[], None)
        .await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.text);
    let after = after.json();
    assert_eq!(after["startups"], json!(1), "{after}");
    assert_eq!(after["startupIsDate"], json!(true));
    assert_eq!(after["lastBackup"], json!("2026-10-26T02:00:00.000Z"));
    // The most frequent schedule's, by v1's name.
    assert_eq!(after["frequency"], json!("Daily"));
    Ok(())
}

//! What a module reaches beyond actions, for a payment provider's sake
//! (TODO-payments P1):
//!
//! - its **webhooks** over HTTP (P1.1, P1.2): a request from another service
//!   reaches the module's handler with its body and headers byte for byte, the
//!   handler writes to its own tables and nowhere else, and the status it
//!   answers is what the caller gets;
//! - the **content-security sources** it suggests (P1.4): listed on its card,
//!   and in an application's policy only once switched on for it.
//!
//! Needs `npm`, which is what installs the fixture modules, and skips without
//! it, as `modules_api.rs` does.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
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
use sha2::{Digest, Sha256};
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

/// The admin's browser: cookies, CSRF and JSON.
struct Admin {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Admin {
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
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Some((name, value)) = raw
                .to_str()
                .ok()
                .and_then(|t| t.split(';').next())
                .and_then(|p| p.split_once('='))
            {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

/// What a provider's server sends: bytes and headers, no cookie, no token.
struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

async fn deliver(router: &Router, path: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Answer {
    let mut builder = Request::builder().method("POST").uri(path);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    Answer {
        status,
        headers,
        body,
    }
}

struct Server {
    admin: Admin,
    router: Router,
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    root: PathBuf,
    _modules: Arc<ModuleServices>,
    _db: TestDb,
    _registries: tokio::sync::RwLockReadGuard<'static, ()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn setup(tag: &str) -> sc_error::Result<Server> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!("sc-module-seams-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let registries = crate::module_registries().read().await;
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
    // A base domain, so an application can be served beside the admin; a
    // request with no `Host` is the admin's.
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
    let mut admin = Admin {
        router: router.clone(),
        cookies: HashMap::new(),
    };
    admin.send("GET", "/api/auth/status", None).await;
    let (status, _) = admin
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok(Server {
        admin,
        router,
        catalog,
        apps,
        root,
        _modules: modules,
        _db: db,
        _registries: registries,
    })
}

async fn deliveries(catalog: &Catalog) -> Vec<Value> {
    let table = catalog
        .get("hook_deliveries")
        .unwrap()
        .expect("declared table");
    let rows = sc_api::rows::list_rows(catalog, &table).await.unwrap();
    let mut rows = rows.as_array().cloned().unwrap_or_default();
    rows.sort_by_key(|r| r["id"].as_i64());
    rows
}

#[tokio::test]
async fn a_webhook_receives_the_bytes_writes_its_own_table_and_answers() -> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let mut server = setup("webhooks").await?;
    let (status, installed) = server
        .admin
        .send(
            "POST",
            "/api/modules",
            Some(json!({
                "source": "local",
                "location": fixture("webhook-module").display().to_string(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{installed}");
    assert_eq!(installed["loaded"], json!(true), "{installed}");

    // P1.2: the installed card lists each webhook with the path it is served
    // at — the package's name without its scope.
    let webhooks = installed["webhooks"].as_array().unwrap();
    assert_eq!(webhooks.len(), 3, "{installed}");
    let deliver_card = webhooks
        .iter()
        .find(|w| w["name"] == json!("deliver"))
        .unwrap();
    assert_eq!(
        deliver_card["path"],
        json!("/_feldspar/hooks/webhook/deliver")
    );
    assert_eq!(deliver_card["description"], json!("Records a delivery"));

    // Bytes a parse-and-reserialise would not survive: CRLF, a byte that is not
    // UTF-8, JSON with spacing a serialiser would change.
    let mut body = b"{ \"id\" :  \"evt_1\",\r\n  \"amount\": 1200 }".to_vec();
    body.push(0xff);
    let sha = hex(&Sha256::digest(&body));
    let answer = deliver(
        &server.router,
        "/_feldspar/hooks/webhook/deliver/acct_42",
        &[
            ("content-type", "application/json; charset=utf-8"),
            ("x-signature", "t=1,v1=abc"),
            ("x-answer-status", "201"),
        ],
        body.clone(),
    )
    .await;
    // No session and no CSRF token, and still served: the status the handler
    // chose reaches the caller.
    assert_eq!(
        answer.status,
        StatusCode::CREATED,
        "{:?}",
        String::from_utf8_lossy(&answer.body)
    );
    let reply: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(reply["received"], json!(body.len()));
    assert_eq!(reply["sha256"], json!(sha));
    assert_eq!(reply["method"], json!("POST"));

    let rows = deliveries(&server.catalog).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["body_sha256"], json!(sha));
    assert_eq!(rows[0]["body_length"], json!(body.len()));
    assert_eq!(rows[0]["signature"], json!("t=1,v1=abc"));
    assert_eq!(
        rows[0]["content_type"],
        json!("application/json; charset=utf-8")
    );
    assert_eq!(rows[0]["rest"], json!("acct_42"));

    // It cannot reach a table it did not declare, nor run its own SQL — and
    // the handler goes on to write its own row.
    let answer = deliver(
        &server.router,
        "/_feldspar/hooks/webhook/deliver",
        &[("x-trespass", "1")],
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    let rows = deliveries(&server.catalog).await;
    let trespass = rows[1]["trespass"].as_str().unwrap();
    assert!(!trespass.contains("allowed"), "{trespass}");
    assert!(
        trespass.contains("a webhook of `@saltcorn-test/webhook` may reach only the tables"),
        "{trespass}"
    );
    assert!(trespass.contains("cannot run its own SQL"), "{trespass}");
    assert_eq!(rows[1]["rest"], json!(""));
    let users = server.catalog.get("users").unwrap().unwrap();
    let all = sc_api::rows::list_rows(&server.catalog, &users).await?;
    assert!(
        !all.to_string().contains("intruder@example.com"),
        "the module wrote a table it did not declare"
    );

    // A text answer with its own header, and a status of the handler's.
    let answer = deliver(
        &server.router,
        "/_feldspar/hooks/webhook/ping",
        &[],
        b"hello".to_vec(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED);
    assert_eq!(answer.body, b"pong hello");
    assert_eq!(answer.headers["x-pong"], "1");
    assert!(
        answer.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );

    // A handler that throws is a 500, so the provider retries.
    let answer = deliver(
        &server.router,
        "/_feldspar/hooks/webhook/broken",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        String::from_utf8_lossy(&answer.body).contains("could not be processed"),
        "{}",
        String::from_utf8_lossy(&answer.body)
    );

    // Nothing there: an unknown webhook, an unknown module.
    for path in [
        "/_feldspar/hooks/webhook/nope",
        "/_feldspar/hooks/nobody/deliver",
    ] {
        let answer = deliver(&server.router, path, &[], Vec::new()).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
    }

    // The body cap: a megabyte, and not a byte more.
    let answer = deliver(
        &server.router,
        "/_feldspar/hooks/webhook/deliver",
        &[],
        vec![b'x'; sc_module::WEBHOOK_MAX_BODY + 1],
    )
    .await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(deliveries(&server.catalog).await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_modules_suggested_sources_reach_an_apps_policy_only_once_switched_on()
-> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let mut server = setup("csp").await?;
    let (status, installed) = server
        .admin
        .send(
            "POST",
            "/api/modules",
            Some(json!({
                "source": "local",
                "location": fixture("csp-module").display().to_string(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{installed}");
    // The card lists what it suggests, by the key an application switches it on
    // with; the suggestion that is not a host is an issue, and not offered.
    let suggested = installed["csp_sources"].as_array().unwrap();
    assert_eq!(suggested.len(), 1, "{installed}");
    let key = "@saltcorn-test/csp:elements";
    assert_eq!(suggested[0]["key"], json!(key));
    assert_eq!(
        suggested[0]["sources"]["script-src"],
        json!(["https://js.provider.example"])
    );
    let issues = installed["issues"].to_string();
    assert!(issues.contains("`sloppy` is not offered"), "{issues}");

    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ));
    let shop = |sources: Value| {
        Application::new("Shop", "shop", FrameworkRef::new("code"))
            .attribute(sc_app::CSP_SOURCES_ATTRIBUTE, sources)
    };
    let policy = |router: Router| async move {
        let request = Request::builder()
            .uri("/")
            .header(header::HOST, "shop.example.com")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .to_owned()
    };

    // Installed, and suggested, and not switched on: the policy is the
    // application's own.
    server.apps.mount(MountedApp::new_with(
        shop(json!([])),
        framework.clone(),
        &server.catalog,
        server.apps.evaluator(),
        server.apps.triggers(),
    )?)?;
    let before = policy(server.router.clone()).await;
    assert!(!before.contains("provider.example"), "{before}");

    // Switched on: each directive gains the hosts, beside what it allowed.
    server.apps.remount(MountedApp::new_with(
        shop(json!([key, "@nobody/gone:elements"])),
        framework,
        &server.catalog,
        server.apps.evaluator(),
        server.apps.triggers(),
    )?);
    let after = policy(server.router.clone()).await;
    assert!(
        after.contains("script-src 'self' https://js.provider.example"),
        "{after}"
    );
    assert!(
        after.contains("frame-src 'self' https://js.provider.example"),
        "{after}"
    );
    assert!(
        after.contains("connect-src 'self' https://api.provider.example"),
        "{after}"
    );
    assert!(after.contains("default-src 'self'"), "{after}");
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

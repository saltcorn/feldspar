//! **Query contracts and rate limits** (TODO F4.1–F4.5), through the real
//! router against a real database (SQLite, a file per test).
//!
//! - F4.1: a code query's declared `returns` is checked in a run's preview —
//!   a mismatch logs a warning and the response goes out unchanged — and not on
//!   the live mount;
//! - F4.2/F4.3: a query's `rate_limit.per_ip` turns the 21st call in a minute
//!   into a `429` with `Retry-After`; another IP is unaffected; a forged
//!   `X-Forwarded-For` from an untrusted peer is ignored, and one from a
//!   trusted proxy is believed;
//! - F4.4: login and password reset are throttled per IP and per email, and
//!   the answer is the same whether or not the account exists;
//! - F4.5: `ratelimit.check("share:" + token, …)` in a public query turns the
//!   sixth wrong guess into a `429`, and another share still opens.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_api::{CustomQuery, Method, QueryLanguage, QueryRateLimit};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{Role, SessionStore, create_user, save_role};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_triggers,
};
use sc_test_harness::Teardown;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "shop.example.com";
const PASSWORD: &str = "correct-horse";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf, Teardown);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-query-contracts-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir, Teardown::new())
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

/// A browser on one host and one address, carrying its cookies and the CSRF
/// header a page would send.
struct Client {
    router: Router,
    host: String,
    peer: Option<SocketAddr>,
    forwarded_for: Option<String>,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: &Router, host: &str, peer: Option<&str>) -> Client {
        Client {
            router: router.clone(),
            host: host.to_owned(),
            peer: peer.map(|p| format!("{p}:40000").parse().unwrap()),
            forwarded_for: None,
            cookies: HashMap::new(),
        }
    }

    fn forwarded_for(mut self, value: &str) -> Client {
        self.forwarded_for = Some(value.to_owned());
        self
    }

    async fn send(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
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
        if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        if let Some(xff) = &self.forwarded_for {
            builder = builder.header("x-forwarded-for", xff);
        }
        let body = match body {
            Some(json) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&json).unwrap())
            }
            None => Body::empty(),
        };
        let mut request = builder.body(body).unwrap();
        if let Some(peer) = self.peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        for raw in headers.get_all(header::SET_COOKIE) {
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
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            headers,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
        )
    }

    async fn get(&mut self, path: &str) -> (StatusCode, HeaderMap, Value) {
        self.send("GET", path, None).await
    }

    async fn post(&mut self, path: &str, body: Value) -> (StatusCode, HeaderMap, Value) {
        // A page is loaded before it posts: that is where its CSRF cookie
        // comes from.
        if !self.cookies.contains_key(CSRF_COOKIE) {
            self.get("/").await;
        }
        self.send("POST", path, Some(body)).await
    }
}

/// A public JavaScript GET query.
fn js(name: &str, path: &str, code: &str) -> CustomQuery {
    CustomQuery::new(name, Method::Get, path, code)
        .language(QueryLanguage::Javascript)
        .min_role(sc_auth::ROLE_PUBLIC)
}

fn shop() -> Application {
    let mut api = ApiConfig::new("rest", "/api");
    let queries = vec![
        // Declares an object with a numeric `count`, and returns a string.
        js("stats", "/stats", r#"return { count: "three" };"#)
            .returns(json!({ "object": { "count": "int" } })),
        js("ping", "/ping", r#"return { ok: true };"#).rate_limit(QueryRateLimit {
            per_ip: Some("20/min".to_owned()),
            per_user: None,
        }),
        // The share pattern: the limit is per share, which no per-IP limit
        // can express.
        js(
            "openShare",
            "/share",
            r#"ratelimit.check("share:" + query.token, { max: 5, window: "15m" });
               if (query.password !== "open-sesame") throw httpError(403, "wrong password");
               return { opened: query.token };"#,
        )
        .rate_limit(QueryRateLimit::default()),
    ];
    sc_api::set_custom_queries(&mut api.config, &queries).unwrap();
    let framework = FrameworkRef::new("code")
        .with("store", "files")
        .with("source", "shop")
        .with("output", "shop/dist")
        .with("command", "true");
    Application::new("Shop", "shop", framework).with_api(api)
}

struct Server {
    router: Router,
    apps: Arc<AppMounts>,
    catalog: Arc<Catalog>,
    app: Application,
}

async fn setup(tmp: &TempDir, trusted_proxies: &str) -> sc_error::Result<Server> {
    let driver = Arc::new(SqliteDriver::open(tmp.path().join("contracts.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    tmp.1.add(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_catalog::bootstrap_code_libraries(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    save_role(&catalog, &Role::new(80, "Member")).await?;

    let evaluator = default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    let app = sc_app::save_application(&catalog, &shop()).await?;

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_evaluator(evaluator)
            .with_triggers(dispatcher),
    );
    apps.mount(MountedApp::new_with(
        app.clone(),
        framework(),
        &catalog,
        apps.evaluator(),
        apps.triggers(),
    )?)?;
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        trusted_proxies: sc_ratelimit::TrustedProxies::parse(trusted_proxies).unwrap(),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;
    Ok(Server {
        router,
        apps,
        catalog,
        app,
    })
}

fn framework() -> Arc<CodeFramework> {
    Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ))
}

/// An address no other test in this binary uses, so the process-wide limiter's
/// counts are this test's own.
fn address(test: u8, n: u8) -> String {
    format!("198.18.{test}.{n}")
}

#[tokio::test]
async fn a_return_type_mismatch_is_a_warning_in_a_preview_and_nothing_live() -> sc_error::Result<()>
{
    let tmp = TempDir::new("returns");
    let server = setup(&tmp, "").await?;

    // Live: the answer as the body gave it, and no warning.
    let _log = sc_log::capture::guard(sc_log::Verbosity::Warning);
    let mut live = Client::new(&server.router, APP_HOST, None);
    let (status, _, body) = live.get("/api/stats").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "count": "three" }));
    let lines = sc_log::capture::take();
    assert!(
        !lines.iter().any(|l| l.contains("returns")),
        "no check on the live mount: {lines:?}"
    );

    // A run's preview of the same application: the same answer, and a
    // warning naming the query and where the value strays.
    let run = sc_agent::RunId::new();
    let info = server.apps.mount_preview(
        run,
        MountedApp::new_with(
            server.app.clone(),
            framework(),
            &server.catalog,
            server.apps.evaluator(),
            server.apps.triggers(),
        )?,
    );
    server.apps.allow_preview_token(run, "preview-token");
    let mut preview = Client::new(&server.router, &info.host, None);
    preview.cookies.insert(
        sc_server::PREVIEW_COOKIE.to_owned(),
        "preview-token".to_owned(),
    );
    let (status, _, body) = preview.get("/api/stats").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "count": "three" }),
        "the response is unchanged"
    );
    let lines = sc_log::capture::take();
    assert!(
        lines.iter().any(|l| l.contains("warning")
            && l.contains("`stats`")
            && l.contains("$.count: expected int")),
        "{lines:?}"
    );
    Ok(())
}

#[tokio::test]
async fn the_21st_call_in_a_minute_is_a_429_and_only_for_that_address() -> sc_error::Result<()> {
    let tmp = TempDir::new("per-ip");
    let server = setup(&tmp, &address(1, 250)).await?;

    let mut one = Client::new(&server.router, APP_HOST, Some(&address(1, 1)));
    for n in 0..20 {
        let (status, _, body) = one.get("/api/ping").await;
        assert_eq!(status, StatusCode::OK, "call {n}: {body}");
    }
    let (status, headers, body) = one.get("/api/ping").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"], json!("too many requests"));
    let retry: u64 = headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=3).contains(&retry), "{retry}");

    // Another address is counted apart.
    let mut two = Client::new(&server.router, APP_HOST, Some(&address(1, 2)));
    assert_eq!(two.get("/api/ping").await.0, StatusCode::OK);

    // The limited address cannot talk its way out with a header: its peer is
    // no trusted proxy, so `X-Forwarded-For` is somebody's say-so.
    let mut forged =
        Client::new(&server.router, APP_HOST, Some(&address(1, 1))).forwarded_for(&address(1, 3));
    assert_eq!(
        forged.get("/api/ping").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );

    // Behind the trusted proxy, the client it names is the one counted.
    let mut proxied =
        Client::new(&server.router, APP_HOST, Some(&address(1, 250))).forwarded_for(&address(1, 1));
    assert_eq!(
        proxied.get("/api/ping").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let mut proxied = Client::new(&server.router, APP_HOST, Some(&address(1, 250)))
        .forwarded_for(&format!("{}, {}", address(1, 1), address(1, 4)));
    assert_eq!(proxied.get("/api/ping").await.0, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn logins_are_throttled_per_email_whether_or_not_the_account_exists() -> sc_error::Result<()>
{
    let tmp = TempDir::new("login-email");
    let server = setup(&tmp, "").await?;
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let real = format!("real-{tag}@example.com");
    let ghost = format!("ghost-{tag}@example.com");
    create_user(&server.catalog, &real, PASSWORD, 80).await?;

    // Ten failures each, from ten different addresses so the per-IP limit is
    // not what stops them.
    for (i, email) in [&real, &ghost].into_iter().enumerate() {
        for n in 0..10 {
            let mut client = Client::new(
                &server.router,
                APP_HOST,
                Some(&address(2, (i * 20 + n) as u8)),
            );
            let (status, _, body) = client
                .post("/api/login", json!({ "email": email, "password": "wrong" }))
                .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{email} #{n}: {body}");
        }
    }
    // The eleventh is refused before the password is looked at — the right
    // one too — and identically for the account that does not exist.
    let mut answers = Vec::new();
    for email in [&real, &ghost] {
        let mut client = Client::new(&server.router, APP_HOST, Some(&address(2, 100)));
        let (status, headers, body) = client
            .post(
                "/api/login",
                json!({ "email": email, "password": PASSWORD }),
            )
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{email}: {body}");
        assert!(headers.contains_key(header::RETRY_AFTER));
        answers.push(body["error"].clone());
    }
    assert_eq!(answers[0], answers[1]);

    // The admin's sign-in counts the same failures.
    let mut admin = Client::new(&server.router, BASE_DOMAIN, Some(&address(2, 101)));
    let (status, headers, body) = admin
        .post("/api/login", json!({ "email": real, "password": PASSWORD }))
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(headers.contains_key(header::RETRY_AFTER));

    // Another account signs in as ever.
    let other = format!("other-{tag}@example.com");
    create_user(&server.catalog, &other, PASSWORD, 80).await?;
    let mut client = Client::new(&server.router, APP_HOST, Some(&address(2, 102)));
    let (status, _, body) = client
        .post(
            "/api/login",
            json!({ "email": other, "password": PASSWORD }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

#[tokio::test]
async fn logins_and_resets_are_throttled_per_ip() -> sc_error::Result<()> {
    let tmp = TempDir::new("login-ip");
    let server = setup(&tmp, "").await?;
    let tag = uuid::Uuid::new_v4().simple().to_string();

    // Twenty attempts at twenty addresses from one machine, then a refusal.
    let mut client = Client::new(&server.router, APP_HOST, Some(&address(3, 1)));
    for n in 0..20 {
        let (status, _, body) = client
            .post(
                "/api/login",
                json!({ "email": format!("u{n}-{tag}@example.com"), "password": "x" }),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "#{n}: {body}");
    }
    let (status, _, _) = client
        .post(
            "/api/login",
            json!({ "email": format!("fresh-{tag}@example.com"), "password": "x" }),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A password reset: five for one address, from anywhere, then a refusal —
    // the same for an address with no account.
    for email in [format!("nobody-{tag}@example.com")] {
        for n in 0..5 {
            let mut c = Client::new(&server.router, APP_HOST, Some(&address(3, 10 + n)));
            let (status, _, body) = c
                .post("/api/forgot-password", json!({ "email": email }))
                .await;
            assert_eq!(status, StatusCode::OK, "#{n}: {body}");
            assert_eq!(body, json!({ "ok": true }));
        }
        let mut c = Client::new(&server.router, APP_HOST, Some(&address(3, 20)));
        let (status, _, _) = c
            .post("/api/forgot-password", json!({ "email": email }))
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }
    Ok(())
}

#[tokio::test]
async fn five_wrong_passwords_lock_one_share_and_only_that_share() -> sc_error::Result<()> {
    let tmp = TempDir::new("share");
    let server = setup(&tmp, "").await?;

    // From five addresses, so nothing per-IP is in play: the count is the
    // share's.
    for n in 0..5 {
        let mut c = Client::new(&server.router, APP_HOST, Some(&address(4, n)));
        let (status, _, body) = c.get("/api/share?token=abc&password=guess").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "#{n}: {body}");
        assert_eq!(body["error"], json!("wrong password"));
    }
    let mut c = Client::new(&server.router, APP_HOST, Some(&address(4, 9)));
    let (status, _, body) = c.get("/api/share?token=abc&password=open-sesame").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"], json!("too many requests"));
    assert!(body["retry_after"].as_u64().unwrap() >= 1, "{body}");

    // Another share still opens.
    let (status, _, body) = c.get("/api/share?token=xyz&password=open-sesame").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "opened": "xyz" }));
    Ok(())
}

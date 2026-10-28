//! Moving the admin UI to a subdomain of its own, and an application onto the
//! base domain in its place (Settings → Development → Admin subdomain).
//!
//! Driven through the assembled router as two browsers would drive it: one on
//! the admin UI's old address, saving the setting, and one on the new address,
//! following it there. What is pinned here is what no unit test can see:
//!
//! - the move is a **save**: the next request to the new host is the admin's,
//!   with no restart, and the screen is told when a browser may follow;
//! - following it **keeps the admin signed in**, through a single-use handoff,
//!   although the session cookie is host-only;
//! - once moved, the base domain is the `@` application's, and a navigation to
//!   the old address that no application claims is redirected to the new one —
//!   while the old address's API keeps answering the screen that is waiting;
//! - and the settings and application forms refuse every arrangement in which
//!   the admin UI and an application would claim one host.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_app::{Application, AssetBundle, BuildSpec, CodeFramework, FrameworkRef};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, SESSION_COOKIE, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const ADMIN_HOST: &str = "admin.example.com";

/// One browser: a host it talks to, and the cookies that host has set.
struct Browser {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

/// One response, as a browser sees it.
struct Answer {
    status: StatusCode,
    location: Option<String>,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

impl Browser {
    fn new(router: Router, host: &str) -> Browser {
        Browser {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
        }
    }

    async fn request(
        &mut self,
        method: &str,
        path: &str,
        accept: Option<&str>,
        body: Option<Value>,
    ) -> Answer {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.host);
        if let Some(accept) = accept {
            builder = builder.header(header::ACCEPT, accept);
        }
        if !self.cookies.is_empty() {
            let cookies = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookies);
        }
        if method != "GET"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            let pair = raw.to_str().unwrap().split(';').next().unwrap_or("");
            if let Some((name, value)) = pair.split_once('=') {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|l| l.to_str().unwrap().to_owned());
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        Answer {
            status,
            location,
            body,
        }
    }

    async fn api(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let answer = self.request(method, path, None, body).await;
        (answer.status, answer.json())
    }

    async fn save_admin_subdomain(&mut self, value: Value) -> (StatusCode, Value) {
        self.api(
            "POST",
            "/api/settings",
            Some(json!({ "values": { "admin_subdomain": value } })),
        )
        .await
    }
}

/// A database with the platform tables and nobody in it.
async fn fresh_catalog() -> sc_error::Result<(Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
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
    sc_config::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    Ok((catalog, db))
}

/// A router serving the admin UI on `example.com` with nothing mounted, and a
/// browser there with an admin signed in.
async fn setup() -> sc_error::Result<(Router, Arc<AppMounts>, Arc<Catalog>, Browser, TestDb)> {
    let (catalog, db) = fresh_catalog().await?;
    let apps =
        Arc::new(AppMounts::new(catalog.clone()).with_base_domain(Some(BASE_DOMAIN.to_owned())));
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;

    let mut old = Browser::new(router.clone(), BASE_DOMAIN);
    old.api("GET", "/api/auth/status", None).await;
    let (status, body) = old
        .api(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok((router, apps, catalog, old, db))
}

/// A stored application on `subdomain`, as the settings check reads them.
async fn store_app(catalog: &Catalog, name: &str, subdomain: &str) -> sc_error::Result<()> {
    let framework = FrameworkRef::new("none")
        .with("store", "site")
        .with("source", "public");
    sc_app::save_application(catalog, &Application::new(name, subdomain, framework)).await?;
    Ok(())
}

/// An application serving one page, which is all a mount needs to be told
/// apart from the admin UI.
fn home_page(catalog: &Arc<Catalog>) -> MountedApp {
    let app = Application::new("Home", "@", FrameworkRef::new("code"));
    let bundle = AssetBundle::new()
        .with("index.html", "<h1>home page</h1>")
        .fallback("index.html");
    MountedApp::new(app, Arc::new(CodeFramework::new("code", bundle)), catalog).unwrap()
}

#[tokio::test]
async fn the_admin_ui_moves_without_a_restart_and_the_admin_follows_signed_in()
-> sc_error::Result<()> {
    let (router, apps, catalog, mut old, _db) = setup().await?;

    // Before the move the base domain is the admin's, and `@` is refused: it
    // would be the admin UI's own host.
    let (status, body) = old
        .api(
            "POST",
            "/api/applications",
            Some(json!({ "name": "Home", "subdomain": "@", "framework": { "name": "none" } })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("Admin subdomain"), "{body}");

    // The save: stored as the router reads it, and live at once.
    let (status, body) = old.save_admin_subdomain(json!(" Admin ")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        sc_config::admin_subdomain(&catalog).await?.as_deref(),
        Some("admin")
    );
    assert_eq!(apps.admin().subdomain().as_deref(), Some("admin"));

    // What the dialog polls: plain HTTP needs no certificate, so it is ready.
    let (status, address) = old.api("GET", "/api/settings/admin-address", None).await;
    assert_eq!(status, StatusCode::OK, "{address}");
    assert_eq!(address["admin_host"], json!(ADMIN_HOST));
    assert_eq!(address["base_domain"], json!(BASE_DOMAIN));
    assert_eq!(address["ready"], json!(true));
    assert_eq!(address["certificate"]["state"], json!("plain_http"));

    // The old address: its API still answers the screen that is waiting, and a
    // page navigation is sent on to the new host, path and all.
    let (status, _) = old.api("GET", "/api/auth/status", None).await;
    assert_eq!(status, StatusCode::OK);
    let page = old
        .request("GET", "/ide/?store=x", Some("text/html"), None)
        .await;
    assert_eq!(page.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        page.location.as_deref(),
        Some("//admin.example.com/ide/?store=x")
    );

    // The new host has no session of its own: the cookie is host-only.
    let mut new = Browser::new(router.clone(), ADMIN_HOST);
    let (_, status_body) = new.api("GET", "/api/auth/status", None).await;
    assert!(status_body["current_user"].is_null(), "{status_body}");

    // Following the move: a handoff minted on the old host...
    let (status, handoff) = old
        .api("POST", "/api/settings/admin-address/handoff", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{handoff}");
    assert_eq!(handoff["host"], json!(ADMIN_HOST));
    let path = handoff["path"].as_str().unwrap().to_owned();

    // ...redeemed on the new one, which signs the same admin in and lands on
    // Settings.
    let landed = new.request("GET", &path, Some("text/html"), None).await;
    assert!(landed.status.is_redirection(), "{}", landed.status);
    assert_eq!(landed.location.as_deref(), Some("/#/settings"));
    assert!(new.cookies.contains_key(SESSION_COOKIE));
    let (_, status_body) = new.api("GET", "/api/auth/status", None).await;
    assert_eq!(
        status_body["current_user"]["email"],
        json!("admin@example.com"),
        "{status_body}"
    );

    // Single use: the same link again is the login screen, not a session.
    let mut replay = Browser::new(router.clone(), ADMIN_HOST);
    let again = replay.request("GET", &path, Some("text/html"), None).await;
    assert_eq!(again.location.as_deref(), Some("/"));
    assert!(!replay.cookies.contains_key(SESSION_COOKIE));

    // A token minted for the admin host is no good anywhere else.
    let (_, handoff) = old
        .api("POST", "/api/settings/admin-address/handoff", None)
        .await;
    let mut elsewhere = Browser::new(router.clone(), "shop.example.com");
    elsewhere
        .request("GET", handoff["path"].as_str().unwrap(), None, None)
        .await;
    assert!(!elsewhere.cookies.contains_key(SESSION_COOKIE));
    Ok(())
}

#[tokio::test]
async fn once_moved_the_base_domain_is_the_root_applications() -> sc_error::Result<()> {
    let (router, apps, _catalog, mut old, _db) = setup().await?;
    let (status, body) = old.save_admin_subdomain(json!("admin")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // An application cannot take the admin UI's subdomain.
    let (status, body) = old
        .api(
            "POST",
            "/api/applications",
            Some(json!({ "name": "Clash", "subdomain": "admin", "framework": { "name": "none" } })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("admin UI"), "{body}");

    apps.mount(home_page(apps.catalog().unwrap()))?;

    // The base domain serves the application — every path of it, the API
    // paths included, as an application's subdomain always has.
    let mut visitor = Browser::new(router.clone(), BASE_DOMAIN);
    let page = visitor.request("GET", "/", Some("text/html"), None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.body, b"<h1>home page</h1>");
    let deep = visitor
        .request("GET", "/api/auth/status", Some("text/html"), None)
        .await;
    assert_eq!(deep.body, b"<h1>home page</h1>");

    // The admin host is the admin's.
    let mut admin = Browser::new(router.clone(), ADMIN_HOST);
    let (status, body) = admin.api("GET", "/api/auth/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["any_user_exists"], json!(true));

    // And the old address's API is the application's too: the admin UI is not
    // there any more for a request to reach.
    let (_, body) = old.api("GET", "/api/auth/status", None).await;
    assert_eq!(
        body,
        Value::Null,
        "the admin API answered on the base domain"
    );
    Ok(())
}

/// An application already being served is moved onto the base domain by
/// editing its subdomain to `@`, and that is a save too: the running mount
/// follows the record, so the base domain is the application's on the next
/// request rather than after a restart, and its old subdomain stops serving it.
#[tokio::test]
async fn an_application_edited_onto_the_base_domain_is_served_there_without_a_restart()
-> sc_error::Result<()> {
    let (router, apps, catalog, mut old, _db) = setup().await?;
    let (status, body) = old.save_admin_subdomain(json!("admin")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Served on `home.example.com`, from a built bundle.
    let framework_ref = FrameworkRef::new("none")
        .with("store", "site")
        .with("source", "public");
    let app = Application::new("Home", "home", framework_ref);
    sc_app::save_application(&catalog, &app).await?;
    let bundle = AssetBundle::new()
        .with("index.html", "<h1>home page</h1>")
        .fallback("index.html");
    let framework = Arc::new(CodeFramework::new("code", bundle).with_build(BuildSpec {
        command: "sh".to_owned(),
        args: vec!["build.sh".to_owned()],
        source_dir: "web".to_owned(),
        output_dir: "web/dist".to_owned(),
        install: None,
        server_entry: None,
    }));
    apps.mount(MountedApp::new(app.clone(), framework, &catalog)?)?;
    let mut subdomain = Browser::new(router.clone(), "home.example.com");
    let page = subdomain.request("GET", "/", Some("text/html"), None).await;
    assert_eq!(page.body, b"<h1>home page</h1>");

    // The admin, on the admin UI's new host, edits the subdomain to `@`.
    let mut admin = Browser::new(router.clone(), ADMIN_HOST);
    admin.cookies = old.cookies.clone();
    let (status, body) = admin
        .api(
            "PUT",
            &format!("/api/applications/{}", app.id.0),
            Some(json!({
                "name": "Home",
                "subdomain": "@",
                "framework": { "name": "none", "config": { "store": "site", "source": "public" } },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The base domain is the application's, not a redirect to the admin UI.
    let mut visitor = Browser::new(router.clone(), BASE_DOMAIN);
    let page = visitor.request("GET", "/", Some("text/html"), None).await;
    assert_eq!(page.status, StatusCode::OK, "{:?}", page.location);
    assert_eq!(page.body, b"<h1>home page</h1>");
    assert_eq!(apps.subdomains(), vec!["@".to_owned()]);

    // And the name it left is no application's any more.
    let left = subdomain.request("GET", "/", Some("text/html"), None).await;
    assert_ne!(left.body, b"<h1>home page</h1>");
    Ok(())
}

/// Every arrangement in which the admin UI and an application would claim one
/// host is refused at the save, with nothing written.
#[tokio::test]
async fn a_move_onto_an_applications_host_is_refused() -> sc_error::Result<()> {
    let (_router, apps, catalog, mut old, _db) = setup().await?;

    // Not a subdomain at all.
    let (status, body) = old.save_admin_subdomain(json!("admin.example.com")).await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("single label"), "{body}");

    // A subdomain an application is served on.
    store_app(&catalog, "Shop", "shop").await?;
    let (status, body) = old.save_admin_subdomain(json!("shop")).await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("Shop"), "{body}");
    assert_eq!(apps.admin().subdomain(), None);
    assert_eq!(sc_config::admin_subdomain(&catalog).await?, None);

    // Back to the base domain while an application is served there.
    let (status, body) = old.save_admin_subdomain(json!("admin")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    store_app(&catalog, "Home", "@").await?;
    let (status, body) = old.save_admin_subdomain(json!("")).await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("Home"), "{body}");
    assert_eq!(apps.admin().subdomain().as_deref(), Some("admin"));
    Ok(())
}

/// Without a base domain there is nothing to be a subdomain of.
#[tokio::test]
async fn without_a_base_domain_the_admin_ui_cannot_move() -> sc_error::Result<()> {
    let (catalog, _db) = fresh_catalog().await?;
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps.clone(),
    )?;
    let mut browser = Browser::new(router, "localhost");
    browser.api("GET", "/api/auth/status", None).await;
    browser
        .api(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    let (status, body) = browser.save_admin_subdomain(json!("admin")).await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("base domain"), "{body}");
    Ok(())
}

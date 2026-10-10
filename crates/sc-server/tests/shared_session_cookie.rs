//! Sharing the session cookie between every application of an instance
//! (Settings → Development → Share sign-in between applications).
//!
//! Driven through the assembled router, reading the `Set-Cookie` headers a
//! browser would act on. What is pinned here:
//!
//! - off (the default), the session cookie is **host-only**, as it always was;
//! - on, a login on any host under the base domain gets a cookie scoped to the
//!   base domain, so the browser sends it to the admin UI and every application —
//!   and a host under no base domain (an IP address) still gets a host-only one;
//! - turning it on or off is a **save** that ends every session, the saving
//!   admin's included, and clears that admin's cookie in both scopes, since a
//!   cookie already in a browser keeps the scope it was given.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const EMAIL: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// One browser on one host: its cookies, and the `Set-Cookie` lines of the
/// last response, whole, because their attributes are what is being tested.
///
/// A cookie is its name *and* its `Domain`, as a browser keeps it, so a
/// host-only `sc_session` and a base domain's can both be held; they are sent
/// oldest first, as a browser sends them.
struct Browser {
    router: Router,
    host: String,
    cookies: Vec<(String, Option<String>, String)>,
    set_cookie: Vec<String>,
}

impl Browser {
    fn new(router: Router, host: &str) -> Browser {
        Browser {
            router,
            host: host.to_owned(),
            cookies: Vec::new(),
            set_cookie: Vec::new(),
        }
    }

    async fn api(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.host);
        if !self.cookies.is_empty() {
            let cookies = self
                .cookies
                .iter()
                .map(|(k, _, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookies);
        }
        if method != "GET"
            && let Some((_, _, csrf)) = self.cookies.iter().find(|(k, _, _)| k == CSRF_COOKIE)
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
        self.set_cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|raw| raw.to_str().unwrap().to_owned())
            .collect();
        for line in &self.set_cookie {
            let pair = line.split(';').next().unwrap_or("");
            if let Some((name, value)) = pair.split_once('=') {
                let domain = domain_of(line);
                let held = self
                    .cookies
                    .iter()
                    .position(|(k, d, _)| k == name && *d == domain);
                match (held, value.is_empty()) {
                    (Some(at), true) => {
                        self.cookies.remove(at);
                    }
                    (Some(at), false) => self.cookies[at].2 = value.to_owned(),
                    (None, true) => {}
                    (None, false) => self
                        .cookies
                        .push((name.to_owned(), domain, value.to_owned())),
                }
            }
        }
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    /// The `Set-Cookie` lines of the last response that name the session.
    fn session_cookies(&self) -> Vec<String> {
        self.set_cookie
            .iter()
            .filter(|line| line.starts_with(&format!("{SESSION_COOKIE}=")))
            .cloned()
            .collect()
    }

    /// The `Domain` of the one session cookie the last response set, `None`
    /// for a host-only one.
    fn session_domain(&self) -> Option<String> {
        let set: Vec<String> = self
            .session_cookies()
            .into_iter()
            .filter(|line| !line.starts_with(&format!("{SESSION_COOKIE}=;")))
            .collect();
        assert_eq!(
            set.len(),
            1,
            "one session cookie set: {:?}",
            self.set_cookie
        );
        domain_of(&set[0])
    }

    async fn login(&mut self) {
        self.api("GET", "/api/auth/status", None).await;
        let (status, body) = self
            .api(
                "POST",
                "/api/login",
                Some(json!({ "email": EMAIL, "password": PASSWORD })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    async fn share(&mut self, shared: bool) -> (StatusCode, Value) {
        self.api(
            "POST",
            "/api/settings",
            Some(json!({ "values": { "shared_session_cookie": shared } })),
        )
        .await
    }

    async fn signed_in(&mut self) -> bool {
        let (_, body) = self.api("GET", "/api/auth/status", None).await;
        body["current_user"]["email"] == EMAIL
    }
}

/// The `Domain` attribute of a `Set-Cookie` line.
fn domain_of(line: &str) -> Option<String> {
    line.split(';').find_map(|attr| {
        let (name, value) = attr.trim().split_once('=')?;
        name.eq_ignore_ascii_case("domain")
            .then(|| value.to_owned())
    })
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
    sc_auth::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    Ok((catalog, db))
}

/// A router serving the admin UI on `example.com`, and a browser there with an
/// admin signed in.
async fn setup() -> sc_error::Result<(Router, Arc<AppMounts>, Browser, TestDb)> {
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
    let mut admin = Browser::new(router.clone(), BASE_DOMAIN);
    admin.api("GET", "/api/auth/status", None).await;
    let (status, body) = admin
        .api(
            "POST",
            "/api/first-user",
            Some(json!({ "email": EMAIL, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok((router, apps, admin, db))
}

#[tokio::test]
async fn sharing_the_session_cookie_scopes_it_to_the_base_domain_and_signs_everyone_out()
-> sc_error::Result<()> {
    let (router, apps, mut admin, _db) = setup().await?;
    // Off by default: the first user's cookie is the host's alone.
    assert_eq!(admin.session_domain(), None);
    assert!(!apps.shared_session_cookie());

    // A second browser, signed in on another host, whose session the save must
    // end as well as the admin's own.
    let mut other = Browser::new(router.clone(), "shop.example.com");
    other.login().await;
    assert!(other.signed_in().await);

    let (status, body) = admin.share(true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["values"]["shared_session_cookie"],
        json!(true),
        "{body}"
    );
    assert!(apps.shared_session_cookie(), "live without a restart");
    // The admin's cookie is cleared in both scopes: the host-only one it holds,
    // and the base domain's, which is where the next one will be.
    let cleared: Vec<Option<String>> = admin
        .session_cookies()
        .iter()
        .map(|l| domain_of(l))
        .collect();
    assert_eq!(cleared.len(), 2, "{:?}", admin.set_cookie);
    assert!(cleared.contains(&None), "{cleared:?}");
    assert!(
        cleared.contains(&Some(BASE_DOMAIN.to_owned())),
        "{cleared:?}"
    );
    assert!(!admin.signed_in().await, "the saving admin is signed out");
    assert!(!other.signed_in().await, "every session is ended");

    // Signing in again, on the base domain or under it, gives a cookie every
    // application is sent.
    admin.login().await;
    assert_eq!(admin.session_domain().as_deref(), Some(BASE_DOMAIN));
    assert!(admin.signed_in().await);
    // What sharing is for: the cookie set on the admin UI's host is good on
    // another host under the base domain, which a browser sends it to.
    let mut elsewhere = Browser::new(router.clone(), "blog.example.com");
    elsewhere.cookies = (admin.cookies.iter())
        .filter(|(name, domain, _)| name == SESSION_COOKIE && domain.is_some())
        .cloned()
        .collect();
    assert!(elsewhere.signed_in().await);
    // The other browser still held its ended host-only cookie; the login that
    // sets the shared one expires it, so it cannot shadow the new session.
    other.login().await;
    assert_eq!(other.session_domain().as_deref(), Some(BASE_DOMAIN));
    let held: Vec<&Option<String>> = (other.cookies.iter())
        .filter(|(name, _, _)| name == SESSION_COOKIE)
        .map(|(_, domain, _)| domain)
        .collect();
    assert_eq!(held, [&Some(BASE_DOMAIN.to_owned())]);
    assert!(other.signed_in().await);
    // A host under no base domain has nothing to share it with.
    let mut by_address = Browser::new(router.clone(), "127.0.0.1:3000");
    by_address.login().await;
    assert_eq!(by_address.session_domain(), None);

    // Saving again without a change ends nothing.
    let (status, body) = admin.share(true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(admin.signed_in().await);

    // And off again: everyone out, and the next cookie is host-only once more.
    let (status, body) = admin.share(false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!apps.shared_session_cookie());
    assert!(!admin.signed_in().await);
    assert!(!other.signed_in().await);
    admin.login().await;
    assert_eq!(admin.session_domain(), None);
    Ok(())
}

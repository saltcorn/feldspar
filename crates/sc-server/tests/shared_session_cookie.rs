//! Sharing the session cookie between every application of an instance
//! (Settings → Authentication → Share sign-in between applications).
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
//!
//! And, beside it in Settings → Authentication, that the session lifetime a
//! sign-in gets is the **stored** one, even when this process did not do the
//! saving (another node, `feldspar config set`, a restore).
//!
//! And that a browser's session cookie **outlives the browser** (TODO A1.1): a
//! `Max-Age` of the session's lifetime unless *Keep people signed in when the
//! browser closes* is off or the sign-in said `remember: false`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_server::{AppMounts, SESSION_COOKIE, ServerConfig, admin_handlers, build_router_with_apps};
use sc_test_harness::{TestDb, TestServer};
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
    /// Sent with every request, as a native client sends its kind.
    headers: Vec<(String, String)>,
}

impl Browser {
    fn new(router: Router, host: &str) -> Browser {
        Browser {
            router,
            host: host.to_owned(),
            cookies: Vec::new(),
            set_cookie: Vec::new(),
            headers: Vec::new(),
        }
    }

    async fn api(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.host);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
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
        self.login_with(json!({})).await;
    }

    /// Sign in with `extra` fields beside the email and password.
    async fn login_with(&mut self, extra: Value) {
        self.api("GET", "/api/auth/status", None).await;
        let mut body = json!({ "email": EMAIL, "password": PASSWORD });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, body) = self.api("POST", "/api/login", Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// The one session cookie the last response set (not a removal).
    fn session_set(&self) -> String {
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
        set[0].clone()
    }

    /// The `Max-Age` of the session cookie the last response set, `None` for
    /// one that ends when the browser closes.
    fn session_max_age(&self) -> Option<i64> {
        let line = self.session_set();
        let max_age = line.split(';').find_map(|attr| {
            let (name, value) = attr.trim().split_once('=')?;
            name.eq_ignore_ascii_case("max-age")
                .then(|| value.parse().unwrap())
        });
        let expires = line
            .split(';')
            .any(|attr| attr.trim().to_ascii_lowercase().starts_with("expires="));
        assert_eq!(
            max_age.is_some(),
            expires,
            "Max-Age and Expires go together: {line}"
        );
        max_age
    }

    fn native(mut self) -> Browser {
        self.headers.push((
            sc_api::auth::CLIENT_KIND_HEADER.to_owned(),
            sc_api::auth::NATIVE_CLIENT.to_owned(),
        ));
        self
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
    let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
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

#[tokio::test]
async fn a_sign_in_gets_the_stored_session_lifetime_even_when_saved_elsewhere()
-> sc_error::Result<()> {
    let (router, apps, _admin, _db) = setup().await?;
    assert_eq!(apps.session_ttl(), chrono::Duration::hours(24));
    // Written straight to `_fd_config`, as another node's save, `feldspar
    // config set` or a restore would: nothing tells this process.
    let catalog = apps.catalog().unwrap().clone();
    sc_config::set_config(&catalog, sc_config::SESSION_TTL_HOURS, json!(8760)).await?;

    // A native client's cookie says how long its session lasts.
    let mut phone = Browser::new(router.clone(), BASE_DOMAIN);
    phone.headers.push((
        sc_api::auth::CLIENT_KIND_HEADER.to_owned(),
        sc_api::auth::NATIVE_CLIENT.to_owned(),
    ));
    phone.login().await;
    let line = phone
        .session_cookies()
        .into_iter()
        .find(|l| !l.starts_with(&format!("{SESSION_COOKIE}=;")))
        .unwrap();
    assert!(
        line.contains(&format!("Max-Age={}", 8760 * 3600)),
        "a year, not the 24h this process started with: {line}"
    );
    assert!(phone.signed_in().await);
    assert_eq!(apps.session_ttl(), chrono::Duration::hours(8760));
    Ok(())
}

/// The reported bug (TODO A1.1): a browser's sign-in ended when the browser
/// closed, whatever the session lifetime. By default its cookie now lasts as
/// long as the session — host-only or on the shared domain alike.
#[tokio::test]
async fn a_browser_sign_in_outlives_the_browser_by_default() -> sc_error::Result<()> {
    let (router, _apps, mut admin, _db) = setup().await?;
    // The first user's own sign-in, and a later one.
    assert_eq!(admin.session_max_age(), Some(24 * 3600));
    let mut browser = Browser::new(router.clone(), BASE_DOMAIN);
    browser.login().await;
    assert_eq!(browser.session_max_age(), Some(24 * 3600));
    assert_eq!(browser.session_domain(), None);
    assert!(browser.signed_in().await);

    // Shared between applications: the base domain's cookie, persistent too.
    let (status, body) = admin.share(true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut shared = Browser::new(router, BASE_DOMAIN);
    shared.login_with(json!({ "remember": true })).await;
    assert_eq!(shared.session_domain().as_deref(), Some(BASE_DOMAIN));
    assert_eq!(shared.session_max_age(), Some(24 * 3600));

    // Logout still clears it.
    shared.api("POST", "/api/logout", None).await;
    assert!(!shared.signed_in().await);
    Ok(())
}

/// "Remember me" unticked: that sign-in's cookie ends with the browser — in a
/// native app too, which asked for it — though the session lasts as long.
#[tokio::test]
async fn a_sign_in_that_is_not_remembered_ends_with_the_browser() -> sc_error::Result<()> {
    let (router, apps, _admin, _db) = setup().await?;
    let mut browser = Browser::new(router.clone(), BASE_DOMAIN);
    browser.login_with(json!({ "remember": false })).await;
    assert_eq!(browser.session_max_age(), None);
    assert!(browser.signed_in().await);

    let mut phone = Browser::new(router, BASE_DOMAIN).native();
    phone.login_with(json!({ "remember": false })).await;
    assert_eq!(phone.session_max_age(), None);
    assert!(phone.signed_in().await);
    assert!(apps.persistent_session_cookie());
    Ok(())
}

/// Turned off — where people share a computer — a browser's cookie ends with the
/// browser and `remember: true` cannot override it; a native app's still lasts.
/// Read from `_fd_config` at the sign-in, so a change saved by another node
/// applies here too, and a save through Settings is accepted and live.
#[tokio::test]
async fn turned_off_a_browser_sign_in_ends_with_the_browser() -> sc_error::Result<()> {
    let (router, apps, mut admin, _db) = setup().await?;
    let catalog = apps.catalog().unwrap().clone();
    sc_config::set_config(&catalog, sc_config::PERSISTENT_SESSION_COOKIE, json!(false)).await?;

    let mut kiosk = Browser::new(router.clone(), BASE_DOMAIN);
    kiosk.login_with(json!({ "remember": true })).await;
    assert_eq!(kiosk.session_max_age(), None);
    assert!(!apps.persistent_session_cookie());

    let mut phone = Browser::new(router.clone(), BASE_DOMAIN).native();
    phone.login().await;
    assert_eq!(phone.session_max_age(), Some(24 * 3600));

    // Back on, through the settings screen.
    let (status, body) = admin
        .api(
            "POST",
            "/api/settings",
            Some(json!({ "values": { "persistent_session_cookie": true } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(apps.persistent_session_cookie());
    let mut browser = Browser::new(router, BASE_DOMAIN);
    browser.login().await;
    assert_eq!(browser.session_max_age(), Some(24 * 3600));
    Ok(())
}

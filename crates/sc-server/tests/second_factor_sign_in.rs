//! **Two-factor authentication** at sign-in (design §7.2), through the real
//! router against Postgres:
//!
//! - an admin turns it on from a signed-in session; their next password sign-in
//!   gets a challenge and **no session**, and the challenge is answered with a
//!   code from the app (once — the same code again is refused) or with a
//!   recovery code (once);
//! - a role that requires it takes an application's member through setting one
//!   up at sign-in, which signs them in and hands over the recovery codes; they
//!   cannot turn it off, and an admin can reset it;
//! - a browser answers the challenge on the server's own page, with a form, and
//!   goes on to where it was going;
//! - becoming a user is not stopped by the user's second factor.
#![allow(
    clippy::cast_possible_truncation,
    reason = "test code: casts over test data whose range the test chose"
)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{RoleDef, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, SecretKey};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, MountedApp, SECOND_FACTOR_COOKIE, SECOND_FACTOR_PAGE_ROUTE,
    SECOND_FACTOR_VERIFY_ROUTE, ServerConfig, admin_handlers, build_router_with_apps,
};
use sc_test_harness::TestDb;
use sc_types::Role;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const CLUB: &str = "club.example.com";
const SHOP: &str = "shop.example.com";
const ROLE_MEMBER: Role = Role::of(80);
const SESSION: &str = "sc_session";

/// A cookie-carrying client addressing one host, echoing CSRF on mutations.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

/// What came back: the status, the body as JSON (or `Null`), its text, and
/// where a redirect pointed.
struct Answer {
    status: StatusCode,
    json: Value,
    text: String,
    location: Option<String>,
}

impl Client {
    async fn new(router: &Router, host: &str) -> Client {
        let mut client = Client {
            router: router.clone(),
            host: host.to_owned(),
            cookies: HashMap::new(),
        };
        client.request("GET", "/", None).await;
        client
    }

    async fn request(&mut self, method: &str, path: &str, body: Option<(&str, Vec<u8>)>) -> Answer {
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
        let form = matches!(body, Some((ct, _)) if ct.starts_with("application/x-www"));
        if method != "GET"
            && !form
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some((content_type, bytes)) => builder
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(bytes))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            let raw = raw.to_str().unwrap_or("");
            let pair = raw.split(';').next().unwrap_or("");
            if let Some((name, value)) = pair.split_once('=') {
                // A sign-in that replaces a session also expires the cookie
                // in the scope it is not in; this client keeps one scope.
                if value.is_empty() && raw.contains("Domain=") {
                    continue;
                }
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        Answer {
            status,
            json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            text: String::from_utf8_lossy(&bytes).into_owned(),
            location,
        }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let body = body.map(|b| ("application/json", serde_json::to_vec(&b).unwrap()));
        let answer = self.request(method, path, body).await;
        (answer.status, answer.json)
    }

    async fn login(&mut self, email: &str, password: &str) -> (StatusCode, Value) {
        let body = json!({ "email": email, "password": password });
        self.send("POST", "/api/login", Some(body)).await
    }

    fn signed_in(&self) -> bool {
        self.cookies.contains_key(SESSION)
    }
}

/// The code an app with `secret` shows `steps` steps from now.
fn code(secret: &str, steps: i64) -> String {
    sc_auth::totp_at(
        &sc_auth::TotpSecret::from_base32(secret).unwrap(),
        Utc::now().timestamp() + steps * sc_auth::STEP_SECONDS,
    )
}

struct Fixture {
    router: Router,
    _catalog: Arc<Catalog>,
    _db: TestDb,
}

async fn setup() -> sc_error::Result<Fixture> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$;",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    catalog.set_secret_key(SecretKey::generate()?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_config::bootstrap_config(&catalog).await?;
    save_role(&catalog, &RoleDef::new(ROLE_MEMBER, "Member")).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", Role::ADMIN).await?;
    create_user(&catalog, "member@example.com", "member-pw", ROLE_MEMBER).await?;

    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let club = Application::new("Club", "club", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_table(sc_auth::USERS_TABLE.to_owned());
    apps.mount(MountedApp::new_with(
        club,
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html>"),
        )),
        &catalog,
        None,
        apps.triggers(),
    )?)?;
    // A second application, written by somebody else (security review
    // 2026-11, §2: a session names the one application that issued it).
    let shop = Application::new("Shop", "shop", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_table(sc_auth::USERS_TABLE.to_owned());
    apps.mount(MountedApp::new_with(
        shop,
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html>"),
        )),
        &catalog,
        None,
        apps.triggers(),
    )?)?;
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
    Ok(Fixture {
        router,
        _catalog: catalog,
        _db: db,
    })
}

/// Turn on the signed-in `client`'s second factor, giving `password` again,
/// returning its secret and recovery codes. Uses the code of the step before
/// now.
async fn turn_on(client: &mut Client, password: &str) -> (String, Vec<String>) {
    let (status, setup) = client
        .send(
            "POST",
            "/_feldspar/second-factor/setup",
            Some(json!({ "password": password })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{setup}");
    let secret = setup["secret"].as_str().unwrap().to_owned();
    assert!(
        setup["otpauth_url"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/")
    );
    assert!(setup["qr_svg"].as_str().unwrap().contains("<svg"));
    let (status, enabled) = client
        .send(
            "POST",
            "/_feldspar/second-factor/enable",
            Some(json!({ "code": code(&secret, -1) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{enabled}");
    let codes = enabled["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    (secret, codes)
}

#[tokio::test]
async fn an_admin_signs_in_with_a_code_once_turned_on() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut desk = Client::new(&fx.router, BASE_DOMAIN).await;
    let (status, me) = desk.login("admin@example.com", "admin-pw").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        me["second_factor"].is_null(),
        "nothing is asked before it is on: {me}"
    );
    let (secret, codes) = turn_on(&mut desk, "admin-pw").await;
    let (_, status) = desk.send("GET", "/_feldspar/second-factor", None).await;
    assert_eq!(status["enabled"], json!(true));
    assert_eq!(status["recovery_codes_left"], json!(codes.len()));

    // The password alone now starts no session.
    let mut laptop = Client::new(&fx.router, BASE_DOMAIN).await;
    let (status, answer) = laptop.login("admin@example.com", "admin-pw").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let challenge = answer["second_factor"].clone();
    assert_eq!(challenge["setup"], json!(false));
    let token = challenge["token"].as_str().unwrap().to_owned();
    assert!(!laptop.signed_in(), "a challenge, not a session");
    let (status, _) = laptop.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A wrong code leaves the challenge to try again; the right one signs in.
    let verify = |code: String| json!({ "token": token, "code": code });
    let (status, _) = laptop
        .send(
            "POST",
            "/_feldspar/second-factor/verify",
            Some(verify("000000".into())),
        )
        .await;
    if code(&secret, 1) != "000000" {
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, me) = laptop
        .send(
            "POST",
            "/_feldspar/second-factor/verify",
            Some(verify(code(&secret, 1))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["email"], json!("admin@example.com"));
    assert!(laptop.signed_in());
    let (status, _) = laptop.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK);
    // The challenge is spent.
    let (status, _) = laptop
        .send(
            "POST",
            "/_feldspar/second-factor/verify",
            Some(verify(code(&secret, 1))),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The same code on another sign-in is a replay; a recovery code is good once.
    let mut phone = Client::new(&fx.router, BASE_DOMAIN).await;
    let (_, answer) = phone.login("admin@example.com", "admin-pw").await;
    let token = answer["second_factor"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = phone
        .send(
            "POST",
            "/_feldspar/second-factor/verify",
            Some(json!({ "token": token, "code": code(&secret, 1) })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a code is used once");
    let (status, _) = phone
        .send(
            "POST",
            "/_feldspar/second-factor/verify",
            Some(json!({ "token": token, "code": codes[0] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(phone.signed_in());

    // Becoming a user is not stopped by that user's second factor.
    let (status, _) = desk.send("GET", "/api/users", None).await;
    assert_eq!(status, StatusCode::OK);

    // Turning it off takes a code (here, a recovery code); then the password
    // alone signs in again.
    let (status, _) = desk
        .send(
            "POST",
            "/_feldspar/second-factor/disable",
            Some(json!({ "code": codes[0] })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a spent recovery code");
    let (status, _) = desk
        .send(
            "POST",
            "/_feldspar/second-factor/disable",
            Some(json!({ "code": codes[1] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut again = Client::new(&fx.router, BASE_DOMAIN).await;
    let (_, me) = again.login("admin@example.com", "admin-pw").await;
    assert!(me["second_factor"].is_null(), "{me}");
    assert!(again.signed_in());
    Ok(())
}

#[tokio::test]
async fn a_role_that_requires_one_sets_it_up_at_sign_in() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut admin = Client::new(&fx.router, BASE_DOMAIN).await;
    admin.login("admin@example.com", "admin-pw").await;
    let (status, role) = admin
        .send(
            "POST",
            &format!("/api/roles/{ROLE_MEMBER}/second-factor"),
            Some(json!({ "required": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{role}");
    assert_eq!(role["require_second_factor"], json!(true));
    let (status, _) = admin
        .send(
            "POST",
            "/api/roles/100/second-factor",
            Some(json!({ "required": true })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "nobody signs in as the public role"
    );

    // The member's application sign-in is taken through setting one up.
    let mut member = Client::new(&fx.router, CLUB).await;
    let (status, answer) = member.login("member@example.com", "member-pw").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["second_factor"]["setup"], json!(true));
    assert!(!member.signed_in());
    let token = answer["second_factor"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, setup) = member
        .send(
            "POST",
            "/_feldspar/second-factor/setup",
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{setup}");
    assert_eq!(setup["account"], json!("member@example.com"));
    assert!(
        setup["otpauth_url"]
            .as_str()
            .unwrap()
            .contains("issuer=example.com"),
        "listed under the base domain: {setup}"
    );
    let secret = setup["secret"].as_str().unwrap().to_owned();
    let (status, enabled) = member
        .send(
            "POST",
            "/_feldspar/second-factor/enable",
            Some(json!({ "token": token, "code": code(&secret, 0) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{enabled}");
    assert_eq!(enabled["user"]["email"], json!("member@example.com"));
    assert_eq!(
        enabled["recovery_codes"].as_array().unwrap().len(),
        sc_auth::RECOVERY_CODE_COUNT
    );
    assert!(member.signed_in());
    let (status, me) = member.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::OK, "{me}");

    // Required, so it stays on.
    let (_, status_now) = member.send("GET", "/_feldspar/second-factor", None).await;
    assert_eq!(status_now["required"], json!(true));
    let (status, _) = member
        .send(
            "POST",
            "/_feldspar/second-factor/disable",
            Some(json!({ "code": code(&secret, 1) })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // An admin resets it for somebody who lost their phone: their sessions end,
    // and the next sign-in sets one up again.
    let (_, users) = admin.send("GET", "/api/users", None).await;
    let row = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == json!("member@example.com"))
        .unwrap()
        .clone();
    assert_eq!(row["second_factor"], json!(true));
    let id = row["id"].as_str().unwrap();
    let (status, reset) = admin
        .send(
            "POST",
            &format!("/api/users/{id}/second-factor/reset"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    assert_eq!(reset["reset"], json!(true));
    let (status, _) = member.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "signed out by the reset");
    let mut later = Client::new(&fx.router, CLUB).await;
    let (_, answer) = later.login("member@example.com", "member-pw").await;
    assert_eq!(answer["second_factor"]["setup"], json!(true));
    // An application whose own pages know nothing of two-factor
    // authentication sends the browser to the server's page on its own host,
    // which sets one up from the challenge in the cookie.
    let page = later
        .request(
            "GET",
            &format!("{SECOND_FACTOR_PAGE_ROUTE}?return_to=/"),
            None,
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.text.contains("Set up two-factor authentication"),
        "{}",
        page.text
    );
    assert!(page.text.contains("<svg"), "the QR code");
    assert!(page.text.contains("/_feldspar/second-factor/enable"));

    // Becoming the member is not stopped by the requirement.
    let (status, became) = admin
        .send("POST", &format!("/api/users/{id}/become"), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "{became}");
    assert!(became["second_factor"].is_null());
    assert!(admin.signed_in());
    Ok(())
}

#[tokio::test]
async fn a_browser_answers_the_challenge_on_the_servers_page() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut desk = Client::new(&fx.router, BASE_DOMAIN).await;
    desk.login("admin@example.com", "admin-pw").await;
    let (secret, _) = turn_on(&mut desk, "admin-pw").await;

    let mut browser = Client::new(&fx.router, BASE_DOMAIN).await;
    browser.login("admin@example.com", "admin-pw").await;
    assert!(
        browser.cookies.contains_key(SECOND_FACTOR_COOKIE),
        "the challenge rides in its own cookie too"
    );
    let page = browser
        .request(
            "GET",
            &format!("{SECOND_FACTOR_PAGE_ROUTE}?return_to=/after"),
            None,
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.text.contains("Two-factor authentication"),
        "{}",
        page.text
    );
    assert!(page.text.contains(SECOND_FACTOR_VERIFY_ROUTE));
    assert!(page.text.contains("name=\"_csrf\""));

    let csrf = browser.cookies.get(CSRF_COOKIE).unwrap().clone();
    let form = |code: &str| format!("_csrf={csrf}&return_to=%2Fafter&code={code}").into_bytes();
    let wrong = if code(&secret, 1) == "111111" {
        "222222"
    } else {
        "111111"
    };
    let answer = browser
        .request(
            "POST",
            SECOND_FACTOR_VERIFY_ROUTE,
            Some(("application/x-www-form-urlencoded", form(wrong))),
        )
        .await;
    assert!(answer.status.is_redirection());
    assert!(
        answer.location.as_deref().unwrap().contains("error=code"),
        "back to the page, saying so: {:?}",
        answer.location
    );
    let answer = browser
        .request(
            "POST",
            SECOND_FACTOR_VERIFY_ROUTE,
            Some(("application/x-www-form-urlencoded", form(&code(&secret, 1)))),
        )
        .await;
    assert!(answer.status.is_redirection(), "{}", answer.status);
    assert_eq!(answer.location.as_deref(), Some("/after"));
    assert!(browser.signed_in());
    assert!(!browser.cookies.contains_key(SECOND_FACTOR_COOKIE), "spent");

    // With no challenge, the page says the sign-in expired.
    let page = browser.request("GET", SECOND_FACTOR_PAGE_ROUTE, None).await;
    assert!(page.text.contains("expired"), "{}", page.text);
    Ok(())
}

/// A session issued on an application's host is not a session on the admin
/// host (security review #15), for the second factor's routes as for every
/// other: it cannot read, turn off or replace an admin's second factor there.
#[tokio::test]
async fn an_applications_session_is_nobody_on_the_admin_host() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut app = Client::new(&fx.router, CLUB).await;
    let (status, me) = app.login("admin@example.com", "admin-pw").await;
    assert_eq!(status, StatusCode::OK, "{me}");
    let token = app.cookies.get(SESSION).cloned().expect("a session cookie");
    assert!(
        token.starts_with(sc_auth::APP_TOKEN_PREFIX),
        "issued for the application"
    );
    let (status, body) = app.send("GET", "/_feldspar/second-factor", None).await;
    assert_eq!(status, StatusCode::OK, "its own host: {body}");

    // The same cookie, carried to the admin host (a shared cookie domain).
    let mut desk = Client::new(&fx.router, BASE_DOMAIN).await;
    desk.cookies.insert(SESSION.to_owned(), token);
    let (status, _) = desk.send("GET", "/_feldspar/second-factor", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = desk
        .send("POST", "/_feldspar/second-factor/setup", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}

/// On an application's host an admin session (sent there when sign-in is
/// shared across the base domain) is no session for the second factor's
/// routes, so the application's script cannot enroll one on a visiting admin;
/// and a session on its own host sets one up only with the password again
/// (security review 2026-11, #3).
#[tokio::test]
async fn setup_needs_the_password_and_a_session_for_this_host() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut desk = Client::new(&fx.router, BASE_DOMAIN).await;
    let (status, _) = desk.login("admin@example.com", "admin-pw").await;
    assert_eq!(status, StatusCode::OK);
    let admin_token = desk
        .cookies
        .get(SESSION)
        .cloned()
        .expect("a session cookie");
    assert!(!admin_token.starts_with(sc_auth::APP_TOKEN_PREFIX));

    // The admin's cookie, carried to an application's host.
    let mut app = Client::new(&fx.router, CLUB).await;
    app.cookies.insert(SESSION.to_owned(), admin_token);
    for (method, route) in [
        ("GET", "/_feldspar/second-factor"),
        ("POST", "/_feldspar/second-factor/setup"),
        ("POST", "/_feldspar/second-factor/enable"),
        ("POST", "/_feldspar/second-factor/disable"),
        ("POST", "/_feldspar/second-factor/recovery-codes"),
    ] {
        let body = (method == "POST").then(|| json!({ "password": "admin-pw", "code": "123456" }));
        let (status, answer) = app.send(method, route, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{route}: {answer}");
    }

    // The member's own session on the application's host: no password, or a
    // wrong one, sets nothing up; the right one does.
    let mut member = Client::new(&fx.router, CLUB).await;
    let (status, _) = member.login("member@example.com", "member-pw").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = member
        .send("POST", "/_feldspar/second-factor/setup", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.to_string().contains("password"), "{body}");
    let (status, _) = member
        .send(
            "POST",
            "/_feldspar/second-factor/setup",
            Some(json!({ "password": "not-it" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = member
        .send(
            "POST",
            "/_feldspar/second-factor/setup",
            Some(json!({ "password": "member-pw" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["secret"].is_string());
    Ok(())
}

/// The account's budget of wrong codes is counted before a code is checked,
/// so a burst of concurrent guesses gets no more than the budget checked
/// (security review 2026-11, #9).
#[tokio::test]
async fn concurrent_guesses_get_no_more_than_the_budget() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut desk = Client::new(&fx.router, BASE_DOMAIN).await;
    desk.login("admin@example.com", "admin-pw").await;
    turn_on(&mut desk, "admin-pw").await;
    let mut laptop = Client::new(&fx.router, BASE_DOMAIN).await;
    let (_, answer) = laptop.login("admin@example.com", "admin-pw").await;
    let token = answer["second_factor"]["token"]
        .as_str()
        .unwrap()
        .to_owned();

    // Twenty wrong codes at once (from no address, so only the account's
    // budget applies).
    let guesses = (0..20).map(|i| {
        let router = fx.router.clone();
        let token = token.clone();
        let cookies = laptop.cookies.clone();
        async move {
            let mut client = Client {
                router,
                host: BASE_DOMAIN.to_owned(),
                cookies,
            };
            let code = format!("{:06}", 999_000 + i);
            client
                .send(
                    "POST",
                    "/_feldspar/second-factor/verify",
                    Some(json!({ "token": token, "code": code })),
                )
                .await
                .0
        }
    });
    let statuses = futures::future::join_all(guesses).await;
    let checked = statuses
        .iter()
        .filter(|s| **s == StatusCode::UNAUTHORIZED)
        .count();
    let refused = statuses
        .iter()
        .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert_eq!(
        (checked, refused),
        (
            sc_auth::MAX_GUESSES as usize,
            20 - sc_auth::MAX_GUESSES as usize
        ),
        "{statuses:?}"
    );
    Ok(())
}

/// Turning a second factor on or off ends the account's other sessions and
/// keeps the one that did it (security review 2026-11, #11).
#[tokio::test]
async fn changing_the_second_factor_ends_the_other_sessions() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut desk = Client::new(&fx.router, BASE_DOMAIN).await;
    desk.login("admin@example.com", "admin-pw").await;
    let mut stolen = Client::new(&fx.router, BASE_DOMAIN).await;
    stolen.login("admin@example.com", "admin-pw").await;
    let (status, _) = stolen.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK);

    let (secret, _) = turn_on(&mut desk, "admin-pw").await;
    let (status, _) = stolen.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the other session ended");
    let (status, _) = desk.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK, "the one that turned it on goes on");

    // A second session, past the code; then turning it off ends that one.
    let mut laptop = Client::new(&fx.router, BASE_DOMAIN).await;
    let (_, answer) = laptop.login("admin@example.com", "admin-pw").await;
    let token = answer["second_factor"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = laptop
        .send(
            "POST",
            "/_feldspar/second-factor/verify",
            Some(json!({ "token": token, "code": code(&secret, 0) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = desk
        .send(
            "POST",
            "/_feldspar/second-factor/disable",
            Some(json!({ "code": code(&secret, 1) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = laptop.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = desk.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

/// An application's session names the application that issued it (security
/// review 2026-11, §2; H2.1). Carried to another application's host — as a
/// shared sign-in cookie is — it is still the user there (`whoami`), but it
/// is not that application's session, so the second factor's routes there
/// answer as to nobody: the other application's script cannot enroll a second
/// factor on, or turn one off for, an account that only visited it.
#[tokio::test]
async fn a_session_names_the_application_that_issued_it() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut club = Client::new(&fx.router, CLUB).await;
    let (status, me) = club.login("member@example.com", "member-pw").await;
    assert_eq!(status, StatusCode::OK, "{me}");
    let token = club
        .cookies
        .get(SESSION)
        .cloned()
        .expect("a session cookie");
    assert!(token.starts_with(sc_auth::APP_TOKEN_PREFIX));
    assert!(
        matches!(
            sc_auth::SessionScope::of_token(&token),
            sc_auth::SessionScope::App(_)
        ),
        "the token names its application"
    );
    let (status, _) = club.send("GET", "/_feldspar/second-factor", None).await;
    assert_eq!(status, StatusCode::OK, "its own application's session");

    let mut shop = Client::new(&fx.router, SHOP).await;
    shop.cookies.insert(SESSION.to_owned(), token);
    let (status, who) = shop.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::OK, "{who}");
    assert_eq!(
        who["email"],
        json!("member@example.com"),
        "still the user here"
    );
    for (method, route) in [
        ("GET", "/_feldspar/second-factor"),
        ("POST", "/_feldspar/second-factor/setup"),
        ("POST", "/_feldspar/second-factor/enable"),
        ("POST", "/_feldspar/second-factor/disable"),
        ("POST", "/_feldspar/second-factor/recovery-codes"),
    ] {
        let body = (method == "POST").then(|| json!({ "password": "member-pw", "code": "123456" }));
        let (status, answer) = shop.send(method, route, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{route}: {answer}");
    }
    Ok(())
}

/// A sign-in waiting for its second factor ends when the account's password
/// is changed meanwhile — here by an admin — even with the right code
/// (security review 2026-11, B4; H2.3): the challenge was issued under the
/// old credentials.
#[tokio::test]
async fn a_password_reset_ends_a_sign_in_waiting_for_its_code() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut member = Client::new(&fx.router, CLUB).await;
    let (_, me) = member.login("member@example.com", "member-pw").await;
    let id = me["id"].as_str().expect("an id").to_owned();
    let (secret, _) = turn_on(&mut member, "member-pw").await;

    let mut laptop = Client::new(&fx.router, CLUB).await;
    let (_, answer) = laptop.login("member@example.com", "member-pw").await;
    let token = answer["second_factor"]["token"]
        .as_str()
        .expect("a challenge")
        .to_owned();

    let mut desk = Client::new(&fx.router, BASE_DOMAIN).await;
    desk.login("admin@example.com", "admin-pw").await;
    let (status, body) = desk
        .send("POST", &format!("/api/users/{id}/random-password"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = laptop
        .send(
            "POST",
            "/_feldspar/second-factor/verify",
            Some(json!({ "token": token, "code": code(&secret, 0) })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(!laptop.signed_in());
    // The member's own session ended with the old password too.
    let (status, _) = member.send("GET", "/_feldspar/second-factor", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}

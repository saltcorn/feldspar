//! **Email verification** of an application's sign-ups (TODO A2, design §7.2).
//!
//! A *club* app whose members sign themselves up, and read the member list
//! only once they have followed the link emailed to them. Driven through the
//! real router against Postgres, with a recording mailer standing in for SMTP:
//!
//! - sign-up emails a link and signs the account in, served as the public role
//!   until the link is followed: `whoami` says so, and the member list is
//!   closed to it;
//! - the link works once, on another device too, and signs its holder in with
//!   the account's own role; a verification link is not a password link;
//! - "send it again" is throttled, and does nothing for a proven address;
//! - a password reset link proves the address too, with nothing further to do;
//! - an application that does not require verification serves the same
//!   unverified account as its role;
//! - a link that cannot be sent leaves no account behind.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_action::{ActionRegistry, TriggerDispatcher};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{Role, SessionStore, create_user, load_user_by_email, save_role};
use sc_catalog::{AccessRules, Catalog, TableId, TableMeta, save_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_email::{Email, Mailbox, Mailer, RecordingMailer, parse_mailbox};
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const CLUB: &str = "club.example.com";
const LOUNGE: &str = "lounge.example.com";
const ROLE_MEMBER: u8 = 80;

/// A cookie-carrying client addressing one host, echoing CSRF on mutations.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn new(router: &Router, host: &str) -> Client {
        let mut client = Client {
            router: router.clone(),
            host: host.to_owned(),
            cookies: HashMap::new(),
        };
        // Load the SPA first: that is what hands a browser its CSRF cookie.
        client.send("GET", "/", None).await;
        client
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
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
        if method != "GET"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
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
            let pair = raw.to_str().unwrap_or("").split(';').next().unwrap_or("");
            if let Some((name, value)) = pair.split_once('=') {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn signup(&mut self, email: &str, password: &str) -> (StatusCode, Value) {
        let body = json!({ "email": email, "password": password });
        self.send("POST", "/api/signup", Some(body)).await
    }

    async fn login(&mut self, email: &str, password: &str) -> StatusCode {
        let body = json!({ "email": email, "password": password });
        self.send("POST", "/api/login", Some(body)).await.0
    }
}

/// The recorder, which can be told to refuse every message.
struct SwitchMailer {
    recorder: RecordingMailer,
    failing: AtomicBool,
}

#[async_trait]
impl Mailer for SwitchMailer {
    async fn send(&self, email: &Email) -> sc_error::Result<()> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(sc_error::Error::msg("the mail server is down"));
        }
        self.recorder.send(email).await
    }

    async fn sender(&self) -> sc_error::Result<Mailbox> {
        self.recorder.sender().await
    }
}

fn app(name: &str, subdomain: &str, api: ApiConfig) -> Application {
    Application::new(name, subdomain, FrameworkRef::new("code"))
        .with_api(api)
        .with_table(TableId(sc_auth::USERS_TABLE.to_owned()))
}

struct Fixture {
    router: Router,
    catalog: Arc<Catalog>,
    mailer: Arc<SwitchMailer>,
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
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    save_role(&catalog, &Role::new(ROLE_MEMBER, "Member")).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", 1).await?;

    // Members read the member list; the public does not.
    let mut meta = TableMeta::new(sc_auth::USERS_TABLE);
    meta.access = AccessRules {
        min_role_read: ROLE_MEMBER,
        min_role_write: 1,
    };
    save_table_meta(&catalog, &meta).await?;

    let mailer = Arc::new(SwitchMailer {
        recorder: RecordingMailer::sending_as(
            parse_mailbox("Club <noreply@example.com>").unwrap(),
        ),
        failing: AtomicBool::new(false),
    });
    let dispatcher = Arc::new(
        TriggerDispatcher::new(Arc::new(ActionRegistry::new()))
            .with_mailer(mailer.clone() as Arc<dyn Mailer>),
    );
    let apps = Arc::new(AppMounts::new(catalog.clone()).with_triggers(dispatcher));
    let bundle = || {
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html>"),
        ))
    };
    let signup = || {
        ApiConfig::new("rest", "/api")
            .with(sc_api::REST_CFG_ALLOW_SIGNUP, true)
            .with(sc_api::REST_CFG_NEW_USER_ROLE, ROLE_MEMBER)
    };
    let club = app(
        "Club",
        "club",
        signup()
            .with(sc_api::REST_CFG_VERIFY_EMAIL, true)
            .with(sc_api::REST_CFG_VERIFY_EMAIL_SUBJECT, "Welcome to the club"),
    );
    sc_app::validate_api_config(&club, &club.apis[0])?;
    let lounge = app("Lounge", "lounge", signup());
    for app in [club, lounge] {
        apps.mount(MountedApp::new_with(
            app,
            bundle(),
            &catalog,
            None,
            apps.triggers(),
        )?)?;
    }
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
        catalog,
        mailer,
        _db: db,
    })
}

/// The token in the one link a message carries, opening `page`.
fn token_in(email: &Email, origin: &str, page: &str) -> String {
    let body = email.text.as_deref().or(email.html.as_deref()).unwrap();
    let prefix = format!("{origin}{page}#token=");
    let start = body
        .find(&prefix)
        .unwrap_or_else(|| panic!("no link in {body}"))
        + prefix.len();
    body[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// The messages sent so far, waiting briefly for one sent off the request.
async fn sent_after(mailer: &SwitchMailer, count: usize) -> Vec<Email> {
    for _ in 0..100 {
        let sent = mailer.recorder.sent();
        if sent.len() >= count {
            return sent;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    mailer.recorder.sent()
}

#[tokio::test]
async fn a_sign_up_has_full_access_once_it_follows_the_emailed_link() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut laptop = Client::new(&fx.router, CLUB).await;
    let (status, body) = laptop.signup("sam@example.com", "sam-pw").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["email_verified"], json!(false));
    assert_eq!(body["role"], json!(100), "served as the public role");

    // The account itself has the member role; the club serves it as nobody.
    let stored = load_user_by_email(&fx.catalog, "sam@example.com")
        .await?
        .unwrap();
    assert_eq!(stored.role, ROLE_MEMBER);
    assert!(stored.email_unproven());

    let sent = fx.mailer.recorder.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to[0].address, "sam@example.com");
    assert_eq!(sent[0].subject, "Welcome to the club");
    let token = token_in(&sent[0], "http://club.example.com", "/verify-email");

    let (status, me) = laptop.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["role"], json!(100));
    assert_eq!(me["email_verified"], json!(false));
    let (status, _) = laptop.send("GET", "/api/users", None).await;
    assert!(
        status.is_client_error(),
        "an unverified account may not read what members read: {status}"
    );

    // A verification link is not a password link.
    let (status, _) = laptop
        .send(
            "POST",
            "/api/set-password",
            Some(json!({ "token": token, "password": "stolen" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Asking again straight away sends nothing more: one link a minute.
    let (status, body) = laptop
        .send("POST", "/api/resend-verification", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fx.mailer.recorder.sent().len(), 1);

    // The link, opened on a phone that has never signed in.
    let mut phone = Client::new(&fx.router, CLUB).await;
    let (status, body) = phone
        .send("POST", "/api/verify-email", Some(json!({ "token": token })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["email"], json!("sam@example.com"));
    assert_eq!(body["email_verified"], json!(true));
    assert_eq!(body["role"], json!(ROLE_MEMBER));
    let (status, me) = phone.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["role"], json!(ROLE_MEMBER));
    let (status, list) = phone.send("GET", "/api/users", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");

    // It works once.
    let (status, _) = phone
        .send("POST", "/api/verify-email", Some(json!({ "token": token })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Signed in afresh on the laptop, the account is a member; the password
    // chosen at sign-up is untouched.
    let mut laptop = Client::new(&fx.router, CLUB).await;
    assert_eq!(laptop.login("sam@example.com", "sam-pw").await, StatusCode::OK);
    let (_, me) = laptop.send("GET", "/api/whoami", None).await;
    assert_eq!(me["role"], json!(ROLE_MEMBER));
    assert_eq!(me["email_verified"], json!(true));

    // A proven address is sent nothing more.
    let (status, _) = laptop
        .send("POST", "/api/resend-verification", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fx.mailer.recorder.sent().len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_password_reset_link_proves_the_address_too() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut client = Client::new(&fx.router, CLUB).await;
    let (status, _) = client.signup("kim@example.com", "kim-pw").await;
    assert_eq!(status, StatusCode::CREATED);

    let mut other = Client::new(&fx.router, CLUB).await;
    let (status, _) = other
        .send(
            "POST",
            "/api/forgot-password",
            Some(json!({ "email": "kim@example.com" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let sent = sent_after(&fx.mailer, 2).await;
    assert_eq!(sent.len(), 2, "the verification link, then the reset link");
    let reset = token_in(&sent[1], "http://club.example.com", "/set-password");
    let (status, body) = other
        .send(
            "POST",
            "/api/set-password",
            Some(json!({ "token": reset, "password": "kim-pw-2" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["email_verified"], json!(true));
    let (_, me) = other.send("GET", "/api/whoami", None).await;
    assert_eq!(me["role"], json!(ROLE_MEMBER));

    // The verification link sent at sign-up has nothing left to do.
    let verify = token_in(&sent[0], "http://club.example.com", "/verify-email");
    let (status, _) = other
        .send("POST", "/api/verify-email", Some(json!({ "token": verify })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn only_an_application_that_requires_verification_holds_an_account_back()
-> sc_error::Result<()> {
    let fx = setup().await?;
    let mut club = Client::new(&fx.router, CLUB).await;
    let (status, _) = club.signup("lee@example.com", "lee-pw").await;
    assert_eq!(status, StatusCode::CREATED);

    // The lounge does not verify: it has no such endpoints, and serves the
    // same unverified account as the member it is.
    let mut lounge = Client::new(&fx.router, LOUNGE).await;
    assert_eq!(lounge.login("lee@example.com", "lee-pw").await, StatusCode::OK);
    let (_, me) = lounge.send("GET", "/api/whoami", None).await;
    assert_eq!(me["role"], json!(ROLE_MEMBER));
    assert_eq!(me["email_verified"], json!(false));
    let (status, _) = lounge.send("GET", "/api/users", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = lounge
        .send("POST", "/api/verify-email", Some(json!({ "token": "x" })))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A lounge sign-up sends no mail.
    let mut new = Client::new(&fx.router, LOUNGE).await;
    let (status, body) = new.signup("max@example.com", "max-pw").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["role"], json!(ROLE_MEMBER));
    assert_eq!(fx.mailer.recorder.sent().len(), 1, "only lee's link");
    Ok(())
}

#[tokio::test]
async fn a_link_that_cannot_be_sent_leaves_no_account() -> sc_error::Result<()> {
    let fx = setup().await?;
    fx.mailer.failing.store(true, Ordering::SeqCst);
    let mut client = Client::new(&fx.router, CLUB).await;
    let (status, body) = client.signup("ash@example.com", "ash-pw").await;
    assert!(status.is_server_error(), "{status} {body}");
    assert!(
        load_user_by_email(&fx.catalog, "ash@example.com")
            .await?
            .is_none()
    );

    // Once mail goes again, signing up again works.
    fx.mailer.failing.store(false, Ordering::SeqCst);
    let (status, body) = client.signup("ash@example.com", "ash-pw").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(fx.mailer.recorder.sent().len(), 1);
    Ok(())
}

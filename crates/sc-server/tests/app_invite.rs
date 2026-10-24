//! An application's **password links** — invite, forgot-password and
//! set-password — and the `users` table as an application's API sees it
//! (design §7.2, §13.4).
//!
//! The scenario is the one the feature exists for: a *therapists* app whose
//! signed-in therapists make accounts for their patients, and a *patients* app
//! those patients use. Driven through the real router against Postgres, with a
//! recording mailer standing in for SMTP:
//!
//! - a therapist invites a patient **into the patients app**, writing the
//!   message themselves; the patient follows the emailed link there, chooses a
//!   password, is signed in, and can sign in again; the link works once;
//! - an invitation is only ever to a less powerful role, only from a role the
//!   settings allow, only to an application this server serves — and inviting
//!   an address that is in use is a `409`;
//! - "forgot password" answers the same whether or not the address has an
//!   account, sends one link a minute at most, and the link sets a new password;
//! - exposed as a table, `users` never shows its password hash and cannot be
//!   used to hand out power: not by insert, not by update, not on a more
//!   powerful account.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_action::{ActionRegistry, TriggerDispatcher};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{Role, SessionStore, User, create_user, load_user_by_email, save_role};
use sc_catalog::{AccessRules, Catalog, TableId, TableMeta, save_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_email::{Email, Mailer, RecordingMailer, parse_mailbox};
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const THERAPISTS: &str = "therapists.example.com";
const PATIENTS: &str = "patients.example.com";
const ROLE_THERAPIST: u8 = 40;
const ROLE_PATIENT: u8 = 80;

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

    async fn login(&mut self, email: &str, password: &str) -> StatusCode {
        let body = json!({ "email": email, "password": password });
        self.send("POST", "/api/login", Some(body)).await.0
    }
}

fn app(name: &str, subdomain: &str, api: ApiConfig, tables: &[&str]) -> Application {
    let mut app = Application::new(name, subdomain, FrameworkRef::new("code")).with_api(api);
    for table in tables {
        app = app.with_table(TableId((*table).to_owned()));
    }
    app
}

struct Fixture {
    router: Router,
    catalog: Arc<Catalog>,
    mailer: Arc<RecordingMailer>,
    admin: User,
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
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    save_role(&catalog, &Role::new(ROLE_THERAPIST, "Therapist")).await?;
    save_role(&catalog, &Role::new(ROLE_PATIENT, "Patient")).await?;
    save_role(&catalog, &Role::new(90, "Former patient")).await?;
    let admin = create_user(&catalog, "admin@example.com", "admin-pw", 1).await?;
    create_user(&catalog, "ther@example.com", "ther-pw", ROLE_THERAPIST).await?;
    create_user(
        &catalog,
        "other-ther@example.com",
        "ther2-pw",
        ROLE_THERAPIST,
    )
    .await?;

    // Therapists read and write accounts; the table is theirs to manage.
    let mut meta = TableMeta::new(sc_auth::USERS_TABLE);
    meta.access = AccessRules {
        min_role_read: ROLE_THERAPIST,
        min_role_write: ROLE_THERAPIST,
    };
    save_table_meta(&catalog, &meta).await?;

    let recorder = Arc::new(RecordingMailer::sending_as(
        parse_mailbox("Feldspar <noreply@example.com>").unwrap(),
    ));
    let dispatcher = Arc::new(
        TriggerDispatcher::new(Arc::new(ActionRegistry::new()))
            .with_mailer(recorder.clone() as Arc<dyn Mailer>),
    );
    let apps = Arc::new(AppMounts::new(catalog.clone()).with_triggers(dispatcher));
    let bundle = || {
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html>"),
        ))
    };
    let therapists = app(
        "Therapists",
        "therapists",
        ApiConfig::new("rest", "/api")
            .with(sc_api::REST_CFG_ALLOW_INVITE, true)
            .with(sc_api::REST_CFG_INVITE_MIN_ROLE, ROLE_THERAPIST),
        &[sc_auth::USERS_TABLE],
    );
    sc_app::validate_api_config(&therapists, &therapists.apis[0])?;
    let patients = app("Patients", "patients", ApiConfig::new("rest", "/api"), &[]);
    for app in [therapists, patients] {
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
        mailer: recorder,
        admin,
        _db: db,
    })
}

/// The token in the one link a message carries.
fn token_in(email: &Email, origin: &str) -> String {
    let body = email.text.as_deref().or(email.html.as_deref()).unwrap();
    let prefix = format!("{origin}/set-password#token=");
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
async fn sent_after(mailer: &RecordingMailer, count: usize) -> Vec<Email> {
    for _ in 0..100 {
        let sent = mailer.sent();
        if sent.len() >= count {
            return sent;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    mailer.sent()
}

#[tokio::test]
async fn a_therapist_invites_a_patient_into_the_patients_app() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut ther = Client::new(&fx.router, THERAPISTS).await;
    assert_eq!(
        ther.login("ther@example.com", "ther-pw").await,
        StatusCode::OK
    );

    let invitation = json!({
        "email": "pat@example.com",
        "role": ROLE_PATIENT,
        "app": "patients",
        "subject": "Welcome to the clinic, {{email}}",
        "body": "Your therapist has set up your account. Choose a password: {{link}}",
        "from": "Dr Ther <ther@example.com>",
    });
    let (status, body) = ther
        .send("POST", "/api/invite", Some(invitation.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["created"], json!(true));
    assert_eq!(body["user"]["role"], json!(ROLE_PATIENT));

    let sent = fx.mailer.sent();
    assert_eq!(sent.len(), 1);
    let email = &sent[0];
    assert_eq!(email.to[0].address, "pat@example.com");
    assert_eq!(email.from.address, "ther@example.com");
    assert_eq!(email.subject, "Welcome to the clinic, pat@example.com");
    let token = token_in(email, "http://patients.example.com");

    // The account is there, and inert until its owner chooses a password.
    let pat = load_user_by_email(&fx.catalog, "pat@example.com")
        .await?
        .unwrap();
    assert_eq!(pat.role, ROLE_PATIENT);
    let mut patient = Client::new(&fx.router, PATIENTS).await;
    assert_eq!(
        patient.login("pat@example.com", "").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        patient.login("pat@example.com", "anything").await,
        StatusCode::UNAUTHORIZED
    );

    // A pending invitation can be sent again; nothing about the account changes.
    let (status, body) = ther
        .send("POST", "/api/invite", Some(invitation.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["created"], json!(false));
    assert_eq!(fx.mailer.sent().len(), 2);

    // The patient follows the first link, in the patients app.
    let choose = json!({ "token": token, "password": "pat-pw" });
    let (status, body) = patient
        .send("POST", "/api/set-password", Some(choose.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["email"], json!("pat@example.com"));
    let (status, me) = patient.send("GET", "/api/whoami", None).await;
    assert_eq!(status, StatusCode::OK, "signed in by the same response");
    assert_eq!(me["email"], json!("pat@example.com"));

    // Once only — and the second link, sent before the password was chosen,
    // went with the first.
    let mut stranger = Client::new(&fx.router, PATIENTS).await;
    let (status, _) = stranger
        .send("POST", "/api/set-password", Some(choose))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let second = token_in(&fx.mailer.sent()[1], "http://patients.example.com");
    let (status, _) = stranger
        .send(
            "POST",
            "/api/set-password",
            Some(json!({ "token": second, "password": "hijack" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        stranger.login("pat@example.com", "pat-pw").await,
        StatusCode::OK
    );

    // An address in use is not invited again.
    let (status, _) = ther.send("POST", "/api/invite", Some(invitation)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(fx.mailer.sent().len(), 2);
    Ok(())
}

#[tokio::test]
async fn an_invitation_hands_out_no_power_and_links_nowhere_else() -> sc_error::Result<()> {
    let fx = setup().await?;
    let mut ther = Client::new(&fx.router, THERAPISTS).await;
    assert_eq!(
        ther.login("ther@example.com", "ther-pw").await,
        StatusCode::OK
    );
    let invite = |email: &str, role: u8, app: &str| {
        Some(json!({ "email": email, "role": role, "app": app }))
    };

    // Not the inviter's own role, and not a more powerful one.
    for role in [ROLE_THERAPIST, 1] {
        let (status, body) = ther
            .send(
                "POST",
                "/api/invite",
                invite("x@example.com", role, "patients"),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "role {role}: {body}");
    }
    // Only to an application this server serves.
    let (status, body) = ther
        .send("POST", "/api/invite", invite("x@example.com", 80, "evil"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // A message without the link is refused before any account exists.
    let (status, _) = ther
        .send(
            "POST",
            "/api/invite",
            Some(json!({ "email": "x@example.com", "role": 80, "body": "hello" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        load_user_by_email(&fx.catalog, "x@example.com")
            .await?
            .is_none()
    );
    assert!(fx.mailer.sent().is_empty());

    // Without `app` the link opens the inviting application itself.
    let (status, _) = ther
        .send(
            "POST",
            "/api/invite",
            Some(json!({ "email": "y@example.com", "role": 80 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    token_in(&fx.mailer.only()?, "http://therapists.example.com");

    // A patient is below the settings' floor, and the patients app offers no
    // invitations at all.
    create_user(&fx.catalog, "p2@example.com", "p2-pw", ROLE_PATIENT).await?;
    let mut patient = Client::new(&fx.router, THERAPISTS).await;
    assert_eq!(
        patient.login("p2@example.com", "p2-pw").await,
        StatusCode::OK
    );
    let (status, _) = patient
        .send(
            "POST",
            "/api/invite",
            invite("z@example.com", 90, "patients"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let mut elsewhere = Client::new(&fx.router, PATIENTS).await;
    assert_eq!(
        elsewhere.login("ther@example.com", "ther-pw").await,
        StatusCode::OK
    );
    let (status, _) = elsewhere
        .send(
            "POST",
            "/api/invite",
            invite("z@example.com", 90, "patients"),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn a_forgotten_password_is_reset_by_emailed_link() -> sc_error::Result<()> {
    let fx = setup().await?;
    create_user(&fx.catalog, "pat@example.com", "old-pw", ROLE_PATIENT).await?;
    let mut patient = Client::new(&fx.router, PATIENTS).await;

    // The same answer for an address with no account, and nothing is sent.
    let (status, body) = patient
        .send(
            "POST",
            "/api/forgot-password",
            Some(json!({ "email": "nobody@example.com" })),
        )
        .await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "ok": true })));

    let ask = Some(json!({ "email": "pat@example.com" }));
    let (status, body) = patient
        .send("POST", "/api/forgot-password", ask.clone())
        .await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "ok": true })));
    let sent = sent_after(&fx.mailer, 1).await;
    assert_eq!(sent.len(), 1, "exactly one message, to the account");
    assert_eq!(sent[0].to[0].address, "pat@example.com");
    assert_eq!(
        sent[0].from.address, "noreply@example.com",
        "the system's sender"
    );
    let token = token_in(&sent[0], "http://patients.example.com");

    // A second request inside the minute sends nothing more.
    patient.send("POST", "/api/forgot-password", ask).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fx.mailer.sent().len(), 1);

    // A blank password is refused *without* spending the link.
    let (status, _) = patient
        .send(
            "POST",
            "/api/set-password",
            Some(json!({ "token": token, "password": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = patient
        .send(
            "POST",
            "/api/set-password",
            Some(json!({ "token": token, "password": "new-pw" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let mut again = Client::new(&fx.router, PATIENTS).await;
    assert_eq!(
        again.login("pat@example.com", "old-pw").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        again.login("pat@example.com", "new-pw").await,
        StatusCode::OK
    );
    Ok(())
}

/// A reset link is built from the host the request came in on, so a `Host`
/// that routes to the app but reads as another site in a URL
/// (`patients.example.com:@evil.com`) must be refused before any mail is sent.
#[tokio::test]
async fn a_reset_asked_for_on_a_forged_host_sends_nothing() -> sc_error::Result<()> {
    let fx = setup().await?;
    create_user(&fx.catalog, "pat@example.com", "old-pw", ROLE_PATIENT).await?;
    let mut attacker = Client::new(&fx.router, PATIENTS).await;
    for forged in [
        "patients.example.com:@evil.com",
        "patients.example.com:80@evil.com",
        "patients.example.com/evil.com",
        "patients.example.com:80:81",
    ] {
        attacker.host = forged.to_owned();
        let (status, _) = attacker
            .send(
                "POST",
                "/api/forgot-password",
                Some(json!({ "email": "pat@example.com" })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{forged}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(fx.mailer.sent().is_empty(), "no link went anywhere");
    Ok(())
}

#[tokio::test]
async fn the_users_table_shows_no_hash_and_hands_out_no_power() -> sc_error::Result<()> {
    let fx = setup().await?;
    let pat = create_user(&fx.catalog, "pat@example.com", "pat-pw", ROLE_PATIENT).await?;
    let ther = load_user_by_email(&fx.catalog, "ther@example.com")
        .await?
        .unwrap();
    let other = load_user_by_email(&fx.catalog, "other-ther@example.com")
        .await?
        .unwrap();
    let mut client = Client::new(&fx.router, THERAPISTS).await;
    assert_eq!(
        client.login("ther@example.com", "ther-pw").await,
        StatusCode::OK
    );

    // Reads: no hash, and no way to ask about one.
    let (status, rows) = client.send("GET", "/api/users", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let rows = rows.as_array().unwrap();
    assert!(rows.len() >= 4);
    assert!(
        rows.iter().all(|r| r.get("password_hash").is_none()),
        "{rows:?}"
    );
    assert!(rows.iter().all(|r| r.get("email").is_some()));
    for probe in [
        "/api/users?password_hash=like.%24argon2*",
        "/api/users?select=email,password_hash",
        "/api/users?order=password_hash",
    ] {
        let (status, body) = client.send("GET", probe, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{probe}: {body}");
    }

    // Inserts: never a hash, never a role as powerful as the caller's.
    let new = |role: u8| json!({ "id": uuid::Uuid::new_v4().to_string(), "email": format!("n{role}@example.com"), "role": role });
    for role in [1, ROLE_THERAPIST] {
        let (status, body) = client.send("POST", "/api/users", Some(new(role))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "role {role}: {body}");
    }
    let mut hashed = new(ROLE_PATIENT);
    hashed["password_hash"] = json!("$argon2id$v=19$m=19456,t=2,p=1$x$y");
    let (status, _) = client.send("POST", "/api/users", Some(hashed)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = client
        .send("POST", "/api/users", Some(new(ROLE_PATIENT)))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body.get("password_hash").is_none(), "{body}");

    // Updates: a less powerful account, yes; a role no more powerful than the
    // caller's, no; a peer's or an admin's account, not at all.
    let path = |id: uuid::Uuid| format!("/api/users/{id}");
    let (status, body) = client
        .send("PUT", &path(pat.id), Some(json!({ "role": 90 })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("password_hash").is_none());
    let (status, _) = client
        .send(
            "PUT",
            &path(pat.id),
            Some(json!({ "role": ROLE_THERAPIST })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for victim in [other.id, fx.admin.id] {
        let (status, _) = client
            .send(
                "PUT",
                &path(victim),
                Some(json!({ "email": "mine@example.com" })),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = client.send("DELETE", &path(victim), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    // Their own row: they may edit it, and keep — but not raise — their role.
    let (status, _) = client
        .send(
            "PUT",
            &path(ther.id),
            Some(json!({ "role": ROLE_THERAPIST })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = client
        .send("PUT", &path(ther.id), Some(json!({ "role": 1 })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = client
        .send("PUT", &path(pat.id), Some(json!({ "password_hash": "x" })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Nothing above moved an account it should not have.
    let admin = load_user_by_email(&fx.catalog, "admin@example.com")
        .await?
        .unwrap();
    assert_eq!(admin.role, 1);
    let other = sc_auth::load_user(&fx.catalog, other.id).await?.unwrap();
    assert_eq!(other.role, ROLE_THERAPIST);
    assert!(
        load_user_by_email(&fx.catalog, "mine@example.com")
            .await?
            .is_none()
    );
    Ok(())
}

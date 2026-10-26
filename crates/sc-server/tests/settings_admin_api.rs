//! The settings API, driven through the assembled router as an admin's browser
//! drives it (design §9, §13.5).
//!
//! `_fd_config`'s own tests cover the table. What can only be pinned down here,
//! at the HTTP boundary, is what the *screen* is handed and what a save does:
//!
//! - the response carries the **declarations** as well as the values, because a
//!   settings screen that knew what a setting meant would need changing every
//!   time one was added;
//! - a **secret never crosses the wire**: the private key reaches the browser as
//!   the sentinel, and handing that sentinel back leaves the stored key alone —
//!   the §11.1 rule an API key lives under, applied to a certificate's key;
//! - a save is refused **whole**, including for what the settings mean together:
//!   `custom` mode with no certificate, or a key that does not match its chain,
//!   is a message in front of the admin rather than a server that will not bind
//!   at the next restart;
//! - and settings are admin-only, like every other configuration endpoint.
//!
//! The Email section (§18.2) is here for the same reasons, plus one of its own:
//! it is the first section with an *act* as well as fields, and "send a test
//! message" is only worth having if it sends through what is **stored**.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_config::{
    ACME_CONTACT_EMAIL, CERTIFICATE_VERIFY, EMAIL_FROM, LOG_SQL, LOG_VERBOSITY, MCP_ENABLED,
    MCP_LOOPBACK_ONLY, MODE_CUSTOM, MODE_LETSENCRYPT, MODE_OFF, SECURITY_NONE, SECURITY_STARTTLS,
    SMTP_CERTIFICATE_CHECK, SMTP_CERTIFICATE_NAME, SMTP_HOST, SMTP_PASSWORD, SMTP_PORT,
    SMTP_SECURITY, SMTP_USERNAME, SSL_CERTIFICATE, SSL_MODE, SSL_PRIVATE_KEY, SslMode,
    mcp_settings, ssl_settings, stored_config,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::{TestDb, TestSmtp};
use sc_types::SECRET_SENTINEL;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A cookie-jar-carrying client over the router (CSRF + session), as the other
/// admin-API tests use.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router) -> Client {
        Client {
            router,
            cookies: HashMap::new(),
        }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let cookie_header = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie_header);
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
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

/// A self-signed certificate for `localhost`, and its key.
fn self_signed() -> (String, String) {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    (key.cert.pem(), key.key_pair.serialize_pem())
}

/// A router over a real database with the platform tables bootstrapped, and an
/// admin logged in.
async fn setup() -> sc_error::Result<(Client, Arc<Catalog>, TestDb)> {
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

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps),
        sessions,
        &ServerConfig::default(),
    )?;

    let mut client = Client::new(router);
    client.send("GET", "/api/auth/status", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    Ok((client, catalog, db))
}

#[tokio::test]
async fn the_screen_is_handed_the_declarations_and_the_values() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    let (status, body) = client.send("GET", "/api/settings", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let ssl = body["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == json!("ssl"))
        .expect("an SSL section");
    assert!(!ssl["label"].as_str().unwrap().is_empty());

    let mode = ssl["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!(SSL_MODE))
        .expect("the mode field");
    // Everything the screen needs to render a control it knows nothing about:
    // the type, the choices, the default, and the sentence under it.
    assert_eq!(mode["type"], json!("text"));
    assert_eq!(
        mode["options"],
        json!([MODE_OFF, MODE_LETSENCRYPT, MODE_CUSTOM])
    );
    assert_eq!(mode["default"], json!(MODE_OFF));
    assert!(!mode["help"].as_str().unwrap().is_empty());

    let key = ssl["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!(SSL_PRIVATE_KEY))
        .expect("the private key field");
    assert_eq!(key["secret"], json!(true));
    assert_eq!(key["multiline"], json!(true));

    // Nothing is stored yet, so the values are the declared defaults.
    assert_eq!(body["values"][SSL_MODE], json!(MODE_OFF));
    // The HTTPS port is the host's (`feldspar.toml`), not a stored setting.
    assert!(body["values"].get("https_port").is_none(), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_save_stores_what_the_server_then_serves_with() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let (certificate, private_key) = self_signed();

    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SSL_MODE: MODE_CUSTOM,
                SSL_CERTIFICATE: certificate,
                SSL_PRIVATE_KEY: private_key,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    // The boot path reads exactly what was saved.
    let settings = ssl_settings(&catalog).await?;
    assert_eq!(settings.mode, SslMode::Custom);
    assert!(settings.certificate.contains("BEGIN CERTIFICATE"));
    // ...and it is servable, which is what the save promised.
    sc_server::TlsSettings::from_ssl(
        &settings,
        sc_server::DEFAULT_HTTPS_PORT,
        sc_server::TlsNames::default(),
        None,
    )?;
    Ok(())
}

/// The §11.1 rule, applied to a certificate's key: it reaches the browser as the
/// sentinel, and a save that hands the sentinel back leaves the stored key
/// alone. Getting this wrong destroys a working certificate the next time
/// anybody edits an unrelated setting.
#[tokio::test]
async fn the_private_key_never_crosses_the_wire_and_a_save_does_not_destroy_it()
-> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let (certificate, private_key) = self_signed();

    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SSL_MODE: MODE_CUSTOM,
                SSL_CERTIFICATE: certificate,
                SSL_PRIVATE_KEY: private_key.clone(),
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["values"][SSL_PRIVATE_KEY], json!(SECRET_SENTINEL));

    let (_, read) = client.send("GET", "/api/settings", None).await;
    assert_eq!(read["values"][SSL_PRIVATE_KEY], json!(SECRET_SENTINEL));
    // The stored value is the key itself — the redaction is the API's, not the
    // table's, or there would be nothing to serve with.
    assert_eq!(
        stored_config(&catalog, SSL_PRIVATE_KEY).await?,
        Some(json!(private_key))
    );

    // Editing another setting, with the sentinel handed back untouched.
    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SSL_MODE: MODE_CUSTOM,
                SSL_CERTIFICATE: read["values"][SSL_CERTIFICATE].clone(),
                SSL_PRIVATE_KEY: SECRET_SENTINEL,
                ACME_CONTACT_EMAIL: "admin@example.com",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        stored_config(&catalog, SSL_PRIVATE_KEY).await?,
        Some(json!(private_key)),
        "the stored key must survive a save that never saw it"
    );
    assert_eq!(
        ssl_settings(&catalog).await?.contact_email,
        "admin@example.com"
    );
    Ok(())
}

#[tokio::test]
async fn a_configuration_that_cannot_serve_is_refused_whole() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let (certificate, _key) = self_signed();
    let (_other_cert, other_key) = self_signed();

    // A mode with nothing behind it.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { SSL_MODE: MODE_CUSTOM, SMTP_PORT: 2525 }})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains(SSL_CERTIFICATE),
        "{body}"
    );
    // Nothing landed — not even the port, which was fine on its own.
    assert_eq!(stored_config(&catalog, SMTP_PORT).await?, None);
    assert_eq!(stored_config(&catalog, SSL_MODE).await?, None);

    // A key that does not go with the chain: valid PEM, wrong pair.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SSL_MODE: MODE_CUSTOM,
                SSL_CERTIFICATE: certificate,
                SSL_PRIVATE_KEY: other_key,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(stored_config(&catalog, SSL_MODE).await?, None);

    // ACME without a contact address: the CA needs somewhere to send warnings.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { SSL_MODE: MODE_LETSENCRYPT }})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains(ACME_CONTACT_EMAIL),
        "{body}"
    );

    // A value of the wrong type is refused by the declaration itself.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { SMTP_PORT: "eight thousand" }})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And a key nobody declared is a typo, not a new setting.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { "ssl_moed": MODE_OFF }})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    Ok(())
}

/// The Email section is declared like every other one, and its password lives
/// under the same rule the TLS private key does: the sentinel crosses the wire,
/// the key stays in the table, and a save that hands the sentinel back keeps it.
#[tokio::test]
async fn the_email_section_round_trips_with_its_password_redacted() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    let (status, body) = client.send("GET", "/api/settings", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let email = body["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == json!("email"))
        .expect("an Email section");
    let fields: Vec<&str> = email["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        fields,
        [
            SMTP_HOST,
            SMTP_PORT,
            SMTP_SECURITY,
            SMTP_USERNAME,
            SMTP_PASSWORD,
            EMAIL_FROM,
            SMTP_CERTIFICATE_NAME,
            SMTP_CERTIFICATE_CHECK
        ]
    );
    let password = email["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!(SMTP_PASSWORD))
        .expect("the password field");
    assert_eq!(password["secret"], json!(true));
    // Submission over STARTTLS, until an admin says otherwise.
    assert_eq!(body["values"][SMTP_PORT], json!(587));
    assert_eq!(body["values"][SMTP_SECURITY], json!(SECURITY_STARTTLS));
    // And the certificate is checked, until an admin says otherwise.
    assert_eq!(
        body["values"][SMTP_CERTIFICATE_CHECK],
        json!(CERTIFICATE_VERIFY)
    );

    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SMTP_HOST: "smtp.example.com",
                SMTP_PORT: 465,
                SMTP_SECURITY: "tls",
                SMTP_USERNAME: "postmaster@example.com",
                SMTP_PASSWORD: "hunter2",
                EMAIL_FROM: "Saltcorn <saltcorn@example.com>",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["values"][SMTP_PASSWORD], json!(SECRET_SENTINEL));
    assert_eq!(
        stored_config(&catalog, SMTP_PASSWORD).await?,
        Some(json!("hunter2"))
    );

    // What the sender then acts on is what was saved.
    let settings = sc_config::EmailSettings::load(&catalog)
        .await?
        .expect("a configured transport");
    assert_eq!(settings.host, "smtp.example.com");
    assert_eq!(settings.port, 465);
    assert_eq!(settings.from.address, "saltcorn@example.com");

    // Editing an unrelated setting with the sentinel handed back untouched must
    // not destroy the password — the failure this rule exists for.
    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SMTP_HOST: "smtp.example.com",
                SMTP_PORT: 587,
                SMTP_SECURITY: SECURITY_STARTTLS,
                SMTP_USERNAME: "postmaster@example.com",
                SMTP_PASSWORD: SECRET_SENTINEL,
                EMAIL_FROM: "Saltcorn <saltcorn@example.com>",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        stored_config(&catalog, SMTP_PASSWORD).await?,
        Some(json!("hunter2")),
        "the stored password must survive a save that never saw it"
    );
    Ok(())
}

/// The cross-field rules are refused **by name**, and nothing lands — the same
/// contract the TLS section's are under.
#[tokio::test]
async fn the_email_cross_field_rules_are_refused_whole() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    let refused = |body: &Value, expected: &str| {
        assert!(
            body["error"].as_str().unwrap().contains(expected),
            "expected `{expected}` in {body}"
        );
    };

    // A host with nowhere to say the message is from.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { SMTP_HOST: "smtp.example.com" }})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    refused(&body, EMAIL_FROM);
    assert_eq!(stored_config(&catalog, SMTP_HOST).await?, None);

    // A username with no password.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SMTP_HOST: "smtp.example.com",
                EMAIL_FROM: "saltcorn@example.com",
                SMTP_USERNAME: "ada",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    refused(&body, SMTP_PASSWORD);

    // Credentials over an unencrypted connection: a password on the wire.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SMTP_HOST: "smtp.example.com",
                EMAIL_FROM: "saltcorn@example.com",
                SMTP_SECURITY: SECURITY_NONE,
                SMTP_USERNAME: "ada",
                SMTP_PASSWORD: "hunter2",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    refused(&body, "clear");

    // A from-address that is not one.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SMTP_HOST: "smtp.example.com",
                EMAIL_FROM: "not-an-address",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    refused(&body, EMAIL_FROM);

    // Nothing at all landed.
    assert_eq!(stored_config(&catalog, SMTP_HOST).await?, None);
    assert_eq!(stored_config(&catalog, SMTP_USERNAME).await?, None);
    Ok(())
}

/// The Email tab's button, end to end: a real SMTP conversation with a listener
/// on loopback, driven by what is **stored**.
#[tokio::test]
async fn a_test_message_is_sent_through_the_stored_settings() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let server = TestSmtp::start().await?;

    // Nothing is configured yet: the answer points at the screen the admin is
    // looking at rather than failing somewhere in a transport.
    let (status, body) = client
        .send("POST", "/api/settings/email/test", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("Settings"),
        "{body}"
    );

    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SMTP_HOST: "127.0.0.1",
                SMTP_PORT: server.port(),
                SMTP_SECURITY: SECURITY_NONE,
                EMAIL_FROM: "Saltcorn <saltcorn@example.com>",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    // No `to`: it goes to the signed-in admin's own address, which is the one
    // person who can go and check whether it arrived.
    let (status, body) = client
        .send("POST", "/api/settings/email/test", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sent_to"], json!("admin@example.com"));

    let received = server.message().await?;
    let transcript = received.transcript();
    assert!(
        transcript.contains("MAIL FROM:<saltcorn@example.com>"),
        "{transcript}"
    );
    assert!(
        transcript.contains("RCPT TO:<admin@example.com>"),
        "{transcript}"
    );
    // Both bodies, because that is what a real message carries and a transport
    // that mangles `multipart/alternative` should fail here.
    assert!(
        received.body.contains("multipart/alternative"),
        "{}",
        received.body
    );
    assert!(
        received.body.contains("Saltcorn test message"),
        "{}",
        received.body
    );
    Ok(())
}

/// A transport that is not there fails with the transport's own words, rather
/// than a success the admin would believe (decision 12).
#[tokio::test]
async fn a_test_message_that_cannot_be_sent_says_so() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    // A port nothing is listening on: bound and dropped.
    let dead_port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };

    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SMTP_HOST: "127.0.0.1",
                SMTP_PORT: dead_port,
                SMTP_SECURITY: SECURITY_NONE,
                EMAIL_FROM: "saltcorn@example.com",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    let (status, body) = client
        .send("POST", "/api/settings/email/test", Some(json!({})))
        .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("mail server"),
        "{body}"
    );

    // A recipient that is not an address is named, and never reaches a socket.
    let (status, body) = client
        .send(
            "POST",
            "/api/settings/email/test",
            Some(json!({ "to": "not-an-address" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("not-an-address"),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn settings_are_admin_only() -> sc_error::Result<()> {
    let (client, _catalog, _db) = setup().await?;
    // A fresh client over the same router: no session, so no authority.
    let mut anonymous = Client::new(client.router.clone());
    anonymous.send("GET", "/api/auth/status", None).await;

    let (status, _) = anonymous.send("GET", "/api/settings", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = anonymous
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { SSL_MODE: MODE_OFF }})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // …including the act, which would otherwise be a way for anyone to make the
    // server send mail.
    let (status, _) = anonymous
        .send("POST", "/api/settings/email/test", Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Each setting is its own row, so a save that omits a key leaves it alone —
/// which is why the screen sends `null` for an emptied box. Without this the
/// only thing an admin could do to a setting is change it, never clear it.
#[tokio::test]
async fn a_null_clears_a_setting_back_to_its_default() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    let (status, _) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { SMTP_PORT: 2525 }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored_config(&catalog, SMTP_PORT).await?, Some(json!(2525)));

    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { SMTP_PORT: Value::Null }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stored_config(&catalog, SMTP_PORT).await?, None);
    // ...and what comes back is the declared default, which is what the screen
    // then shows in the box the admin just emptied.
    assert_eq!(body["values"][SMTP_PORT], json!(587));
    Ok(())
}

/// The Development section (§16): the two switches that decide what a running
/// server prints.
///
/// What is worth driving through HTTP here is that saving them **moves the
/// process**, immediately. The values in `_fd_config` are inert — the thing that
/// decides whether the next statement is echoed is a `sc_log` atomic — so a save
/// that stored them and did not apply them would look right in the form, and in
/// the database, and change nothing about the server the admin is watching.
#[tokio::test]
async fn saving_the_development_settings_moves_the_logging_switches() -> sc_error::Result<()> {
    let _switches = crate::logging_switches().lock().await;
    let (mut client, catalog, _db) = setup().await?;

    // The screen is handed the checkbox and the five levels, and nothing here
    // had to name them: they come from the same declaration the parser reads.
    let (status, body) = client.send("GET", "/api/settings", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let development = body["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == json!("development"))
        .expect("a Development section");
    assert_eq!(development["label"], json!("Development"));
    let fields = development["fields"].as_array().unwrap();
    let log_sql = fields
        .iter()
        .find(|f| f["name"] == json!(LOG_SQL))
        .expect("the Log SQL field");
    assert_eq!(log_sql["type"], json!("bool"));
    assert_eq!(log_sql["label"], json!("Log SQL"));
    let verbosity = fields
        .iter()
        .find(|f| f["name"] == json!(LOG_VERBOSITY))
        .expect("the verbosity field");
    assert_eq!(
        verbosity["options"],
        json!(["error", "warning", "info", "verbose", "trace"])
    );
    assert_eq!(verbosity["default"], json!("warning"));
    // Nothing has been saved, so the server is at its quiet default: requests
    // are not logged.
    assert!(!sc_log::log_sql_enabled());

    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { LOG_SQL: true, LOG_VERBOSITY: "info" }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["values"][LOG_SQL], json!(true));
    assert_eq!(saved["values"][LOG_VERBOSITY], json!("info"));
    // The process itself, not just the row: SQL is echoed, and Info is reached,
    // which is the level at which every request is logged.
    assert!(sc_log::log_sql_enabled());
    assert_eq!(sc_log::verbosity(), sc_log::Verbosity::Info);
    assert!(sc_log::enabled(sc_log::Verbosity::Info));
    assert!(!sc_log::enabled(sc_log::Verbosity::Verbose));
    // And a boot against this database would come up the same way.
    let stored = sc_config::development_settings(&catalog).await?;
    assert!(stored.log_sql);
    assert_eq!(stored.verbosity, sc_log::Verbosity::Info);

    // A save of another section says nothing about logging, so it leaves the
    // switches where they are — and here, where many servers share one
    // process, it is what keeps one test's save from turning off another's.
    // The process is moved by hand first, so a re-apply of what is stored
    // would show.
    sc_log::set_log_sql(false);
    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { EMAIL_FROM: "Feldspar <feldspar@example.com>" }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert!(
        !sc_log::log_sql_enabled(),
        "a save of the Email section re-applied the stored Log SQL"
    );
    sc_log::set_log_sql(true);

    // Unticking it is the half that has to work as well: a switch that only
    // turns on is a server that has to be restarted to be quiet again.
    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { LOG_SQL: false, LOG_VERBOSITY: Value::Null }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert!(!sc_log::log_sql_enabled());
    assert_eq!(sc_log::verbosity(), sc_log::DEFAULT_VERBOSITY);
    assert!(!sc_log::enabled(sc_log::Verbosity::Info));
    Ok(())
}

/// The two MCP switches, from the declaration the screen renders to the value
/// the route will act on (§13.6).
///
/// What is worth driving through HTTP rather than through `sc-config`'s own
/// tests: an installation nobody has touched serves **no** MCP server, and the
/// switch that turns it on is a save rather than a restart — so what
/// `mcp_settings` reads a moment later has to be what the admin just ticked.
#[tokio::test]
async fn saving_the_mcp_switches_opens_and_closes_the_route() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    let (status, body) = client.send("GET", "/api/settings", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let development = body["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == json!("development"))
        .expect("a Development section");
    let fields = development["fields"].as_array().unwrap();
    for (key, default) in [
        (MCP_ENABLED, json!(false)),
        (MCP_LOOPBACK_ONLY, json!(true)),
    ] {
        let field = fields
            .iter()
            .find(|f| f["name"] == json!(key))
            .unwrap_or_else(|| panic!("the {key} field"));
        assert_eq!(field["type"], json!("bool"), "{key}");
        assert_eq!(field["default"], default, "{key}");
    }

    // Nothing saved: off, and local-only if it were on.
    let settings = mcp_settings(&catalog).await?;
    assert!(!settings.enabled);
    assert!(settings.loopback_only);

    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { MCP_ENABLED: true, MCP_LOOPBACK_ONLY: false }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["values"][MCP_ENABLED], json!(true));
    let settings = mcp_settings(&catalog).await?;
    assert!(settings.enabled);
    assert!(!settings.loopback_only);

    // And unticking it shuts the route again, which is the half that matters:
    // a switch that only turns on is a credential surface that needs a restart
    // to close.
    let (status, saved) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { MCP_ENABLED: false, MCP_LOOPBACK_ONLY: Value::Null }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let settings = mcp_settings(&catalog).await?;
    assert!(!settings.enabled);
    assert!(settings.loopback_only);
    Ok(())
}

/// A level that is not a level is refused in front of the admin, and nothing is
/// stored — the rule every other section's cross-field check follows.
#[tokio::test]
async fn a_verbosity_that_is_not_a_level_is_refused() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    let (status, body) = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { LOG_VERBOSITY: "extremely" }})),
        )
        .await;
    assert_ne!(status, StatusCode::OK);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("extremely"),
        "{body}"
    );
    assert_eq!(stored_config(&catalog, LOG_VERBOSITY).await?, None);
    Ok(())
}

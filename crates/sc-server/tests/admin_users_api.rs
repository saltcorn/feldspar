//! End-to-end tests for user administration (design §7.1), driven through the
//! assembled router against a real Postgres database.
//!
//! `admin_api.rs` walks the whole admin story and creates one user on the way
//! past; this is the users screen itself, and what it asserts is the four things
//! the screen can do that an ordinary row edit cannot:
//!
//! - **A user is the table's shape, not a fixed form.** A column the admin added
//!   to `users` is listed, created and edited like any other field.
//! - **A blank password is a request.** One is generated, returned exactly once,
//!   and works.
//! - **Disabling and force-logout end sessions.** Not "stop new logins" — the
//!   session the account is *in the middle of* stops resolving.
//! - **Becoming a user is a swap.** The admin session that authorised it does not
//!   survive alongside the one it turned into.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, ServerConfig, admin_handlers, build_router,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A cookie-jar-carrying client over the router, as a browser-based SPA would be
/// (the same one `admin_api.rs` uses).
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

    /// A second browser against the same server, sharing nothing but the router
    /// — with its own CSRF cookie, which is what its opening read is for.
    async fn fork(&self) -> Client {
        let mut client = Client::new(self.router.clone());
        client.send("GET", "/api/auth/status", None).await;
        client
    }
}

/// A router over the admin endpoints backed by a fresh catalog, with the first
/// (admin) user created and signed in, and role 40 available to hold.
async fn setup() -> sc_error::Result<(Client, TestDb)> {
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
    // The overlay tables a field edit writes its settings into, which `serve`
    // has by the time anyone reaches a screen.
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog, apps),
        sessions,
        &ServerConfig::default(),
    )?;

    let mut client = Client::new(router);
    // A read first, which is what hands the browser its CSRF cookie; every
    // mutation after this echoes it, as the SPA does.
    client.send("GET", "/api/auth/status", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = client
        .send(
            "POST",
            "/api/roles",
            Some(json!({ "role": 40, "name": "Staff", "description": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    Ok((client, db))
}

/// The listed user with this email.
fn find<'a>(list: &'a Value, email: &str) -> &'a Value {
    list.as_array()
        .expect("a list of users")
        .iter()
        .find(|u| u["email"] == json!(email))
        .unwrap_or_else(|| panic!("no user {email} in {list}"))
}

#[tokio::test]
async fn a_user_form_is_the_users_table_including_the_columns_the_admin_added()
-> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // An admin adds a column to `users` — the one table §7.1 invites them to.
    let (status, _) = client
        .send(
            "POST",
            "/api/tables/users/fields",
            Some(json!({ "name": "nickname", "type": "string" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // …and it is part of creating a user, not something to fill in afterwards.
    let (status, body) = client
        .send(
            "POST",
            "/api/users",
            Some(json!({
                "email": "sam@example.com",
                "password": "sams-password",
                "role": 40,
                "extra": { "nickname": "Sam" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["user"]["extra"]["nickname"], json!("Sam"));
    let id = body["user"]["id"].as_str().unwrap().to_owned();

    let (_, list) = client.send("GET", "/api/users", None).await;
    assert_eq!(find(&list, "sam@example.com")["extra"]["nickname"], "Sam");
    assert_eq!(find(&list, "sam@example.com")["disabled"], json!(false));

    // Editing changes the role and the added column, and leaves the password
    // alone — the form's password box is empty because nobody is changing it.
    let (status, body) = client
        .send(
            "PUT",
            &format!("/api/users/{id}"),
            Some(json!({
                "email": "sam@example.com",
                "password": "",
                "role": 100,
                "extra": { "nickname": "Sammy" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["role"], json!(100));
    assert_eq!(body["extra"]["nickname"], json!("Sammy"));

    let mut sam = client.fork().await;
    let (status, _) = sam
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "sam@example.com", "password": "sams-password" })),
        )
        .await;
    // Role 100 now, so the *admin* login refuses them — but on the role gate,
    // which is a 401 that says nothing, rather than on the password.
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The system's own columns are not admin-defined fields, whatever the body
    // claims — a password hash written through the bag would be a way in.
    let (status, _) = client
        .send(
            "PUT",
            &format!("/api/users/{id}"),
            Some(json!({
                "email": "sam@example.com",
                "role": 100,
                "extra": { "password_hash": "$argon2id$forged" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    Ok(())
}

#[tokio::test]
async fn a_blank_password_is_generated_and_returned_once() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // Role 1: an admin, so the generated password can be checked the only way
    // that matters — by signing in with it.
    let (status, body) = client
        .send(
            "POST",
            "/api/users",
            Some(json!({ "email": "second@example.com", "password": "", "role": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let password = body["generated_password"]
        .as_str()
        .expect("a blank password asks for a generated one")
        .to_owned();
    assert!(password.len() >= 16, "got {password}");

    let mut second = client.fork().await;
    let (status, body) = second
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "second@example.com", "password": password })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["email"], json!("second@example.com"));

    // Resetting hands back a new one and retires the old one.
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, reset) = client
        .send("POST", &format!("/api/users/{id}/random-password"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reset["email"], json!("second@example.com"));
    let next = reset["password"].as_str().unwrap().to_owned();
    assert_ne!(next, password);

    let mut again = client.fork().await;
    let (status, _) = again
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "second@example.com", "password": password })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the old password is spent"
    );
    let (status, _) = again
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "second@example.com", "password": next })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    Ok(())
}

#[tokio::test]
async fn disabling_and_force_logout_end_the_session_already_in_progress() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    let (_, body) = client
        .send(
            "POST",
            "/api/users",
            Some(json!({ "email": "other@example.com", "password": "otherpass", "role": 1 })),
        )
        .await;
    let id = body["user"]["id"].as_str().unwrap().to_owned();

    // They sign in, on their own browser.
    let mut other = client.fork().await;
    let (status, _) = other
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "other@example.com", "password": "otherpass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = other.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK);

    // Forced out: the cookie they are still holding stops being a session.
    let (status, body) = client
        .send("POST", &format!("/api/users/{id}/force-logout"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    let (status, _) = other.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // They can still sign in — a forced logout withdraws the session, not the
    // account.
    let (status, _) = other
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "other@example.com", "password": "otherpass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Disabling withdraws both: the live session and the next login.
    let (status, body) = client
        .send(
            "POST",
            &format!("/api/users/{id}/disabled"),
            Some(json!({ "disabled": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["disabled"], json!(true));
    let (status, _) = other.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = other
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "other@example.com", "password": "otherpass" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // …and re-enabling gives the account back.
    let (status, _) = client
        .send(
            "POST",
            &format!("/api/users/{id}/disabled"),
            Some(json!({ "disabled": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = other
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "other@example.com", "password": "otherpass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // An admin cannot lock themselves out mid-session; another admin can.
    let (_, status_body) = client.send("GET", "/api/auth/status", None).await;
    let me = status_body["current_user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = client
        .send(
            "POST",
            &format!("/api/users/{me}/disabled"),
            Some(json!({ "disabled": true })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = client
        .send("DELETE", &format!("/api/users/{me}"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    Ok(())
}

#[tokio::test]
async fn becoming_a_user_swaps_the_session_rather_than_adding_one() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    let (_, body) = client
        .send(
            "POST",
            "/api/users",
            Some(json!({ "email": "staff@example.com", "password": "staffpass", "role": 40 })),
        )
        .await;
    let id = body["user"]["id"].as_str().unwrap().to_owned();

    let admin_session = client.cookies.get(SESSION_COOKIE).cloned().unwrap();

    let (status, body) = client
        .send("POST", &format!("/api/users/{id}/become"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["email"], json!("staff@example.com"));
    assert_eq!(body["role"], json!(40));

    // A new session, and the admin one is gone rather than merely unused.
    let now = client.cookies.get(SESSION_COOKIE).cloned().unwrap();
    assert_ne!(now, admin_session);
    assert_eq!(
        client.send("GET", "/api/auth/status", None).await.1["current_user"]["email"],
        json!("staff@example.com")
    );
    // Which is to say: an admin route is now refused, and putting the old cookie
    // back does not get it back.
    let (status, _) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    client
        .cookies
        .insert(SESSION_COOKIE.to_owned(), admin_session);
    let (status, _) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    Ok(())
}

#[tokio::test]
async fn a_user_can_be_deleted() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    let (_, body) = client
        .send(
            "POST",
            "/api/users",
            Some(json!({ "email": "temp@example.com", "password": "temppass", "role": 40 })),
        )
        .await;
    let id = body["user"]["id"].as_str().unwrap().to_owned();

    let (status, body) = client
        .send("DELETE", &format!("/api/users/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(true));

    let (_, list) = client.send("GET", "/api/users", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Deleting it again is a 404, not a second success.
    let (status, _) = client
        .send("DELETE", &format!("/api/users/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    Ok(())
}

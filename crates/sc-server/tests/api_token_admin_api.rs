//! The three API token endpoints (design §13.6, TODO phase 2.4), driven through
//! the assembled router against a real Postgres database.
//!
//! `sc-auth`'s own suite tests the credential; this tests the *API over it*, and
//! what that adds is four decisions the storage does not make:
//!
//! - **The plaintext appears in exactly one response.** Minting carries it; the
//!   list never does, and neither does the mint's own token object.
//! - **A mint is for the calling admin**, not for whoever the body names.
//! - **The six flags are the copilot's six flags**, written explicitly, so what
//!   is stored is what the administrator agreed to rather than what a default
//!   said at the time.
//! - **Revocation leaves the row.** The list still shows it, marked dead.
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
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The cookie-jar client `admin_users_api` uses, in miniature.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
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
            && method != "HEAD"
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
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
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
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

/// A router over the admin endpoints with the first admin signed in.
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
    sc_auth::bootstrap(&catalog).await?;
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

    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok((client, db))
}

/// The whole screen's story in one walk: mint, read the list, revoke, read it
/// again — with the plaintext accounted for at every step.
#[tokio::test]
async fn a_token_is_minted_once_listed_without_its_secret_and_revoked_in_place()
-> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    let (status, list) = client.send("GET", "/api/api-tokens", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 0);

    let (status, minted) = client
        .send(
            "POST",
            "/api/api-tokens",
            Some(json!({
                "label": "claude-code on my laptop",
                "grants": { "allow_drop": false, "allow_triggers": true },
                "expires_in_days": 90,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");

    let secret = minted["secret"].as_str().expect("the one plaintext");
    assert!(secret.starts_with("fspk_"), "{secret}");
    // Even in the response that carries it, the credential is beside the row
    // rather than in it: the token object is the same shape the list is made of.
    assert!(!minted["token"].to_string().contains(secret));

    // The seven flags, all of them, whatever the body left out. A stored `grants`
    // records what was agreed to; a sparse one would re-read itself against
    // tomorrow's defaults.
    let grants = minted["token"]["grants"].as_object().unwrap();
    // Seven flags and `allow_all`, which the body did not ask for.
    assert_eq!(grants.len(), 8, "{grants:?}");
    assert_eq!(grants["allow_all"], json!(false));
    assert_eq!(grants["allow_create"], json!(true)); // the default, written down
    assert_eq!(grants["allow_drop"], json!(false));
    // A token is granted what the mint did not mention.
    assert_eq!(grants["allow_access_changes"], json!(true));
    assert_eq!(grants["allow_modules"], json!(true));
    assert_eq!(grants["allow_triggers"], json!(true));

    assert!(minted["token"]["expires_at"].is_string());
    assert!(minted["token"]["last_used_at"].is_null());
    assert_eq!(minted["token"]["live"], json!(true));

    // The list carries the row and not the credential — nor its hash, which is
    // the value that would identify it.
    let (status, list) = client.send("GET", "/api/api-tokens", None).await;
    assert_eq!(status, StatusCode::OK);
    let rendered = list.to_string();
    assert!(
        !rendered.contains(secret),
        "the list must not carry it back"
    );
    assert!(!rendered.contains("hash"), "nor the hash: {rendered}");
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["label"], json!("claude-code on my laptop"));

    // Revoked: the row stays, marked, because a revocation is a thing that
    // happened and this list is where an admin sees that it did.
    let id = list[0]["id"].as_str().unwrap().to_owned();
    let (status, revoked) = client
        .send("POST", &format!("/api/api-tokens/{id}/revoke"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revoked["revoked"], json!(true));

    let (_, list) = client.send("GET", "/api/api-tokens", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(list[0]["revoked_at"].is_string());
    assert_eq!(list[0]["live"], json!(false));

    // A second revoke is not an error and changes nothing.
    let (status, revoked) = client
        .send("POST", &format!("/api/api-tokens/{id}/revoke"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revoked["revoked"], json!(false));
    Ok(())
}

/// A token runs as the admin who minted it, and the body does not get to say
/// otherwise — handing out somebody else's authority is not a thing this API
/// does, even for an administrator who could create that user anyway.
#[tokio::test]
async fn a_mint_is_for_the_calling_admin_whatever_the_body_says() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    let (_, users) = client.send("GET", "/api/users", None).await;
    let me = users[0]["id"].as_str().unwrap().to_owned();

    let (status, other) = client
        .send(
            "POST",
            "/api/users",
            Some(json!({ "email": "other@example.com", "password": "", "role": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
    let other_id = other["user"]["id"].as_str().unwrap();

    let (status, minted) = client
        .send(
            "POST",
            "/api/api-tokens",
            Some(json!({ "label": "for somebody else", "user_id": other_id })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    assert_eq!(
        minted["token"]["user_id"],
        json!(me),
        "a token names the caller, not the body"
    );
    Ok(())
}

/// The panel's "all grants" toggle: `allow_all` is stored on the token, beside
/// the seven explicit flags, and an unticked box beside it stays unticked.
#[tokio::test]
async fn a_mint_with_allow_all_stores_it_on_the_token() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let (status, minted) = client
        .send(
            "POST",
            "/api/api-tokens",
            Some(json!({
                "label": "every grant",
                "grants": { "allow_all": true, "allow_drop": false },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{minted}");
    let grants = minted["token"]["grants"].as_object().unwrap();
    assert_eq!(grants["allow_all"], json!(true));
    assert_eq!(grants["allow_drop"], json!(false));
    assert_eq!(grants["allow_modules"], json!(true));

    let (status, refused) = client
        .send(
            "POST",
            "/api/api-tokens",
            Some(json!({ "label": "x", "grants": { "allow_all": "yes" } })),
        )
        .await;
    assert_ne!(status, StatusCode::CREATED, "{refused}");
    assert!(refused.to_string().contains("allow_all"), "{refused}");
    Ok(())
}

/// The refusals a form has to be able to show, each naming what to fix.
#[tokio::test]
async fn a_mint_refuses_a_blank_label_a_bad_flag_and_a_nonsense_expiry() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    for (body, expected) in [
        (json!({ "label": "   " }), "label"),
        (
            json!({ "label": "x", "grants": { "allow_drop": "yes" } }),
            "allow_drop",
        ),
        (
            json!({ "label": "x", "expires_in_days": 0 }),
            "expires_in_days",
        ),
    ] {
        let (status, answer) = client.send("POST", "/api/api-tokens", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
        assert!(
            answer.to_string().contains(expected),
            "the refusal should name `{expected}`: {answer}"
        );
    }

    // Nothing was stored by any of them.
    let (_, list) = client.send("GET", "/api/api-tokens", None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    Ok(())
}

//! Phase 4 integration test: ownership-formula settings through the admin API
//! (§7.3, TODO Phase 4).
//!
//! Storage landed in `_fd_tables` attributes and the merge parses/validates on
//! load; this asserts the *admin-facing* contract over HTTP: a formula round
//! trips and survives the merge, every invalid shape is a 400 naming the
//! problem with nothing written, the RLS flag is refused when it could never be
//! honoured, and a stored formula the schema has drifted from **fails closed**
//! — it grants nothing, and the table reports why instead of breaking.
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

/// A cookie-jar-carrying client over the router, as in `table_settings_api.rs`.
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
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;

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

/// A `books` table with a plain-text `owner` column, created through the API.
async fn create_books(client: &mut Client) {
    let (status, _) = client
        .send("POST", "/api/tables", Some(json!({ "name": "books" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = client
        .send(
            "POST",
            "/api/tables/books/fields",
            Some(json!({ "name": "owner", "type": "text" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

/// The standard settings body with the given ownership pair.
fn settings(formula: &str, rls: bool) -> Value {
    json!({
        "label": "",
        "description": "",
        "min_role_read": 100,
        "min_role_write": 1,
        "ownership_formula": formula,
        "rls_enabled": rls,
        "sync_enabled": false,
    })
}

fn listed<'a>(tables: &'a Value, name: &str) -> &'a Value {
    tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!(name))
        .unwrap_or_else(|| panic!("table `{name}` should be listed"))
}

#[tokio::test]
async fn a_formula_round_trips_and_every_invalid_shape_is_refused_unwritten() -> sc_error::Result<()>
{
    let (mut client, _catalog, _db) = setup().await?;
    create_books(&mut client).await;

    // --- a valid formula saves and round trips ---------------------------
    let (status, saved) = client
        .send(
            "PUT",
            "/api/tables/books",
            Some(settings("owner === user.email", false)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["ownership_formula"], json!("owner === user.email"));
    assert_eq!(saved["ownership_error"], Value::Null);
    assert_eq!(saved["rls_enabled"], json!(false));
    // Postgres can do RLS, so the toggle is offered.
    assert_eq!(saved["rls_available"], json!(true));

    // The listing — where the SPA reads it back — carries the same.
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    let books = listed(&tables, "books");
    assert_eq!(books["ownership_formula"], json!("owner === user.email"));
    assert_eq!(books["ownership_error"], Value::Null);

    // --- every invalid shape is a 400 naming the problem ------------------
    for (formula, names) in [
        // A parse error, with its position.
        ("owner === ((", "parse error"),
        // An unknown identifier, by name.
        ("writer === user.email", "unknown identifier `writer`"),
        // A Ⱶ-path through a non-Key field.
        ("ownerⱵname === 'x'", "not a Key field"),
        // A user field the user table does not have.
        ("owner === user.shoe_size", "no field `shoe_size`"),
        // A statement, not an expression.
        ("owner === 'x'; drop()", "single expression"),
    ] {
        let (status, err) = client
            .send("PUT", "/api/tables/books", Some(settings(formula, false)))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{formula}: {err}");
        let message = err["error"].as_str().unwrap_or_default();
        assert!(
            message.contains(names),
            "{formula}: expected `{names}` in: {message}"
        );

        // …and nothing was written: the stored formula is still the valid one.
        let (_, tables) = client.send("GET", "/api/tables", None).await;
        assert_eq!(
            listed(&tables, "books")["ownership_formula"],
            json!("owner === user.email"),
            "{formula} must not overwrite the stored formula"
        );
    }

    // --- clearing: an empty formula removes it ----------------------------
    let (status, cleared) = client
        .send("PUT", "/api/tables/books", Some(settings("", false)))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cleared["ownership_formula"], json!(""));
    Ok(())
}

#[tokio::test]
async fn the_rls_flag_is_refused_when_it_could_never_be_honoured() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    create_books(&mut client).await;

    // An untranslatable formula is fine *without* RLS — the reified evaluator
    // exists exactly for it (§5)…
    let untranslatable = "owner.includes(user.email)";
    let (status, saved) = client
        .send(
            "PUT",
            "/api/tables/books",
            Some(settings(untranslatable, false)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    // …but RLS is a policy the *database* evaluates, so the same formula with
    // the flag on is refused, naming the construct SQL cannot hold.
    let (status, err) = client
        .send(
            "PUT",
            "/api/tables/books",
            Some(settings(untranslatable, true)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = err["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("row-level security") && message.contains("function call"),
        "got: {message}"
    );
    // The refused flag was not stored.
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    assert_eq!(listed(&tables, "books")["rls_enabled"], json!(false));

    // RLS with no formula at all has nothing to enforce.
    let (status, err) = client
        .send("PUT", "/api/tables/books", Some(settings("", true)))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("needs an ownership formula"),
        "got: {err}"
    );

    // A translatable formula with the flag on stores both.
    let (status, saved) = client
        .send(
            "PUT",
            "/api/tables/books",
            Some(settings("owner === user.email", true)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["rls_enabled"], json!(true));
    Ok(())
}

#[tokio::test]
async fn a_stored_formula_the_schema_drifted_from_fails_closed_and_reports() -> sc_error::Result<()>
{
    let (mut client, catalog, db) = setup().await?;
    create_books(&mut client).await;

    let (status, _) = client
        .send(
            "PUT",
            "/api/tables/books",
            Some(settings("owner === user.email", false)),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // The schema drifts under the stored formula — a migration run outside
    // Saltcorn drops the column the formula reads.
    db.client()
        .await?
        .batch_execute("ALTER TABLE books DROP COLUMN owner")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    catalog.reload().await?;

    // The formula fails closed: not in effect, and the table says why — while
    // staying fully usable and still showing the source the admin can fix.
    let (status, tables) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK);
    let books = listed(&tables, "books");
    assert_eq!(books["ownership_formula"], json!("owner === user.email"));
    let error = books["ownership_error"].as_str().unwrap_or_default();
    assert!(error.contains("unknown identifier `owner`"), "got: {error}");

    // Repairing the formula through the API clears the report.
    let (status, repaired) = client
        .send(
            "PUT",
            "/api/tables/books",
            Some(settings("user !== null", false)),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(repaired["ownership_error"], Value::Null);
    Ok(())
}

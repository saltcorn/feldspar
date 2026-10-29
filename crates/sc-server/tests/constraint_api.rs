//! The constraint endpoints over HTTP (TODO Phase 4): the three verbs a
//! constraint has, admin-only, and what each one leaves behind.
//!
//! Deliberately end-to-end through the router rather than through the schema
//! editor (which `sc-api/tests/constraints.rs` covers): what is being checked
//! here is the *wire* — that one body shape serves four kinds, that a created
//! constraint appears in `listConstraints` with the name it was given, that
//! `managed` distinguishes the ones Saltcorn made, and that none of it is
//! reachable without being an admin.
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

/// A cookie-jar client over the router (session + CSRF), as a browser SPA would.
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

/// A router over the admin endpoints with a logged-in admin and a `book` table.
///
/// The catalog comes back alongside the client because one thing the API does not
/// report is whether an overlay row still *merges*: a field configured as a rich
/// type that is not registered is reported as an issue, not as an error, and the
/// only way to assert it did not happen is to ask the catalog.
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

    // Seed the CSRF cookie before the first mutation (double-submit).
    client.send("GET", "/api/auth/status", None).await;
    client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    client
        .send("POST", "/api/tables", Some(json!({ "name": "book" })))
        .await;
    for (name, type_name) in [("title", "text"), ("blurb", "text"), ("pages", "int")] {
        client
            .send(
                "POST",
                "/api/tables/book/fields",
                Some(json!({ "name": name, "type": type_name })),
            )
            .await;
    }
    Ok((client, catalog, db))
}

/// The constraint named `name` from a `listConstraints` array.
fn constraint<'a>(list: &'a Value, name: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("constraint `{name}` not listed: {list}"))
}

#[tokio::test]
async fn the_four_kinds_go_up_and_come_back_down() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    // One body shape, four kinds — the `type` decides which of its fields are
    // read, and the name comes back derived rather than asked for.
    let cases = [
        (
            json!({
                "type": "unique",
                "fields": ["title", "pages"],
                "error_message": "that title and length is already here",
            }),
            "sc_uq_book_title_pages",
        ),
        (
            json!({ "type": "index", "fields": ["pages"] }),
            "sc_ix_book_pages",
        ),
        (
            json!({ "type": "full_text_search", "language": "english" }),
            "sc_fts_book",
        ),
        (
            json!({
                "type": "formula",
                "name": "long_enough",
                "formula": "pages > 0",
                "error_message": "a book has pages",
            }),
            "sc_ck_book_long_enough",
        ),
    ];
    for (body, expected) in &cases {
        let (status, created) = client
            .send("POST", "/api/tables/book/constraints", Some(body.clone()))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["name"], json!(expected), "{created}");
        assert_eq!(created["managed"], json!(true));
    }

    let (status, list) = client
        .send("GET", "/api/tables/book/constraints", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 4, "{list}");

    let unique = constraint(&list, "sc_uq_book_title_pages");
    assert_eq!(unique["type"], json!("unique"));
    assert_eq!(unique["fields"], json!(["title", "pages"]));
    assert_eq!(
        unique["error_message"],
        json!("that title and length is already here")
    );

    // The full-text index reports the language it was built with, not merely
    // that it is an expression index: the language is what decides whether a
    // search can use it.
    assert_eq!(
        constraint(&list, "sc_fts_book")["language"],
        json!("english")
    );
    // …and the row constraint reports the formula, read back out of the
    // trigger's comment.
    assert_eq!(
        constraint(&list, "sc_ck_book_long_enough")["formula"],
        json!("pages > 0")
    );

    // A row written through the ordinary row endpoint meets the constraint, and
    // is told about it in the admin's own words.
    client
        .send(
            "POST",
            "/api/tables/book/rows",
            Some(json!({ "title": "A", "pages": 10 })),
        )
        .await;
    let (status, refused) = client
        .send(
            "POST",
            "/api/tables/book/rows",
            Some(json!({ "title": "B", "pages": 0 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused.to_string().contains("a book has pages"),
        "{refused}"
    );

    // Deleting takes it away, and the rule with it.
    let (status, dropped) = client
        .send(
            "DELETE",
            "/api/tables/book/constraints/sc_ck_book_long_enough",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{dropped}");
    assert_eq!(dropped["dropped"], json!("sc_ck_book_long_enough"));
    let (_, list) = client
        .send("GET", "/api/tables/book/constraints", None)
        .await;
    assert_eq!(list.as_array().unwrap().len(), 3, "{list}");
    let (status, _) = client
        .send(
            "POST",
            "/api/tables/book/rows",
            Some(json!({ "title": "C", "pages": 0 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn a_constraint_nobody_here_made_is_listed_but_not_marked_as_ours() -> sc_error::Result<()> {
    // The zero-setup rule (§9) applied to constraints: a `UNIQUE` added in
    // `psql` is part of the schema, so it is listed — with `managed: false`, so
    // a screen can say who owns it rather than pretending Saltcorn did.
    let (mut client, catalog, db) = setup().await?;
    db.client()
        .await?
        .batch_execute("ALTER TABLE book ADD CONSTRAINT book_title_key UNIQUE (title)")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    // The cache is what a request reads, and nothing told it the schema moved —
    // the same as for a column added behind the server's back.
    catalog.reload().await?;

    let (status, list) = client
        .send("GET", "/api/tables/book/constraints", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let theirs = constraint(&list, "book_title_key");
    assert_eq!(theirs["type"], json!("unique"));
    assert_eq!(theirs["fields"], json!(["title"]));
    assert_eq!(theirs["managed"], json!(false));
    assert_eq!(theirs["error_message"], json!(null));
    Ok(())
}

#[tokio::test]
async fn a_bad_constraint_is_refused_by_name_and_leaves_nothing_behind() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    for (body, expect) in [
        (
            json!({ "type": "sideways", "fields": ["title"] }),
            "sideways",
        ),
        (json!({ "type": "unique", "fields": ["nope"] }), "nope"),
        (
            json!({ "type": "formula", "name": "mine", "formula": "user.id === pages" }),
            "user",
        ),
        (
            json!({ "type": "formula", "name": "no space allowed", "formula": "pages > 0" }),
            "no space allowed",
        ),
    ] {
        let (status, refused) = client
            .send("POST", "/api/tables/book/constraints", Some(body.clone()))
            .await;
        // A 400 or a 404 depending on whether the body is malformed or names
        // something that is not there; what matters is that it is refused, and
        // that the refusal says which part of it was wrong.
        assert!(status.is_client_error(), "{status}: {refused}");
        assert!(
            refused.to_string().contains(expect),
            "expected `{expect}` in {refused}"
        );
    }
    let (_, list) = client
        .send("GET", "/api/tables/book/constraints", None)
        .await;
    assert!(list.as_array().unwrap().is_empty(), "{list}");

    // And a constraint that is not there cannot be deleted — a 404 rather than
    // a cheerful "dropped".
    let (status, _) = client
        .send(
            "DELETE",
            "/api/tables/book/constraints/sc_uq_book_nothing",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn the_constraint_endpoints_are_admin_only() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    client.send("POST", "/api/logout", None).await;

    for (method, path, body) in [
        ("GET", "/api/tables/book/constraints", None),
        (
            "POST",
            "/api/tables/book/constraints",
            Some(json!({ "type": "index", "fields": ["title"] })),
        ),
        (
            "DELETE",
            "/api/tables/book/constraints/sc_ix_book_title",
            None,
        ),
    ] {
        let (status, body) = client.send(method, path, body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} should need an admin: {body}"
        );
    }
    Ok(())
}

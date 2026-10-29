//! A **metadata table** through the admin API: one of Saltcorn's own `_fd_*`
//! tables, added to the tables list so that its rows and settings can be edited
//! while its schema stays Saltcorn's.
//!
//! Asserted at the HTTP boundary because the rule spans three layers — the
//! overlay row that exposes the table (`sc-catalog`), the schema editor that
//! still refuses to reshape it (`sc-api`), and the listing and row endpoints the
//! SPA drives (`sc-server`).
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

/// A cookie-jar-carrying client over the router, mirroring `admin_api.rs`.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
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
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }
}

/// A router over a fresh catalog with `_fd_tables` and `_fd_file_stores`
/// bootstrapped, and an admin logged in.
async fn setup() -> sc_error::Result<(Client, Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;

    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), Arc::new(AppMounts::new(catalog.clone()))),
        Arc::new(SessionStore::default()),
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
    Ok((client, catalog, db))
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|t| {
            t.as_str()
                .or_else(|| t["name"].as_str())
                .unwrap()
                .to_owned()
        })
        .collect()
}

const STORES: &str = "_fd_file_stores";

#[tokio::test]
async fn a_metadata_table_is_listed_configured_and_edited_but_never_reshaped()
-> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    // Hidden until added, and offered for adding.
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    assert!(!names(&tables).contains(&STORES.to_owned()));
    let (status, available) = client.send("GET", "/api/metadata-tables", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        names(&available).contains(&STORES.to_owned()),
        "{available}"
    );
    assert!(!names(&available).contains(&"users".to_owned()));

    // Added with no settings: admin-only, flagged, and never RLS-capable.
    let (status, added) = client
        .send(
            "POST",
            "/api/tables/metadata",
            Some(json!({ "name": STORES })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["metadata"], json!(true));
    assert_eq!(added["min_role_read"], json!(1));
    assert_eq!(added["rls_available"], json!(false));

    let (_, tables) = client.send("GET", "/api/tables", None).await;
    assert!(names(&tables).contains(&STORES.to_owned()));
    let (_, available) = client.send("GET", "/api/metadata-tables", None).await;
    assert!(!names(&available).contains(&STORES.to_owned()));

    // Neither twice, nor a table that is not Saltcorn's.
    let (status, _) = client
        .send(
            "POST",
            "/api/tables/metadata",
            Some(json!({ "name": STORES })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = client
        .send(
            "POST",
            "/api/tables/metadata",
            Some(json!({ "name": "users" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Its settings are the admin's.
    let settings = |rls: bool| {
        json!({
            "label": "File stores", "description": "", "min_role_read": 40,
            "min_role_write": 1, "ownership_formula": "", "rls_enabled": rls, "sync_enabled": false,
        })
    };
    let (status, updated) = client
        .send(
            "PUT",
            &format!("/api/tables/{STORES}"),
            Some(settings(false)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["label"], json!("File stores"));
    assert_eq!(updated["min_role_read"], json!(40));
    assert_eq!(catalog.require(STORES)?.access.min_role_read, 40);
    // …except row-level security, which would be forced on the server too.
    let (status, refused) = client
        .send(
            "PUT",
            &format!("/api/tables/{STORES}"),
            Some(settings(true)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        refused.to_string().contains("row-level security"),
        "{refused}"
    );

    // Its rows are editable, countable and exportable like any table's. The
    // catalog is not built from this table, so writing it reloads nothing.
    let before = catalog.generation();
    let (status, row) = client
        .send(
            "POST",
            &format!("/api/tables/{STORES}/rows"),
            Some(json!({
                "id": "00000000-0000-4000-8000-000000000001", "name": "archive",
                "backend": "local", "config": {}, "attributes": {},
            })),
        )
        .await;
    assert!(status.is_success(), "{status} {row}");
    assert_eq!(
        catalog.generation(),
        before,
        "no catalog reload for _fd_file_stores"
    );
    let (_, count) = client
        .send("GET", &format!("/api/tables/{STORES}/rows/count"), None)
        .await;
    assert_eq!(count["count"], json!(1));
    let (status, csv) = client
        .send("GET", &format!("/api/tables/{STORES}/csv"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(csv["csv"].as_str().unwrap().contains("archive"), "{csv}");

    // Its shape is not.
    let (status, refused) = client
        .send(
            "POST",
            &format!("/api/tables/{STORES}/fields"),
            Some(json!({ "name": "extra", "type": "text" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(refused.to_string().contains("system table"), "{refused}");

    // "Forget settings" would take it off the list, so it is refused by name…
    let (status, _) = client
        .send("DELETE", &format!("/api/tables/{STORES}/settings"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // …and deleting it takes it off the list without dropping anything.
    let (status, _) = client
        .send("DELETE", &format!("/api/tables/{STORES}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    assert!(!names(&tables).contains(&STORES.to_owned()));
    let stores = catalog.require(STORES)?;
    assert!(stores.is_hidden());
    assert_eq!(stores.access.min_role_read, 1, "its settings go with it");
    assert_eq!(sc_catalog::list_file_stores(&catalog).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn editing_a_row_the_catalog_is_built_from_takes_effect_at_once() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let (status, _) = client
        .send("POST", "/api/tables", Some(json!({ "name": "books" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = client
        .send(
            "POST",
            "/api/tables/metadata",
            Some(json!({ "name": "_fd_tables" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(catalog.require("books")?.access.min_role_read, 1);

    // A settings row for `books`, written as a plain row of `_fd_tables`.
    let id = "00000000-0000-4000-8000-000000000002";
    let (status, row) = client
        .send(
            "POST",
            "/api/tables/_fd_tables/rows",
            Some(json!({
                "id": id, "name": "books", "label": "Books", "description": "",
                "min_role_read": 40, "min_role_write": 1, "attributes": {},
            })),
        )
        .await;
    assert!(status.is_success(), "{status} {row}");
    let books = catalog.require("books")?;
    assert_eq!(books.access.min_role_read, 40);
    assert_eq!(books.label, "Books");

    // Edited…
    let (status, row) = client
        .send(
            "PUT",
            &format!("/api/tables/_fd_tables/rows/{id}"),
            Some(json!({ "min_role_read": 10 })),
        )
        .await;
    assert!(status.is_success(), "{status} {row}");
    assert_eq!(catalog.require("books")?.access.min_role_read, 10);

    // …and deleted: `books` is back to the admin-only default.
    let (status, _) = client
        .send("DELETE", &format!("/api/tables/_fd_tables/rows/{id}"), None)
        .await;
    assert!(status.is_success());
    assert_eq!(catalog.require("books")?.access.min_role_read, 1);
    Ok(())
}

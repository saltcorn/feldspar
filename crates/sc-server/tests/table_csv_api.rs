//! The table page's data endpoints, over HTTP: the row count, and a table's
//! rows out and back in as CSV (design §13.1).
//!
//! These are what the admin SPA's "Table data" strip is built on, and the claim
//! worth an end-to-end test is the **round trip**: what `exportTableCsv` hands
//! back is a document `importTableCsv` accepts, with the values intact through
//! the quoting a comma and an embedded newline force. Alongside that, the two
//! things a bulk import must get right — that a bad row is reported by line and
//! does not take the good rows down with it, and that the same validation an
//! ordinary write goes through applies here, because an import *is* an ordinary
//! write repeated.
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
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
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

/// A router over a fresh catalog with an admin logged in. The [`TestDb`] must
/// stay in scope: dropping it deletes the database.
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
    sc_catalog::bootstrap_table_meta(&catalog).await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog, apps),
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
    Ok((client, db))
}

/// A `book` table with an integer key, a text title, a page count and a date.
///
/// The key is a field like any other and is declared here: nothing invents one
/// (GOALS), and an export/import round trip is about the key as much as the
/// values.
async fn create_book_table(client: &mut Client) {
    let (status, _) = client
        .send("POST", "/api/tables", Some(json!({ "name": "book" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "adding the key");
    for (name, ty) in [("title", "text"), ("pages", "int"), ("published", "date")] {
        let (status, _) = client
            .send(
                "POST",
                "/api/tables/book/fields",
                Some(json!({ "name": name, "type": ty })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "adding `{name}`");
    }
}

#[tokio::test]
async fn rows_are_counted_without_reading_them() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    create_book_table(&mut client).await;

    let (status, body) = client
        .send("GET", "/api/tables/book/rows/count", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], json!(0));

    for title in ["Dune", "Emma"] {
        let (status, _) = client
            .send(
                "POST",
                "/api/tables/book/rows",
                Some(json!({ "title": title })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (_, body) = client
        .send("GET", "/api/tables/book/rows/count", None)
        .await;
    assert_eq!(body["count"], json!(2));

    // A table that is not there is a 404, not a count of zero: "no such table"
    // and "no rows" are different answers and the page shows different things.
    let (status, _) = client
        .send("GET", "/api/tables/nope/rows/count", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn a_table_exports_to_csv_and_imports_back_from_it() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    create_book_table(&mut client).await;

    // A title with a comma and one with an embedded newline: the two values a
    // naive splitter mangles, and the reason the format is not hand-rolled.
    for row in [
        json!({ "title": "Dune, part one", "pages": 412, "published": "1965-08-01" }),
        json!({ "title": "Line\nbreak", "pages": 7, "published": null }),
    ] {
        let (status, _) = client
            .send("POST", "/api/tables/book/rows", Some(row))
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (status, body) = client.send("GET", "/api/tables/book/csv", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["filename"], json!("book.csv"));
    let exported = body["csv"].as_str().unwrap().to_owned();
    // The header is the table's stored columns, in declaration order.
    assert_eq!(
        exported.lines().next().unwrap(),
        "id,title,pages,published",
        "exported: {exported}"
    );
    // The comma and the newline are inside quotes rather than splitting a row.
    assert!(
        exported.contains("\"Dune, part one\""),
        "exported: {exported}"
    );

    // Import the same document into a second table with the same shape, minus
    // the `id` column — which is what an admin moving data between tables does,
    // and what keeps the primary keys the database's to assign.
    let (status, _) = client
        .send("POST", "/api/tables", Some(json!({ "name": "wishlist" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    client
        .send(
            "POST",
            "/api/tables/wishlist/fields",
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    for (name, ty) in [("title", "text"), ("pages", "int"), ("published", "date")] {
        client
            .send(
                "POST",
                "/api/tables/wishlist/fields",
                Some(json!({ "name": name, "type": ty })),
            )
            .await;
    }
    let without_id = strip_column(&exported, "id");
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/wishlist/csv",
            Some(json!({ "csv": without_id })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["inserted"], json!(2));
    assert_eq!(body["errors"], json!([]));

    // The values survived the round trip — quoting, the typed columns and the
    // blank cell that means null.
    let (_, rows) = client.send("GET", "/api/tables/wishlist/rows", None).await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let dune = rows
        .iter()
        .find(|r| r["title"] == json!("Dune, part one"))
        .expect("the comma-bearing title round-tripped");
    assert_eq!(dune["pages"], json!(412));
    assert_eq!(dune["published"], json!("1965-08-01"));
    let broken = rows
        .iter()
        .find(|r| r["title"] == json!("Line\nbreak"))
        .expect("the newline-bearing title round-tripped");
    // An empty cell is absence, not the empty string — a `date` column has no
    // other way to spell "there is no date".
    assert_eq!(broken["published"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn a_bad_row_is_reported_by_line_and_the_good_rows_still_land() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    create_book_table(&mut client).await;

    // Line 3 has a page count that is not a number. An import is not a
    // transaction: the other two rows are the admin's, and refusing the file
    // whole would lose them for one typo.
    let document = "title,pages\nDune,412\nEmma,many\nHamlet,160\n";
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/csv",
            Some(json!({ "csv": document })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["inserted"], json!(2));
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    let message = errors[0].as_str().unwrap();
    assert!(message.starts_with("line 3:"), "message: {message}");
    assert!(message.contains("pages"), "message: {message}");

    let (_, body) = client
        .send("GET", "/api/tables/book/rows/count", None)
        .await;
    assert_eq!(body["count"], json!(2));

    // A header naming a column the table does not have is **ignored**, and the
    // rest of the row is written: a real export carries columns the table was
    // never given, and refusing the file for one of them would refuse most
    // files. (The wrong-file mistake is caught by the required-field check —
    // see `a_required_field_no_column_supplies_refuses_the_file`.)
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/csv",
            Some(json!({ "csv": "title,isbn\nDune,123\n" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["inserted"], json!(1));
    assert_eq!(body["errors"], json!([]));
    let (_, body) = client
        .send("GET", "/api/tables/book/rows/count", None)
        .await;
    assert_eq!(body["count"], json!(3), "the known column was written");
    Ok(())
}

/// A CSV document with one column removed, header and all.
///
/// Only used on documents this test wrote, whose cells are simple enough to
/// split on commas outside quotes — the point being made is about the endpoints,
/// not about this helper.
fn strip_column(document: &str, column: &str) -> String {
    let mut reader = csv::Reader::from_reader(document.as_bytes());
    let header: Vec<String> = reader
        .headers()
        .unwrap()
        .iter()
        .map(str::to_owned)
        .collect();
    let keep: Vec<usize> = (0..header.len()).filter(|i| header[*i] != column).collect();
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer
        .write_record(keep.iter().map(|i| &header[*i]))
        .unwrap();
    for record in reader.records() {
        let record = record.unwrap();
        writer
            .write_record(keep.iter().map(|i| record.get(*i).unwrap_or("")))
            .unwrap();
    }
    String::from_utf8(writer.into_inner().unwrap()).unwrap()
}

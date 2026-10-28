//! CSV import into an existing table, and **creating a table from a CSV**, over
//! HTTP (design §13.1).
//!
//! This suite is deliberately a port of Saltcorn 1's own CSV tests
//! (`packages/saltcorn-data/tests/table.test.ts`), because the format is the one
//! part of an application that arrives from *outside* it: a file an admin
//! exported from a spreadsheet two years ago must still import, and "what does
//! Saltcorn do with this file?" already has an answer that people depend on. The
//! claims are therefore Saltcorn 1's claims — a header matched by its label, an
//! unknown column ignored, a required field missing refusing the file, a primary
//! key column replacing rows, a foreign key given as a summary value, and a
//! table whose fields and types are deduced from the file it was made from.
//!
//! Where the answer here is deliberately **not** Saltcorn 1's, the test says so
//! and why:
//!
//!   - a date is read as ISO-8601 only (there is no locale setting to read
//!     `15.03.2011` against);
//!   - a large integer is an `int` column, because `int` here is 64-bit and
//!     Saltcorn 1's was 32-bit — its fallback to a text column was working
//!     around the narrower type;
//!   - a header cell of pure punctuation is refused rather than silently
//!     dropped, because a file with one is a file with a problem in it.
//!
//! Two things Saltcorn 1 does that this suite also expects, and which the tables
//! here are shaped by: a table is created with **no primary key** unless a field
//! says it is one (GOALS), so every fixture declares its key like any other
//! field, and a CSV's `id` column becomes that key — of whatever type its values
//! turn out to be, integer or UUID or text.
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

    // --- the calls these tests make, named for what they mean ----------------

    /// Create an empty table.
    async fn create_table(&mut self, name: &str) {
        let (status, body) = self
            .send("POST", "/api/tables", Some(json!({ "name": name })))
            .await;
        assert_eq!(status, StatusCode::CREATED, "creating `{name}`: {body}");
    }

    /// Add a field, from the same body the admin UI sends.
    async fn create_field(&mut self, table: &str, field: Value) {
        let (status, body) = self
            .send(
                "POST",
                &format!("/api/tables/{table}/fields"),
                Some(field.clone()),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "adding {field} to `{table}`: {body}"
        );
    }

    /// Insert one row, answering with it.
    async fn insert(&mut self, table: &str, row: Value) -> Value {
        let (status, body) = self
            .send("POST", &format!("/api/tables/{table}/rows"), Some(row))
            .await;
        assert_eq!(status, StatusCode::CREATED, "inserting: {body}");
        body
    }

    /// Import a CSV document into an existing table.
    async fn import(&mut self, table: &str, csv: &str) -> (StatusCode, Value) {
        self.send(
            "POST",
            &format!("/api/tables/{table}/csv"),
            Some(json!({ "csv": csv })),
        )
        .await
    }

    /// Create a table from a CSV document.
    async fn create_from_csv(&mut self, name: &str, csv: &str) -> (StatusCode, Value) {
        self.send(
            "POST",
            "/api/tables/csv",
            Some(json!({ "name": name, "csv": csv })),
        )
        .await
    }

    /// Every row of a table.
    async fn rows(&mut self, table: &str) -> Vec<Value> {
        let (status, body) = self
            .send("GET", &format!("/api/tables/{table}/rows"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "listing `{table}`: {body}");
        body.as_array().cloned().unwrap_or_default()
    }

    /// The one row of `table` whose `column` holds `value`.
    async fn row_where(&mut self, table: &str, column: &str, value: &str) -> Value {
        let rows = self.rows(table).await;
        rows.into_iter()
            .find(|r| r[column] == json!(value))
            .unwrap_or_else(|| panic!("no row of `{table}` with {column} = {value}"))
    }

    /// Every field of a table, as `listFields` reports them.
    async fn fields(&mut self, table: &str) -> Vec<Value> {
        let (status, body) = self
            .send("GET", &format!("/api/tables/{table}/fields"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "listing fields: {body}");
        body.as_array().cloned().unwrap_or_default()
    }

    /// Whether the catalog has this table at all.
    async fn table_exists(&mut self, name: &str) -> bool {
        let (_, body) = self.send("GET", "/api/tables", None).await;
        body.as_array()
            .map(|tables| tables.iter().any(|t| t["name"] == json!(name)))
            .unwrap_or(false)
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
    // A field's label is an overlay row (§3.2), and a header matching a field by
    // its *label* is half of what this suite is about.
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

/// Saltcorn 1's fixture: `books`, with a required author and a page count.
///
/// The key is declared like any other field, because nothing invents one
/// (GOALS): an `int` field with `primary_key` numbers itself, which is what the
/// inserts here rely on.
async fn books(client: &mut Client) {
    client.create_table("books").await;
    client
        .create_field(
            "books",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        )
        .await;
    client
        .create_field(
            "books",
            json!({ "name": "author", "type": "text", "required": true, "label": "Author" }),
        )
        .await;
    client
        .create_field(
            "books",
            json!({ "name": "pages", "type": "int", "label": "Pages" }),
        )
        .await;
}

// --- importing into a table that exists ---------------------------------------

#[tokio::test]
async fn a_header_matches_a_field_by_its_label_and_the_cells_are_trimmed() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    books(&mut client).await;

    // `Pages` is the field's *label*, not its name, and the cells carry the
    // space a person types after a comma. Both are what a spreadsheet produces.
    let (status, body) = client
        .import("books", "author,Pages\nJoe Celko, 856\nGordon Kane, 217")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(2));
    assert_eq!(body["updated"], json!(0));
    assert_eq!(body["errors"], json!([]));

    let row = client.row_where("books", "author", "Gordon Kane").await;
    assert_eq!(row["pages"], json!(217));
    Ok(())
}

#[tokio::test]
async fn columns_the_table_does_not_have_are_ignored() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    books(&mut client).await;

    let (status, body) = client
        .import(
            "books",
            "author,Pages,Pages1,citations\n\
             William H Press, 852,7,100\n\
             Peter Rossi, 212,9,200",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(2));

    let row = client.row_where("books", "author", "Peter Rossi").await;
    assert_eq!(row["pages"], json!(212));
    Ok(())
}

#[tokio::test]
async fn a_file_naming_primary_keys_replaces_the_rows_that_have_them() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    books(&mut client).await;
    let melville = client
        .insert(
            "books",
            json!({ "author": "Herman Melville", "pages": 200 }),
        )
        .await;
    let id = melville["id"].as_i64().expect("the row has a key");

    // One key that is there and one that is not: the first row is replaced, the
    // second is inserted with the key the file chose.
    let (status, body) = client
        .import(
            "books",
            &format!("id,author,Pages\n{id}, Noam Chomsky, 540\n17, David Harvey, 612"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(1));
    assert_eq!(body["updated"], json!(1));
    assert_eq!(body["errors"], json!([]));

    let rows = client.rows("books").await;
    assert_eq!(rows.len(), 2, "the replaced row was not duplicated");
    let replaced = rows
        .iter()
        .find(|r| r["id"] == json!(id))
        .expect("the row is still addressable by its key");
    assert_eq!(replaced["author"], json!("Noam Chomsky"));
    assert_eq!(replaced["pages"], json!(540));
    let inserted = rows
        .iter()
        .find(|r| r["id"] == json!(17))
        .expect("the key the file chose was used");
    assert_eq!(inserted["author"], json!("David Harvey"));

    // The same key twice in one file is the file contradicting itself, and the
    // second one is refused rather than silently winning.
    let (_, body) = client
        .import(
            "books",
            &format!("id,author,Pages\n{id}, Noam Chomsky, 541\n{id}, Someone Else, 1"),
        )
        .await;
    assert_eq!(body["updated"], json!(1));
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    assert!(
        errors[0].as_str().unwrap().contains("more than once"),
        "message: {}",
        errors[0]
    );
    Ok(())
}

#[tokio::test]
async fn a_blank_primary_key_cell_is_an_insert() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    books(&mut client).await;
    let melville = client
        .insert(
            "books",
            json!({ "author": "Herman Melville", "pages": 200 }),
        )
        .await;
    let id = melville["id"].as_i64().unwrap();

    let (status, body) = client
        .import(
            "books",
            &format!("id,author,Pages\n{id}, Noam Chomsky, 541\n,Hadas Thier, 250"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(1));
    assert_eq!(body["updated"], json!(1));

    let row = client.row_where("books", "author", "Hadas Thier").await;
    assert_eq!(row["pages"], json!(250));
    assert!(
        row["id"].as_i64().unwrap() > 0,
        "the database assigned a key: {row}"
    );
    Ok(())
}

#[tokio::test]
async fn a_required_field_no_column_supplies_refuses_the_file() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    books(&mut client).await;

    // Nothing here supplies `author`, which is required. That is the wrong file
    // — the check an ignored unknown column no longer makes — and it is refused
    // whole rather than becoming one rejection per row.
    let (status, body) = client.import("books", "Pages,citations\n856,7\n").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.to_string().contains("Author"),
        "the refusal names the field: {body}"
    );
    let (_, count) = client
        .send("GET", "/api/tables/books/rows/count", None)
        .await;
    assert_eq!(count["count"], json!(0), "nothing was written");
    Ok(())
}

#[tokio::test]
async fn a_cell_the_column_cannot_take_is_rejected_by_line() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    books(&mut client).await;

    let (status, body) = client
        .import(
            "books",
            "author,Pages\nLeonardo Boff, 99\nDavid MacKay, ITILA",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(1));
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    let message = errors[0].as_str().unwrap();
    assert!(message.starts_with("line 3:"), "message: {message}");
    assert!(message.contains("pages"), "message: {message}");

    let rows = client.rows("books").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["author"], json!("Leonardo Boff"));
    Ok(())
}

#[tokio::test]
async fn a_date_column_is_read_as_iso_and_anything_else_is_rejected() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("name_dobs").await;
    client
        .create_field(
            "name_dobs",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        )
        .await;
    client
        .create_field(
            "name_dobs",
            json!({ "name": "name", "type": "text", "required": true }),
        )
        .await;
    client
        .create_field("name_dobs", json!({ "name": "dob", "type": "date" }))
        .await;

    // **Divergence from Saltcorn 1**, stated: it reads a date against the
    // application's locale, so `15.03.2011` is a date there when the locale is
    // German. There is no locale setting here, and a format that means two
    // different days in two different countries is not one to guess at — so the
    // ISO date imports and the localised one is refused on its line.
    let (status, body) = client
        .import(
            "name_dobs",
            "Name,DOB\nDavid MacKay, 2012-08-13\nJulius Caesar, 15.03.2011",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(1));
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    assert!(
        errors[0].as_str().unwrap().starts_with("line 3:"),
        "message: {}",
        errors[0]
    );

    let row = client.row_where("name_dobs", "name", "David MacKay").await;
    assert_eq!(row["dob"], json!("2012-08-13"));
    Ok(())
}

/// `books` with two authors in it, and a `book_reviews` table keyed to it by the
/// author name — Saltcorn 1's foreign-key fixture.
async fn book_reviews(client: &mut Client) -> (i64, i64) {
    books(client).await;
    let melville = client
        .insert(
            "books",
            json!({ "author": "Herman Melville", "pages": 200 }),
        )
        .await;
    let tolstoy = client
        .insert("books", json!({ "author": "Leo Tolstoy", "pages": 1225 }))
        .await;

    client.create_table("book_reviews").await;
    client
        .create_field(
            "book_reviews",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        )
        .await;
    client
        .create_field(
            "book_reviews",
            json!({ "name": "review", "type": "text", "required": true }),
        )
        .await;
    client
        .create_field(
            "book_reviews",
            json!({
                "name": "author",
                "kind": {
                    "type": "key",
                    "target_table": "books",
                    "target_field": "id",
                    "summary_field": "author",
                },
            }),
        )
        .await;
    (
        melville["id"].as_i64().unwrap(),
        tolstoy["id"].as_i64().unwrap(),
    )
}

#[tokio::test]
async fn a_key_column_takes_the_key_itself() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let (melville, tolstoy) = book_reviews(&mut client).await;

    let (status, body) = client
        .import(
            "book_reviews",
            &format!("author,review\n{melville}, Awesome\n{tolstoy}, Stunning"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(2));

    let row = client.row_where("book_reviews", "review", "Awesome").await;
    assert_eq!(row["author"], json!(melville));
    Ok(())
}

#[tokio::test]
async fn a_key_column_takes_a_summary_value_and_looks_it_up() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let (melville, tolstoy) = book_reviews(&mut client).await;

    // The value a person has: the author's *name*. Also a quoted cell with a
    // newline in it, and a trailing comma — both of which a spreadsheet writes.
    let (status, body) = client
        .import(
            "book_reviews",
            "author,review\nLeo Tolstoy,\"Funny\nas hell\",\nHerman Melville, Whaley",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(2), "errors: {}", body["errors"]);

    let row = client
        .row_where("book_reviews", "review", "Funny\nas hell")
        .await;
    assert_eq!(row["author"], json!(tolstoy));
    let row = client.row_where("book_reviews", "review", "Whaley").await;
    assert_eq!(row["author"], json!(melville));
    Ok(())
}

#[tokio::test]
async fn a_summary_value_matching_nothing_is_the_rows_error() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    book_reviews(&mut client).await;

    let (status, body) = client
        .import("book_reviews", "author,review\n    China Mieville, Scar")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(0));
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    let message = errors[0].as_str().unwrap();
    assert!(message.starts_with("line 2:"), "message: {message}");
    assert!(message.contains("China Mieville"), "message: {message}");
    assert!(message.contains("books"), "message: {message}");
    Ok(())
}

#[tokio::test]
async fn a_self_join_key_may_point_forward_in_the_file() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("project").await;
    client
        .create_field(
            "project",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        )
        .await;
    client
        .create_field(
            "project",
            json!({ "name": "name", "type": "text", "required": true }),
        )
        .await;
    client
        .create_field(
            "project",
            json!({
                "name": "parent",
                "kind": {
                    "type": "key",
                    "target_table": "project",
                    "target_field": "id",
                    "summary_field": "name",
                },
            }),
        )
        .await;

    // Saltcorn 1's own self-join case, and the reason the import holds one
    // transaction with `SET CONSTRAINTS ALL DEFERRED`: line 2 points at a row
    // line 3 has not created yet. Row by row against an immediate constraint
    // this is impossible; deferred to the commit, by which time both rows are
    // there, it is ordinary.
    let (status, body) = client
        .import("project", "id,name,parent\n1,Biology, 2\n2,Homework,")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(2), "errors: {}", body["errors"]);

    let row = client.row_where("project", "name", "Biology").await;
    assert_eq!(row["parent"], json!(2));

    // A **summary** value is a different matter, and the difference is worth
    // pinning: resolving `Chores` to a key is a read, and a read cannot see a
    // row that does not exist yet, deferred constraints or not. What it *can*
    // see — because it runs on the import's own transaction — is a row an
    // earlier line of the same file inserted and nothing has committed.
    let (status, body) = client
        .import("project", "id,name,parent\n3,Chores,\n4,Reading, Chores")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(2), "errors: {}", body["errors"]);
    let row = client.row_where("project", "name", "Reading").await;
    assert_eq!(row["parent"], json!(3));

    // The other way round, the summary is refused on its own line — the lookup
    // happens there, so that is where it can be reported.
    let (_, body) = client
        .import("project", "id,name,parent\n7,Shopping, Cooking\n8,Cooking,")
        .await;
    assert_eq!(body["inserted"], json!(1));
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    assert!(
        errors[0].as_str().unwrap().starts_with("line 2:"),
        "message: {}",
        errors[0]
    );

    // What deferral cannot rescue: a key that matches nothing **anywhere** in
    // the file. Postgres raises that at the commit, when the line that caused it
    // is long past, so the import fails whole and says why rather than pinning
    // it on a line it cannot know.
    let (status, body) = client
        .import("project", "id,name,parent\n5,Errands, 99\n6,Laundry,")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("rolled back"),
        "the refusal says the import was undone: {body}"
    );
    // And undone means undone: neither row of that file is there.
    let rows = client.rows("project").await;
    assert_eq!(rows.len(), 5, "rows: {rows:?}");
    Ok(())
}

#[tokio::test]
async fn the_identity_sequence_moves_past_the_keys_the_file_chose() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    books(&mut client).await;

    // Without this, the next ordinary insert would try to reuse key 1 and
    // collide with a row the import placed — which looks like a bug in the row
    // editor rather than in the import that caused it.
    let (status, body) = client
        .import(
            "books",
            "id,author,Pages\n1,Herman Melville,200\n2,Leo Tolstoy,1225",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["inserted"], json!(2));

    let row = client
        .insert("books", json!({ "author": "Hadas Thier" }))
        .await;
    assert_eq!(row["id"], json!(3));
    Ok(())
}

// --- creating a table from a CSV ----------------------------------------------

#[tokio::test]
async fn a_table_is_created_from_a_csv_with_its_types_deduced() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    let (status, body) = client
        .create_from_csv(
            "invoice",
            "item,cost,count, vatable\nBook, 5,4, f\nPencil, 0.5,2, t",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["table"]["name"], json!("invoice"));
    assert_eq!(body["inserted"], json!(2));

    let fields = client.fields("invoice").await;
    let typed = |name: &str| -> String {
        fields
            .iter()
            .find(|f| f["name"] == json!(name))
            .unwrap_or_else(|| panic!("no field `{name}`"))["type"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(typed("item"), "text");
    assert_eq!(typed("cost"), "float");
    assert_eq!(typed("count"), "int");
    assert_eq!(typed("vatable"), "bool");
    // Four columns in the file, four fields — and **no primary key**, because
    // the file declared none and nothing invents one (GOALS). The field list
    // says so in red until the admin adds one.
    assert_eq!(fields.len(), 4, "fields: {fields:?}");
    assert!(
        fields.iter().all(|f| f["primary_key"] == json!(false)),
        "fields: {fields:?}"
    );

    let row = client.row_where("invoice", "item", "Pencil").await;
    assert_eq!(row["vatable"], json!(true));
    assert_eq!(row["cost"], json!(0.5));
    assert_eq!(client.rows("invoice").await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_header_that_cannot_be_a_column_name_creates_nothing() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    let (status, body) = client
        .create_from_csv(
            "invoice1",
            "item,cost,!, vatable\nBook, 5,4, f\nPencil, 0.5,2, t",
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("column name"),
        "the refusal says what is wrong: {body}"
    );
    assert!(!client.table_exists("invoice1").await);
    Ok(())
}

#[tokio::test]
async fn a_duplicated_header_is_one_column() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    let (status, body) = client
        .create_from_csv(
            "invoice1",
            "item,cost,cost, vatable\nBook, 5,4, f\nPencil, 0.5,2, t",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // item, cost and vatable: the second `cost` is dropped rather than refused,
    // and the first one is the answer.
    let fields = client.fields("invoice1").await;
    assert_eq!(fields.len(), 3, "fields: {fields:?}");
    let row = client.row_where("invoice1", "item", "Book").await;
    assert_eq!(row["cost"], json!(5.0));
    Ok(())
}

#[tokio::test]
async fn an_id_column_becomes_the_primary_key() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // Capitalised on purpose: catching the sequence up with the keys the file
    // chose asks Postgres for the sequence *by name*, and an unquoted `Invoice3`
    // there would be folded to `invoice3` and found to be no table at all.
    let (status, body) = client
        .create_from_csv("Invoice3", "id,cost,count, vatable\n1, 5,4, f\n2, 0.5,2, t")
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let fields = client.fields("Invoice3").await;
    assert_eq!(fields.len(), 4, "fields: {fields:?}");
    let id = fields
        .iter()
        .find(|f| f["name"] == json!("id"))
        .expect("the key is there");
    assert_eq!(id["primary_key"], json!(true));

    let rows = client.rows("Invoice3").await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r["id"] == json!(1)));

    // And the sequence is past the keys the file chose, so the next ordinary
    // insert does not collide with them.
    let row = client
        .insert(
            "Invoice3",
            json!({ "cost": 0.2, "count": 1, "vatable": true }),
        )
        .await;
    assert_eq!(row["id"], json!(3));
    assert_eq!(client.rows("Invoice3").await.len(), 3);
    Ok(())
}

#[tokio::test]
async fn an_id_column_may_not_have_gaps_whatever_its_type() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // A gap in the key column. No type rescues this: a primary key cannot be
    // null, so the file is refused and no table is left behind.
    let (status, body) = client
        .create_from_csv("invoice4", "id,cost, vatable\n1, 5, f\n, 0.5, t")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("every row"), "{body}");
    assert!(!client.table_exists("invoice4").await);
    Ok(())
}

#[tokio::test]
async fn a_text_or_uuid_id_column_becomes_a_key_of_that_type() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // Saltcorn 1's `String` and `UUID` primary keys, and the same answer here:
    // the key is a field like any other, so it is of whatever type its values
    // are. Nothing about a key says "integer".
    let (status, body) = client
        .create_from_csv("invoice5", "id,cost, vatable\nBook, 5, f\nPencil, 0.5, t")
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let fields = client.fields("invoice5").await;
    let id = fields.iter().find(|f| f["name"] == json!("id")).unwrap();
    assert_eq!(id["primary_key"], json!(true));
    assert_eq!(id["type"], json!("text"));
    assert_eq!(client.rows("invoice5").await.len(), 2);

    let (status, body) = client
        .create_from_csv(
            "invoice6",
            "id,cost\n179f7e88-ae48-495e-a080-68c471fac2ac, 5\nd1403829-cc1e-49b5-bcdc-488973e640ba, 0.5",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let fields = client.fields("invoice6").await;
    let id = fields.iter().find(|f| f["name"] == json!("id")).unwrap();
    assert_eq!(id["primary_key"], json!(true));
    assert_eq!(id["type"], json!("uuid"));
    let rows = client.rows("invoice6").await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|r| r["id"] == json!("179f7e88-ae48-495e-a080-68c471fac2ac")),
        "the file's own UUIDs are the keys: {rows:?}"
    );

    // And a UUID key fills itself in for a row that does not bring one, which is
    // what makes the table usable from a form afterwards.
    let row = client.insert("invoice6", json!({ "cost": 1.5 })).await;
    assert!(
        row["id"].as_str().is_some_and(|id| id.len() == 36),
        "a generated UUID key: {row}"
    );
    Ok(())
}

#[tokio::test]
async fn a_repeated_id_creates_nothing() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // The rows are refused, and because the table was made for *this* file, the
    // table goes with them rather than being left half-filled.
    let (status, body) = client
        .create_from_csv("invoice7", "id,cost,count, vatable\n1, 5,4, f\n1, 0.5,2, t")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("more than once"),
        "the refusal says which row: {body}"
    );
    assert!(!client.table_exists("invoice7").await);
    Ok(())
}

#[tokio::test]
async fn a_column_with_a_gap_is_nullable() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    let (status, body) = client
        .create_from_csv(
            "invoice_missing",
            "item,cost,count, vatable\nBook, 5,4, f\nPencil, 0.5,, t",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let fields = client.fields("invoice_missing").await;
    let count = fields
        .iter()
        .find(|f| f["name"] == json!("count"))
        .expect("the column is a field");
    // Still an integer — the gap says nothing about the type, only about
    // whether the column may be empty.
    assert_eq!(count["type"], json!("int"));
    assert_eq!(count["required"], json!(false));
    let item = fields
        .iter()
        .find(|f| f["name"] == json!("item"))
        .expect("the column is a field");
    assert_eq!(item["required"], json!(true), "no cell of `item` is empty");

    let row = client.row_where("invoice_missing", "item", "Pencil").await;
    assert_eq!(row["count"], Value::Null);
    let row = client.row_where("invoice_missing", "item", "Book").await;
    assert_eq!(row["count"], json!(4));
    Ok(())
}

#[tokio::test]
async fn a_header_with_a_space_becomes_a_field_that_keeps_it_as_a_label() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    for (table, header) in [("invoice8", "Item Name"), ("invoice9", "Item_Name")] {
        let csv = format!("{header},cost\nBook, 5\nPencil, 0.5");
        let (status, body) = client.create_from_csv(table, &csv).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let fields = client.fields(table).await;
        let field = fields
            .iter()
            .find(|f| f["name"] == json!("item_name"))
            .unwrap_or_else(|| panic!("`{header}` became `item_name`: {fields:?}"));
        assert_eq!(field["type"], json!("text"));
        assert_eq!(field["label"], json!("Item Name"));
        assert_eq!(client.rows(table).await.len(), 2);
    }
    Ok(())
}

#[tokio::test]
async fn a_large_integer_is_an_integer_column() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // **Divergence from Saltcorn 1**, stated: it made a column holding
    // 4084787842 a *text* column, because its `Integer` was a 32-bit one and the
    // value does not fit. `int` here is 64-bit, so this is simply an integer.
    let (status, body) = client
        .create_from_csv("invoice10", "ref,cost\n1, 5\n4084787842, 0.5")
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let fields = client.fields("invoice10").await;
    let field = fields.iter().find(|f| f["name"] == json!("ref")).unwrap();
    assert_eq!(field["type"], json!("int"));
    let rows = client.rows("invoice10").await;
    assert!(rows.iter().any(|r| r["ref"] == json!(4084787842_i64)));
    Ok(())
}

#[tokio::test]
async fn a_json_column_is_deduced_and_parsed() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // No space before the opening quote: a quote is only a quote at the start of
    // a field (RFC 4180), so ` "{…}"` is the literal text of a quoted string and
    // not JSON. Saltcorn 1's parser is more forgiving there; this one is not,
    // and a JSON column is spelled the way a CSV writer writes one.
    let csv = "id,cost,attrs\n1, 5,\"{\"\"foo\"\":5}\"\n3, 0.5,\"[7]\"";
    let (status, body) = client.create_from_csv("invoice11", csv).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let fields = client.fields("invoice11").await;
    let field = fields.iter().find(|f| f["name"] == json!("attrs")).unwrap();
    assert_eq!(field["type"], json!("json"));

    let rows = client.rows("invoice11").await;
    assert_eq!(rows.len(), 2);
    let first = rows.iter().find(|r| r["id"] == json!(1)).unwrap();
    // Stored as JSON, not as the text of it.
    assert_eq!(first["attrs"]["foo"], json!(5));
    let second = rows.iter().find(|r| r["id"] == json!(3)).unwrap();
    assert_eq!(second["attrs"], json!([7]));
    Ok(())
}

//! Phase 3.3 integration test: creating and editing fields through the admin API,
//! against a real Postgres database (design §6, §3.3).
//!
//! What is asserted, end to end through the router: `createField` takes a `type`
//! (basic or rich) and derives the column's SQL type from it; a rich field and a
//! `File` field come back from `listFields` with their overlay merged on;
//! `updateField` edits the overlay in place; `listFieldTypes` offers basic types,
//! rich types and the Key/File kinds each with the attribute spec the editor
//! renders; and an unknown type name is refused.
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
    Ok((client, catalog, db))
}

/// The field named `name` from a `listFields` array.
fn field<'a>(fields: &'a Value, name: &str) -> &'a Value {
    fields
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!(name))
        .unwrap_or_else(|| panic!("field `{name}` not listed"))
}

#[tokio::test]
async fn create_read_and_edit_a_rich_field_and_a_file_field() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    // A rich `String` field: `type` is the rich type's name, and its attributes
    // ride along. The SQL type is derived (text), never sent.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({
                "name": "title",
                "type": "string",
                "attributes": { "max_length": 200, "regex": "^[A-Za-z ]+$" }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["type"], json!("string"));
    assert_eq!(
        body["sql_type"],
        json!("text"),
        "derived from the rich type"
    );
    assert_eq!(body["attributes"]["max_length"], json!(200));

    // A `File` field: a plain text column plus a File kind pointing at a store,
    // folder and MIME allow-list.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({
                "name": "cover",
                "type": "text",
                "kind": { "type": "file", "store": "uploads", "folder": "covers",
                          "mime_allow": ["image/png"] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"]["type"], json!("file"));
    assert_eq!(body["kind"]["store"], json!("uploads"));

    // Both read back merged through `listFields`.
    let (_, fields) = client.send("GET", "/api/tables/book/fields", None).await;
    assert_eq!(field(&fields, "title")["type"], json!("string"));
    assert_eq!(
        field(&fields, "title")["attributes"]["max_length"],
        json!(200)
    );
    assert_eq!(field(&fields, "cover")["kind"]["type"], json!("file"));
    assert_eq!(field(&fields, "cover")["kind"]["folder"], json!("covers"));

    // `updateField` edits the overlay in place. The whole overlay is stated (as
    // for a table's settings), so the type is re-sent to keep the field rich.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/title",
            Some(json!({
                "type": "string",
                "label": "Book title",
                "attributes": { "max_length": 100 }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["label"], json!("Book title"));
    assert_eq!(body["attributes"]["max_length"], json!(100));
    assert_eq!(body["type"], json!("string"), "still rich after the edit");

    Ok(())
}

/// The round trip the field editor performs when an admin opens a field, changes
/// one thing and saves: every field is read from `listFields` and written
/// straight back through `updateField`.
///
/// `updateField` is whole-object — what is left out is cleared — so the editor
/// re-sends everything it read, including the type of a field that has no rich
/// type at all. That is the case worth pinning down: `text` names the column's
/// *basic* type, so it must be recorded as "no rich type" rather than as a rich
/// type called `text`, which is not registered and would leave the field's
/// overlay reported as broken while `listFields` went on describing it correctly.
#[tokio::test]
async fn a_field_read_and_written_straight_back_is_unchanged() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    for body in [
        json!({ "name": "title", "type": "text" }),
        json!({ "name": "subtitle", "type": "string", "attributes": { "max_length": 200 } }),
        json!({ "name": "pages", "type": "int" }),
    ] {
        let (status, body) = client
            .send("POST", "/api/tables/book/fields", Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let (_, before) = client.send("GET", "/api/tables/book/fields", None).await;
    for f in before.as_array().unwrap() {
        let name = f["name"].as_str().unwrap();
        // Exactly what the editor sends back: the whole overlay, and nothing that
        // belongs to the column.
        let (status, body) = client
            .send(
                "PUT",
                &format!("/api/tables/book/fields/{name}"),
                Some(json!({
                    "type": f["type"],
                    "label": f["label"],
                    "description": f["description"],
                    "attributes": f["attributes"],
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
    }

    let (_, after) = client.send("GET", "/api/tables/book/fields", None).await;
    assert_eq!(after, before, "a field written back unchanged is unchanged");

    // And no field is left configured as something the catalog cannot merge —
    // the failure `listFields` alone would not show.
    let issues = catalog.field_overlay_issues()?;
    assert!(issues.is_empty(), "{issues:?}");

    Ok(())
}

#[tokio::test]
async fn list_field_types_offers_basic_rich_and_kinds_with_specs() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    let (status, body) = client.send("GET", "/api/field-types", None).await;
    assert_eq!(status, StatusCode::OK);
    let types = body.as_array().unwrap();

    let by_name = |name: &str| types.iter().find(|t| t["name"] == json!(name));

    // A basic type, with no attribute spec.
    let text = by_name("text").expect("text listed");
    assert_eq!(text["category"], json!("basic"));
    assert!(text["config_spec"].as_array().unwrap().is_empty());

    // A rich type, carrying its attribute spec (so the editor can render it).
    let string = by_name("string").expect("string listed");
    assert_eq!(string["category"], json!("rich"));
    let spec_names: Vec<&str> = string["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(spec_names.contains(&"max_length"), "{spec_names:?}");

    // The File kind, with its parameter spec.
    let file = by_name("file").expect("file kind listed");
    assert_eq!(file["category"], json!("kind"));
    let file_params: Vec<&str> = file["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(file_params.contains(&"store"), "{file_params:?}");

    // And the Key kind exists too.
    assert!(by_name("key").is_some(), "key kind listed");
    Ok(())
}

#[tokio::test]
async fn an_unknown_type_is_refused_by_name() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({ "name": "mystery", "type": "wibble" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("wibble"),
        "the error names the unknown type: {body}"
    );

    // And no half-made column was left behind.
    let (_, fields) = client.send("GET", "/api/tables/book/fields", None).await;
    assert!(
        fields
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["name"] != json!("mystery")),
        "no `mystery` column exists"
    );
    Ok(())
}

/// A **calculated field**: created with no column (a virtual overlay field), an
/// `sc-expr` expression under a `calc` kind, computed on read, not writable, and
/// with a broken expression refused up front.
#[tokio::test]
async fn create_and_read_a_calculated_field() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    // A real column the calc field reads.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({ "name": "pages", "type": "int8" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The calc field: no storage type of its own matters — `pages * 2` computed
    // on read. `type` names only how the value is displayed.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({
                "name": "double_pages",
                "type": "int8",
                "kind": { "type": "calc", "expression": "pages * 2" }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"]["type"], json!("calc"));
    assert_eq!(body["kind"]["expression"], json!("pages * 2"));

    // It reads back merged, and is *not* a real column: `information_schema` has
    // `pages` but no `double_pages`.
    let (_, fields) = client.send("GET", "/api/tables/book/fields", None).await;
    assert_eq!(
        field(&fields, "double_pages")["kind"]["type"],
        json!("calc")
    );
    let cols = _db
        .client()
        .await?
        .query(
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'book'",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let names: Vec<String> = cols.iter().map(|r| r.get::<_, String>(0)).collect();
    assert!(
        names.iter().any(|n| n == "pages"),
        "pages is a column: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "double_pages"),
        "the calc field is virtual, not a column: {names:?}"
    );

    // Computed on read: insert a row with pages = 100, and the calc field is 200.
    let (status, row) = client
        .send(
            "POST",
            "/api/tables/book/rows",
            Some(json!({ "pages": 100 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    let (_, rows) = client.send("GET", "/api/tables/book/rows", None).await;
    let first = &rows.as_array().unwrap()[0];
    assert_eq!(
        first["double_pages"],
        json!(200),
        "computed on read: {first}"
    );

    // A calc field is not writable: naming it on a write is refused.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/rows",
            Some(json!({ "pages": 5, "double_pages": 999 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A broken expression is a 400 naming the field, and writes no overlay.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({
                "name": "bad_calc",
                "type": "int8",
                "kind": { "type": "calc", "expression": "nonexistent_col + 1" }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("bad_calc"),
        "the error names the field: {body}"
    );

    // Using `user` in a calc field is refused (no caller in the calc scope).
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({
                "name": "mine",
                "type": "int8",
                "kind": { "type": "calc", "expression": "pages === user.id" }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("user"),
        "the error mentions `user`: {body}"
    );

    // Neither invalid field was created.
    let (_, fields) = client.send("GET", "/api/tables/book/fields", None).await;
    for missing in ["bad_calc", "mine"] {
        assert!(
            fields
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["name"] != json!(missing)),
            "`{missing}` must not exist"
        );
    }

    Ok(())
}

/// A **`Key` field created without a `type`**: the storage type is the target
/// field's, worked out by the server rather than sent.
///
/// This is what lets the admin UI stop asking "stored as" — a question whose
/// only correct answer is the one the target column already gives, and whose
/// wrong answers are columns Postgres refuses to reference. The same request
/// carries the target field and the summary field, so what a reference points at
/// and what it is shown as are chosen together.
#[tokio::test]
async fn a_key_field_takes_its_storage_type_from_its_target() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    // The table to point at: its key, declared like any other field (nothing
    // invents one), and a column to summarise rows by.
    client
        .send("POST", "/api/tables", Some(json!({ "name": "author" })))
        .await;
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/author/fields",
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/author/fields",
            Some(json!({ "name": "full_name", "type": "text" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // No `type` in the request at all.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({
                "name": "written_by",
                "kind": { "type": "key", "target_table": "author",
                          "target_field": "id", "summary_field": "full_name" }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["sql_type"],
        json!("int8"),
        "the type comes from `author.id`, not from the request: {body}"
    );
    assert_eq!(body["kind"]["target_table"], json!("author"));
    assert_eq!(body["kind"]["target_field"], json!("id"));
    assert_eq!(body["kind"]["summary_field"], json!("full_name"));

    // It is a real foreign key in the database, not just an overlay.
    let refs = _db
        .client()
        .await?
        .query(
            "SELECT c.conname FROM pg_constraint c \
             WHERE c.conrelid = 'book'::regclass AND c.contype = 'f'",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert_eq!(refs.len(), 1, "one foreign key on book");

    // A field that is *not* a reference still needs a type, and the refusal says
    // so rather than inventing one.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({ "name": "untyped" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("needs a type"),
        "the error says a type is needed: {body}"
    );

    // A summary field the target does not have is refused by name.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({
                "name": "bad_ref",
                "kind": { "type": "key", "target_table": "author",
                          "target_field": "id", "summary_field": "nickname" }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("nickname"),
        "the error names the missing summary field: {body}"
    );

    Ok(())
}

/// The other direction of a `Key`: which tables point *at* this one.
///
/// The fields screen shows both — a key field names its target, and the table
/// lists what references it — so the second half needs an endpoint, and it is
/// the whole catalog's question rather than one table's. Two things it must get
/// right: every key from a table that has more than one, and no self-joins (a
/// table's key onto itself is already a row in its own field list).
#[tokio::test]
async fn inbound_keys_name_the_tables_that_point_here() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    client
        .send("POST", "/api/tables", Some(json!({ "name": "author" })))
        .await;
    for table in ["author", "book"] {
        let (status, body) = client
            .send(
                "POST",
                &format!("/api/tables/{table}/fields"),
                Some(json!({ "name": "id", "type": "int", "primary_key": true })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    // Two keys from `book` onto `author`, and one from `author` onto itself.
    for (table, name, target) in [
        ("book", "written_by", "author"),
        ("book", "edited_by", "author"),
        ("author", "mentor", "author"),
    ] {
        let (status, body) = client
            .send(
                "POST",
                &format!("/api/tables/{table}/fields"),
                Some(json!({
                    "name": name,
                    "kind": { "type": "key", "target_table": target, "target_field": "id" }
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let (status, body) = client
        .send("GET", "/api/tables/author/inbound-keys", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!([
            { "table": "book", "field": "written_by" },
            { "table": "book", "field": "edited_by" },
        ]),
        "both of `book`'s keys, and never `author.mentor`: {body}"
    );

    // Nothing points at `book`, and that is an empty list rather than an error.
    let (status, body) = client
        .send("GET", "/api/tables/book/inbound-keys", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));

    // A table that is not there is a 404: "who points at nothing" is not a
    // question with an empty answer.
    let (status, body) = client
        .send("GET", "/api/tables/nosuch/inbound-keys", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    Ok(())
}

/// Whether a field accepts nulls is editable after the fact, both ways: made
/// required once its rows all have a value, and optional again. The one refusal
/// that depends on the data names the rows in the way rather than the database's
/// constraint, and a key field can never be made optional.
#[tokio::test]
async fn a_field_can_be_made_required_and_optional_again() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    for body in [
        json!({ "name": "id", "type": "int", "primary_key": true }),
        json!({ "name": "title", "type": "text" }),
        json!({ "name": "pages", "type": "int" }),
    ] {
        let (status, body) = client
            .send("POST", "/api/tables/book/fields", Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = client
        .send("POST", "/api/tables/book/rows", Some(json!({ "pages": 1 })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].clone();

    // A row with no title is in the way, and the message says so.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/title",
            Some(json!({ "type": "text", "required": true })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("1 row has no value"),
        "names the rows: {body}"
    );
    let (_, fields) = client.send("GET", "/api/tables/book/fields", None).await;
    assert_eq!(field(&fields, "title")["nullable"], json!(true));

    // Fill it in, and the same request goes through.
    let (status, body) = client
        .send(
            "PUT",
            &format!("/api/tables/book/rows/{id}"),
            Some(json!({ "title": "Dune" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/title",
            Some(json!({ "type": "text", "required": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nullable"], json!(false));
    assert_eq!(body["required"], json!(true));
    // The database enforces it now.
    let (status, _) = client
        .send("POST", "/api/tables/book/rows", Some(json!({ "pages": 1 })))
        .await;
    assert_ne!(status, StatusCode::CREATED, "a title-less row is refused");

    // An edit that does not mention it leaves it alone.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/title",
            Some(json!({ "type": "text", "label": "Title" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nullable"], json!(false));

    // And back again.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/title",
            Some(json!({ "type": "text", "required": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["nullable"], json!(true));
    let (status, body) = client
        .send("POST", "/api/tables/book/rows", Some(json!({ "pages": 1 })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The key is the exception: a key column never takes a null.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/id",
            Some(json!({ "type": "int", "required": false })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("primary key"), "{body}");
    Ok(())
}

/// The table a `Key` field's foreign key in the database points at, or `None`
/// when the column has none — read off `pg_constraint`, because the field list
/// is what was wrong when only the overlay changed.
async fn fk_target(db: &sc_test_harness::TestDb, table: &str, column: &str) -> Option<String> {
    let rows = db
        .client()
        .await
        .unwrap()
        .query(
            "SELECT c.confrelid::regclass::text FROM pg_constraint c \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[1] \
             WHERE c.conrelid = $1::text::regclass AND c.contype = 'f' AND a.attname = $2",
            &[&table, &column],
        )
        .await
        .unwrap();
    assert!(
        rows.len() <= 1,
        "at most one foreign key on `{table}.{column}`"
    );
    rows.first().map(|r| r.get::<_, String>(0))
}

/// Editing a `Key` onto another table repoints the foreign key itself.
///
/// The merge takes a key's target from the database whenever a foreign key
/// stands behind the column (§3.2), so an edit that wrote only the overlay was
/// saved without complaint and read straight back pointing where it always
/// had. The reference has to move where it lives — and a field that stops being
/// a key has to lose it, and a target stored as something else is refused
/// rather than retyping the column.
#[tokio::test]
async fn a_key_field_can_be_pointed_at_another_table() -> sc_error::Result<()> {
    let (mut client, _catalog, db) = setup().await?;
    for table in ["author", "publisher", "code"] {
        client
            .send("POST", "/api/tables", Some(json!({ "name": table })))
            .await;
    }
    for (table, body) in [
        (
            "author",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        ),
        (
            "publisher",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        ),
        ("publisher", json!({ "name": "name", "type": "text" })),
        (
            "code",
            json!({ "name": "id", "type": "text", "primary_key": true }),
        ),
        (
            "book",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        ),
        (
            "book",
            json!({ "name": "by", "kind": { "type": "key", "target_table": "author",
                                            "target_field": "id" } }),
        ),
    ] {
        let (status, body) = client
            .send("POST", &format!("/api/tables/{table}/fields"), Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    assert_eq!(
        fk_target(&db, "book", "by").await.as_deref(),
        Some("author")
    );

    // What the field editor sends on save: the whole overlay, no `type`.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/by",
            Some(json!({
                "label": "", "description": "", "primary_key": false,
                "kind": { "type": "key", "target_table": "publisher",
                          "target_field": "id", "summary_field": "name" }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kind"]["target_table"], json!("publisher"), "{body}");
    assert_eq!(body["kind"]["summary_field"], json!("name"), "{body}");
    let (_, fields) = client.send("GET", "/api/tables/book/fields", None).await;
    assert_eq!(
        field(&fields, "by")["kind"]["target_table"],
        json!("publisher")
    );
    assert_eq!(
        fk_target(&db, "book", "by").await.as_deref(),
        Some("publisher")
    );

    // A row the new target does not have makes the move fail, and the key
    // stays where it was — the overlay with it.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/publisher/rows",
            Some(json!({ "name": "Ace" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/rows",
            Some(json!({ "by": body["id"] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/by",
            Some(json!({ "kind": { "type": "key", "target_table": "author",
                                   "target_field": "id" } })),
        )
        .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fk_target(&db, "book", "by").await.as_deref(),
        Some("publisher")
    );
    let (_, fields) = client.send("GET", "/api/tables/book/fields", None).await;
    assert_eq!(
        field(&fields, "by")["kind"]["target_table"],
        json!("publisher")
    );

    // A target stored as something else is refused by name.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/by",
            Some(json!({ "kind": { "type": "key", "target_table": "code",
                                   "target_field": "id" } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("stored as"), "{body}");
    assert_eq!(
        fk_target(&db, "book", "by").await.as_deref(),
        Some("publisher")
    );

    // No longer a key: the foreign key goes, and the field reads back plain.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/by",
            Some(json!({ "type": "int" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fk_target(&db, "book", "by").await, None);
    let (_, fields) = client.send("GET", "/api/tables/book/fields", None).await;
    assert_ne!(
        field(&fields, "by")["kind"]["type"],
        json!("key"),
        "{fields}"
    );

    // And a plain column made a key gains one.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/by",
            Some(
                json!({ "kind": { "type": "key", "target_table": "publisher",
                                   "target_field": "id" } }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        fk_target(&db, "book", "by").await.as_deref(),
        Some("publisher")
    );
    Ok(())
}

/// `name` on a field edit renames the column; omitted, it keeps its name.
#[tokio::test]
async fn a_field_is_renamed_with_its_data() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    for body in [
        json!({ "name": "id", "type": "int", "primary_key": true }),
        json!({ "name": "title", "type": "text", "label": "Title" }),
    ] {
        let (status, body) = client
            .send("POST", "/api/tables/book/fields", Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/rows",
            Some(json!({ "title": "Dune" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].clone();

    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/book/fields/title",
            Some(json!({ "name": "heading", "type": "text", "label": "Heading" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], json!("heading"));
    let (_, fields) = client.send("GET", "/api/tables/book/fields", None).await;
    assert_eq!(field(&fields, "heading")["label"], json!("Heading"));
    assert!(
        fields
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["name"] != "title")
    );
    let (_, rows) = client.send("GET", "/api/tables/book/rows", None).await;
    assert_eq!(rows, json!([{ "id": id, "heading": "Dune" }]), "{rows}");
    Ok(())
}

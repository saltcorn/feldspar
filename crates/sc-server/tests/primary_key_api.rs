//! The primary key as **a field like any other** (GOALS, design §3.3), over
//! HTTP.
//!
//! The goals are explicit: "do not create primary key fields when table is
//! created. User must create primary key fields like other fields." So a created
//! table has no key at all, and everything that follows from that is what this
//! file pins:
//!
//!   - a table is created with no columns and no key, and rows can still be
//!     inserted into it and read back — it is a usable table, just not an
//!     addressable one;
//!   - `primary_key` on a field is what gives a table its key, whether the field
//!     is added at creation or years later to a table that never had one;
//!   - an `int` key numbers itself and a `uuid` key generates itself, because a
//!     key nobody can supply a value for is a table no form can insert into;
//!   - two fields with `primary_key` make a **composite** key, which the goals
//!     require the whole system to work with;
//!   - a key can be taken off a field again, leaving the table with none;
//!   - without a key, the row endpoints that address one row refuse — that is
//!     the cost the admin UI's red banner is warning about.
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

    async fn create_table(&mut self, name: &str) {
        let (status, body) = self
            .send("POST", "/api/tables", Some(json!({ "name": name })))
            .await;
        assert_eq!(status, StatusCode::CREATED, "creating `{name}`: {body}");
    }

    async fn create_field(&mut self, table: &str, field: Value) -> (StatusCode, Value) {
        self.send("POST", &format!("/api/tables/{table}/fields"), Some(field))
            .await
    }

    async fn update_field(&mut self, table: &str, field: &str, body: Value) -> (StatusCode, Value) {
        self.send(
            "PUT",
            &format!("/api/tables/{table}/fields/{field}"),
            Some(body),
        )
        .await
    }

    async fn fields(&mut self, table: &str) -> Vec<Value> {
        let (status, body) = self
            .send("GET", &format!("/api/tables/{table}/fields"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "listing fields: {body}");
        body.as_array().cloned().unwrap_or_default()
    }

    /// The names of the fields that say they are part of the key, in order.
    async fn key_of(&mut self, table: &str) -> Vec<String> {
        self.fields(table)
            .await
            .iter()
            .filter(|f| f["primary_key"] == json!(true))
            .filter_map(|f| f["name"].as_str().map(str::to_owned))
            .collect()
    }

    async fn insert(&mut self, table: &str, row: Value) -> (StatusCode, Value) {
        self.send("POST", &format!("/api/tables/{table}/rows"), Some(row))
            .await
    }

    async fn rows(&mut self, table: &str) -> Vec<Value> {
        let (_, body) = self
            .send("GET", &format!("/api/tables/{table}/rows"), None)
            .await;
        body.as_array().cloned().unwrap_or_default()
    }
}

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

#[tokio::test]
async fn a_created_table_has_no_columns_and_no_key() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("note").await;

    assert!(client.fields("note").await.is_empty(), "no invented `id`");
    assert!(client.key_of("note").await.is_empty());

    // And it is still a table: a row goes in and comes back. What it is not is
    // *addressable* — see the test below.
    client
        .create_field("note", json!({ "name": "body", "type": "text" }))
        .await;
    let (status, body) = client.insert("note", json!({ "body": "hello" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(client.rows("note").await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_field_that_says_it_is_the_key_gives_the_table_one() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("invoice").await;

    let (status, body) = client
        .create_field(
            "invoice",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["primary_key"], json!(true));
    // A key column is NOT NULL whether or not the caller said `required`.
    assert_eq!(body["nullable"], json!(false));
    assert_eq!(client.key_of("invoice").await, vec!["id".to_owned()]);

    client
        .create_field("invoice", json!({ "name": "amount", "type": "int" }))
        .await;

    // An `int` key numbers itself: the row names no key and gets one.
    let (status, first) = client.insert("invoice", json!({ "amount": 10 })).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["id"], json!(1));
    let (_, second) = client.insert("invoice", json!({ "amount": 20 })).await;
    assert_eq!(second["id"], json!(2));

    // And the row is addressable, which is the whole point of having a key.
    let (status, updated) = client
        .send(
            "PUT",
            "/api/tables/invoice/rows/1",
            Some(json!({ "amount": 11 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["amount"], json!(11));
    Ok(())
}

#[tokio::test]
async fn a_uuid_key_generates_itself() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("document").await;
    client
        .create_field(
            "document",
            json!({ "name": "id", "type": "uuid", "primary_key": true }),
        )
        .await;
    client
        .create_field("document", json!({ "name": "title", "type": "text" }))
        .await;

    let (status, row) = client
        .insert("document", json!({ "title": "Minutes" }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    let generated = row["id"].as_str().expect("a generated key");
    assert_eq!(generated.len(), 36, "a UUID: {generated}");

    // A key the writer *does* supply is used as given — the generator is a
    // default, not a rule.
    let mine = "179f7e88-ae48-495e-a080-68c471fac2ac";
    let (status, row) = client
        .insert("document", json!({ "id": mine, "title": "Agenda" }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["id"], json!(mine));
    Ok(())
}

#[tokio::test]
async fn a_key_switched_on_afterwards_numbers_itself_from_where_the_rows_left_off()
-> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("ticket").await;
    // An ordinary `int` column, not a key: this is the table an admin realises
    // later needs one, and the column they reach for when they do.
    client
        .create_field("ticket", json!({ "name": "ref", "type": "int" }))
        .await;
    client
        .create_field("ticket", json!({ "name": "subject", "type": "text" }))
        .await;
    for (ref_, subject) in [(4, "Lift stuck"), (7, "Door jammed")] {
        let (status, body) = client
            .insert("ticket", json!({ "ref": ref_, "subject": subject }))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    // Nothing fills it in yet, and the field says so.
    let before = client.fields("ticket").await;
    let before = before.iter().find(|f| f["name"] == "ref").expect("ref");
    assert_eq!(before["generated"], json!(false));

    let (status, body) = client
        .update_field("ticket", "ref", json!({ "primary_key": true }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(client.key_of("ticket").await, vec!["ref".to_owned()]);

    // Ticking the box is what gives the column its identity — a key switched on
    // afterwards has to fill itself in exactly as one declared at creation does,
    // or the same checkbox would mean two different things depending on when it
    // was ticked.
    let after = client.fields("ticket").await;
    let after = after.iter().find(|f| f["name"] == "ref").expect("ref");
    assert_eq!(after["generated"], json!(true), "{after}");

    // And it numbers itself past the rows that were already there. A sequence
    // starting at 1 would collide with the `ref` of 4 sitting in the table.
    let (status, row) = client
        .insert("ticket", json!({ "subject": "Alarm sounding" }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["ref"], json!(8), "past the largest key there is");
    let (_, row) = client.insert("ticket", json!({ "subject": "Leak" })).await;
    assert_eq!(row["ref"], json!(9));

    // Every row is still there, and one that predates the key is addressable by
    // the value it already had.
    let rows = client.rows("ticket").await;
    assert_eq!(rows.len(), 4);
    let (status, row) = client
        .send(
            "PUT",
            "/api/tables/ticket/rows/7",
            Some(json!({ "subject": "Door jammed shut" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["subject"], json!("Door jammed shut"));
    Ok(())
}

#[tokio::test]
async fn a_uuid_key_switched_on_afterwards_generates_itself() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("note").await;
    client
        .create_field("note", json!({ "name": "id", "type": "uuid" }))
        .await;
    client
        .create_field("note", json!({ "name": "body", "type": "text" }))
        .await;

    let (status, body) = client
        .update_field("note", "id", json!({ "primary_key": true }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["generated"], json!(true), "{body}");

    let (status, row) = client.insert("note", json!({ "body": "Ideas" })).await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    assert_eq!(row["id"].as_str().expect("a generated key").len(), 36);
    Ok(())
}

#[tokio::test]
async fn a_key_of_a_type_that_cannot_generate_one_is_left_to_the_writer() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("country").await;
    client
        .create_field(
            "country",
            json!({ "name": "code", "type": "text", "primary_key": true }),
        )
        .await;

    // There is no value to invent for a `text` key, so nothing is invented and
    // the field says so rather than promising a generator it does not have.
    let fields = client.fields("country").await;
    let code = fields.iter().find(|f| f["name"] == "code").expect("code");
    assert_eq!(code["primary_key"], json!(true));
    assert_eq!(code["generated"], json!(false), "{code}");

    // A write that omits it is refused by the NOT NULL every key column carries,
    // which is the honest outcome: the writer supplies this key.
    let (status, _) = client.insert("country", json!({})).await;
    assert_ne!(status, StatusCode::CREATED);
    let (status, row) = client.insert("country", json!({ "code": "SE" })).await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    Ok(())
}

#[tokio::test]
async fn a_table_that_never_had_a_key_can_be_given_one() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("reading").await;
    client
        .create_field("reading", json!({ "name": "code", "type": "text" }))
        .await;
    client
        .create_field("reading", json!({ "name": "value", "type": "int" }))
        .await;
    client
        .insert("reading", json!({ "code": "a", "value": 1 }))
        .await;

    // A row is there already, and the column it will key on is unique across the
    // rows — which is the case this has to work in, because a table gets its key
    // late exactly when somebody notices it needs one.
    assert!(client.key_of("reading").await.is_empty());
    let (status, body) = client
        .update_field("reading", "code", json!({ "primary_key": true }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(client.key_of("reading").await, vec!["code".to_owned()]);

    // It is a real key: the duplicate is refused by the database.
    let (status, _) = client
        .insert("reading", json!({ "code": "a", "value": 2 }))
        .await;
    assert_ne!(status, StatusCode::CREATED, "the key constrains rows");

    // And the row is addressable by it now.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/reading/rows/a",
            Some(json!({ "value": 5 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["value"], json!(5));

    // Taking it off again leaves the table with no key — the state it was
    // created in, and one it must be able to return to.
    let (status, body) = client
        .update_field("reading", "code", json!({ "primary_key": false }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(client.key_of("reading").await.is_empty());
    let (status, _) = client
        .insert("reading", json!({ "code": "a", "value": 9 }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "the constraint went with it");
    Ok(())
}

#[tokio::test]
async fn two_fields_make_a_composite_key() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("member").await;
    for name in ["org", "person"] {
        let (status, body) = client
            .create_field(
                "member",
                json!({ "name": name, "type": "text", "primary_key": true }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "adding `{name}`: {body}");
    }
    client
        .create_field("member", json!({ "name": "joined", "type": "date" }))
        .await;

    // Both columns, in the order they were declared — the goals require the
    // system to work with composite keys, so this is a key, not two fields that
    // happen to be marked.
    assert_eq!(
        client.key_of("member").await,
        vec!["org".to_owned(), "person".to_owned()]
    );

    let (status, body) = client
        .insert("member", json!({ "org": "acme", "person": "ada" }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // The pair is what must be unique: the same org with another person is fine,
    // the same pair twice is not.
    let (status, _) = client
        .insert("member", json!({ "org": "acme", "person": "bob" }))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = client
        .insert("member", json!({ "org": "acme", "person": "ada" }))
        .await;
    assert_ne!(status, StatusCode::CREATED, "the pair is the key");
    Ok(())
}

#[tokio::test]
async fn without_a_key_a_single_row_cannot_be_addressed() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("log").await;
    client
        .create_field("log", json!({ "name": "line", "type": "text" }))
        .await;
    client.insert("log", json!({ "line": "started" })).await;

    // This is what the red banner on the field list is warning about: the rows
    // are there and readable, but "change *that* row" has no way to say which.
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/log/rows/1",
            Some(json!({ "line": "stopped" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("primary key"),
        "the refusal says what is missing: {body}"
    );

    let (status, _) = client.send("DELETE", "/api/tables/log/rows/1", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Nor can another table reference it as a table: a `Key` names the column it
    // points at, and a table with no key has none to name. (The field editor
    // fills that dropdown from the target's fields, so the same refusal is what
    // an admin would meet.)
    client.create_table("log_note").await;
    let (status, body) = client
        .create_field(
            "log_note",
            json!({
                "name": "log",
                "kind": { "type": "key", "target_table": "log", "target_field": "" },
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("target_field"), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_calculated_field_cannot_be_the_key() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    client.create_table("shape").await;
    client
        .create_field("shape", json!({ "name": "side", "type": "int" }))
        .await;

    let (status, body) = client
        .create_field(
            "shape",
            json!({
                "name": "area",
                "type": "int",
                "primary_key": true,
                "kind": { "type": "calc", "expression": "side * side" },
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("calculated"),
        "the refusal says why: {body}"
    );
    assert!(client.key_of("shape").await.is_empty());
    Ok(())
}

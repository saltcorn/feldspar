//! Phase 1.3 integration test: configuring a table through the admin API, as the
//! SPA drives it (design §9, §13.1).
//!
//! §1.1 made a table's access rules storable and §1.2 made them merge; this is
//! the first phase in which an *admin* can set them, so what is asserted here is
//! the whole round trip over HTTP: the listing carries the rules, a `PUT` sets
//! them, a second `PUT` edits rather than duplicates, a bad role is refused with
//! nothing written, forgetting returns the table to admin-only, and settings
//! whose table has been dropped are still listed and can be cleaned up.
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

/// A router over a fresh catalog with the platform tables bootstrapped, and an
/// admin logged in. The [`TestDb`] must stay in scope: dropping it deletes the
/// database.
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
    // The overlay table, as the real boot path (`connect_catalog`) does.
    sc_catalog::bootstrap_table_meta(&catalog).await?;

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

/// The role numbers in a `listRoles` response, in order.
fn role_numbers(roles: &Value) -> Vec<i64> {
    roles
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["role"].as_i64().unwrap())
        .collect()
}

/// One named table out of a `listTables` response. The listing also contains
/// `users`, which is an ordinary table like any other.
fn listed_table<'a>(tables: &'a Value, name: &str) -> &'a Value {
    tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!(name))
        .unwrap_or_else(|| panic!("table `{name}` should be listed"))
}

#[tokio::test]
async fn a_table_is_configured_edited_and_forgotten_over_http() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    let (status, table) = client
        .send("POST", "/api/tables", Some(json!({ "name": "books" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // A brand-new table is unconfigured and admin-only. `configured` is what
    // distinguishes it from a table an admin deliberately set to 1/1 — both
    // report the same roles, and only one has settings to forget.
    assert_eq!(table["min_role_read"], json!(1));
    assert_eq!(table["min_role_write"], json!(1));
    assert_eq!(table["configured"], json!(false));
    assert_eq!(table["label"], json!("books"));

    // --- configure it ----------------------------------------------------
    let (status, saved) = client
        .send(
            "PUT",
            "/api/tables/books",
            Some(json!({
                "label": "Books",
                "description": "The library catalogue",
                "min_role_read": 100,
                "min_role_write": 1,
                "ownership_formula": "",
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["min_role_read"], json!(100));
    assert_eq!(saved["min_role_write"], json!(1));
    assert_eq!(saved["label"], json!("Books"));
    assert_eq!(saved["configured"], json!(true));

    // The listing carries it too — that is where an admin scans for "which of
    // these can the public read?".
    let (status, tables) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK);
    let books = listed_table(&tables, "books");
    assert_eq!(books["min_role_read"], json!(100));
    assert_eq!(books["description"], json!("The library catalogue"));

    // The users table sits beside it, unconfigured and admin-only: configuring
    // one table must not touch another.
    let users = listed_table(&tables, "users");
    assert_eq!(users["min_role_read"], json!(1));
    assert_eq!(users["configured"], json!(false));

    // --- edit it ---------------------------------------------------------
    // A second save must update the existing row. Creating a second row for one
    // table is refused by the storage layer, so an editor that did not reuse the
    // row would make every edit after the first fail.
    let (status, edited) = client
        .send(
            "PUT",
            "/api/tables/books",
            Some(json!({
                "label": "Library",
                "description": "",
                "min_role_read": 40,
                "min_role_write": 40,
                "ownership_formula": "",
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["min_role_read"], json!(40));
    assert_eq!(edited["label"], json!("Library"));
    assert_eq!(edited["configured"], json!(true));

    // --- forget it -------------------------------------------------------
    let (status, body) = client
        .send("DELETE", "/api/tables/books/settings", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(true));

    // Back to the closed default — not to the previously saved value — and the
    // table, with its columns, is untouched.
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    let books = listed_table(&tables, "books");
    assert_eq!(books["min_role_read"], json!(1));
    assert_eq!(books["min_role_write"], json!(1));
    assert_eq!(books["configured"], json!(false));
    assert_eq!(books["label"], json!("books"));

    // The table is untouched: forgetting settings is not a schema change. It has
    // the columns it was created with — none, here, since nothing invents a
    // primary key (GOALS) and this test never added a field.
    let (status, fields) = client.send("GET", "/api/tables/books/fields", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fields.as_array().unwrap().len(), 0, "{fields}");

    // Forgetting settings that are not there is not an error; it says so.
    let (status, body) = client
        .send("DELETE", "/api/tables/books/settings", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(false));
    Ok(())
}

#[tokio::test]
async fn a_role_off_the_scale_is_refused_and_nothing_is_written() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    client
        .send("POST", "/api/tables", Some(json!({ "name": "books" })))
        .await;

    for bad in [0, 101, -1] {
        let (status, _) = client
            .send(
                "PUT",
                "/api/tables/books",
                Some(json!({
                    "label": "",
                    "description": "",
                    "min_role_read": bad,
                    "min_role_write": 1,
                    "ownership_formula": "",
                    "rls_enabled": false,
                    "sync_enabled": false,
                })),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "role {bad} should be refused"
        );
    }

    // Nothing was written: the table is still unconfigured, not configured with
    // some clamped role that quietly decided who can read it.
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    assert_eq!(listed_table(&tables, "books")["configured"], json!(false));

    // A table that does not exist is a 404, not a stray overlay row.
    let (status, _) = client
        .send(
            "PUT",
            "/api/tables/nope",
            Some(json!({
                "label": "",
                "description": "",
                "min_role_read": 1,
                "min_role_write": 1,
                "ownership_formula": "",
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, orphans) = client
        .send("GET", "/api/table-settings/orphans", None)
        .await;
    assert!(orphans.as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn settings_outlive_their_table_and_can_be_cleaned_up() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    client
        .send("POST", "/api/tables", Some(json!({ "name": "books" })))
        .await;
    client
        .send(
            "PUT",
            "/api/tables/books",
            Some(json!({
                "label": "Books",
                "description": "",
                "min_role_read": 100,
                "min_role_write": 1,
                "ownership_formula": "",
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;

    let (_, orphans) = client
        .send("GET", "/api/table-settings/orphans", None)
        .await;
    assert!(orphans.as_array().unwrap().is_empty());

    // Drop the table outside Saltcorn — a restore, or a migration run by hand.
    catalog
        .primary()
        .apply_schema(&sc_db::SchemaChange::DropTable {
            name: "books".to_owned(),
            if_exists: false,
        })
        .await?;
    catalog.reload().await?;

    // The settings are kept, and *visible*: keeping a row nobody can see would
    // be indistinguishable from a leak.
    let (status, orphans) = client
        .send("GET", "/api/table-settings/orphans", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let orphans = orphans.as_array().unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0]["name"], json!("books"));
    assert_eq!(orphans[0]["min_role_read"], json!(100));

    // They are still waiting when the table comes back.
    client
        .send("POST", "/api/tables", Some(json!({ "name": "books" })))
        .await;
    let (_, tables) = client.send("GET", "/api/tables", None).await;
    assert_eq!(listed_table(&tables, "books")["min_role_read"], json!(100));
    let (_, orphans) = client
        .send("GET", "/api/table-settings/orphans", None)
        .await;
    assert!(orphans.as_array().unwrap().is_empty());

    // And an orphan can be cleaned up by name, which is the only handle there
    // is when the table it names is gone.
    catalog
        .primary()
        .apply_schema(&sc_db::SchemaChange::DropTable {
            name: "books".to_owned(),
            if_exists: false,
        })
        .await?;
    catalog.reload().await?;
    let (status, body) = client
        .send("DELETE", "/api/tables/books/settings", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(true));
    let (_, orphans) = client
        .send("GET", "/api/table-settings/orphans", None)
        .await;
    assert!(orphans.as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn roles_are_rows_that_can_be_created_and_deleted() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;

    // A fresh installation is bootstrapped with the two roles the system itself
    // depends on, and only those. An invented middle role nobody uses would be
    // one every admin has to read and decide to delete.
    let (status, roles) = client.send("GET", "/api/roles", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(role_numbers(&roles), vec![1, 100]);
    assert_eq!(roles.as_array().unwrap()[0]["name"], json!("Admin"));
    assert_eq!(roles.as_array().unwrap()[0]["builtin"], json!(true));

    // Creating a role adds it to the list — a real row now, not a number
    // inferred from who happens to hold it.
    let (status, _) = client
        .send(
            "POST",
            "/api/roles",
            Some(json!({ "role": 40, "name": "Editor", "description": "Edits things." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, roles) = client.send("GET", "/api/roles", None).await;
    assert_eq!(role_numbers(&roles), vec![1, 40, 100]);
    let editor = roles
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == json!(40))
        .unwrap();
    assert_eq!(editor["name"], json!("Editor"));
    assert_eq!(editor["builtin"], json!(false));

    // A built-in role cannot be deleted; a created one, held by nobody, can.
    let (status, _) = client.send("DELETE", "/api/roles/1", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = client.send("DELETE", "/api/roles/40", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(true));
    let (_, roles) = client.send("GET", "/api/roles", None).await;
    assert_eq!(role_numbers(&roles), vec![1, 100]);
    Ok(())
}

/// The `_fd_changes` triggers on `table`, by name.
async fn change_triggers(db: &TestDb, table: &str) -> Vec<String> {
    db.client()
        .await
        .unwrap()
        .query(
            "SELECT tgname::text FROM pg_trigger \
              WHERE tgrelid = $1::text::regclass AND NOT tgisinternal ORDER BY tgname",
            &[&table],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// TODO-history H1.6: the history switch rides the same `PUT` as sync's —
/// off by default, offered where sync is, and on means the shared change trigger.
#[tokio::test]
async fn row_history_is_switched_on_and_off_with_the_table_settings() -> sc_error::Result<()> {
    let (mut client, catalog, db) = setup().await?;
    // The change log the trigger writes to, as the real boot path makes it.
    sc_catalog::bootstrap_change_log(&catalog).await?;

    let (status, table) = client
        .send("POST", "/api/tables", Some(json!({ "name": "notes" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // No key yet: history, like sync, is not on offer.
    assert_eq!(table["history_enabled"], json!(false));
    assert_eq!(table["history_available"], json!(false));
    assert_eq!(table["sync_available"], json!(false));

    let (status, _) = client
        .send(
            "POST",
            "/api/tables/notes/fields",
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    assert!(status.is_success(), "{status}");
    let (status, tables) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK);
    let notes = listed_table(&tables, "notes");
    assert_eq!(notes["history_enabled"], json!(false));
    assert_eq!(notes["history_available"], json!(true));
    assert_eq!(notes["sync_available"], json!(true));

    let settings = |history: Option<bool>| {
        let mut body = json!({
            "label": "Notes", "description": "", "min_role_read": 1, "min_role_write": 1,
            "ownership_formula": "", "rls_enabled": false, "sync_enabled": false,
        });
        if let Some(on) = history {
            body["history_enabled"] = json!(on);
        }
        body
    };

    let (status, saved) = client
        .send("PUT", "/api/tables/notes", Some(settings(Some(true))))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["history_enabled"], json!(true));
    assert_eq!(saved["sync_enabled"], json!(false));
    assert!(catalog.require("notes")?.history_enabled());
    assert_eq!(change_triggers(&db, "notes").await, vec!["_fd_changes"]);

    let (status, tables) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed_table(&tables, "notes")["history_enabled"],
        json!(true)
    );

    // The body is the whole configuration: leaving the switch out turns it off.
    let (status, saved) = client
        .send("PUT", "/api/tables/notes", Some(settings(None)))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["history_enabled"], json!(false));
    assert!(change_triggers(&db, "notes").await.is_empty());
    Ok(())
}

/// Row history H2: the admin API reads a row's history, the deleted rows, a
/// row as it was and the changesets, each request one changeset by the admin
/// who made it; and the history's read role is a table setting.
#[tokio::test]
async fn row_history_is_read_through_the_admin_api() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    sc_catalog::bootstrap_change_log(&catalog).await?;
    client
        .send("POST", "/api/tables", Some(json!({ "name": "notes" })))
        .await;
    for field in [
        json!({ "name": "id", "type": "int", "primary_key": true }),
        json!({ "name": "title", "type": "text" }),
    ] {
        let (status, _) = client
            .send("POST", "/api/tables/notes/fields", Some(field))
            .await;
        assert!(status.is_success(), "{status}");
    }
    let (status, saved) = client
        .send(
            "PUT",
            "/api/tables/notes",
            Some(json!({
                "label": "Notes", "description": "", "min_role_read": 1, "min_role_write": 1,
                "ownership_formula": "", "rls_enabled": false, "sync_enabled": false,
                "history_enabled": true, "history_min_role": 40, "history_keep_days": 90,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["history_min_role"], json!(40));
    assert_eq!(saved["history_keep_days"], json!(90));
    assert_eq!(
        catalog.require("notes")?.attributes["history"]["keep_days"],
        json!(90)
    );

    for (method, path, body) in [
        (
            "POST",
            "/api/tables/notes/rows",
            Some(json!({ "id": 1, "title": "milk" })),
        ),
        (
            "PUT",
            "/api/tables/notes/rows/1",
            Some(json!({ "title": "oat milk" })),
        ),
        (
            "POST",
            "/api/tables/notes/rows",
            Some(json!({ "id": 2, "title": "eggs" })),
        ),
        ("DELETE", "/api/tables/notes/rows/2", None),
    ] {
        let (status, answer) = client.send(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {answer}");
    }

    let (status, history) = client
        .send("GET", "/api/tables/notes/rows/1/history", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history.as_array().unwrap().len(), 2);
    assert_eq!(
        history[0]["changes"],
        json!([{ "field": "title", "old": "milk", "new": "oat milk", "removed": false }])
    );
    assert_eq!(history[0]["changeset"]["source"], json!("request"));
    assert_eq!(
        history[0]["changeset"]["user_email"],
        json!("admin@example.com")
    );

    let first = history[1]["seq"].to_string();
    let (status, rebuilt) = client
        .send(
            "GET",
            &format!("/api/tables/notes/rows/1?as_of={first}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rebuilt}");
    assert_eq!(rebuilt["row"]["title"], json!("milk"));
    // The count route is not a row called `count`.
    let (status, count) = client
        .send("GET", "/api/tables/notes/rows/count", None)
        .await;
    assert_eq!((status, count["count"].clone()), (StatusCode::OK, json!(1)));

    let (status, deleted) = client.send("GET", "/api/tables/notes/deleted", None).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted[0]["key"], json!("2"));
    assert_eq!(deleted[0]["row"]["title"], json!("eggs"));

    let (status, list) = client
        .send("GET", "/api/changesets?table=notes&source=request", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 4);
    let id = list[0]["id"].as_str().unwrap().to_owned();
    let (status, one) = client
        .send("GET", &format!("/api/changesets/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["entries"][0]["op"], json!("d"));

    // The admin API is the admin's, whatever role the table opens history to.
    client.send("POST", "/api/logout", None).await;
    let (status, _) = client
        .send("GET", "/api/tables/notes/rows/1/history", None)
        .await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "{status}"
    );
    Ok(())
}

/// Row history H3: restore, revert and undo over the admin API. A restore and
/// an undo are each a changeset of their own (`restore`, `undo`), and a
/// refused undo answers its conflicts rather than an error.
#[tokio::test]
async fn rows_are_restored_reverted_and_undone_through_the_admin_api() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    sc_catalog::bootstrap_change_log(&catalog).await?;
    client
        .send("POST", "/api/tables", Some(json!({ "name": "notes" })))
        .await;
    for field in [
        json!({ "name": "id", "type": "int", "primary_key": true }),
        json!({ "name": "title", "type": "text" }),
    ] {
        let (status, _) = client
            .send("POST", "/api/tables/notes/fields", Some(field))
            .await;
        assert!(status.is_success(), "{status}");
    }
    let (status, saved) = client
        .send(
            "PUT",
            "/api/tables/notes",
            Some(json!({
                "label": "Notes", "description": "", "min_role_read": 1, "min_role_write": 1,
                "ownership_formula": "", "rls_enabled": false, "sync_enabled": false,
                "history_enabled": true,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    for (method, path, body) in [
        (
            "POST",
            "/api/tables/notes/rows",
            Some(json!({ "id": 1, "title": "milk" })),
        ),
        (
            "PUT",
            "/api/tables/notes/rows/1",
            Some(json!({ "title": "oat milk" })),
        ),
        (
            "POST",
            "/api/tables/notes/rows",
            Some(json!({ "id": 2, "title": "eggs" })),
        ),
        ("DELETE", "/api/tables/notes/rows/2", None),
    ] {
        let (status, answer) = client.send(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {answer}");
    }

    // Restore the deleted row.
    let (status, restored) = client
        .send("POST", "/api/tables/notes/deleted/2/restore", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(restored["row"]["title"], json!("eggs"));
    assert_eq!(restored["dropped"], json!([]));

    // Set row 1 back to its first version.
    let (status, reverted) = client
        .send(
            "POST",
            "/api/tables/notes/rows/1/revert",
            Some(json!({ "version": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reverted}");
    assert_eq!(reverted["row"]["title"], json!("milk"));

    // Undo the revert: row 1 is "oat milk" again, in an `undo` changeset.
    let revert = reverted["changeset"].as_str().unwrap().to_owned();
    let (status, outcome) = client
        .send("POST", &format!("/api/changesets/{revert}/undo"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{outcome}");
    assert_eq!(outcome["undone"], json!(true), "{outcome}");
    let (_, history) = client
        .send("GET", "/api/tables/notes/rows/1/history", None)
        .await;
    assert_eq!(history[0]["changeset"]["source"], json!("undo"));
    assert_eq!(history[0]["changes"][0]["new"], json!("oat milk"));

    // The restore, after row 2 has been edited since: refused, naming the field.
    client
        .send(
            "PUT",
            "/api/tables/notes/rows/2",
            Some(json!({ "title": "brown eggs" })),
        )
        .await;
    let restore = restored["changeset"].as_str().unwrap().to_owned();
    let (status, outcome) = client
        .send(
            "POST",
            &format!("/api/changesets/{restore}/undo"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{outcome}");
    assert_eq!(outcome["undone"], json!(false));
    assert_eq!(outcome["conflicts"][0]["field"], json!("title"));
    assert_eq!(outcome["conflicts"][0]["actual"], json!("brown eggs"));
    let (status, outcome) = client
        .send(
            "POST",
            &format!("/api/changesets/{restore}/undo"),
            Some(json!({ "force": true })),
        )
        .await;
    assert_eq!(
        (status, outcome["undone"].clone()),
        (StatusCode::OK, json!(true))
    );
    let (_, deleted) = client.send("GET", "/api/tables/notes/deleted", None).await;
    assert_eq!(deleted[0]["key"], json!("2"));

    client.send("POST", "/api/logout", None).await;
    let (status, _) = client
        .send("POST", "/api/tables/notes/deleted/2/restore", None)
        .await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "{status}"
    );
    Ok(())
}

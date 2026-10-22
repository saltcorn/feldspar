//! Offline sync S2: a device registers, takes everything it may read in its
//! first pull, and afterwards only what changed since its watermark (design
//! §14.11).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use sc_api::{ApiProvider, ApiRequest, ApiResponse, Method, RestProvider, schema_edit};
use sc_auth::{User, bootstrap, create_user};
use sc_catalog::{Catalog, bootstrap_change_log, bootstrap_field_meta, bootstrap_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

const APP: uuid::Uuid = uuid::Uuid::from_u128(0x5eed);

struct Fixture {
    cat: Arc<Catalog>,
    api: RestProvider,
    ann: User,
    bob: User,
}

/// `notes` belong to their `owner` (admin-only otherwise); `secrets` are
/// admin-only. Both synced, both in the application.
async fn fixture(db: &TestDb) -> Result<Fixture> {
    // The local test template may carry `users` tables from a v1 database.
    sql(
        db,
        "DO $$ DECLARE r record; BEGIN \
           FOR r IN SELECT table_schema FROM information_schema.tables \
           WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
             EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
           END LOOP; END $$;
         CREATE EXTENSION IF NOT EXISTS pgcrypto;
         CREATE TABLE notes (id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
                             title text, owner uuid);
         CREATE TABLE secrets (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), body text);",
    )
    .await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap(&cat).await?;
    bootstrap_table_meta(&cat).await?;
    bootstrap_field_meta(&cat).await?;
    bootstrap_change_log(&cat).await?;
    cat.reload().await?;

    let settings = |formula: &str| schema_edit::TableSettings {
        min_role_read: Some(1),
        min_role_write: Some(1),
        ownership_formula: Some(formula.into()),
        sync_enabled: Some(true),
        ..Default::default()
    };
    schema_edit::apply(
        &cat,
        &[
            schema_edit::Operation::AlterTable {
                table: "notes".into(),
                settings: settings("owner === user.id"),
            },
            schema_edit::Operation::AlterTable {
                table: "secrets".into(),
                settings: settings(""),
            },
        ],
        &schema_edit::ApplyOptions::default(),
    )
    .await?;

    sc_auth::save_role(&cat, &sc_auth::Role::new(80, "User")).await?;
    let ann = create_user(&cat, "ann@example.com", "ann-password", 80).await?;
    let bob = create_user(&cat, "bob@example.com", "bob-password", 80).await?;
    let api = RestProvider::project("/api", &[cat.require("notes")?, cat.require("secrets")?])
        .with_application(APP);
    Ok(Fixture {
        cat: Arc::new(cat),
        api,
        ann,
        bob,
    })
}

async fn sql(db: &TestDb, statements: &str) -> Result<()> {
    db.client()
        .await?
        .batch_execute(statements)
        .await
        .map_err(|e| Error::database(format!("{e:?}")))
}

async fn post(f: &Fixture, user: &User, path: &str, body: Json) -> Result<ApiResponse> {
    f.api
        .handle(
            ApiRequest::new(Method::Post, path).body(body),
            &f.cat,
            Some(user),
        )
        .await
}

async fn register(f: &Fixture, user: &User) -> Result<String> {
    let answer = post(f, user, "/api/sync/register", json!({ "name": "phone" })).await?;
    assert_eq!(answer.status, 200, "{}", answer.body);
    Ok(answer.body["client_id"].as_str().unwrap().to_owned())
}

async fn pull(f: &Fixture, user: &User, device: &str, watermark: &Json) -> Result<Json> {
    let answer = post(
        f,
        user,
        "/api/sync/pull",
        json!({ "client_id": device, "watermark": watermark, "tables": ["notes"] }),
    )
    .await?;
    assert_eq!(answer.status, 200, "{}", answer.body);
    Ok(answer.body)
}

/// The `notes` entry of a pull, as (reset, row titles, removed keys).
fn notes(answer: &Json) -> (bool, BTreeSet<String>, BTreeSet<String>) {
    let entry = answer["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["table"] == "notes")
        .expect("notes is synced");
    let titles = entry["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["title"].as_str().unwrap().to_owned())
        .collect();
    let removed = entry["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_owned())
        .collect();
    (entry["reset"] == json!(true), titles, removed)
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn a_first_pull_returns_every_readable_row() -> Result<()> {
    let db = TestDb::new().await?;
    let f = fixture(&db).await?;
    let (ann, bob) = (f.ann.id, f.bob.id);
    sql(
        &db,
        &format!(
            "INSERT INTO notes (title, owner) VALUES ('ann 1', '{ann}'), ('bob 1', '{bob}');
             INSERT INTO secrets (body) VALUES ('admins only');"
        ),
    )
    .await?;

    let device = register(&f, &f.ann).await?;
    let answer = pull(&f, &f.ann, &device, &Json::Null).await?;
    // Only her own note, and no admin-only table at all.
    assert_eq!(notes(&answer), (true, set(&["ann 1"]), set(&[])));
    let tables: Vec<&str> = answer["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["table"].as_str().unwrap())
        .collect();
    assert_eq!(tables, vec!["notes"]);
    assert!(sc_catalog::SyncWatermark::parse(answer["watermark"].as_str().unwrap()).is_ok());

    // A device id is not a key: it is Ann's, for this application.
    let stolen = post(
        &f,
        &f.bob,
        "/api/sync/pull",
        json!({ "client_id": device, "watermark": null, "tables": [] }),
    )
    .await
    .unwrap_err();
    assert!(stolen.to_string().contains("unknown device"), "{stolen}");

    // Signed out, there is no syncing at all.
    let anonymous = f
        .api
        .handle(
            ApiRequest::new(Method::Post, "/api/sync/register").body(json!({})),
            &f.cat,
            None,
        )
        .await?;
    assert_eq!(anonymous.status, 401);
    Ok(())
}

#[tokio::test]
async fn a_later_pull_brings_what_changed_and_what_is_gone() -> Result<()> {
    let db = TestDb::new().await?;
    let f = fixture(&db).await?;
    let (ann, bob) = (f.ann.id, f.bob.id);
    let gone = "00000000-0000-4000-8000-0000000000a3";
    sql(
        &db,
        &format!(
            "INSERT INTO notes (title, owner) VALUES ('ann 1', '{ann}'), ('bob 1', '{bob}');
             INSERT INTO notes (id, title, owner) VALUES ('{gone}', 'ann 3', '{ann}');"
        ),
    )
    .await?;
    let device = register(&f, &f.ann).await?;
    let first = pull(&f, &f.ann, &device, &Json::Null).await?;
    assert_eq!(notes(&first).1, set(&["ann 1", "ann 3"]));

    // Nothing changed: nothing comes.
    let quiet = pull(&f, &f.ann, &device, &first["watermark"]).await?;
    assert_eq!(notes(&quiet), (false, set(&[]), set(&[])));

    sql(
        &db,
        &format!(
            "UPDATE notes SET title = 'ann 1 edited' WHERE title = 'ann 1';
             DELETE FROM notes WHERE id = '{gone}';
             UPDATE notes SET owner = '{ann}' WHERE title = 'bob 1';
             INSERT INTO notes (title, owner) VALUES ('bob 2', '{bob}');"
        ),
    )
    .await?;
    let answer = pull(&f, &f.ann, &device, &quiet["watermark"]).await?;
    let (reset, titles, removed) = notes(&answer);
    assert!(!reset);
    // The edit, and the note handed to her; never Bob's own.
    assert_eq!(titles, set(&["ann 1 edited", "bob 1"]));
    assert!(removed.contains(gone), "{removed:?}");

    // A table the device does not hold yet comes in whole.
    let fresh = post(
        &f,
        &f.ann,
        "/api/sync/pull",
        json!({ "client_id": device, "watermark": answer["watermark"], "tables": [] }),
    )
    .await?;
    assert_eq!(
        notes(&fresh.body),
        (true, set(&["ann 1 edited", "bob 1"]), set(&[]))
    );
    Ok(())
}

#[tokio::test]
async fn a_transaction_that_commits_late_is_not_skipped() -> Result<()> {
    let db = TestDb::new().await?;
    let f = fixture(&db).await?;
    let ann = f.ann.id;
    let device = register(&f, &f.ann).await?;
    let first = pull(&f, &f.ann, &device, &Json::Null).await?;

    // A transaction starts writing, then another one commits before it.
    let slow = db.client().await?;
    slow.batch_execute(&format!(
        "BEGIN; INSERT INTO notes (title, owner) VALUES ('late', '{ann}');"
    ))
    .await
    .map_err(|e| Error::database(format!("{e:?}")))?;
    sql(
        &db,
        &format!("INSERT INTO notes (title, owner) VALUES ('early', '{ann}');"),
    )
    .await?;

    // The committed one comes now; the slow one is remembered as still running.
    let early = pull(&f, &f.ann, &device, &first["watermark"]).await?;
    assert_eq!(notes(&early).1, set(&["early"]));

    slow.batch_execute("COMMIT")
        .await
        .map_err(|e| Error::database(format!("{e:?}")))?;
    let answer = pull(&f, &f.ann, &device, &early["watermark"]).await?;
    assert_eq!(notes(&answer).1, set(&["late"]));
    Ok(())
}

#[tokio::test]
async fn an_open_transaction_elsewhere_does_not_stall_pulls() -> Result<()> {
    let db = TestDb::new().await?;
    let f = fixture(&db).await?;
    let ann = f.ann.id;
    let device = register(&f, &f.ann).await?;
    let first = pull(&f, &f.ann, &device, &Json::Null).await?;

    // A write to another table, left open (a forgotten `BEGIN` in psql).
    let idle = db.client().await?;
    idle.batch_execute("BEGIN; INSERT INTO secrets (body) VALUES ('pending');")
        .await
        .map_err(|e| Error::database(format!("{e:?}")))?;

    sql(
        &db,
        &format!("INSERT INTO notes (title, owner) VALUES ('one', '{ann}');"),
    )
    .await?;
    let second = pull(&f, &f.ann, &device, &first["watermark"]).await?;
    assert_eq!(notes(&second).1, set(&["one"]));

    // Each pull moves on: what came is not sent again.
    sql(
        &db,
        &format!("INSERT INTO notes (title, owner) VALUES ('two', '{ann}');"),
    )
    .await?;
    let third = pull(&f, &f.ann, &device, &second["watermark"]).await?;
    assert_eq!(notes(&third).1, set(&["two"]));

    idle.batch_execute("ROLLBACK")
        .await
        .map_err(|e| Error::database(format!("{e:?}")))?;
    Ok(())
}

#[tokio::test]
async fn a_watermark_that_is_not_one_is_refused() -> Result<()> {
    let db = TestDb::new().await?;
    let f = fixture(&db).await?;
    let device = register(&f, &f.ann).await?;
    for bad in ["12", "5:3:", "1:9:12", "1:9:x"] {
        let err = post(
            &f,
            &f.ann,
            "/api/sync/pull",
            json!({ "client_id": device, "watermark": bad, "tables": ["notes"] }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("`watermark`"), "{bad}: {err}");
    }
    Ok(())
}

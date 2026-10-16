//! Offline sync S3, first step: a device uploads the changes it made offline,
//! applied in its order as its user, each refused or applied on its own, and a
//! retried upload is applied once (design §14.11).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_api::{ApiProvider, ApiRequest, Method, RestProvider, schema_edit};
use sc_auth::{User, bootstrap, create_user};
use sc_catalog::{Catalog, bootstrap_field_meta, bootstrap_sync, bootstrap_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

const APP: uuid::Uuid = uuid::Uuid::from_u128(0x5eed);
const NOTE: &str = "00000000-0000-4000-8000-0000000000n1";
const ITEM: &str = "00000000-0000-4000-8000-0000000000i1";

struct Fixture {
    cat: Arc<Catalog>,
    api: RestProvider,
    ann: User,
    bob: User,
}

/// `notes` belong to their `owner`; `items` hang off a note. Both synced.
/// `drafts` is in the application but not synced.
async fn fixture(db: &TestDb) -> Result<Fixture> {
    sql(
        db,
        "DO $$ DECLARE r record; BEGIN \
           FOR r IN SELECT table_schema FROM information_schema.tables \
           WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
             EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
           END LOOP; END $$;
         CREATE EXTENSION IF NOT EXISTS pgcrypto;
         CREATE TABLE notes (id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
                             title text NOT NULL, owner uuid);
         CREATE TABLE items (id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
                             note uuid REFERENCES notes(id), label text);
         CREATE TABLE drafts (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), body text);",
    )
    .await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap(&cat).await?;
    bootstrap_table_meta(&cat).await?;
    bootstrap_field_meta(&cat).await?;
    bootstrap_sync(&cat).await?;
    cat.reload().await?;

    let settings = |formula: &str, sync: bool| schema_edit::TableSettings {
        min_role_read: Some(1),
        min_role_write: Some(1),
        ownership_formula: Some(formula.into()),
        sync_enabled: Some(sync),
        ..Default::default()
    };
    let alter = |table: &str, settings| schema_edit::Operation::AlterTable {
        table: table.into(),
        settings,
    };
    schema_edit::apply(
        &cat,
        &[
            alter("notes", settings("owner === user.id", true)),
            // Anyone signed in may write items.
            alter(
                "items",
                schema_edit::TableSettings {
                    min_role_read: Some(80),
                    min_role_write: Some(80),
                    sync_enabled: Some(true),
                    ..Default::default()
                },
            ),
            alter(
                "drafts",
                schema_edit::TableSettings {
                    min_role_read: Some(80),
                    min_role_write: Some(80),
                    ..Default::default()
                },
            ),
        ],
        &schema_edit::ApplyOptions::default(),
    )
    .await?;

    sc_auth::save_role(&cat, &sc_auth::Role::new(80, "User")).await?;
    let ann = create_user(&cat, "ann@example.com", "ann-password", 80).await?;
    let bob = create_user(&cat, "bob@example.com", "bob-password", 80).await?;
    let api = RestProvider::project(
        "/api",
        &[
            cat.require("notes")?,
            cat.require("items")?,
            cat.require("drafts")?,
        ],
    )
    .with_application(APP)
    // Ownership checks on writes run the formula.
    .with_evaluator(Arc::new(DenoEvaluator::new()) as Arc<dyn JsEvaluator>);
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

async fn query(db: &TestDb, statement: &str) -> Result<Vec<String>> {
    let rows = db
        .client()
        .await?
        .query(statement, &[])
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

async fn post(f: &Fixture, user: &User, path: &str, body: Json) -> Result<Json> {
    let answer = f
        .api
        .handle(
            ApiRequest::new(Method::Post, path).body(body),
            &f.cat,
            Some(user),
        )
        .await?;
    assert_eq!(answer.status, 200, "{}", answer.body);
    Ok(answer.body)
}

async fn register(f: &Fixture, user: &User) -> Result<String> {
    let answer = post(f, user, "/api/sync/register", json!({ "name": "phone" })).await?;
    Ok(answer["client_id"].as_str().unwrap().to_owned())
}

async fn push(f: &Fixture, user: &User, device: &str, changes: Json) -> Result<Vec<Json>> {
    let answer = post(
        f,
        user,
        "/api/sync/push",
        json!({ "client_id": device, "changes": changes }),
    )
    .await?;
    Ok(answer["results"].as_array().unwrap().clone())
}

fn statuses(results: &[Json]) -> Vec<&str> {
    results
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn a_device_uploads_what_it_did_offline() -> Result<()> {
    let db = TestDb::new().await?;
    let f = fixture(&db).await?;
    let ann = f.ann.id.to_string();
    let device = register(&f, &f.ann).await?;
    let note = NOTE.replace('n', "a");
    let item = ITEM.replace('i', "b");

    // A note and an item pointing at it, made offline; then edited; then the
    // item deleted.
    let results = push(
        &f,
        &f.ann,
        &device,
        json!([
            { "mutation": 1, "table": "notes", "op": "insert",
              "row": { "id": note, "title": "shopping", "owner": ann } },
            { "mutation": 2, "table": "items", "op": "insert", "key": item,
              "row": { "note": note, "label": "milk" } },
            { "mutation": 3, "table": "notes", "op": "update", "key": note,
              "row": { "title": "groceries" } },
            { "mutation": 4, "table": "items", "op": "delete", "key": item },
        ]),
    )
    .await?;
    assert_eq!(
        statuses(&results),
        vec!["applied", "applied", "applied", "applied"],
        "{results:?}"
    );
    assert_eq!(
        query(&db, "SELECT title FROM notes").await?,
        vec!["groceries"]
    );
    assert!(query(&db, "SELECT label FROM items").await?.is_empty());

    // The change-log knows each change came from this device, and which one.
    let log = query(
        &db,
        "SELECT table_name || ' ' || op || ' ' || mutation FROM _fd_changes \
          WHERE client_id::text = (SELECT id::text FROM _fd_sync_clients) ORDER BY seq",
    )
    .await?;
    assert_eq!(
        log,
        vec!["notes i 1", "items i 2", "notes u 3", "items d 4"]
    );
    Ok(())
}

#[tokio::test]
async fn a_refused_change_does_not_stop_the_others() -> Result<()> {
    let db = TestDb::new().await?;
    let f = fixture(&db).await?;
    let (ann, bob) = (f.ann.id.to_string(), f.bob.id.to_string());
    let device = register(&f, &f.ann).await?;
    let mine = NOTE.replace('n', "a");
    let gone = NOTE.replace('n', "f");

    let results = push(
        &f,
        &f.ann,
        &device,
        json!([
            // Ownership: Ann cannot write Bob's note.
            { "mutation": 1, "table": "notes", "op": "insert",
              "row": { "id": NOTE.replace('n', "b"), "title": "his", "owner": bob } },
            // Validation: a note needs a title.
            { "mutation": 2, "table": "notes", "op": "insert",
              "row": { "id": NOTE.replace('n', "c"), "owner": ann } },
            // A foreign key: the note is not there.
            { "mutation": 3, "table": "items", "op": "insert", "key": ITEM.replace('i', "c"),
              "row": { "note": gone, "label": "orphan" } },
            // Not synced.
            { "mutation": 4, "table": "drafts", "op": "insert", "key": NOTE.replace('n', "d"),
              "row": { "body": "x" } },
            // Fine.
            { "mutation": 5, "table": "notes", "op": "insert",
              "row": { "id": mine, "title": "mine", "owner": ann } },
            // A key change.
            { "mutation": 6, "table": "notes", "op": "update", "key": mine,
              "row": { "id": NOTE.replace('n', "e") } },
            // A row that is not there.
            { "mutation": 7, "table": "notes", "op": "update", "key": gone,
              "row": { "title": "nothing" } },
            // A delete the foreign keys forbid: the note still has an item.
            { "mutation": 8, "table": "items", "op": "insert", "key": ITEM.replace('i', "d"),
              "row": { "note": mine, "label": "kept" } },
            { "mutation": 9, "table": "notes", "op": "delete", "key": mine },
        ]),
    )
    .await?;
    assert_eq!(
        statuses(&results),
        vec![
            "refused", "refused", "refused", "refused", "applied", "refused", "refused", "applied",
            "refused"
        ],
        "{results:?}"
    );
    let error = |i: usize| results[i]["error"].as_str().unwrap().to_owned();
    assert!(error(0).contains("ownership"), "{}", error(0));
    assert!(error(1).contains("title"), "{}", error(1));
    assert!(error(2).contains("foreign key"), "{}", error(2));
    assert!(error(3).contains("not a synced table"), "{}", error(3));
    assert!(error(5).contains("primary key"), "{}", error(5));
    assert!(error(8).contains("foreign key"), "{}", error(8));
    assert_eq!(query(&db, "SELECT title FROM notes").await?, vec!["mine"]);
    Ok(())
}

#[tokio::test]
async fn a_retried_upload_is_applied_once() -> Result<()> {
    let db = TestDb::new().await?;
    let f = fixture(&db).await?;
    let ann = f.ann.id.to_string();
    let device = register(&f, &f.ann).await?;
    let changes = |from: i64| {
        json!([
            { "mutation": from, "table": "items", "op": "insert",
              "key": ITEM.replace('i', &from.to_string()), "row": { "label": "one" } },
            { "mutation": from + 1, "table": "notes", "op": "insert",
              "row": { "id": NOTE.replace('n', "a"), "title": "first", "owner": ann } },
        ])
    };
    let first = push(&f, &f.ann, &device, changes(1)).await?;
    assert_eq!(statuses(&first), vec!["applied", "applied"]);

    // The answer was lost; the device sends the same again, and one more.
    let mut again = changes(1).as_array().unwrap().clone();
    again.push(json!({ "mutation": 3, "table": "notes", "op": "update",
                       "key": NOTE.replace('n', "a"), "row": { "title": "second" } }));
    let retried = push(&f, &f.ann, &device, Json::Array(again)).await?;
    assert_eq!(statuses(&retried), vec!["skipped", "skipped", "applied"]);
    assert_eq!(query(&db, "SELECT label FROM items").await?, vec!["one"]);
    assert_eq!(query(&db, "SELECT title FROM notes").await?, vec!["second"]);

    // Another user's device id is no way in.
    let err = f
        .api
        .handle(
            ApiRequest::new(Method::Post, "/api/sync/push")
                .body(json!({ "client_id": device, "changes": [] })),
            &f.cat,
            Some(&f.bob),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown device"), "{err}");

    // A malformed upload is refused whole.
    let err = f
        .api
        .handle(
            ApiRequest::new(Method::Post, "/api/sync/push").body(json!({
                "client_id": device,
                "changes": [
                    { "mutation": 5, "table": "notes", "op": "delete", "key": "x" },
                    { "mutation": 4, "table": "notes", "op": "delete", "key": "y" },
                ],
            })),
            &f.cat,
            Some(&f.ann),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("must increase"), "{err}");
    Ok(())
}

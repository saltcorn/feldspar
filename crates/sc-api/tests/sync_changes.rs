//! Offline sync S1: switching a table's sync on records every change to it in
//! `_fd_changes` (design §14.11).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_api::{rows, schema_edit};
use sc_catalog::{Catalog, bootstrap_change_log, bootstrap_field_meta, bootstrap_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

async fn setup(db: &TestDb) -> Result<Catalog> {
    db.client()
        .await?
        .batch_execute(
            // `gen_random_uuid()` is built in from Postgres 13; on 12 it is pgcrypto's.
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;
             CREATE TABLE todo (id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
                                title text, done bool NOT NULL DEFAULT false);
             CREATE TABLE pairs (a bigint, b bigint, PRIMARY KEY (a, b));
             CREATE TABLE loose (note text);",
        )
        .await
        .map_err(|e| Error::database(format!("{e:?}")))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    bootstrap_change_log(&catalog).await?;
    catalog.reload().await?;
    Ok(catalog)
}

async fn set_sync(catalog: &Catalog, table: &str, enabled: bool) -> Result<()> {
    let op = schema_edit::Operation::AlterTable {
        table: table.to_owned(),
        settings: schema_edit::TableSettings {
            sync_enabled: Some(enabled),
            ..Default::default()
        },
    };
    schema_edit::apply(catalog, &[op], &schema_edit::ApplyOptions::default()).await?;
    Ok(())
}

/// One log entry with its changeset: `(row_key, op, old_values, new_values,
/// source, client_id, mutation)`.
type Entry = (
    String,
    String,
    Json,
    Json,
    String,
    Option<String>,
    Option<i64>,
);

/// The log, oldest first.
async fn changes(db: &TestDb) -> Result<Vec<Entry>> {
    let rows = db
        .client()
        .await?
        .query(
            "SELECT c.row_key, c.op, coalesce(c.old_values, 'null'::jsonb),
                    coalesce(c.new_values, 'null'::jsonb), s.source, s.client_id::text,
                    s.mutation
               FROM _fd_changes c JOIN _fd_changesets s ON s.id = c.changeset
              ORDER BY c.seq",
            &[],
        )
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get(0),
                r.get(1),
                r.get(2),
                r.get(3),
                r.get(4),
                r.get(5),
                r.get(6),
            )
        })
        .collect())
}

async fn sql(db: &TestDb, statements: &str) -> Result<()> {
    db.client()
        .await?
        .batch_execute(statements)
        .await
        .map_err(|e| Error::database(format!("{e:?}")))
}

#[tokio::test]
async fn a_synced_table_records_every_change() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    set_sync(&catalog, "todo", true).await?;
    let todo = catalog.require("todo")?;
    assert!(todo.sync_enabled());

    // Through the row layer, and with plain SQL: both are recorded.
    let created = rows::create_row(&catalog, &todo, &json!({ "title": "milk" })).await?;
    let id = created["id"].as_str().unwrap().to_owned();
    sql(
        &db,
        &format!("UPDATE todo SET done = true WHERE id = '{id}'"),
    )
    .await?;
    // Nothing changes, so nothing is recorded.
    sql(
        &db,
        &format!("UPDATE todo SET done = true WHERE id = '{id}'"),
    )
    .await?;
    // A device's upload names itself in its changeset.
    sql(
        &db,
        &format!(
            r#"BEGIN;
             SET LOCAL sc.changeset = '{{"id": "6f1c9a3e-0000-4000-8000-0000000000c5",
               "source": "sync", "client_id": "6f1c9a3e-0000-4000-8000-000000000001",
               "mutation": 7}}';
             UPDATE todo SET title = 'oat milk' WHERE id = '{id}';
             COMMIT;"#
        ),
    )
    .await?;
    // A new key is a delete of the old row and an insert of the new one.
    let new_id = "00000000-0000-4000-8000-000000000042";
    sql(
        &db,
        &format!("UPDATE todo SET id = '{new_id}' WHERE id = '{id}'"),
    )
    .await?;
    sql(&db, &format!("DELETE FROM todo WHERE id = '{new_id}'")).await?;

    let device = Some("6f1c9a3e-0000-4000-8000-000000000001".to_owned());
    let row = |id: &str, title: &str| json!({ "id": id, "title": title, "done": true });
    let entry = |key: &str, op: &str, old: Json, new: Json, source: &str| {
        (
            key.to_owned(),
            op.to_owned(),
            old,
            new,
            source.to_owned(),
            None,
            None,
        )
    };
    assert_eq!(
        changes(&db).await?,
        vec![
            entry(
                &id,
                "i",
                Json::Null,
                json!({ "id": id, "title": "milk", "done": false }),
                "sql"
            ),
            entry(
                &id,
                "u",
                json!({ "done": false }),
                json!({ "done": true }),
                "sql"
            ),
            (
                id.clone(),
                "u".into(),
                json!({ "title": "milk" }),
                json!({ "title": "oat milk" }),
                "sync".into(),
                device,
                Some(7)
            ),
            entry(&id, "d", row(&id, "oat milk"), Json::Null, "sql"),
            entry(new_id, "i", Json::Null, row(new_id, "oat milk"), "sql"),
            entry(new_id, "d", row(new_id, "oat milk"), Json::Null, "sql"),
        ]
    );

    // A rolled-back write leaves no entry.
    sql(
        &db,
        "BEGIN; INSERT INTO todo (title) VALUES ('never'); ROLLBACK;",
    )
    .await?;
    assert_eq!(changes(&db).await?.len(), 6);

    // Switched off, the trigger is gone and the log is kept.
    set_sync(&catalog, "todo", false).await?;
    assert!(!catalog.require("todo")?.sync_enabled());
    sql(&db, "INSERT INTO todo (title) VALUES ('unrecorded')").await?;
    assert_eq!(changes(&db).await?.len(), 6);
    Ok(())
}

async fn set_history(catalog: &Catalog, table: &str, enabled: bool) -> Result<()> {
    let op = schema_edit::Operation::AlterTable {
        table: table.to_owned(),
        settings: schema_edit::TableSettings {
            history_enabled: Some(enabled),
            ..Default::default()
        },
    };
    schema_edit::apply(catalog, &[op], &schema_edit::ApplyOptions::default()).await?;
    Ok(())
}

/// The change-log triggers on `table`, by name.
async fn triggers(db: &TestDb, table: &str) -> Result<Vec<String>> {
    let rows = db
        .client()
        .await?
        .query(
            "SELECT tgname::text FROM pg_trigger
              WHERE tgrelid = $1::text::regclass AND NOT tgisinternal ORDER BY tgname",
            &[&table],
        )
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

/// History-H0: sync and history share one trigger, which stays while either
/// is on.
#[tokio::test]
async fn the_trigger_stays_while_sync_or_history_is_on() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    set_sync(&catalog, "todo", true).await?;
    set_history(&catalog, "todo", true).await?;
    assert_eq!(triggers(&db, "todo").await?, vec!["_fd_changes"]);

    // Sync off, history still on: the trigger stays and keeps recording.
    set_sync(&catalog, "todo", false).await?;
    let todo = catalog.require("todo")?;
    assert!(!todo.sync_enabled() && todo.history_enabled());
    assert_eq!(triggers(&db, "todo").await?, vec!["_fd_changes"]);
    sql(&db, "INSERT INTO todo (title) VALUES ('kept')").await?;
    assert_eq!(changes(&db).await?.len(), 1);

    // Both off: gone.
    set_history(&catalog, "todo", false).await?;
    assert!(triggers(&db, "todo").await?.is_empty());
    sql(&db, "INSERT INTO todo (title) VALUES ('unrecorded')").await?;
    assert_eq!(changes(&db).await?.len(), 1);

    // History alone makes the trigger, and turning sync off that was never on
    // leaves it alone.
    set_history(&catalog, "todo", true).await?;
    set_sync(&catalog, "todo", false).await?;
    assert_eq!(triggers(&db, "todo").await?, vec!["_fd_changes"]);

    // History is refused where sync is, and names itself.
    let err = set_history(&catalog, "pairs", true)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("row history needs a key of one column"),
        "{err}"
    );
    Ok(())
}

/// `TABLES_RENAME.sql`'s H0 section renames an old `_fd_sync` trigger, and
/// running it again changes nothing.
#[tokio::test]
async fn the_rename_script_moves_the_old_trigger() -> Result<()> {
    let db = TestDb::new().await?;
    setup(&db).await?;
    let script = include_str!("../../../TABLES_RENAME.sql");
    let section = &script[script
        .find("-- TODO-history H0")
        .expect("the H0 section is in TABLES_RENAME.sql")..];
    let section = &section[section.find("DO $fd$").unwrap()..];
    let section = &section[..section.find("END $fd$;").unwrap() + "END $fd$;".len()];

    sql(
        &db,
        "CREATE TRIGGER _fd_sync AFTER INSERT OR UPDATE OR DELETE ON todo
           FOR EACH ROW EXECUTE FUNCTION _fd_record_change('id');",
    )
    .await?;
    sql(&db, section).await?;
    assert_eq!(triggers(&db, "todo").await?, vec!["_fd_changes"]);
    sql(&db, section).await?;
    assert_eq!(triggers(&db, "todo").await?, vec!["_fd_changes"]);
    sql(&db, "INSERT INTO todo (title) VALUES ('milk')").await?;
    assert_eq!(changes(&db).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn sync_needs_a_key_of_one_column() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    let err = set_sync(&catalog, "pairs", true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("several columns"), "{err}");
    let err = set_sync(&catalog, "loose", true)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no primary key"), "{err}");
    // Refused whole: nothing was switched on.
    assert!(!catalog.require("pairs")?.sync_enabled());

    // Nor may a synced table lose its key.
    set_sync(&catalog, "todo", true).await?;
    let unkey = schema_edit::Operation::AlterField {
        table: "todo".into(),
        field: "id".into(),
        settings: schema_edit::FieldSettings {
            primary_key: Some(false),
            ..Default::default()
        },
    };
    let err = schema_edit::apply(&catalog, &[unkey], &schema_edit::ApplyOptions::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no primary key"), "{err}");
    Ok(())
}

#[tokio::test]
async fn the_change_log_cannot_be_swapped_by_the_search_path() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    set_sync(&catalog, "todo", true).await?;

    // A decoy change-log in a schema the writer puts first: the entry must still
    // land in the real one.
    sql(
        &db,
        "CREATE SCHEMA decoy;
         CREATE TABLE decoy._fd_changes (LIKE public._fd_changes);
         BEGIN;
         SET LOCAL search_path = decoy, public;
         INSERT INTO todo (title) VALUES ('milk');
         COMMIT;",
    )
    .await?;
    assert_eq!(changes(&db).await?.len(), 1);
    let client = db.client().await?;
    let decoy: i64 = client
        .query_one("SELECT count(*) FROM decoy._fd_changes", &[])
        .await
        .map_err(|e| Error::database(e.to_string()))?
        .get(0);
    assert_eq!(decoy, 0);

    // It writes with its owner's rights, so a writer needs none on the log.
    let definer: bool = client
        .query_one(
            "SELECT prosecdef FROM pg_proc WHERE proname = '_fd_record_change'",
            &[],
        )
        .await
        .map_err(|e| Error::database(e.to_string()))?
        .get(0);
    assert!(definer);
    Ok(())
}

/// SQLite records a synced table's changes too (TODO-history H6.2): its
/// triggers are generated from the table's columns, and the watermark is the
/// log's `seq`.
#[tokio::test]
async fn sqlite_records_a_synced_tables_changes() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(sc_db_sqlite::SqliteDriver::open_in_memory()?);
    let catalog = Catalog::init(driver).await?;
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    bootstrap_change_log(&catalog).await?;
    assert!(catalog.get("_fd_changes")?.is_some());
    catalog
        .create_table(
            "todo",
            &[
                sc_catalog::DataField::plain(
                    "id",
                    sc_types::TypeRef::Basic(sc_types::BasicType::Text),
                )
                .required()
                .primary_key(),
                sc_catalog::DataField::plain(
                    "title",
                    sc_types::TypeRef::Basic(sc_types::BasicType::Text),
                ),
            ],
        )
        .await?;
    set_sync(&catalog, "todo", true).await?;
    let tx = sc_catalog::SharedTx::begin_snapshot(&catalog).await?;
    let before = sc_catalog::sync_watermark(&catalog, &tx).await?;
    tx.commit().await?;
    let todo = catalog.require("todo")?;
    rows::create_row(&catalog, &todo, &json!({ "id": "a", "title": "milk" })).await?;
    let tx = sc_catalog::SharedTx::begin_snapshot(&catalog).await?;
    let after = sc_catalog::sync_watermark(&catalog, &tx).await?;
    let keys = sc_catalog::changed_keys(&catalog, &tx, "todo", &before, &after).await?;
    tx.commit().await?;
    assert_eq!(keys.into_iter().collect::<Vec<_>>(), vec!["a".to_owned()]);
    Ok(())
}

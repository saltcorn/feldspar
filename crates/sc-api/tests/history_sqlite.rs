//! Row history on a **SQLite** primary database (TODO-history H6.2): the
//! per-table triggers, the changeset passed through the settings table, the
//! merge, the readers, restore and undo, the triggers following schema
//! changes, the cleanup job, and `seq` as sync's watermark.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_api::history::HistoryReader;
use sc_api::undo::{Reverser, UndoOutcome};
use sc_api::{rows, schema_edit};
use sc_auth::User;
use sc_catalog::retention::clean_change_log;
use sc_catalog::{
    CallerContext, Catalog, Changeset, ChangesetSource, SchemaStep, SharedTx, bootstrap_change_log,
    bootstrap_field_meta, bootstrap_table_meta, changed_keys, changeset, sync_watermark,
};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::{Statement, Value};
use serde_json::{Value as Json, json};

const ADMIN: &str = "00000000-0000-4000-8000-0000000000ad";

/// `todo` and `audit` keeping history, `todo` with one row written before it
/// did.
async fn setup() -> Result<Arc<Catalog>> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    catalog
        .apply_schema_batch(&[SchemaStep::Sql(
            "CREATE TABLE todo (id INTEGER PRIMARY KEY, title text, \
                                done boolean NOT NULL DEFAULT 0, meta jsonb);
             CREATE TABLE audit (id INTEGER PRIMARY KEY, what text);
             INSERT INTO todo (title) VALUES ('milk');"
                .into(),
        )])
        .await?;
    catalog.reload().await?;
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    bootstrap_change_log(&catalog).await?;
    catalog.reload().await?;
    for table in ["todo", "audit"] {
        alter(
            &catalog,
            table,
            schema_edit::TableSettings {
                history_enabled: Some(true),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(catalog)
}

async fn alter(catalog: &Catalog, table: &str, settings: schema_edit::TableSettings) -> Result<()> {
    let op = schema_edit::Operation::AlterTable {
        table: table.to_owned(),
        settings,
    };
    schema_edit::apply(catalog, &[op], &schema_edit::ApplyOptions::default()).await?;
    Ok(())
}

async fn edit(catalog: &Catalog, op: schema_edit::Operation) -> Result<()> {
    schema_edit::apply(catalog, &[op], &schema_edit::ApplyOptions::default()).await?;
    Ok(())
}

async fn rows_of(catalog: &Catalog, sql: &str) -> Result<Vec<sc_db::Row>> {
    catalog
        .primary()
        .query(&Statement::raw(sql, Vec::new()))
        .await?
        .try_collect()
        .await
}

fn json_of(row: &sc_db::Row, column: &str) -> Json {
    match row.get(column) {
        Some(Value::Json(json)) => json.clone(),
        Some(Value::Text(text)) => serde_json::from_str(text).unwrap_or(Json::Null),
        _ => Json::Null,
    }
}

fn text_of(row: &sc_db::Row, column: &str) -> String {
    match row.get(column) {
        Some(Value::Text(text)) => text.clone(),
        Some(Value::Uuid(id)) => id.to_string(),
        Some(other) => format!("{other:?}"),
        None => String::new(),
    }
}

/// Every entry, oldest first: `(table, key, op, old, new)`.
async fn entries(catalog: &Catalog) -> Result<Vec<(String, String, String, Json, Json)>> {
    Ok(rows_of(
        catalog,
        "SELECT table_name, row_key, op, old_values, new_values FROM _fd_changes ORDER BY seq",
    )
    .await?
    .iter()
    .map(|r| {
        (
            text_of(r, "table_name"),
            text_of(r, "row_key"),
            text_of(r, "op"),
            json_of(r, "old_values"),
            json_of(r, "new_values"),
        )
    })
    .collect())
}

fn admin() -> User {
    User::new(ADMIN.parse().unwrap(), 1).unwrap()
}

fn caller() -> CallerContext {
    CallerContext::new(1, Some(json!({ "id": ADMIN, "role": 1 })))
}

fn request() -> Arc<Changeset> {
    Arc::new(Changeset::new(ChangesetSource::Request).session(Some("s1".into())))
}

/// Statements run in one transaction under `changeset`'s settings, the way
/// `set_caller_context` passes them.
async fn in_changeset(catalog: &Catalog, changeset: &Changeset, sql: &str) -> Result<()> {
    let mut tx = catalog.primary().begin().await?;
    tx.set_local("sc.role", "1").await?;
    tx.set_local("sc.user", "").await?;
    tx.set_local("sc.changeset", &changeset.guc_value(None))
        .await?;
    tx.batch(sql).await?;
    tx.commit().await
}

#[tokio::test]
async fn writes_are_merged_into_one_entry_per_row_per_changeset() -> Result<()> {
    let catalog = setup().await?;
    let todo = catalog.require("todo")?;
    let audit = catalog.require("audit")?;

    // A request's update and a second write in the same changeset (as a
    // trigger's action makes it, in its own transaction) to the same row and
    // to another table: one entry per row.
    let first = request();
    changeset::scope(first.clone(), async {
        rows::update_row_ctx(
            &catalog,
            &todo,
            "1",
            &json!({ "title": "oat milk" }),
            Some(&caller()),
        )
        .await?;
        rows::update_row_ctx(
            &catalog,
            &todo,
            "1",
            &json!({ "done": true }),
            Some(&caller()),
        )
        .await?;
        rows::create_row_ctx(
            &catalog,
            &audit,
            &json!({ "what": "done" }),
            Some(&caller()),
        )
        .await
    })
    .await?;
    let recorded = entries(&catalog).await?;
    // The row written before history was on has no insert entry.
    assert_eq!(
        recorded,
        vec![
            (
                "todo".into(),
                "1".into(),
                "u".into(),
                json!({ "title": "milk", "done": false }),
                json!({ "title": "oat milk", "done": true }),
            ),
            (
                "audit".into(),
                "1".into(),
                "i".into(),
                Json::Null,
                json!({ "id": 1, "what": "done" }),
            ),
        ]
    );
    let sets = rows_of(
        &catalog,
        "SELECT id, source, user_id, session FROM _fd_changesets",
    )
    .await?;
    assert_eq!(sets.len(), 1);
    assert_eq!(text_of(&sets[0], "id"), first.id.to_string());
    assert_eq!(text_of(&sets[0], "source"), "request");
    assert_eq!(text_of(&sets[0], "user_id"), ADMIN);
    assert_eq!(text_of(&sets[0], "session"), "s1");
    // Nothing is left in the settings once the transactions committed.
    assert!(
        rows_of(&catalog, "SELECT * FROM _fd_tx_settings")
            .await?
            .is_empty()
    );

    // Insert then update in one changeset: one insert, the final row, with the
    // JSON column as JSON and the boolean as a boolean.
    let second = request();
    changeset::scope(second.clone(), async {
        let row = rows::create_row_ctx(
            &catalog,
            &todo,
            &json!({ "title": "bread", "meta": { "aisle": [3, 4] } }),
            Some(&caller()),
        )
        .await?;
        let id = row["id"].to_string();
        rows::update_row_ctx(
            &catalog,
            &todo,
            &id,
            &json!({ "title": "rye" }),
            Some(&caller()),
        )
        .await
    })
    .await?;
    assert_eq!(
        entries(&catalog).await?[2],
        (
            "todo".into(),
            "2".into(),
            "i".into(),
            Json::Null,
            json!({ "id": 2, "title": "rye", "done": false, "meta": { "aisle": [3, 4] } }),
        )
    );

    // Insert then delete in one transaction leaves nothing.
    let before = entries(&catalog).await?.len();
    in_changeset(
        &catalog,
        &request(),
        "INSERT INTO todo (id, title) VALUES (9, 'gone'); DELETE FROM todo WHERE id = 9;",
    )
    .await?;
    assert_eq!(entries(&catalog).await?.len(), before);

    // An update that changes nothing records nothing.
    rows::update_row_ctx(
        &catalog,
        &todo,
        "1",
        &json!({ "title": "oat milk" }),
        Some(&caller()),
    )
    .await?;
    assert_eq!(entries(&catalog).await?.len(), before);

    // A write from outside Saltcorn (no settings) is a `sql` changeset of its
    // own, and leaves no settings behind.
    rows_of(&catalog, "UPDATE todo SET title = 'soy milk' WHERE id = 1").await?;
    let sql_sets = rows_of(
        &catalog,
        "SELECT id FROM _fd_changesets WHERE source = 'sql'",
    )
    .await?;
    assert_eq!(sql_sets.len(), 1);
    assert!(
        rows_of(&catalog, "SELECT * FROM _fd_tx_settings")
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_merge_moves_seq_forward_so_a_pull_in_between_sees_it_again() -> Result<()> {
    let catalog = setup().await?;
    let changeset = request();
    in_changeset(
        &catalog,
        &changeset,
        "UPDATE todo SET title = 'oat milk' WHERE id = 1",
    )
    .await?;

    // A device pulls between the changeset's two transactions.
    let tx = SharedTx::begin_snapshot(&catalog).await?;
    let pulled = sync_watermark(&catalog, &tx).await?;
    tx.commit().await?;

    in_changeset(
        &catalog,
        &changeset,
        "UPDATE todo SET title = 'milk' WHERE id = 1",
    )
    .await?;
    // The change and its reversal leave an entry with no values, which a pull
    // still sees, because the merge gave it a later `seq`.
    assert_eq!(
        entries(&catalog).await?,
        vec![(
            "todo".into(),
            "1".into(),
            "u".into(),
            Json::Null,
            Json::Null
        )]
    );
    let tx = SharedTx::begin_snapshot(&catalog).await?;
    let now = sync_watermark(&catalog, &tx).await?;
    let keys = changed_keys(&catalog, &tx, "todo", &pulled, &now).await?;
    tx.commit().await?;
    assert_eq!(keys.into_iter().collect::<Vec<_>>(), vec!["1".to_owned()]);
    assert!(pulled.not_after(&now) && pulled != now);
    Ok(())
}

#[tokio::test]
async fn history_is_read_restored_and_undone() -> Result<()> {
    let catalog = setup().await?;
    let todo = catalog.require("todo")?;
    let audit = catalog.require("audit")?;
    let user = admin();
    let reader = HistoryReader::new(&catalog, Some(&user), None);
    let reverser = Reverser::new(&catalog, Some(&user), None);

    let edit_one = request();
    changeset::scope(edit_one.clone(), async {
        rows::update_row_ctx(
            &catalog,
            &todo,
            "1",
            &json!({ "title": "oat milk" }),
            Some(&caller()),
        )
        .await?;
        rows::create_row_ctx(
            &catalog,
            &audit,
            &json!({ "what": "renamed" }),
            Some(&caller()),
        )
        .await
    })
    .await?;
    let history = reader.row_history("todo", "1").await?;
    assert_eq!(history[0]["version"], json!(1));
    assert_eq!(
        history[0]["changes"],
        json!([{ "field": "title", "old": "milk", "new": "oat milk", "removed": false }])
    );
    let seq = history[0]["seq"].as_i64().unwrap();
    let then = reader
        .row_as_of("todo", "1", &(seq - 1).to_string())
        .await?;
    assert_eq!(then["row"]["title"], json!("milk"));
    let changesets = reader.list_changesets(Default::default()).await?;
    assert_eq!(changesets[0]["tables"], json!(["audit", "todo"]));

    // Undo the changeset across both tables.
    let outcome =
        changeset::scope(request(), reverser.undo_changeset(edit_one.id, None, false)).await?;
    assert!(
        matches!(outcome, UndoOutcome::Undone { entries: 2, .. }),
        "{outcome:?}"
    );
    let title = rows_of(&catalog, "SELECT title FROM todo WHERE id = 1").await?;
    assert_eq!(text_of(&title[0], "title"), "milk");
    assert!(rows_of(&catalog, "SELECT * FROM audit").await?.is_empty());

    // Delete, list as deleted, restore.
    rows::delete_row_ctx(&catalog, &todo, "1", Some(&caller())).await?;
    let deleted = reader.deleted_rows("todo", None, 0).await?;
    assert_eq!(deleted[0]["key"], json!("1"));
    assert_eq!(deleted[0]["row"]["title"], json!("milk"));
    let restored = changeset::scope(request(), reverser.restore_row("todo", "1")).await?;
    assert_eq!(restored.row["title"], json!("milk"));
    assert_eq!(reader.deleted_rows("todo", None, 0).await?, json!([]));
    Ok(())
}

#[tokio::test]
async fn the_triggers_follow_the_tables_columns() -> Result<()> {
    let catalog = setup().await?;
    let todo = || catalog.require("todo").unwrap();

    // A column added is recorded from the next write on.
    edit(
        &catalog,
        schema_edit::Operation::AddField {
            table: "todo".into(),
            field: schema_edit::FieldSpec {
                name: "note".into(),
                type_name: "text".into(),
                ..Default::default()
            },
        },
    )
    .await?;
    rows::update_row_ctx(
        &catalog,
        &todo(),
        "1",
        &json!({ "note": "2%" }),
        Some(&caller()),
    )
    .await?;
    assert_eq!(entries(&catalog).await?[0].4, json!({ "note": "2%" }));

    // A rename rewrites the entries' keys, and the next write records the new
    // name.
    edit(
        &catalog,
        schema_edit::Operation::AlterField {
            table: "todo".into(),
            field: "note".into(),
            settings: schema_edit::FieldSettings {
                name: Some("remark".into()),
                ..Default::default()
            },
        },
    )
    .await?;
    assert_eq!(entries(&catalog).await?[0].4, json!({ "remark": "2%" }));
    rows::update_row_ctx(
        &catalog,
        &todo(),
        "1",
        &json!({ "remark": "whole" }),
        Some(&caller()),
    )
    .await?;
    assert_eq!(
        entries(&catalog).await?[1].3,
        json!({ "remark": "2%" }),
        "the trigger was regenerated with the new name"
    );

    // A column a trigger names can be dropped, and writes go on being recorded.
    edit(
        &catalog,
        schema_edit::Operation::DropField {
            table: "todo".into(),
            field: "remark".into(),
        },
    )
    .await?;
    rows::update_row_ctx(
        &catalog,
        &todo(),
        "1",
        &json!({ "title": "cream" }),
        Some(&caller()),
    )
    .await?;
    assert_eq!(entries(&catalog).await?[2].4, json!({ "title": "cream" }));

    // Switching history off drops the triggers.
    alter(
        &catalog,
        "todo",
        schema_edit::TableSettings {
            history_enabled: Some(false),
            ..Default::default()
        },
    )
    .await?;
    let triggers = rows_of(
        &catalog,
        "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'todo'",
    )
    .await?;
    assert!(triggers.is_empty());
    Ok(())
}

#[tokio::test]
async fn the_cleanup_job_runs_on_sqlite() -> Result<()> {
    let catalog = setup().await?;
    alter(
        &catalog,
        "todo",
        schema_edit::TableSettings {
            history_keep_days: Some(10),
            ..Default::default()
        },
    )
    .await?;
    rows_of(&catalog, "UPDATE todo SET title = 'old' WHERE id = 1").await?;
    rows_of(&catalog, "UPDATE todo SET title = 'new' WHERE id = 1").await?;
    rows_of(
        &catalog,
        "UPDATE _fd_changes SET at = '2020-01-01T00:00:00.000Z'",
    )
    .await?;
    rows_of(
        &catalog,
        "UPDATE _fd_changesets SET at = '2020-01-01T00:00:00.000Z'",
    )
    .await?;
    let done = clean_change_log(&catalog, chrono::Utc::now()).await?;
    assert_eq!(done.entries, 2);
    assert_eq!(done.changesets, 2);
    assert!(entries(&catalog).await?.is_empty());
    // And the ids a removed entry had are not given out again.
    rows_of(&catalog, "UPDATE todo SET title = 'newer' WHERE id = 1").await?;
    let seq = rows_of(&catalog, "SELECT seq FROM _fd_changes").await?;
    assert!(matches!(seq[0].get("seq"), Some(Value::Int(n)) if *n > 2));
    Ok(())
}

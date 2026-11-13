//! Row history of the `users` table (security review 2026-11, E1): the change
//! log records the whole row, `password_hash` included, but no history answer
//! shows it, and restoring a `users` row leaves the password as it is rather
//! than failing on it.
use std::sync::Arc;

use sc_access::rows;
use sc_api::history::HistoryReader;
use sc_api::schema_edit;
use sc_api::undo::Reverser;
use sc_auth::{COL_PASSWORD_HASH, User};
use sc_catalog::{
    Catalog, Changeset, ChangesetSource, Principal, bootstrap_change_log, bootstrap_field_meta,
    bootstrap_table_meta, changeset,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use sc_types::Role;
use serde_json::{Value as Json, json};

const ADMIN: &str = "00000000-0000-4000-8000-0000000000ad";

fn caller() -> Principal {
    Principal::recorded(Role::ADMIN, Some(json!({ "id": ADMIN, "role": 1 })))
}

async fn action(f: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    changeset::scope(Arc::new(Changeset::new(ChangesetSource::Request)), f).await
}

/// Every string anywhere in `value`.
fn strings(value: &Json, out: &mut Vec<String>) {
    match value {
        Json::String(s) => out.push(s.clone()),
        Json::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Json::Object(map) => map.iter().for_each(|(k, v)| {
            out.push(k.clone());
            strings(v, out);
        }),
        _ => {}
    }
}

fn assert_no_hash(answer: &Json, hash: &str) {
    let mut all = Vec::new();
    strings(answer, &mut all);
    assert!(
        !all.iter().any(|s| s == COL_PASSWORD_HASH || s == hash),
        "the password hash leaked: {answer}"
    );
}

#[tokio::test]
async fn users_history_hides_the_password_hash_and_a_revert_keeps_it() -> Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    bootstrap_change_log(&catalog).await?;
    sc_auth::bootstrap(&catalog).await?;
    catalog.reload().await?;
    let op = schema_edit::Operation::AlterTable {
        table: "users".to_owned(),
        settings: schema_edit::TableSettings {
            history_enabled: Some(true),
            ..Default::default()
        },
    };
    schema_edit::apply(&catalog, &[op], &schema_edit::ApplyOptions::default()).await?;

    let mut id = String::new();
    action(async {
        let ann =
            sc_auth::create_user(&catalog, "ann@example.com", "a long password", Role::PUBLIC)
                .await?;
        id = ann.id().to_string();
        Ok(())
    })
    .await?;
    let users = catalog.require("users")?;
    let admin_caller = caller();
    action(async {
        rows::update_row(
            &catalog,
            &users,
            &id,
            &json!({ "email": "ann@example.org" }),
            sc_catalog::Unchecked::vouched(&admin_caller, "test"),
        )
        .await?;
        Ok(())
    })
    .await?;
    let hash = db
        .client()
        .await?
        .query_one(
            "SELECT password_hash FROM users WHERE id = $1::text::uuid",
            &[&id],
        )
        .await
        .unwrap()
        .get::<_, String>(0);
    // The log does hold it — which is why every reader must hide it.
    let logged = db
        .client()
        .await?
        .query_one(
            "SELECT count(*) FROM _fd_changes WHERE new_values ? 'password_hash'",
            &[],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert!(logged > 0, "the change log should record the whole row");

    let admin = User::new(ADMIN.parse().unwrap(), Role::ADMIN);
    let reader = HistoryReader::new(&catalog, Some(&admin), None);
    let history = reader.row_history("users", &id).await?;
    assert_eq!(history.as_array().map(Vec::len), Some(2), "{history}");
    assert_no_hash(&history, &hash);
    let now = chrono::Utc::now().to_rfc3339();
    let as_of = reader.row_as_of("users", &id, &now).await?;
    assert_eq!(as_of["row"]["email"], "ann@example.org");
    assert_no_hash(&as_of, &hash);

    // Reverting to the first version writes the email back and leaves the
    // password alone (it used to fail: the old image named `password_hash`).
    let reverser = Reverser::new(&catalog, Some(&admin), None);
    let reverted = reverser.revert_row("users", &id, 1).await?;
    assert_eq!(reverted.row["email"], "ann@example.com");
    assert_no_hash(&reverted.to_json(), &hash);
    let after = db
        .client()
        .await?
        .query_one(
            "SELECT password_hash FROM users WHERE id = $1::text::uuid",
            &[&id],
        )
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(after, hash);
    Ok(())
}

//! Time in an ownership formula on SQLite (TODO-type-hardening H7.2): the
//! runtime check translates `new Date()`, `Date.now()`, `.getTime()` and date
//! comparisons into SQLite's spelling (`sc_query::time`), with no evaluator to
//! fall back on — the `Scoped` reads here have none, so a formula that did not
//! translate would be refused rather than decided.

use std::sync::Arc;

use chrono::{Duration, Utc};
use sc_access::Scoped;
use sc_access::rows::RowQuery;
use sc_catalog::{Authority, Catalog, DataField, Principal};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_types::{BasicType, Role, TypeRef};
use serde_json::{Value as Json, json};

fn titles(rows: &Json) -> Vec<&str> {
    let mut out: Vec<&str> = rows
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r["title"].as_str()).collect())
        .unwrap_or_default();
    out.sort_unstable();
    out
}

#[tokio::test]
async fn an_embargo_and_an_edit_window_translate_for_sqlite() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    let teardown = sc_test_harness::Teardown::new();
    teardown.add(catalog.teardown_hook());
    catalog
        .create_table(
            "posts",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(ColumnGenerator::Identity),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)).required(),
                DataField::plain("owner", TypeRef::Basic(BasicType::Text)),
                DataField::plain("at", TypeRef::Basic(BasicType::Timestamp)),
                DataField::plain("due", TypeRef::Basic(BasicType::Date)),
            ],
        )
        .await?;
    catalog.reload().await?;
    let mut table = catalog.require("posts")?;
    let now = Utc::now();
    let today = now.date_naive();
    let system = Scoped::system(&catalog, "test: seed");
    for (title, minutes, due) in [
        ("published", -60, today),
        ("embargoed", 60, today),
        ("fresh", -5, today),
        ("stale", -20, today),
        ("overdue", -60, today.succ_opt().unwrap_or(today)),
    ] {
        system
            .insert(
                &table,
                &json!({
                    "title": title,
                    "owner": "a@x",
                    "at": (now + Duration::minutes(minutes)).to_rfc3339(),
                    "due": due.to_string(),
                }),
            )
            .await?;
    }
    let alice = Principal::recorded(
        Role::of(80),
        Some(json!({ "id": uuid::Uuid::new_v4().to_string(), "email": "a@x" })),
    );
    let scoped = Scoped::new(&catalog, Authority::Caller(&alice));

    table.ownership = Some(sc_expr::Formula::parse(
        "_read && at <= new Date() && due <= new Date()",
    )?);
    assert_eq!(
        titles(&scoped.read(&table, &RowQuery::new()).await?),
        ["fresh", "published", "stale"]
    );

    table.ownership = Some(sc_expr::Formula::parse(
        "owner === user.email && at.getTime() > Date.now() - 15*60e3 && at.getTime() < Date.now()",
    )?);
    assert_eq!(
        titles(&scoped.read(&table, &RowQuery::new()).await?),
        ["fresh"]
    );
    Ok(())
}

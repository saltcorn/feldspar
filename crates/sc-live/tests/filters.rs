//! A `table_changes` subscription's filter against the database's own verdict
//! (TODO.md "Live updates" L3.6): **parity**.
//!
//! For a matrix of filters × rows — nulls in every column, a `%` in a title,
//! mixed case for `like`/`ilike` — the Rust verdict ([`RowFilter::matches`]) is
//! asserted equal to "the REST read with this filter returns the row": the same
//! filter object lowered by the same function, run as a `WHERE` by the
//! database. Once on SQLite (no server needed) and once on Postgres.
//!
//! Text **ranges** are left out on purpose: the database orders text by its
//! collation, the filter by bytes, and the two agree only on text of one case
//! (see `sc_live::filter`).

use std::collections::BTreeSet;
use std::sync::Arc;

use sc_api::rows::{self, RowQuery};
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_live::{LikeRules, RowFilter};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};

/// The rows: every column null somewhere, a literal `%`, and two spellings of
/// "beta" for the case rules.
fn rows() -> Vec<Json> {
    vec![
        json!({ "id": 1, "board": 7, "title": "alpha", "position": 1.5, "due": "2026-03-01", "done": true }),
        json!({ "id": 2, "board": 8, "title": "beta test", "position": null, "due": "2026-07-15", "done": false }),
        json!({ "id": 3, "board": null, "title": null, "position": null, "due": null, "done": null }),
        json!({ "id": 4, "board": 7, "title": "100% done", "position": -2.0, "due": "2026-12-31", "done": false }),
        json!({ "id": 5, "board": null, "title": "Beta", "position": 0.0, "due": "2026-01-01", "done": true }),
    ]
}

/// The filters, in the REST read's vocabulary.
fn filters() -> Vec<Json> {
    vec![
        json!({}),
        json!({ "board": 7 }),
        json!({ "board": { "eq": 8 } }),
        json!({ "board": { "ne": 7 } }),
        json!({ "board": null }),
        json!({ "board": { "eq": null } }),
        json!({ "board": { "ne": null } }),
        json!({ "board": { "is_null": true } }),
        json!({ "board": { "is_null": false } }),
        json!({ "board": { "in": [7, 9] } }),
        json!({ "board": { "nin": [7] } }),
        json!({ "position": { "gt": 0 } }),
        json!({ "position": { "gte": 0 } }),
        json!({ "position": { "lt": 1.5 } }),
        json!({ "position": { "lte": 1.5 } }),
        json!({ "due": { "lt": "2026-06-01" } }),
        json!({ "due": { "gte": "2026-07-15" } }),
        json!({ "done": true }),
        json!({ "done": { "ne": true } }),
        json!({ "title": "alpha" }),
        json!({ "title": { "ne": "alpha" } }),
        json!({ "title": { "like": "%eta%" } }),
        json!({ "title": { "like": "beta%" } }),
        json!({ "title": { "ilike": "BETA%" } }),
        json!({ "title": { "like": "100%" } }),
        json!({ "title": { "like": "_lpha" } }),
        json!({ "title": { "in": ["alpha", "Beta"] } }),
        json!({ "or": [{ "board": 8 }, { "position": { "lt": 0 } }] }),
        json!({ "or": [{ "board": 7 }, { "title": "Beta" }] }),
        json!({ "and": [{ "board": 7 }, { "done": false }] }),
        json!({ "not": { "board": 7 } }),
        json!({ "not": { "done": true } }),
        json!({ "board": 7, "done": true }),
    ]
}

/// The ids of the rows of `rows` a JSON array holds.
fn ids(rows: &[Json]) -> BTreeSet<i64> {
    rows.iter()
        .filter_map(|row| row.get("id").and_then(Json::as_i64))
        .collect()
}

/// Write the rows into a fresh `cards` table on `catalog`, then hold every
/// filter's Rust verdict to the database's.
async fn parity(catalog: Arc<Catalog>) -> Result<()> {
    catalog
        .create_table(
            "cards",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key(),
                DataField::plain("board", TypeRef::Basic(BasicType::Int)),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
                DataField::plain("position", TypeRef::Basic(BasicType::Float)),
                DataField::plain("due", TypeRef::Basic(BasicType::Date)),
                DataField::plain("done", TypeRef::Basic(BasicType::Bool)),
            ],
        )
        .await?;
    catalog.reload().await?;
    let table = catalog.require("cards")?;
    for row in rows() {
        rows::create_row(&catalog, &table, &row).await?;
    }
    let like = LikeRules::of(catalog.driver_for(&table)?.as_ref());
    // The rows as the REST read returns them — what a change's image is.
    let all = match rows::list_rows_query(&catalog, &table, &RowQuery::new(), None).await? {
        Json::Array(all) => all,
        other => panic!("a read returns an array, got {other}"),
    };
    assert_eq!(all.len(), rows().len());

    for filter in filters() {
        let lowered = sc_api::filter::api_where(&table, Some(&filter))?;
        let read =
            rows::list_rows_query(&catalog, &table, &RowQuery::new().where_(lowered), None).await?;
        let read = ids(read.as_array().map(Vec::as_slice).unwrap_or_default());
        let parsed = RowFilter::parse(&table, Some(&filter), like)?;
        let rust: Vec<Json> = all
            .iter()
            .filter(|row| parsed.matches(&rows::row_json_values(&table, row)))
            .cloned()
            .collect();
        assert_eq!(
            ids(&rust),
            read,
            "the filter {filter} matched different rows in Rust than in the database"
        );
    }
    Ok(())
}

#[tokio::test]
async fn the_rust_verdict_is_the_sqlite_reads() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    assert_eq!(
        LikeRules::of(catalog.primary().as_ref()),
        LikeRules::SQLITE,
        "SQLite's dialect says its LIKE folds case"
    );
    parity(catalog).await
}

#[tokio::test]
async fn the_rust_verdict_is_the_postgres_reads() -> Result<()> {
    let db = TestDb::new().await?;
    let driver: Arc<dyn DatabaseDriver> = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver).await?);
    assert_eq!(
        LikeRules::of(catalog.primary().as_ref()),
        LikeRules::POSTGRES
    );
    parity(catalog).await
}

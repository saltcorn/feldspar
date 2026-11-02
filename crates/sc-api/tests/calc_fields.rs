//! Non-stored calculated fields end-to-end through the read path (TODO Phase 8).
//!
//! A calc field is a virtual overlay field with an `sc-expr` expression computed
//! on read. This proves the read path fills it — plain arithmetic, a
//! calc-field-reading-a-calc-field (dependency order + inlining), a Ⱶ-join and a
//! Ↄ-aggregation — that a write naming one is refused, and that a dependency
//! cycle is reported and fails closed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_access::rows;
use sc_catalog::{Catalog, DataFieldKind, FieldMeta, bootstrap_field_meta, save_field_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use serde_json::Value as Json;

async fn setup(db: &TestDb) -> Result<Catalog> {
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE publishers (id bigint primary key, name text);
             CREATE TABLE books (id bigint primary key, title text, pages bigint,
                 publisher bigint references publishers(id));
             CREATE TABLE reviews (id bigint primary key, book bigint references books(id),
                 rating bigint);
             INSERT INTO publishers VALUES (1, 'ACME');
             INSERT INTO books VALUES (1, 'A', 100, 1);
             INSERT INTO reviews VALUES (1, 1, 5), (2, 1, 3);",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_field_meta(&catalog).await?;
    Ok(catalog)
}

async fn add_calc(catalog: &Catalog, table: &str, name: &str, expr: &str) -> Result<()> {
    let meta = FieldMeta::new(table, name).kind(DataFieldKind::Calc {
        expression: expr.into(),
    });
    save_field_meta(catalog, &meta).await
}

#[tokio::test]
async fn calc_fields_are_computed_on_read() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    catalog.reload().await?;

    add_calc(&catalog, "books", "double_pages", "pages * 2").await?;
    // Reads another calc field: dependency order + SQL inlining.
    add_calc(&catalog, "books", "quad_pages", "double_pages * 2").await?;
    // A Ⱶ-join calc field, and a Ↄ-aggregation calc field.
    add_calc(&catalog, "books", "pub_name", "publisherⱵname").await?;
    add_calc(&catalog, "books", "review_count", "reviewsↃbook.length").await?;

    // No merge issues: every calc field is valid.
    assert!(
        catalog.field_overlay_issues()?.is_empty(),
        "unexpected issues: {:?}",
        catalog.field_overlay_issues()?
    );

    let books = catalog.require("books")?;
    let list = rows::list_rows(&catalog, &books, sc_catalog::Unchecked::system("test")).await?;
    let row = &list.as_array().unwrap()[0];

    assert_eq!(row["double_pages"], Json::from(200));
    assert_eq!(row["quad_pages"], Json::from(400));
    assert_eq!(row["pub_name"], Json::from("ACME"));
    assert_eq!(row["review_count"], Json::from(2));
    // The real columns are still there.
    assert_eq!(row["pages"], Json::from(100));
    Ok(())
}

#[tokio::test]
async fn a_calc_field_cannot_be_written() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    catalog.reload().await?;
    add_calc(&catalog, "books", "double_pages", "pages * 2").await?;

    let books = catalog.require("books")?;
    let err = rows::update_row(
        &catalog,
        &books,
        "1",
        &serde_json::json!({ "double_pages": 5 }),
        sc_catalog::Unchecked::system("test"),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("calculated field")
            && err.to_string().contains("cannot be written"),
        "got: {err}"
    );
    // The stored row is untouched.
    let list = rows::list_rows(&catalog, &books, sc_catalog::Unchecked::system("test")).await?;
    assert_eq!(list.as_array().unwrap()[0]["double_pages"], Json::from(200));
    Ok(())
}

#[tokio::test]
async fn a_calc_field_cycle_is_reported_and_the_fields_vanish() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    catalog.reload().await?;
    // a reads b, b reads a — a cycle.
    add_calc(&catalog, "books", "a", "b + 1").await?;
    add_calc(&catalog, "books", "b", "a + 1").await?;

    let issues = catalog.field_overlay_issues()?;
    assert!(
        issues.iter().any(|i| i.message.contains("cycle")),
        "cycle not reported: {issues:?}"
    );
    // Fail closed: neither calc field is exposed.
    let books = catalog.require("books")?;
    assert!(books.field("a").is_none() && books.field("b").is_none());
    let list = rows::list_rows(&catalog, &books, sc_catalog::Unchecked::system("test")).await?;
    let row = &list.as_array().unwrap()[0];
    assert!(row.get("a").is_none() && row.get("b").is_none());
    Ok(())
}

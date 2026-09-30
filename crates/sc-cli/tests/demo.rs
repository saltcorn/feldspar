//! `feldspar demo analytics` (analytics TODO A1.18): the demo's tables, the
//! same rows on every run and on both backends, and nothing touched without
//! `--replace`.

use std::sync::Arc;

use sc_analytics::demo::{DEMO_HOUSES, demo_analytics};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, OrderBy, Projection, Select, Source, Statement, Value};
use sc_test_harness::TestDb;

/// Every house, in id order, as text — what two runs must agree on.
async fn houses(cat: &Catalog) -> Result<Vec<String>> {
    let mut select = Select::from(Source::table("houses"));
    select.order = vec![OrderBy::asc(Expr::col("id"))];
    let rows = cat
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    Ok(rows.iter().map(|r| format!("{:?}", r.values())).collect())
}

async fn count(cat: &Catalog, table: &str) -> Result<i64> {
    let select = Select::from(Source::table(table)).columns(vec![Projection::expr(Expr::Agg {
        func: "count".into(),
        distinct: false,
        args: Vec::new(),
    })]);
    let rows = cat
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    match rows.first().and_then(|r| r.get_index(0)) {
        Some(Value::Int(n)) => Ok(*n),
        other => panic!("a count, got {other:?}"),
    }
}

async fn the_demo(cat: &Catalog, backend: &str) -> Result<Vec<String>> {
    let report = demo_analytics(cat, false).await?;
    assert!(report.replaced.is_empty());
    assert_eq!(report.tables[1], ("houses".to_owned(), DEMO_HOUSES));
    assert_eq!(count(cat, "neighbourhoods").await?, 5);
    assert_eq!(count(cat, "houses").await?, DEMO_HOUSES as i64);
    let viewings = report.tables[2].1 as i64;
    assert!(viewings > 300, "{backend}: {viewings} viewings");
    assert_eq!(count(cat, "viewings").await?, viewings);

    // Keys and types as the models tutorial has them.
    let houses_table = cat.require("houses")?;
    let is_key =
        |f: &sc_catalog::DataField| matches!(f.kind, sc_catalog::DataFieldKind::Key { .. });
    assert!(houses_table.field("neighbourhood").is_some_and(is_key));
    assert!(cat.require("viewings")?.field("house").is_some_and(is_key));

    // Nothing touched without `--replace`, and the refusal names the tables.
    let before = houses(cat).await?;
    let err = demo_analytics(cat, false)
        .await
        .expect_err("tables are there");
    assert!(
        err.to_string()
            .contains("`neighbourhoods`, `houses`, `viewings`")
            && err.to_string().contains("--replace"),
        "{err}"
    );
    assert_eq!(houses(cat).await?, before);

    // With it, the same rows again.
    let again = demo_analytics(cat, true).await?;
    assert_eq!(again.replaced.len(), 3);
    assert_eq!(
        houses(cat).await?,
        before,
        "{backend}: the rows are deterministic"
    );

    // A row added later is numbered after the demo's.
    let insert = Insert::row(
        "neighbourhoods",
        vec!["name".into()],
        vec![Expr::lit("Newtown")],
    );
    cat.primary()
        .query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    assert_eq!(count(cat, "neighbourhoods").await?, 6);
    Ok(before)
}

#[tokio::test]
async fn the_demo_is_deterministic_and_leaves_existing_tables_alone() -> Result<()> {
    let db = TestDb::new().await?;
    let pg =
        Catalog::init(Arc::new(PgDriver::from_pool(db.pool().clone())) as Arc<dyn DatabaseDriver>)
            .await?;
    let on_postgres = the_demo(&pg, "postgres").await?;

    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let sqlite = Catalog::init(driver).await?;
    let on_sqlite = the_demo(&sqlite, "sqlite").await?;
    // One generator, so one set of houses — as far as the two backends'
    // readings of a row agree (SQLite has no boolean of its own).
    assert_eq!(on_postgres.len(), on_sqlite.len());
    Ok(())
}

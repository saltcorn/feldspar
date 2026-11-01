//! Phase 4 integration test: drive the [`Catalog`] end-to-end against a real
//! Postgres database.
//!
//! Walks the whole Phase 4 story in one place:
//!
//! 1. **init from an existing DB** — a table is created out-of-band (straight
//!    through the driver), then `Catalog::init` introspects it into the cache.
//! 2. **create table + fields via the catalog** — `create_table` and
//!    `create_field` issue the schema changes and refresh the cache.
//! 3. **reflect in introspection** — the created table and column show up both in
//!    the catalog cache and in a fresh driver introspection, and the table's
//!    provider can query the rows back.

use std::sync::Arc;

use sc_catalog::{Catalog, DataField, DataFieldKind, DbId, TableSource};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_query::{Expr, Insert, Projection, Select, Source, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}
fn int() -> TypeRef {
    TypeRef::Basic(BasicType::Int)
}
fn boolean() -> TypeRef {
    TypeRef::Basic(BasicType::Bool)
}

#[tokio::test]
async fn catalog_init_create_and_reflect() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));

    // --- 1. init from an existing DB ------------------------------------------
    // Create a table straight through the driver so the catalog discovers a
    // pre-existing table on init (no registration step).
    driver
        .apply_schema(&sc_db::SchemaChange::CreateTable {
            name: "author".into(),
            columns: vec![
                sc_db::ColumnDef::new("id", "int8").not_null(),
                sc_db::ColumnDef::new("name", "text").not_null(),
            ],
            primary_key: vec!["id".into()],
            unlogged: false,
        })
        .await?;

    let catalog = Catalog::init(driver.clone() as Arc<dyn DatabaseDriver>).await?;

    let author = catalog.require("author")?;
    assert_eq!(author.name, "author");
    assert_eq!(author.database, DbId::primary());
    assert_eq!(author.source, TableSource::Database);
    assert_eq!(author.primary_key, vec!["id".to_string()]);
    let name = author.field("name").expect("name field");
    assert!(name.required);
    assert_eq!(name.base.type_, text());

    // --- 2. create table + fields via the catalog -----------------------------
    let book = catalog
        .create_table(
            "book",
            &[
                DataField::plain("id", int()).required().primary_key(),
                DataField::plain("title", text()).required(),
            ],
        )
        .await?;
    assert_eq!(book.name, "book");
    assert_eq!(book.primary_key, vec!["id".to_string()]);
    assert_eq!(book.fields.len(), 2);

    // Add two more fields through the catalog.
    catalog
        .create_field("book", &DataField::plain("in_print", boolean()))
        .await?;
    let book = catalog
        .create_field("book", &DataField::plain("author_id", int()))
        .await?;
    assert_eq!(book.fields.len(), 4);
    let in_print = book.field("in_print").expect("in_print field");
    assert_eq!(in_print.base.type_, boolean());
    assert_eq!(in_print.kind, DataFieldKind::Plain);
    assert!(!in_print.required); // added nullable

    // --- 3. reflect in introspection ------------------------------------------
    // The catalog cache and a fresh driver introspection must agree.
    let introspected = driver.introspect().await?;
    let book_physical = introspected
        .iter()
        .find(|t| t.name == "book")
        .expect("book present in introspection");
    let physical_cols: Vec<&str> = book_physical
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(physical_cols, ["id", "title", "in_print", "author_id"]);

    let cached_cols: Vec<&str> = book.fields.iter().map(|f| f.base.name.as_str()).collect();
    assert_eq!(cached_cols, physical_cols);

    // Both catalog tables are listed (the DB may hold other pre-existing
    // tables), and `tables()` returns them sorted by name.
    let listed = catalog.tables()?;
    let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"author"), "author listed: {names:?}");
    assert!(names.contains(&"book"), "book listed: {names:?}");
    assert!(names.windows(2).all(|w| w[0] <= w[1]), "sorted: {names:?}");

    // --- the table's provider can round-trip a row ----------------------------
    let provider = catalog.provider(&book, sc_catalog::Unchecked::system("test"))?;
    assert_eq!(provider.fields().len(), 4);

    provider
        .write(&Statement::from(Insert {
            table: "book".into(),
            columns: vec!["id".into(), "title".into(), "in_print".into()],
            rows: vec![vec![Expr::lit(1_i64), Expr::lit("Dune"), Expr::lit(true)]],
            returning: Vec::new(),
        }))
        .await?
        .try_collect()
        .await?;

    let rows = provider
        .query(
            &Select::from(Source::table("book"))
                .columns(vec![
                    Projection::expr(Expr::col("id")),
                    Projection::expr(Expr::col("title")),
                    Projection::expr(Expr::col("in_print")),
                ])
                .filter(Expr::col("id").eq(Expr::lit(1_i64))),
        )
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("title"), Some(&Value::Text("Dune".into())));
    assert_eq!(rows[0].get("in_print"), Some(&Value::Bool(true)));

    Ok(())
}

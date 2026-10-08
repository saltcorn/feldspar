//! The row layer against a **SQLite** primary database.
//!
//! `sc-db-sqlite`'s own tests prove the driver; this proves the layer above it
//! works on a backend that is not Postgres — which is the whole point of there
//! being a trait. Two paths are worth pinning because both used to be Postgres
//! text sent unconditionally:
//!
//! 1. a **CSV import**, which defers its foreign keys to commit and, on
//!    Postgres, winds the identity sequence past the keys it just wrote; and
//! 2. a **custom SQL query**, which runs in a caller-context transaction whose
//!    `SET LOCAL` only means anything where there are policies to read it.
//!
//! Neither needs a server or a harness: a SQLite database is a file.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_catalog::{Catalog, DataField};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_sqlite::SqliteDriver;
use sc_query::{Expr, Insert, Projection, Select, Source, Statement, Value};
use sc_types::{BasicType, TypeRef};

/// A catalog over a private in-memory SQLite database, with one `book` table
/// whose key numbers itself.
async fn catalog_with_books() -> sc_error::Result<Arc<Catalog>> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    catalog
        .create_table(
            "book",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(ColumnGenerator::Identity),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)).required(),
                DataField::plain("pages", TypeRef::Basic(BasicType::Int)),
            ],
        )
        .await?;
    catalog.reload().await?;
    Ok(catalog)
}

/// A CSV that carries its own keys, imported into a SQLite table — and the
/// insert afterwards, which is what a key that has not caught up would collide
/// with.
#[tokio::test]
async fn a_csv_with_its_own_keys_imports_and_the_next_insert_does_not_collide()
-> sc_error::Result<()> {
    let catalog = catalog_with_books().await?;
    let table = catalog.require("book")?;

    let outcome = sc_api::csv::import_table(
        &catalog,
        &table,
        "id,title,pages\n1,Orlando,288\n2,The Waves,228\n",
        None,
    )
    .await?;
    assert_eq!(outcome.inserted, 2, "{:?}", outcome.errors);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    // The rows are there…
    let rows = catalog
        .provider(&table)?
        .query(&Select::from(Source::table("book")))
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 2);

    // …and the key is past them: a rowid key derives its next value from the
    // table, so there is no sequence to have fallen behind — which is exactly
    // why the Postgres statement that winds one must not be sent here.
    let inserted = catalog
        .provider(&table)?
        .write(&Statement::from(
            Insert::row("book", vec!["title".into()], vec![Expr::lit("Flush")])
                .returning(vec![Projection::expr(Expr::col("id"))]),
        ))
        .await?
        .try_collect()
        .await?;
    assert_eq!(inserted[0].get("id"), Some(&Value::Int(3)));
    Ok(())
}

/// A second import of the same keys is an update rather than a collision — the
/// upsert path, which is where the deferred constraints and the row savepoints
/// live.
#[tokio::test]
async fn a_second_import_of_the_same_keys_updates_them() -> sc_error::Result<()> {
    let catalog = catalog_with_books().await?;
    let table = catalog.require("book")?;

    sc_api::csv::import_table(&catalog, &table, "id,title,pages\n1,Orlando,288\n", None).await?;
    let outcome =
        sc_api::csv::import_table(&catalog, &table, "id,title,pages\n1,Orlando,300\n", None)
            .await?;
    assert_eq!(outcome.updated, 1, "{:?}", outcome.errors);
    assert_eq!(outcome.inserted, 0);

    let rows = catalog
        .provider(&table)?
        .query(&Select::from(Source::table("book")))
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("pages"), Some(&Value::Int(300)));
    Ok(())
}

/// A caller-context read on a backend with no policies: the context has nothing
/// to be handed to, and the statement must run rather than fail asking for a
/// transaction-local setting SQLite does not have.
#[tokio::test]
async fn a_caller_context_read_runs_where_there_are_no_policies() -> sc_error::Result<()> {
    let catalog = catalog_with_books().await?;
    let table = catalog.require("book")?;
    catalog
        .provider(&table)?
        .write(&Statement::from(Insert::row(
            "book",
            vec!["title".into()],
            vec![Expr::lit("Orlando")],
        )))
        .await?
        .try_collect()
        .await?;

    let caller = sc_catalog::CallerContext::anonymous(1);
    let statement = Statement::from(Select::from(Source::table("book")));
    let rows = sc_catalog::run_in_context(&catalog, &caller, &statement).await?;
    assert_eq!(rows.len(), 1);

    // …and the read-only form, which a custom SQL query declared read-only uses.
    let rows = sc_catalog::run_in_context_read_only(&catalog, &caller, &statement).await?;
    assert_eq!(rows.len(), 1);

    // A write in a read-only transaction is still refused — by the database,
    // which is the point of asking it for one.
    let write = Statement::from(Insert::row(
        "book",
        vec!["title".into()],
        vec![Expr::lit("The Waves")],
    ));
    assert!(
        sc_catalog::run_in_context_read_only(&catalog, &caller, &write)
            .await
            .is_err(),
        "a read-only transaction must refuse a write"
    );

    // The connection that transaction used goes back to the pool usable: a
    // `query_only` left on it would make the next writer fail.
    catalog
        .provider(&table)?
        .write(&Statement::from(Insert::row(
            "book",
            vec!["title".into()],
            vec![Expr::lit("Flush")],
        )))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// A geometry field needs PostGIS (analytics TODO A5.1), so on SQLite it is
/// refused with a sentence that says so — before anything is created.
#[tokio::test]
async fn a_geometry_field_is_refused_on_sqlite_with_a_sentence() -> sc_error::Result<()> {
    let catalog = catalog_with_books().await?;
    let err = sc_api::schema_edit::apply(
        &catalog,
        &[sc_api::schema_edit::Operation::CreateTable {
            name: "places".into(),
            database: String::new(),
            settings: sc_api::schema_edit::TableSettings::default(),
            fields: vec![sc_api::schema_edit::FieldSpec {
                name: "location".into(),
                type_name: "geometry_point".into(),
                ..sc_api::schema_edit::FieldSpec::default()
            }],
        }],
        &sc_api::schema_edit::ApplyOptions::default(),
    )
    .await
    .expect_err("refused");
    let message = err.to_string();
    assert!(
        message.contains("field `location` cannot be a geometry")
            && message.contains("PostGIS")
            && message.contains("SQLite"),
        "{message}"
    );
    assert!(catalog.get("places")?.is_none());
    Ok(())
}

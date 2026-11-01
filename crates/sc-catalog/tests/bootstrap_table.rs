//! What `Catalog::bootstrap_table` does when a release adds a column to a
//! platform table that is already in somebody's database (design §9).
//!
//! The additive reconcile is the only migration this prototype has, and it has
//! one boundary: a `NOT NULL` column cannot be added to rows that have no value
//! for it. That boundary was previously left to the database to discover, which
//! reported it as an opaque constraint violation underneath whatever the boot
//! step said it was doing ("ensuring the triggers table exists"). Asserted here:
//! a nullable column still lands on a table with rows, a required one lands on a
//! table without any, and a required one on a table with rows stops with a
//! sentence naming the table and the column — leaving the table as it was.

use std::sync::Arc;

use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_query::{Expr, Insert, Statement};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}

/// The declaration as a first release shipped it.
fn v1_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", text()).required().primary_key(),
        DataField::plain("name", text()).required(),
    ]
}

async fn catalog(db: &TestDb) -> sc_error::Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}

async fn insert_row(cat: &Catalog, table: &str, id: &str, name: &str) -> sc_error::Result<()> {
    let t = cat.require(table)?;
    cat.provider(&t, sc_catalog::Unchecked::system("test"))?
        .write(&Statement::from(Insert::row(
            table,
            vec!["id".into(), "name".into()],
            vec![Expr::lit(id), Expr::lit(name)],
        )))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_nullable_column_reaches_a_table_that_already_has_rows() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    cat.bootstrap_table("_fd_widgets", &v1_fields()).await?;
    insert_row(&cat, "_fd_widgets", "a", "first").await?;

    // The next release declares one more column. Nullable, so the stored row's
    // missing value is a real state rather than a refusal.
    let mut fields = v1_fields();
    fields.push(DataField::plain("description", text()));
    let table = cat.bootstrap_table("_fd_widgets", &fields).await?;

    let added = table.field("description").expect("description added");
    assert!(!added.required);
    Ok(())
}

#[tokio::test]
async fn a_required_column_reaches_a_table_with_no_rows() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    // The table is there from a previous release but nothing was ever written to
    // it: there is nothing for NOT NULL to contradict, so the server starts.
    cat.bootstrap_table("_fd_widgets", &v1_fields()).await?;

    let mut fields = v1_fields();
    fields.push(DataField::plain("body", text()).required());
    let table = cat.bootstrap_table("_fd_widgets", &fields).await?;

    let added = table.field("body").expect("body added");
    assert!(added.required, "added as NOT NULL, as declared");

    // And it is genuinely NOT NULL in the database, not merely in the cache.
    let physical = cat.primary().introspect().await?;
    let widgets = physical
        .iter()
        .find(|t| t.name == "_fd_widgets")
        .expect("_fd_widgets present");
    let body = widgets
        .columns
        .iter()
        .find(|c| c.name == "body")
        .expect("body column");
    assert!(!body.nullable);
    Ok(())
}

#[tokio::test]
async fn a_required_column_on_a_table_with_rows_is_refused_by_name() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    cat.bootstrap_table("_fd_widgets", &v1_fields()).await?;
    insert_row(&cat, "_fd_widgets", "a", "first").await?;

    let mut fields = v1_fields();
    fields.push(DataField::plain("body", text()).required());
    let err = cat
        .bootstrap_table("_fd_widgets", &fields)
        .await
        .expect_err("a NOT NULL column cannot be added over stored rows");

    // The message has to carry both names: this reaches an admin wrapped in
    // whichever boot step was running, and "null value violates not-null
    // constraint" would not say which table or which column.
    let msg = err.to_string();
    assert!(msg.contains("_fd_widgets"), "{msg}");
    assert!(msg.contains("body"), "{msg}");

    // The table is left exactly as it was — no half-added column.
    let table = cat.require("_fd_widgets")?;
    assert!(table.field("body").is_none());
    let names: Vec<&str> = table.fields.iter().map(|f| f.base.name.as_str()).collect();
    assert_eq!(names, ["id", "name"]);
    Ok(())
}

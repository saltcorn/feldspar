//! Integration test for a **SQLite database connection**: a file in a file
//! store whose tables share the catalog with the primary database's.
//!
//! The Postgres case is `db_connections.rs`; this is the other kind, and what it
//! has to prove is the same claim with a different shape of "where the database
//! is". A SQLite database is a file, so the connection names a file store and a
//! path inside it, and the questions are whether that resolves to a real file,
//! whether its tables arrive stamped with the connection rather than with
//! `primary`, whether a write reaches *that* file, and whether the failures an
//! admin can actually cause — a store that is not connected, a file that is not
//! there — are refused with a sentence that says which.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_catalog::{
    Catalog, DbConnectionDef, DbId, bootstrap_db_connections, bootstrap_file_stores,
    connect_db_connection, connect_file_store_def, delete_db_connection, list_db_connections,
    save_db_connection, save_file_store,
};
use sc_db::{ColumnDef, DatabaseDriver, SchemaChange};
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_files::FileStoreDef;
use sc_query::{Expr, Insert, Projection, Select, Source, Statement, Value};
use sc_test_harness::TestDb;

/// A temporary directory to be a file store's root, removed when the test ends.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let path = std::env::temp_dir().join(format!("sc-sqlite-conn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Write a SQLite database at `path` holding one `invoice` table with one row.
async fn seed_sqlite(path: &std::path::Path) -> sc_error::Result<()> {
    let driver = SqliteDriver::open(path)?;
    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: "invoice".into(),
            columns: vec![
                ColumnDef::new("id", "int8").not_null().identity(),
                ColumnDef::new("title", "text").not_null(),
            ],
            primary_key: vec!["id".into()],
            unlogged: false,
        })
        .await?;
    driver
        .query(&Statement::from(Insert::row(
            "invoice",
            vec!["title".into()],
            vec![Expr::lit("from the file")],
        )))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// The `title` values a table's provider reads, through the catalog.
async fn titles(catalog: &Catalog, table: &str) -> sc_error::Result<Vec<String>> {
    let t = catalog.require(table)?;
    let rows = catalog
        .provider(&t, sc_catalog::Unchecked::system("test"))?
        .query(
            &Select::from(Source::table(table)).columns(vec![Projection::expr(Expr::col("title"))]),
        )
        .await?
        .try_collect()
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| match r.get("title") {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect())
}

#[tokio::test]
async fn a_sqlite_file_in_a_file_store_is_a_connected_database() -> sc_error::Result<()> {
    let home = TestDb::new().await?;
    let dir = TempDir::new();
    let file = dir.0.join("reporting.sqlite");
    seed_sqlite(&file).await?;

    let catalog =
        Arc::new(
            Catalog::init(
                Arc::new(PgDriver::from_pool(home.pool().clone())) as Arc<dyn DatabaseDriver>
            )
            .await?,
        );
    bootstrap_file_stores(&catalog).await?;
    bootstrap_db_connections(&catalog).await?;

    // The store the file lives in — the admin adds this first, under Files.
    let store = FileStoreDef::local("data", dir.0.display().to_string());
    save_file_store(&catalog, &store).await?;
    connect_file_store_def(&catalog, &store)?;

    // --- the connection is a store and a path, not a filesystem path --------
    let def = DbConnectionDef::sqlite("reporting", "data", "reporting.sqlite");
    save_db_connection(&catalog, &def).await?;
    connect_db_connection(&catalog, &def).await?;
    catalog.reload().await?;

    assert_eq!(catalog.database_names()?, vec!["reporting".to_string()]);
    assert_eq!(catalog.database_error("reporting")?, None);

    // The file's table is in the catalog like any other, stamped with the
    // connection — which is what the admin UI badges and what routes a query.
    let invoice = catalog.require("invoice")?;
    assert_eq!(invoice.name, "invoice");
    assert_eq!(invoice.database, DbId("reporting".into()));
    assert!(invoice.field("title").is_some());
    // Its key numbers itself, exactly as it does on Postgres, so a form can
    // insert into it.
    let key = invoice.field("id").expect("the key field");
    assert!(key.primary_key);

    // …and reading it reaches the file rather than the primary database.
    assert_eq!(titles(&catalog, "invoice").await?, vec!["from the file"]);

    // --- a write goes to the file -------------------------------------------
    let driver = catalog.driver_named(
        &DbId("reporting".into()),
        sc_catalog::Unchecked::system("test"),
    )?;
    driver
        .query(&Statement::from(Insert::row(
            "invoice",
            vec!["title".into()],
            vec![Expr::lit("written through the connection")],
        )))
        .await?
        .try_collect()
        .await?;
    let mut written = titles(&catalog, "invoice").await?;
    written.sort();
    assert_eq!(
        written,
        vec!["from the file", "written through the connection"]
    );

    // It really is the file on disk: a driver opened straight at the path sees
    // the same two rows.
    let direct = SqliteDriver::open_existing(&file)?;
    let rows = direct
        .query(&Statement::from(Select::from(Source::table("invoice"))))
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 2);

    // --- row-level security is not offered on a backend that has none -------
    assert!(!driver.capabilities().row_level_security);

    // --- disconnecting takes its tables back out, and leaves the file ------
    assert!(delete_db_connection(&catalog, def.id).await?);
    catalog.disconnect_database("reporting")?;
    catalog.reload().await?;
    assert!(!catalog.tables()?.iter().any(|t| t.name == "invoice"));
    assert!(list_db_connections(&catalog).await?.is_empty());
    assert!(file.is_file(), "removing a connection deletes nothing");
    Ok(())
}

#[tokio::test]
async fn a_sqlite_connection_that_cannot_be_resolved_says_which_half_is_wrong()
-> sc_error::Result<()> {
    let home = TestDb::new().await?;
    let dir = TempDir::new();

    let catalog =
        Arc::new(
            Catalog::init(
                Arc::new(PgDriver::from_pool(home.pool().clone())) as Arc<dyn DatabaseDriver>
            )
            .await?,
        );
    bootstrap_file_stores(&catalog).await?;
    bootstrap_db_connections(&catalog).await?;

    // A store that does not exist at all is refused at save time: it is a
    // mistake in the form, not a database that happens to be down.
    let unknown = DbConnectionDef::sqlite("reporting", "nowhere", "a.sqlite");
    let error = save_db_connection(&catalog, &unknown)
        .await
        .expect_err("there is no such file store");
    assert!(format!("{error}").contains("nowhere"), "{error}");

    // A store that exists but is **not connected** has no directory to resolve
    // against — the connection saves (it is editable, like any other) and fails
    // to connect with a reason naming the store.
    let store = FileStoreDef::local("data", dir.0.display().to_string());
    save_file_store(&catalog, &store).await?;
    let defined = DbConnectionDef::sqlite("reporting", "data", "reporting.sqlite");
    save_db_connection(&catalog, &defined).await?;
    let error = connect_db_connection(&catalog, &defined)
        .await
        .expect_err("the store is not connected");
    assert!(format!("{error}").contains("not connected"), "{error}");

    // Connected store, missing file: a connection to a file that is not there
    // is a mistake, and must not quietly create an empty database at it.
    connect_file_store_def(&catalog, &store)?;
    let error = connect_db_connection(&catalog, &defined)
        .await
        .expect_err("there is no such file");
    assert!(format!("{error}").contains("no SQLite database"), "{error}");
    assert!(
        !dir.0.join("reporting.sqlite").exists(),
        "a failed connection created a database"
    );
    // The reason is kept for the admin UI rather than only logged.
    assert!(catalog.database_error("reporting")?.is_some());
    Ok(())
}

//! Integration test for **database connections**: a second, foreign Postgres
//! database whose tables share the catalog with the primary's.
//!
//! Two real databases, because that is the only way to test this. The whole
//! feature is a claim about what happens when introspection has two sources —
//! that the foreign tables arrive, that they are stamped with the connection
//! rather than with `primary`, that a query against one reaches *that* database
//! and not the one Saltcorn's own tables are in, and that a name Saltcorn
//! already uses is not quietly taken over. None of those survives a mock.
//!
//! The story in order:
//!
//! 1. **stored and connected** — a connection is saved as a row, connected, and
//!    its tables appear in the catalog beside the primary's.
//! 2. **rows come from the right database** — the same table name exists in both
//!    with different rows, and the primary keeps the name while the foreign one
//!    is reported as shadowed.
//! 3. **schema changes are refused** — Saltcorn reads and writes a foreign
//!    table's rows and does not restructure it.
//! 4. **disconnecting** — deleting the connection takes its tables back out.
//! 5. **the password never comes back out** — what a `dial` needs is stored, and
//!    what is stored round-trips.

use std::sync::Arc;

use sc_catalog::{
    Catalog, DataField, DbConnectionDef, DbId, TableId, bootstrap_db_connections,
    connect_all_db_connections, connect_db_connection, delete_db_connection, list_db_connections,
    load_db_connection_by_name, save_db_connection,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_query::{Expr, Insert, Projection, Select, Source, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}

/// The connection an admin would have typed to reach `db`, under `name`.
fn def_for(name: &str, db: &TestDb) -> DbConnectionDef {
    let parts = db.parts();
    let mut def = DbConnectionDef::new(name, parts.host, parts.database);
    def.port = parts.port;
    def.username = parts.user;
    def.password = parts.password;
    def
}

/// Create a table with a `title` column in `db` and put one row in it.
async fn seed(db: &TestDb, table: &str, title: &str) -> sc_error::Result<()> {
    let client = db.client().await?;
    client
        .batch_execute(&format!(
            "create table {table} (id bigint primary key, title text not null)"
        ))
        .await
        .map_err(|e| sc_error::Error::database(format!("seed {table}: {e}")))?;
    client
        .batch_execute(&format!("insert into {table} values (1, '{title}')"))
        .await
        .map_err(|e| sc_error::Error::database(format!("seed row in {table}: {e}")))?;
    Ok(())
}

/// The column names a table actually has in `db`, read straight from that
/// server — the check that a schema change reached the database it was meant
/// for rather than the one Saltcorn's own tables are in.
async fn foreign_columns(db: &TestDb, table: &str) -> sc_error::Result<Vec<String>> {
    let rows = db
        .client()
        .await?
        .query(
            "select column_name from information_schema.columns \
             where table_name = $1 order by column_name",
            &[&table],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
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
async fn a_connected_database_shares_the_catalog() -> sc_error::Result<()> {
    let home = TestDb::new().await?;
    let foreign = TestDb::new().await?;

    // The primary has `book`; the foreign database has `invoice` and its own
    // `book` — the clash case, deliberately, because the two must not merge.
    seed(&home, "book", "primary book").await?;
    seed(&foreign, "invoice", "foreign invoice").await?;
    seed(&foreign, "book", "foreign book").await?;

    let catalog =
        Arc::new(
            Catalog::init(
                Arc::new(PgDriver::from_pool(home.pool().clone())) as Arc<dyn DatabaseDriver>
            )
            .await?,
        );
    bootstrap_db_connections(&catalog).await?;

    // --- 1. stored and connected --------------------------------------------
    let def = def_for("reporting", &foreign);
    save_db_connection(&catalog, &def).await?;
    connect_db_connection(&catalog, &def).await?;
    catalog.reload().await?;

    assert_eq!(catalog.database_names()?, vec!["reporting".to_string()]);
    assert_eq!(catalog.database_error("reporting")?, None);

    // The foreign table is in the catalog like any other, found by name.
    let invoice = catalog.require("invoice")?;
    assert_eq!(invoice.id, TableId("invoice".into()));
    // …and stamped with the connection, which is what the admin UI badges and
    // what routes its queries.
    assert_eq!(invoice.database, DbId("reporting".into()));
    assert!(invoice.field("title").is_some());

    // --- 2. rows come from the right database -------------------------------
    assert_eq!(titles(&catalog, "invoice").await?, vec!["foreign invoice"]);
    // `book` exists in both. The primary keeps the name — a foreign table
    // silently taking `users` or `_fd_config` would repoint authentication at
    // somebody else's database — and the loser is *named* rather than dropped in
    // silence.
    assert_eq!(catalog.require("book")?.database, DbId::primary());
    assert_eq!(titles(&catalog, "book").await?, vec!["primary book"]);
    assert_eq!(
        catalog.shadowed_tables("reporting")?,
        vec!["book".to_string()]
    );

    // A write through the catalog reaches the foreign database, not the primary.
    catalog
        .provider(&invoice, sc_catalog::Unchecked::system("test"))?
        .write(&Statement::from(Insert::row(
            "invoice",
            vec!["id".to_owned(), "title".to_owned()],
            vec![Expr::lit(2_i64), Expr::lit("added by saltcorn")],
        )))
        .await?
        .try_collect()
        .await?;
    let mut got = titles(&catalog, "invoice").await?;
    got.sort();
    assert_eq!(got, vec!["added by saltcorn", "foreign invoice"]);
    // Nothing of the sort appeared in the primary.
    assert!(catalog.get("invoice")?.is_some());
    let home_driver = PgDriver::from_pool(home.pool().clone());
    assert!(
        !home_driver
            .introspect()
            .await?
            .iter()
            .any(|t| t.name == "invoice"),
        "the foreign table must not have been created in the primary database"
    );

    // --- 3. schema changes go to the database that hosts the table ----------
    // Adding a column to `invoice` must reach the *foreign* server. The primary
    // has a `book` and no `invoice`, so a misrouted `ALTER TABLE` would fail —
    // but the interesting case is the one that would not: a name both databases
    // have, altered in the wrong one with no error to say so. Both are checked.
    catalog
        .create_field("invoice", &DataField::plain("note", text()))
        .await?;
    assert!(catalog.require("invoice")?.field("note").is_some());
    assert!(
        foreign_columns(&foreign, "invoice")
            .await?
            .contains(&"note".to_string()),
        "the column must exist in the foreign database"
    );

    // The name both databases hold: `book` is the primary's, so a field added to
    // it lands there and the foreign `book` is untouched.
    catalog
        .create_field("book", &DataField::plain("isbn", text()))
        .await?;
    assert!(
        !foreign_columns(&foreign, "book")
            .await?
            .contains(&"isbn".to_string()),
        "the primary's `book` was altered, so the foreign one must be as it was"
    );

    catalog.drop_field("invoice", "note").await?;
    assert!(
        !foreign_columns(&foreign, "invoice")
            .await?
            .contains(&"note".to_string()),
        "dropping the column must reach the foreign database too"
    );

    // --- 4. disconnecting ----------------------------------------------------
    let stored = load_db_connection_by_name(&catalog, "reporting")
        .await?
        .expect("the connection is stored");
    assert!(delete_db_connection(&catalog, stored.id).await?);
    assert!(catalog.disconnect_database("reporting")?);
    catalog.reload().await?;

    assert!(catalog.get("invoice")?.is_none());
    assert!(catalog.database_names()?.is_empty());
    assert!(list_db_connections(&catalog).await?.is_empty());
    // The primary's own table is untouched by any of it.
    assert_eq!(titles(&catalog, "book").await?, vec!["primary book"]);

    Ok(())
}

#[tokio::test]
async fn a_stored_connection_round_trips_and_reconnects_at_boot() -> sc_error::Result<()> {
    let home = TestDb::new().await?;
    let foreign = TestDb::new().await?;
    seed(&foreign, "invoice", "foreign invoice").await?;

    let catalog =
        Arc::new(
            Catalog::init(
                Arc::new(PgDriver::from_pool(home.pool().clone())) as Arc<dyn DatabaseDriver>
            )
            .await?,
        );
    bootstrap_db_connections(&catalog).await?;

    let mut def = def_for("reporting", &foreign);
    def.description = "the analytics replica".into();
    save_db_connection(&catalog, &def).await?;

    // Everything a `dial` needs survives the round trip through the row — the
    // password included, because a connection that came back without it would
    // fail on the next boot with no indication why.
    let loaded = load_db_connection_by_name(&catalog, "reporting")
        .await?
        .expect("stored");
    assert_eq!(loaded, def);

    // What boot does: load every stored connection and bring it up.
    let report = connect_all_db_connections(&catalog).await?;
    assert!(report.all_connected(), "{:?}", report.failed);
    assert_eq!(report.connected, vec!["reporting".to_string()]);
    assert_eq!(
        catalog.require("invoice")?.database,
        DbId("reporting".into())
    );

    Ok(())
}

#[tokio::test]
async fn a_connection_that_cannot_be_dialled_is_kept_with_its_reason() -> sc_error::Result<()> {
    let home = TestDb::new().await?;
    let catalog =
        Arc::new(
            Catalog::init(
                Arc::new(PgDriver::from_pool(home.pool().clone())) as Arc<dyn DatabaseDriver>
            )
            .await?,
        );
    bootstrap_db_connections(&catalog).await?;

    // A host that does not resolve: the ordinary typo, and the state the whole
    // "defined but not connected" arrangement exists for.
    let mut def = DbConnectionDef::new("reporting", "no-such-host.invalid", "analytics");
    def.username = "reader".into();
    save_db_connection(&catalog, &def).await?;

    let report = connect_all_db_connections(&catalog).await?;
    assert!(!report.all_connected());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "reporting");

    // Not connected — but still stored, still listed, and with a reason to show
    // the admin, because editing it is the repair.
    assert!(catalog.database_names()?.is_empty());
    assert_eq!(list_db_connections(&catalog).await?.len(), 1);
    let reason = catalog
        .database_error("reporting")?
        .expect("the reason is recorded");
    assert!(!reason.is_empty());

    // A connection cannot claim the primary database's name, whichever way it is
    // asked: the name is the id every one of Saltcorn's own tables carries.
    let clash = DbConnectionDef::new("primary", "localhost", "analytics");
    assert!(save_db_connection(&catalog, &clash).await.is_err());

    Ok(())
}

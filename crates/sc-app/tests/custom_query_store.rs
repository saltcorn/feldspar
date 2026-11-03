//! Custom SQL queries as part of an **application** — saved, described, typed
//! and projected (TODO "API improvements" Phase 4).
//!
//! `sc-api`'s `custom_queries` suite covers what a query *does* when it is
//! called. This one covers the half that belongs to the application it lives in:
//!
//! - a query is stored in the API row's own configuration and comes back intact;
//! - **it is prepared on save**, so the stored result columns are never older
//!   than the stored SQL, and one that will not prepare never becomes a row at
//!   all — carrying Postgres's own message to whoever typed it;
//! - and the endpoint set an application projects, and therefore the typed
//!   client generated from it, has the query's method on it.
//!
//! The last one is the point of the whole feature: an admin adds a query and the
//! app's client gains a typed method for it, with no hand-written `fetch`
//! anywhere near it (§13.1).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::Arc;

use sc_api::{CustomParam, CustomQuery, Method, ValueType};
use sc_app::{
    ApiConfig, Application, FrameworkRef, app_client, app_endpoints, bootstrap, load_application,
    save_application,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;

const FIXTURE: &str = "
    CREATE TABLE books (id bigint primary key, title text not null, copies bigint not null);
    INSERT INTO books VALUES (1, 'Emma', 3), (2, 'Brand', 2);
";

async fn catalog(db: &TestDb) -> Result<Catalog> {
    db.client()
        .await?
        .batch_execute(FIXTURE)
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap(&cat).await?;
    Ok(cat)
}

/// The app, with a REST API carrying `queries`.
fn library(queries: &[CustomQuery]) -> Application {
    let mut config = sc_types::Attrs::new();
    sc_api::set_custom_queries(&mut config, queries).expect("storable");
    Application::new(
        "Library",
        "library",
        FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "sh build.sh"),
    )
    .with_table("books".to_owned())
    .with_api(ApiConfig::new(sc_api::REST_PROVIDER, "/api").with_config(config))
}

fn stocked() -> CustomQuery {
    CustomQuery::new(
        "wellStocked",
        Method::Get,
        "/reports/well-stocked",
        "SELECT title, copies FROM books WHERE copies >= :least ORDER BY copies DESC",
    )
    .params([CustomParam::new("least", ValueType::Int)])
    .description("Titles held in at least this many copies")
    .min_role(Role::of(40))
}

#[tokio::test]
async fn a_saved_query_is_described_by_the_database_and_typed_in_the_client() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    let app = library(&[stocked()]);
    // What was saved carried no result columns at all…
    assert!(
        sc_api::custom_queries(&app.apis[0].config)?[0]
            .columns
            .is_empty()
    );
    save_application(&cat, &app).await?;

    // …and what came back carries the ones Postgres reported, because saving
    // prepares. The admin declares the parameters; the database types the result.
    let loaded = load_application(&cat, app.id).await?.expect("saved");
    let stored = sc_api::custom_queries(&loaded.apis[0].config)?;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].name, "wellStocked");
    assert_eq!(stored[0].min_role, Role::of(40));
    assert_eq!(stored[0].params[0].name, "least");
    let columns: Vec<(&str, ValueType)> = stored[0]
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.ty))
        .collect();
    assert_eq!(
        columns,
        vec![("title", ValueType::Text), ("copies", ValueType::Int)]
    );

    // The application's endpoint set has it, at its own path and role floor…
    let endpoints = app_endpoints(&loaded, &cat)?;
    let ep = endpoints.find("wellStocked").expect("projected");
    assert_eq!(ep.method, Method::Get);
    assert_eq!(ep.path.pattern(), "/api/reports/well-stocked");
    assert_eq!(ep.auth, sc_api::AuthRequirement::MinRole(Role::of(40)));

    // …and the generated client has a typed method for it, which is the whole
    // point: nobody writes a `fetch` beside a generated client.
    let client = app_client(&loaded, &cat)?;
    assert!(client.contains("wellStocked("), "{client}");
    assert!(client.contains("least"), "{client}");
    assert!(client.contains("/api/reports/well-stocked"), "{client}");
    Ok(())
}

#[tokio::test]
async fn a_query_that_will_not_prepare_cannot_be_saved() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    let mut broken = stocked();
    broken.code = "SELECT titel FROM books WHERE copies >= :least".into();
    let err = save_application(&cat, &library(&[broken]))
        .await
        .expect_err("this SQL does not prepare");
    // Postgres's own message, which is the part that lands its author on the typo.
    assert!(err.to_string().contains("titel"), "{err}");

    // Nothing was written: a refused query never becomes an application.
    assert!(sc_app::list_applications(&cat).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn the_stored_columns_are_never_older_than_the_stored_sql() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    let app = library(&[stocked()]);
    save_application(&cat, &app).await?;

    // Edit the SQL to return a different shape and save again. The columns are
    // re-described in the same statement that stores the new SQL, so there is no
    // window in which they describe the old one.
    let mut edited = stocked();
    edited.code =
        "SELECT title AS name, copies * 2 AS doubled FROM books WHERE copies >= :least".into();
    let mut app = app;
    let mut config = sc_types::Attrs::new();
    sc_api::set_custom_queries(&mut config, &[edited])?;
    app.apis[0].config = config;
    save_application(&cat, &app).await?;

    let loaded = load_application(&cat, app.id).await?.expect("saved");
    let stored = sc_api::custom_queries(&loaded.apis[0].config)?;
    let columns: Vec<&str> = stored[0].columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, vec!["name", "doubled"]);
    Ok(())
}

#[tokio::test]
async fn a_query_that_collides_with_the_apps_tables_is_refused_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    // The path the `books` table's own routes already answer. `resolve` takes
    // the first matching endpoint and the tables are registered first, so this
    // would be an endpoint in the client that the server never reaches.
    let mut shadowed = stocked();
    shadowed.path = "/books/well-stocked".into();
    let err = save_application(&cat, &library(&[shadowed]))
        .await
        .expect_err("a shadowed path is refused");
    assert!(err.to_string().contains("books"), "{err}");

    // …and the name of one of that table's own client methods.
    let mut renamed = stocked();
    renamed.name = "listBooks".into();
    let err = save_application(&cat, &library(&[renamed]))
        .await
        .expect_err("a colliding name is refused");
    assert!(err.to_string().contains("listBooks"), "{err}");

    assert!(sc_app::list_applications(&cat).await?.is_empty());
    Ok(())
}

/// The app's API configuration replaced by `queries`.
fn with_queries(app: &mut Application, queries: &[CustomQuery]) {
    let mut config = sc_types::Attrs::new();
    sc_api::set_custom_queries(&mut config, queries).expect("storable");
    app.apis[0].config = config;
}

fn stored_columns(app: &Application, name: &str) -> Vec<String> {
    sc_api::custom_queries(&app.apis[0].config)
        .expect("stored")
        .into_iter()
        .find(|q| q.name == name)
        .expect("stored query")
        .columns
        .into_iter()
        .map(|c| c.name)
        .collect()
}

/// User report: after a table rename, every save was refused while any query
/// still named the old table, so the queries could not be fixed one at a time.
/// Only a query the save changes is held to preparing; an unchanged one that no
/// longer prepares keeps the columns it was stored with.
#[tokio::test]
async fn after_a_rename_the_queries_can_be_fixed_one_save_at_a_time() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = Arc::new(catalog(&db).await?);
    db.on_teardown(cat.teardown_hook());

    let mut counted = stocked();
    counted.name = "copiesHeld".into();
    counted.path = "/reports/copies".into();
    counted.code = "SELECT sum(copies) AS total FROM books WHERE copies >= :least".into();
    let mut app = library(&[stocked(), counted.clone()]);
    app = save_application(&cat, &app).await?;

    db.client()
        .await?
        .batch_execute("ALTER TABLE books RENAME TO titles")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    cat.reload().await?;
    app.tables = vec!["titles".to_owned()];
    app.api_tables = app.tables.clone();

    // A save that changes nothing about the queries is not refused…
    let saved = save_application(&cat, &app).await?;
    assert_eq!(stored_columns(&saved, "copiesHeld"), vec!["total"]);

    // …nor one that fixes the first query while the second still names `books`.
    let mut fixed = stocked();
    fixed.code = "SELECT title, copies FROM titles WHERE copies >= :least".into();
    with_queries(&mut app, &[fixed.clone(), counted.clone()]);
    let saved = save_application(&cat, &app).await?;
    assert_eq!(
        stored_columns(&saved, "wellStocked"),
        vec!["title", "copies"]
    );
    // The unchanged, broken one kept the columns it was stored with.
    assert_eq!(stored_columns(&saved, "copiesHeld"), vec!["total"]);

    // A query the save changes is still held to preparing: edited but still
    // naming the old table, it is refused with Postgres's message…
    let mut still_broken = counted.clone();
    still_broken.code = "SELECT sum(copies) AS held FROM books WHERE copies >= :least".into();
    with_queries(&mut app, &[fixed.clone(), still_broken]);
    let err = save_application(&cat, &app)
        .await
        .expect_err("an edited query that does not prepare is refused");
    assert!(err.to_string().contains("books"), "{err}");

    // …and so is a new one.
    let mut added = counted.clone();
    added.name = "another".into();
    added.path = "/reports/another".into();
    with_queries(&mut app, &[fixed.clone(), counted.clone(), added]);
    assert!(save_application(&cat, &app).await.is_err());

    // Fixing the second query is a save of its own, and it is described again.
    let mut counted_fixed = counted;
    counted_fixed.code = "SELECT sum(copies) AS held FROM titles WHERE copies >= :least".into();
    with_queries(&mut app, &[fixed, counted_fixed]);
    let saved = save_application(&cat, &app).await?;
    assert_eq!(stored_columns(&saved, "copiesHeld"), vec!["held"]);
    Ok(())
}

/// An unchanged query that still prepares is described again on save, so its
/// stored columns follow the schema (a column added under a `SELECT *`).
#[tokio::test]
async fn an_unchanged_query_is_described_again_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = Arc::new(catalog(&db).await?);
    db.on_teardown(cat.teardown_hook());

    let mut everything = stocked();
    everything.code = "SELECT * FROM books WHERE copies >= :least".into();
    let app = save_application(&cat, &library(&[everything])).await?;
    assert_eq!(
        stored_columns(&app, "wellStocked"),
        vec!["id", "title", "copies"]
    );

    db.client()
        .await?
        .batch_execute("ALTER TABLE books ADD COLUMN shelf text")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let saved = save_application(&cat, &app).await?;
    assert_eq!(
        stored_columns(&saved, "wellStocked"),
        vec!["id", "title", "copies", "shelf"]
    );
    Ok(())
}

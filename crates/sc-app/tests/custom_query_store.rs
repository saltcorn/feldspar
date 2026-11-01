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

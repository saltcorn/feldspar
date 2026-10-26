//! Full-text search on a read, on both backends (TODO-agent-skills AS1).
//!
//! The same assertions run against Postgres (a GIN index over `to_tsvector`)
//! and SQLite (an FTS5 table kept by triggers): what matches, what ranks first,
//! what the operators of a phrase mean, and that a field the caller may not
//! read — a secret — is never matched. Then what is particular to each: on
//! Postgres, that the search really is answered by the index; on SQLite, that
//! the index follows writes and survives its table's text fields changing.

use std::sync::Arc;

use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation};
use sc_api::{query_string, rows};
use sc_auth::ROLE_ADMIN;
use sc_catalog::{
    Catalog, ConstraintKind, DataField, DataFieldKind, SecretKey, TableConstraint,
    bootstrap_field_meta, bootstrap_table_meta,
};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::SqlDialect;
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};

/// `posts(id, title, body, note secret)` with four rows, and — when `indexed` —
/// a full-text constraint in English.
async fn posts(driver: Arc<dyn DatabaseDriver>, indexed: bool) -> Result<Arc<Catalog>> {
    let catalog = Arc::new(Catalog::init(driver).await?);
    catalog.set_secret_key(SecretKey::generate()?);
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    catalog
        .create_table(
            "posts",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(ColumnGenerator::Identity),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
                DataField::plain("body", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    catalog.reload().await?;
    apply(
        &catalog,
        Operation::AddField {
            table: "posts".into(),
            field: FieldSpec {
                name: "note".into(),
                kind: DataFieldKind::Secret,
                ..FieldSpec::default()
            },
        },
    )
    .await?;
    if indexed {
        apply(
            &catalog,
            Operation::AddConstraint {
                table: "posts".into(),
                given_name: String::new(),
                constraint: TableConstraint::new(
                    "",
                    ConstraintKind::FullTextSearch {
                        language: "english".into(),
                    },
                ),
            },
        )
        .await?;
    }
    let table = catalog.require("posts")?;
    for row in [
        json!({ "title": "Rust in production", "body": "Why we moved to rust: rust, rust and more rust." }),
        json!({ "title": "Java and Rust", "body": "Garbage collection against ownership." }),
        json!({ "title": "Gardening", "body": "Running tomatoes up a cane.", "note": "rust" }),
        json!({ "title": "Cooking", "body": "A segmentation fault in the kitchen." }),
    ] {
        rows::create_row(&catalog, &table, &row).await?;
    }
    Ok(catalog)
}

async fn apply(catalog: &Catalog, op: Operation) -> Result<()> {
    schema_edit::apply(catalog, &[op], &ApplyOptions::default()).await?;
    Ok(())
}

/// The titles a `GET ?search=…` (plus any other pairs) answers, in order.
async fn titles(catalog: &Catalog, pairs: &[(&str, &str)]) -> Result<Vec<String>> {
    let table = catalog.require("posts")?;
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let query = query_string::row_query(&table, &pairs, 50)?;
    let found = sc_api::read_rows_as(catalog, &table, &query, ROLE_ADMIN, None, None).await?;
    Ok(found
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("title").and_then(Json::as_str).map(str::to_owned))
        .collect())
}

/// What both backends must agree on.
async fn the_common_contract(catalog: &Catalog) -> Result<()> {
    // Ranked: the post that says it four times comes before the one that says
    // it once — and the secret that says it is not read at all.
    assert_eq!(
        titles(catalog, &[("search", "rust")]).await?,
        ["Rust in production", "Java and Rust"]
    );
    // An exclusion, a quoted phrase and an alternative.
    assert_eq!(
        titles(catalog, &[("search", "rust -java")]).await?,
        ["Rust in production"]
    );
    let mut either = titles(
        catalog,
        &[("search", r#""garbage collection" or tomatoes"#)],
    )
    .await?;
    either.sort();
    assert_eq!(either, ["Gardening", "Java and Rust"]);
    // Only an exclusion: every row that does not say it.
    let mut not_rust = titles(catalog, &[("search", "-rust")]).await?;
    not_rust.sort();
    assert_eq!(not_rust, ["Cooking", "Gardening"]);
    // An order replaces relevance, and can name it.
    assert_eq!(
        titles(catalog, &[("search", "rust"), ("order", "title")]).await?,
        ["Java and Rust", "Rust in production"]
    );
    assert_eq!(
        titles(catalog, &[("search", "rust"), ("order", "relevance.asc")]).await?,
        ["Java and Rust", "Rust in production"]
    );
    // A search combines with a filter.
    assert_eq!(
        titles(catalog, &[("search", "rust"), ("title", "ilike.java%")]).await?,
        ["Java and Rust"]
    );

    // The fields a caller names are the only ones matched — the shape an
    // agent's allow-list takes.
    let table = catalog.require("posts")?;
    let only_title = sc_api::search::text_search(&table, "ownership", Some(&["title".into()]))?;
    let query = rows::RowQuery::new().searching(only_title);
    let found = sc_api::read_rows_as(catalog, &table, &query, ROLE_ADMIN, None, None).await?;
    assert_eq!(found, json!([]));
    let body = sc_api::search::text_search(&table, "ownership", Some(&["body".into()]))?;
    let found = sc_api::read_rows_as(
        catalog,
        &table,
        &rows::RowQuery::new().searching(body),
        ROLE_ADMIN,
        None,
        None,
    )
    .await?;
    assert_eq!(found.as_array().map(Vec::len), Some(1));
    // A secret cannot be named either.
    let err = sc_api::search::text_search(&table, "rust", Some(&["note".into()]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("is a secret"), "{err}");
    // …and nothing to look for is refused rather than matching nothing.
    assert!(titles(catalog, &[("search", r#" "" or "#)]).await.is_err());
    Ok(())
}

async fn pg_driver(db: &TestDb) -> Arc<dyn DatabaseDriver> {
    Arc::new(PgDriver::from_pool(db.pool().clone()))
}

#[tokio::test]
async fn postgres_searches_ranks_and_uses_the_index() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = posts(pg_driver(&db).await, true).await?;
    the_common_contract(&catalog).await?;
    // English stemming: *running* is *run*.
    assert_eq!(titles(&catalog, &[("search", "run")]).await?, ["Gardening"]);

    // The statement a search renders is answered by the constraint's index —
    // the document is spelled once, for both, and this is what that is for.
    let table = catalog.require("posts")?;
    let search = sc_api::search::text_search(&table, "rust", None)?;
    let select = sc_query::Select::from(sc_query::Source::table("posts"))
        .columns(vec![sc_query::Projection::expr(sc_query::Expr::col("id"))])
        .filter(sc_query::Expr::TextMatch(Box::new(search)));
    let (sql, _) = sc_db_postgres::PgDialect.render(&select.into())?;
    let client = db.client().await?;
    let db_err = |e: tokio_postgres::Error| sc_error::Error::database(e.to_string());
    client
        .batch_execute("SET enable_seqscan = off")
        .await
        .map_err(db_err)?;
    let plan: Vec<String> = client
        .query(&format!("EXPLAIN {sql}"), &[&"rust"])
        .await
        .map_err(db_err)?
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        plan.iter().any(|line| line.contains("sc_fts_posts")),
        "{}",
        plan.join("\n")
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_searches_through_an_fts5_table_that_follows_the_rows() -> Result<()> {
    let catalog = posts(Arc::new(SqliteDriver::open_in_memory()?), true).await?;
    the_common_contract(&catalog).await?;
    assert_eq!(titles(&catalog, &[("search", "run")]).await?, ["Gardening"]);
    // Words, not substrings: this is the index answering, not a scan.
    assert!(titles(&catalog, &[("search", "arden")]).await?.is_empty());

    // The FTS5 table is the table's constraint, not a table of its own.
    let table = catalog.require("posts")?;
    assert!(catalog.get("sc_fts_posts")?.is_none());
    assert!(table.constraints.iter().any(|c| c.name == "sc_fts_posts"
        && c.kind
            == ConstraintKind::FullTextSearch {
                language: "english".into()
            }));

    // Writes reach the index.
    let cooking = titles(&catalog, &[("search", "kitchen")]).await?;
    assert_eq!(cooking, ["Cooking"]);
    let table = catalog.require("posts")?;
    rows::update_row(&catalog, &table, "4", &json!({ "body": "Baking bread." })).await?;
    assert!(titles(&catalog, &[("search", "kitchen")]).await?.is_empty());
    assert_eq!(titles(&catalog, &[("search", "bread")]).await?, ["Cooking"]);
    rows::delete_row(&catalog, &table, "4").await?;
    assert!(titles(&catalog, &[("search", "bread")]).await?.is_empty());

    // A text field added is searchable; one dropped goes — and dropping it is
    // possible at all, though the index's triggers named it.
    apply(
        &catalog,
        Operation::AddField {
            table: "posts".into(),
            field: FieldSpec {
                name: "summary".into(),
                type_name: "text".into(),
                ..FieldSpec::default()
            },
        },
    )
    .await?;
    let table = catalog.require("posts")?;
    rows::update_row(
        &catalog,
        &table,
        "3",
        &json!({ "summary": "Allotment diary" }),
    )
    .await?;
    assert_eq!(
        titles(&catalog, &[("search", "allotment")]).await?,
        ["Gardening"]
    );
    apply(
        &catalog,
        Operation::DropField {
            table: "posts".into(),
            field: "body".into(),
        },
    )
    .await?;
    assert!(
        titles(&catalog, &[("search", "tomatoes")])
            .await?
            .is_empty()
    );
    assert_eq!(
        titles(&catalog, &[("search", "gardening")]).await?,
        ["Gardening"]
    );
    Ok(())
}

#[tokio::test]
async fn a_table_without_the_index_is_scanned_only_while_it_is_small() -> Result<()> {
    let catalog = posts(Arc::new(SqliteDriver::open_in_memory()?), false).await?;
    // A scan matches words anywhere and does not stem, but it answers — and
    // still never reads the secret.
    assert_eq!(
        titles(&catalog, &[("search", "rust -java")]).await?,
        ["Rust in production"]
    );
    let mut rust = titles(&catalog, &[("search", "rust")]).await?;
    rust.sort();
    assert_eq!(rust, ["Java and Rust", "Rust in production"]);

    let mut tx = catalog.primary().begin().await?;
    tx.batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000) \
         INSERT INTO posts (title) SELECT 'filler ' || i FROM n",
    )
    .await?;
    tx.commit().await?;
    let err = titles(&catalog, &[("search", "rust")])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("has no full-text search index"), "{err}");
    assert!(err.contains("full_text_search"), "{err}");
    Ok(())
}

#[tokio::test]
async fn rest_and_graphql_take_a_search() -> Result<()> {
    use sc_api::{ApiProvider, ApiRequest, GraphqlProvider, Method, RestProvider};

    let catalog = posts(Arc::new(SqliteDriver::open_in_memory()?), true).await?;
    let table = catalog.require("posts")?;
    let user = sc_auth::User::new(uuid::Uuid::new_v4(), ROLE_ADMIN)?;
    let title_list = |body: &Json| -> Vec<String> {
        body.as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["title"].as_str().map(str::to_owned))
            .collect()
    };

    let rest = RestProvider::project("/api", std::slice::from_ref(&table));
    let list = rest
        .handle(
            ApiRequest::get("/api/posts").query("search", "rust -java"),
            &catalog,
            Some(&user),
        )
        .await?;
    assert_eq!(list.status, 200, "{}", list.body);
    assert_eq!(title_list(&list.body), ["Rust in production"]);
    // `relevance` means something only beside a search.
    let refused = rest
        .handle(
            ApiRequest::get("/api/posts").query("order", "relevance"),
            &catalog,
            Some(&user),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("has no `search`"), "{refused}");

    let graphql = GraphqlProvider::project("/graphql", std::slice::from_ref(&table))?;
    let read = graphql
        .handle(
            ApiRequest::new(Method::Post, "/graphql")
                .body(json!({ "query": r#"{ posts(search: "rust") { title } }"# })),
            &catalog,
            Some(&user),
        )
        .await?;
    assert_eq!(
        title_list(&read.body["data"]["posts"]),
        ["Rust in production", "Java and Rust"],
        "{}",
        read.body
    );
    Ok(())
}

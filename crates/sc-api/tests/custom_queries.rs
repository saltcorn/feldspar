//! Custom SQL queries against a real Postgres (TODO "API improvements" Phase 4).
//!
//! The model's own rules — the name, the path, the one-statement rule, the
//! declared-versus-used parameters — are unit-tested in `sc-api`'s
//! `rest::custom`, and the `:name` scanner in `sc-query`. What needs a database
//! is everything those cannot prove:
//!
//! - **The query returns what the SQL returns.** Two parameters, bound, against
//!   the same statement run by hand.
//! - **An argument is a value.** `'; DROP TABLE books; --` supplied as an
//!   argument answers with no rows and leaves the table standing, because it
//!   never reaches the statement's *text*.
//! - **Its authority is its own, and it is real.** A caller below the query's
//!   `min_role` is refused; a query over an RLS-protected table sees only the
//!   caller's rows, because it runs in the same caller-context transaction a row
//!   operation does.
//! - **A `GET` cannot write.** The method is the admin's choice; the read-only
//!   transaction is not, and it is the database that refuses.
//! - **The database types the result**, and a statement that will not prepare
//!   comes back carrying Postgres's own message.
//!
//! Each authorization assertion here would *fail open* if the rule it names were
//! dropped.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::Arc;

use sc_api::{
    ApiProvider, ApiRequest, ApiResponse, CustomParam, CustomQuery, Method, RestProvider,
    ValueType, describe_custom_query,
};
use sc_auth::User;
use sc_catalog::{Catalog, TableMeta, bootstrap_table_meta, enable_rls, save_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::Value;
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

/// The milestone's own example, small enough to check by eye.
const FIXTURE: &str = "
    CREATE TABLE books (
        id bigint primary key,
        title text not null,
        published date,
        owner text,
        copies bigint not null default 0);
    INSERT INTO books VALUES
        (1, 'Emma',       '2019-03-01', 'ada@example.com', 3),
        (2, 'Persuasion', '2021-06-01', 'ada@example.com', 5),
        (3, 'Brand',      '2022-01-15', 'bo@example.com',  2),
        (4, 'Peer Gynt',  '2023-09-30', 'bo@example.com',  7);
";

async fn setup(db: &TestDb) -> Result<Arc<Catalog>> {
    db.client()
        .await?
        .batch_execute(FIXTURE)
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    Ok(Arc::new(catalog))
}

/// The REST provider over `books`, with `queries` projected on top — each one
/// described first, exactly as saving the application does.
async fn provider(cat: &Catalog, queries: Vec<CustomQuery>) -> Result<RestProvider> {
    let mut described = Vec::new();
    for mut query in queries {
        query.columns = describe_custom_query(cat, &query).await?;
        described.push(query);
    }
    RestProvider::project("/api", &[cat.require("books")?]).with_queries(described)
}

fn admin() -> Result<User> {
    Ok(User::new(uuid::Uuid::new_v4(), Role::ADMIN))
}

/// A caller at the public role, identified by the email an ownership policy
/// reads.
fn caller(email: &str, role: Role) -> Result<User> {
    let mut user = User::new(uuid::Uuid::new_v4(), role);
    user.extra
        .insert("email".to_owned(), Value::Text(email.to_owned()));
    Ok(user)
}

/// A `GET` call with its arguments in the query string.
async fn call(
    api: &RestProvider,
    cat: &Arc<Catalog>,
    user: Option<&User>,
    path: &str,
    args: &[(&str, &str)],
) -> Result<ApiResponse> {
    let mut req = ApiRequest::get(path);
    for (key, value) in args {
        req = req.query(*key, *value);
    }
    api.handle(req, cat, user).await
}

/// The books-in-a-window query the suite leans on: two parameters, both bound.
fn window() -> CustomQuery {
    CustomQuery::new(
        "booksInWindow",
        Method::Get,
        "/reports/window",
        "SELECT title, copies FROM books \
         WHERE published >= :from AND published < :until ORDER BY published",
    )
    .params([
        CustomParam::new("from", ValueType::Date),
        CustomParam::new("until", ValueType::Date),
    ])
    .min_role(Role::ADMIN)
}

#[tokio::test]
async fn a_two_parameter_query_returns_what_the_sql_returns() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let api = provider(&cat, vec![window()]).await?;

    let resp = call(
        &api,
        &cat,
        Some(&admin()?),
        "/api/reports/window",
        &[("from", "2020-01-01"), ("until", "2023-01-01")],
    )
    .await?;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(
        resp.body,
        json!([
            { "title": "Persuasion", "copies": 5 },
            { "title": "Brand", "copies": 2 },
        ])
    );

    // …which is the same answer the statement gives run by hand.
    let by_hand = db
        .client()
        .await?
        .query(
            "SELECT title, copies FROM books WHERE published >= $1 AND published < $2 \
             ORDER BY published",
            &[
                &chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
                &chrono::NaiveDate::from_ymd_opt(2023, 1, 1).unwrap(),
            ],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let titles: Vec<String> = by_hand.iter().map(|r| r.get::<_, String>(0)).collect();
    assert_eq!(titles, vec!["Persuasion", "Brand"]);

    // The columns Postgres reported are the columns the client is typed with.
    let ep = api.endpoints().find("booksInWindow").unwrap();
    let ts = sc_api::generate_client(api.endpoints());
    assert!(ts.contains("booksInWindow("), "{ts}");
    assert!(
        format!("{:?}", ep.output).contains("title"),
        "{:?}",
        ep.output
    );
    Ok(())
}

#[tokio::test]
async fn an_argument_that_looks_like_sql_is_a_value_and_the_table_survives() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let query = CustomQuery::new(
        "booksByOwner",
        Method::Get,
        "/reports/by-owner",
        "SELECT title FROM books WHERE owner = :owner ORDER BY id",
    )
    .params([CustomParam::new("owner", ValueType::Text)]);
    let api = provider(&cat, vec![query]).await?;

    let resp = call(
        &api,
        &cat,
        Some(&admin()?),
        "/api/reports/by-owner",
        &[("owner", "ada@example.com'; DROP TABLE books; --")],
    )
    .await?;
    // A value, so it simply matches nothing.
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(resp.body, json!([]));

    // And the table is still there, with all four rows.
    let still = db
        .client()
        .await?
        .query("SELECT count(*) FROM books", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert_eq!(still[0].get::<_, i64>(0), 4);
    Ok(())
}

#[tokio::test]
async fn a_missing_or_ill_typed_argument_is_refused_naming_it() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let api = provider(&cat, vec![window()]).await?;
    let admin = admin()?;

    let err = call(
        &api,
        &cat,
        Some(&admin),
        "/api/reports/window",
        &[("from", "2020-01-01")],
    )
    .await
    .expect_err("a required argument is required");
    assert!(err.to_string().contains("until"), "{err}");
    assert!(
        matches!(err.repr(), sc_error::Repr::Invalid(_)),
        "expected a 400, got {err:?}"
    );

    // A wrongly-typed one is refused *before* the statement runs: the coercion
    // is against the declared type, not a database error read back afterwards.
    let err = call(
        &api,
        &cat,
        Some(&admin),
        "/api/reports/window",
        &[("from", "the third of never"), ("until", "2023-01-01")],
    )
    .await
    .expect_err("a date argument must be a date");
    assert!(err.to_string().contains("from"), "{err}");
    assert!(
        matches!(err.repr(), sc_error::Repr::Invalid(_)),
        "expected a 400, got {err:?}"
    );
    Ok(())
}

#[tokio::test]
async fn an_optional_argument_that_is_omitted_binds_null() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let query = CustomQuery::new(
        "searchBooks",
        Method::Get,
        "/reports/search",
        "SELECT title FROM books WHERE (:q IS NULL OR title = :q) ORDER BY id",
    )
    .params([CustomParam::new("q", ValueType::Text).optional()]);
    let api = provider(&cat, vec![query]).await?;
    let admin = admin()?;

    let all = call(&api, &cat, Some(&admin), "/api/reports/search", &[]).await?;
    assert_eq!(all.body.as_array().expect("rows").len(), 4, "{}", all.body);

    let one = call(
        &api,
        &cat,
        Some(&admin),
        "/api/reports/search",
        &[("q", "Brand")],
    )
    .await?;
    assert_eq!(one.body, json!([{ "title": "Brand" }]));
    Ok(())
}

#[tokio::test]
async fn a_caller_below_the_queries_role_floor_is_refused() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let api = provider(&cat, vec![window()]).await?;

    // The floor is the query's own — admin here — and the endpoint's auth is
    // enforced before anything runs.
    let resp = call(
        &api,
        &cat,
        Some(&caller("bo@example.com", Role::of(80))?),
        "/api/reports/window",
        &[("from", "2020-01-01"), ("until", "2023-01-01")],
    )
    .await?;
    assert_eq!(resp.status, 403, "{}", resp.body);

    // …and an anonymous caller is asked to authenticate rather than told no.
    let resp = call(
        &api,
        &cat,
        None,
        "/api/reports/window",
        &[("from", "2020-01-01"), ("until", "2023-01-01")],
    )
    .await?;
    assert_eq!(resp.status, 401, "{}", resp.body);
    Ok(())
}

#[tokio::test]
async fn a_get_query_that_writes_is_refused_by_the_read_only_transaction() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    // The method is the admin's choice and nothing infers it from the SQL — so
    // an `UPDATE` behind a `GET` is expressible, and it is the *transaction*
    // that refuses it. A cache or a crawler must not be able to cause a write.
    let query = CustomQuery::new(
        "bumpCopies",
        Method::Get,
        "/reports/bump",
        "UPDATE books SET copies = copies + 1 WHERE id = :id RETURNING copies",
    )
    .params([CustomParam::new("id", ValueType::Int)]);
    let api = provider(&cat, vec![query]).await?;

    let err = call(
        &api,
        &cat,
        Some(&admin()?),
        "/api/reports/bump",
        &[("id", "1")],
    )
    .await
    .expect_err("a GET may not write");
    assert!(
        err.to_string().contains("read-only") || err.to_string().contains("read only"),
        "{err}"
    );

    // Nothing moved.
    let after = db
        .client()
        .await?
        .query("SELECT copies FROM books WHERE id = 1", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert_eq!(after[0].get::<_, i64>(0), 3);
    Ok(())
}

#[tokio::test]
async fn a_post_query_may_write_and_commits() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let query = CustomQuery::new(
        "bumpCopies",
        Method::Post,
        "/reports/bump",
        "UPDATE books SET copies = copies + 1 WHERE id = :id RETURNING copies",
    )
    .params([CustomParam::new("id", ValueType::Int)]);
    let api = provider(&cat, vec![query]).await?;

    let resp = api
        .handle(
            ApiRequest::new(Method::Post, "/api/reports/bump").body(json!({ "id": 1 })),
            &cat,
            Some(&admin()?),
        )
        .await?;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(resp.body, json!([{ "copies": 4 }]));

    // Committed, not rolled back with the response.
    let after = db
        .client()
        .await?
        .query("SELECT copies FROM books WHERE id = 1", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert_eq!(after[0].get::<_, i64>(0), 4);
    Ok(())
}

#[tokio::test]
async fn a_query_over_an_rls_table_sees_only_the_callers_rows() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    // Raw SQL does not go through the row layer, so ownership *formulae* do not
    // apply to it — but the database's own policies do, because a custom query
    // runs in the same caller-context transaction a row operation does. This is
    // the one authorization rule that survives the escape hatch, and it is the
    // reason the escape hatch is tolerable.
    bootstrap_table_meta(&cat).await?;
    let mut books = TableMeta::new("books");
    books.set_ownership_formula(Some("owner === user.email"));
    books.set_rls_enabled(true);
    save_table_meta(&cat, &books).await?;
    enable_rls(&cat, &cat.require("books")?).await?;

    let query = CustomQuery::new(
        "titleCount",
        Method::Get,
        "/reports/count",
        "SELECT count(*) AS n FROM books WHERE copies >= :least",
    )
    .params([CustomParam::new("least", ValueType::Int)])
    // Below admin on purpose: an admin passes the policy's role floor, so a
    // test run as one would prove nothing.
    .min_role(Role::PUBLIC);
    let api = provider(&cat, vec![query]).await?;

    let count = |resp: &ApiResponse| -> i64 {
        resp.body[0]["n"].as_i64().unwrap_or_else(|| {
            panic!("expected a count, got {}", resp.body);
        })
    };

    let ada = call(
        &api,
        &cat,
        Some(&caller("ada@example.com", Role::PUBLIC)?),
        "/api/reports/count",
        &[("least", "0")],
    )
    .await?;
    assert_eq!(count(&ada), 2, "ada owns two books: {}", ada.body);

    let bo = call(
        &api,
        &cat,
        Some(&caller("bo@example.com", Role::PUBLIC)?),
        "/api/reports/count",
        &[("least", "0")],
    )
    .await?;
    assert_eq!(count(&bo), 2, "bo owns two books: {}", bo.body);

    // An admin is above the policies' role floor and sees the table.
    let all = call(
        &api,
        &cat,
        Some(&admin()?),
        "/api/reports/count",
        &[("least", "0")],
    )
    .await?;
    assert_eq!(count(&all), 4, "{}", all.body);
    Ok(())
}

#[tokio::test]
async fn the_database_types_the_result_and_refuses_sql_that_will_not_prepare() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;

    // Every column typed by what Postgres said it would produce — including the
    // ones no table has, which is exactly why the admin does not declare them.
    let typed = CustomQuery::new(
        "shape",
        Method::Get,
        "/reports/shape",
        "SELECT title, published, copies * 2 AS doubled, now() AS at FROM books",
    );
    let columns = describe_custom_query(&cat, &typed).await?;
    let named: Vec<(&str, ValueType)> = columns
        .iter()
        .map(|c| (c.name.as_str(), c.ty))
        .collect::<Vec<_>>();
    assert_eq!(
        named,
        vec![
            ("title", ValueType::Text),
            ("published", ValueType::Date),
            ("doubled", ValueType::Int),
            ("at", ValueType::Timestamp),
        ]
    );

    // A statement that will not prepare comes back carrying Postgres's own
    // message, which is the part that lands its author on the typo.
    let broken = CustomQuery::new(
        "broken",
        Method::Get,
        "/reports/broken",
        "SELECT titel FROM books",
    );
    let err = describe_custom_query(&cat, &broken)
        .await
        .expect_err("this SQL does not prepare");
    assert!(err.to_string().contains("titel"), "{err}");

    // …and describing does not run it: a query that would write has not.
    let write = CustomQuery::new(
        "wouldWrite",
        Method::Get,
        "/reports/would-write",
        "DELETE FROM books RETURNING id",
    );
    describe_custom_query(&cat, &write).await?;
    let after = db
        .client()
        .await?
        .query("SELECT count(*) FROM books", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert_eq!(after[0].get::<_, i64>(0), 4);
    Ok(())
}

#[tokio::test]
async fn two_result_columns_of_one_name_are_refused_rather_than_collapsed() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    // The response is a JSON object per row, so a duplicated name would lose a
    // column silently. The alias is the admin's to choose.
    let q = CustomQuery::new(
        "dup",
        Method::Get,
        "/reports/dup",
        "SELECT id, id FROM books",
    );
    let err = describe_custom_query(&cat, &q)
        .await
        .expect_err("two columns named `id`");
    assert!(err.to_string().contains("alias"), "{err}");
    Ok(())
}

/// A `NULL` in a result column comes back as JSON null rather than as a missing
/// property — the client is typed with every column optional, and this is why.
#[tokio::test]
async fn a_null_result_column_answers_null() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    db.client()
        .await?
        .batch_execute("INSERT INTO books VALUES (5, 'Untitled', NULL, NULL, 0)")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let q = CustomQuery::new(
        "undated",
        Method::Get,
        "/reports/undated",
        "SELECT title, published FROM books WHERE published IS NULL",
    );
    let api = provider(&cat, vec![q]).await?;
    let resp = call(&api, &cat, Some(&admin()?), "/api/reports/undated", &[]).await?;
    assert_eq!(
        resp.body,
        json!([{ "title": "Untitled", "published": Json::Null }])
    );
    Ok(())
}

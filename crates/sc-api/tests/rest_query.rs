//! The REST read query string against a real Postgres (TODO "API improvements"
//! Phase 2).
//!
//! The parser's own rules are unit-tested in `sc-api`'s `rest::query` — what
//! needs a database is everything the parser cannot prove about itself:
//!
//! - **The milestone's query answers what GraphQL answers.** `?select=…&filter&
//!   order&limit` and the equivalent GraphQL document are two syntaxes over one
//!   read layer, so they must return the same rows. Asserting it against the
//!   *other* provider rather than against a literal is the point: if the two ever
//!   diverge, one of them has grown a second read path.
//! - **An embed is one statement.** Measured by recording the SQL, not by timing:
//!   an embed that quietly became a second query would still be fast on four rows
//!   and wrong on four thousand.
//! - **Every authorization rule fails closed.** Each of these would *fail open*
//!   if the rule it names were dropped: a formula-bounded read stays bounded when
//!   the caller adds a filter, an ordering and a page; an embed into a table the
//!   caller may not read is refused by name rather than answered; and `select`
//!   never becomes a way around either.
//! - **Everything not taken is refused by name.** A silently dropped filter is
//!   rows the caller did not ask for, which is the worst failure this API can
//!   have — so an unknown column, an unsupported operator and `!inner` are each
//!   an error that says which one it is.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_api::{ApiProvider, ApiRequest, ApiResponse, GraphqlProvider, Method, RestProvider};
use sc_auth::User;
use sc_catalog::{Catalog, TableMeta, bootstrap_table_meta, enable_rls, save_table_meta};
use sc_db::{DatabaseDriver, DbCapabilities, PhysicalTable, RowStream, SchemaChange, Transaction};
use sc_db_postgres::PgDriver;
use sc_error::{Repr, Result};
use sc_query::{SqlDialect, Statement, Value};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

/// A driver that records the SQL of every statement run through it, delegating
/// everything to the real one — the same instrument the GraphQL suites use.
struct Recording {
    inner: Arc<dyn DatabaseDriver>,
    sql: Sql,
}

/// The statements run so far, shared with the test.
type Sql = Arc<Mutex<Vec<String>>>;

#[async_trait]
impl DatabaseDriver for Recording {
    async fn introspect(&self) -> Result<Vec<PhysicalTable>> {
        self.inner.introspect().await
    }

    async fn query(&self, stmt: &Statement) -> Result<RowStream> {
        if let Ok((sql, _)) = self.inner.dialect().render(stmt) {
            self.sql.lock().expect("the sql log").push(sql);
        }
        self.inner.query(stmt).await
    }

    async fn apply_schema(&self, change: &SchemaChange) -> Result<()> {
        self.inner.apply_schema(change).await
    }

    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        self.inner.begin().await
    }

    fn capabilities(&self) -> DbCapabilities {
        self.inner.capabilities()
    }

    fn dialect(&self) -> &dyn SqlDialect {
        self.inner.dialect()
    }
}

/// Books by authors in countries, published over four years — the milestone's
/// own example, plus the two rows every rule needs: a book with **no** author
/// (so a null key has to answer `null`) and an author with no publisher (so a
/// null key *behind* a join has to as well).
const FIXTURE: &str = "
    CREATE TABLE publishers (id bigint primary key, city text not null);
    CREATE TABLE authors (
        id bigint primary key,
        name text not null,
        country text,
        publisher bigint references publishers(id));
    CREATE TABLE books (
        id bigint primary key,
        title text not null,
        published date,
        owner text,
        author bigint references authors(id));
    INSERT INTO publishers VALUES (1, 'London');
    INSERT INTO authors VALUES
        (1, 'Austen', 'GB', 1),
        (2, 'Ibsen',  'NO', NULL);
    INSERT INTO books VALUES
        (1, 'Emma',       '2019-03-01', 'ada@example.com', 1),
        (2, 'Persuasion', '2021-06-01', 'ada@example.com', 1),
        (3, 'Brand',      '2022-01-15', 'bo@example.com',  2),
        (4, 'Peer Gynt',  '2023-09-30', 'bo@example.com',  2),
        (5, 'Anonymous',  '2020-01-01', 'ada@example.com', NULL);
";

async fn setup(db: &TestDb) -> Result<(Arc<Catalog>, Sql)> {
    db.client()
        .await?
        .batch_execute(FIXTURE)
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let sql: Sql = Arc::new(Mutex::new(Vec::new()));
    let driver = Arc::new(Recording {
        inner: Arc::new(PgDriver::from_pool(db.pool().clone())),
        sql: Arc::clone(&sql),
    });
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    Ok((Arc::new(catalog), sql))
}

/// The REST provider over the three tables the application declares.
fn provider(cat: &Catalog) -> Result<RestProvider> {
    Ok(RestProvider::project(
        "/api",
        &[
            cat.require("books")?,
            cat.require("authors")?,
            cat.require("publishers")?,
        ],
    ))
}

/// The GraphQL provider over the same tables — the other syntax over the same
/// read layer.
fn graphql(cat: &Catalog) -> Result<GraphqlProvider> {
    GraphqlProvider::project(
        "/graphql",
        &[
            cat.require("books")?,
            cat.require("authors")?,
            cat.require("publishers")?,
        ],
    )
}

fn admin() -> Result<User> {
    Ok(User::new(uuid::Uuid::new_v4(), Role::ADMIN))
}

/// A caller at the public role, identified by the email an ownership formula
/// (and a generated policy) reads.
fn caller(email: &str) -> Result<User> {
    let mut user = User::new(uuid::Uuid::new_v4(), Role::PUBLIC);
    user.extra
        .insert("email".to_owned(), Value::Text(email.to_owned()));
    Ok(user)
}

/// `GET /api/books?<query>` as `user`.
async fn list(
    api: &RestProvider,
    cat: &Arc<Catalog>,
    user: Option<&User>,
    query: &[(&str, &str)],
) -> Result<ApiResponse> {
    let mut req = ApiRequest::get("/api/books");
    for (key, value) in query {
        req = req.query(*key, *value);
    }
    api.handle(req, cat, user).await
}

/// The rows of a successful list response.
async fn rows(
    api: &RestProvider,
    cat: &Arc<Catalog>,
    user: Option<&User>,
    query: &[(&str, &str)],
) -> Result<Vec<Json>> {
    let resp = list(api, cat, user, query).await?;
    assert_eq!(resp.status, 200, "{}", resp.body);
    Ok(resp.body.as_array().expect("an array of rows").clone())
}

/// The message a refused read carries, asserting it is an `Invalid` — which is
/// what the server renders as a **400**.
async fn refusal(
    api: &RestProvider,
    cat: &Arc<Catalog>,
    user: Option<&User>,
    query: &[(&str, &str)],
) -> String {
    let err = list(api, cat, user, query)
        .await
        .expect_err("this read is refused");
    assert!(
        matches!(err.repr(), Repr::Invalid(_)),
        "expected a 400, got {err:?}"
    );
    format!("{err}")
}

/// The message a read refused for **authorization** carries, asserting it is a
/// 403 — for a signed-in caller the provider answers that itself — or, for an
/// anonymous one, an `Auth` error (the server's 401). The distinction matters:
/// "you may not read `authors`" is not a malformed request the caller can fix
/// by rewriting it.
async fn denial(
    api: &RestProvider,
    cat: &Arc<Catalog>,
    user: Option<&User>,
    query: &[(&str, &str)],
) -> String {
    match list(api, cat, user, query).await {
        Ok(resp) => {
            assert_eq!(resp.status, 403, "{}", resp.body);
            resp.body["error"].as_str().unwrap_or_default().to_owned()
        }
        Err(err) => {
            assert!(
                user.is_none() && matches!(err.repr(), Repr::Auth(_)),
                "expected a 403, got {err:?}"
            );
            format!("{err}")
        }
    }
}

/// The statements run since the log was last cleared.
fn recorded(sql: &Sql) -> Vec<String> {
    sql.lock().expect("the sql log").clone()
}

/// Forget every statement run so far.
fn clear(sql: &Sql) {
    sql.lock().expect("the sql log").clear();
}

// ---------------------------------------------------------------------------
// The milestone's query
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_milestones_query_answers_what_its_graphql_equivalent_does() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;
    let user = admin()?;

    // GET /api/books?select=title,published,author(name,country)
    //               &published=gte.2020-01-01&order=published.desc&limit=20
    let rest = rows(
        &api,
        &cat,
        Some(&user),
        &[
            ("select", "title,published,author(name,country)"),
            ("published", "gte.2020-01-01"),
            ("order", "published.desc"),
            ("limit", "20"),
        ],
    )
    .await?;

    assert_eq!(
        rest,
        vec![
            json!({ "title": "Peer Gynt", "published": "2023-09-30",
                    "author": { "name": "Ibsen", "country": "NO" } }),
            json!({ "title": "Brand", "published": "2022-01-15",
                    "author": { "name": "Ibsen", "country": "NO" } }),
            json!({ "title": "Persuasion", "published": "2021-06-01",
                    "author": { "name": "Austen", "country": "GB" } }),
            // A null key is a null object, not an error and not an object of
            // nulls: this book has no author, and says so.
            json!({ "title": "Anonymous", "published": "2020-01-01", "author": Json::Null }),
        ]
    );

    // The same question in the other syntax, over the same read layer.
    let gql = graphql(&cat)?;
    let body = gql
        .handle(
            ApiRequest::new(Method::Post, "/graphql").body(json!({
                "query": r#"{
                    books(
                        where: { published: { gte: "2020-01-01" } },
                        order_by: [{ published: desc }],
                        limit: 20
                    ) { title published author { name country } }
                }"#
            })),
            &cat,
            Some(&user),
        )
        .await?;
    assert!(body.body.get("errors").is_none(), "{}", body.body);
    assert_eq!(body.body["data"]["books"], Json::Array(rest));
    Ok(())
}

#[tokio::test]
async fn an_embed_is_one_statement_however_deep_it_goes() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;
    let user = admin()?;

    clear(&sql);
    let got = rows(
        &api,
        &cat,
        Some(&user),
        &[
            // Two hops, an alias, and a null key at each level.
            ("select", "title,writer:author(name,publisher(city))"),
            ("order", "id.asc"),
        ],
    )
    .await?;
    assert_eq!(
        got,
        vec![
            json!({ "title": "Emma", "writer": { "name": "Austen", "publisher": { "city": "London" } } }),
            json!({ "title": "Persuasion", "writer": { "name": "Austen", "publisher": { "city": "London" } } }),
            json!({ "title": "Brand", "writer": { "name": "Ibsen", "publisher": Json::Null } }),
            json!({ "title": "Peer Gynt", "writer": { "name": "Ibsen", "publisher": Json::Null } }),
            json!({ "title": "Anonymous", "writer": Json::Null }),
        ]
    );

    // Five books, two authors, one publisher — and **one** statement, with the
    // joined values projected into it as correlated subqueries.
    let statements = recorded(&sql);
    assert_eq!(statements.len(), 1, "{statements:#?}");
    let sql = &statements[0];
    assert!(sql.contains("authorⱵname"), "{sql}");
    assert!(sql.contains("authorⱵpublisherⱵcity"), "{sql}");
    Ok(())
}

#[tokio::test]
async fn a_read_is_bounded_by_the_applications_row_cap_however_large_a_limit_asks_for() -> Result<()>
{
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?.with_row_cap(2);
    let user = admin()?;

    // A caller's number is a request, not a permission.
    assert_eq!(
        rows(&api, &cat, Some(&user), &[("limit", "1000")])
            .await?
            .len(),
        2
    );
    // …and no number at all takes the cap rather than the table.
    assert_eq!(rows(&api, &cat, Some(&user), &[]).await?.len(), 2);
    // A smaller bound is still the caller's.
    assert_eq!(
        rows(&api, &cat, Some(&user), &[("limit", "1")])
            .await?
            .len(),
        1
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Refusals: everything not taken is refused by name
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_column_an_unsupported_operator_and_inner_are_each_refused_by_name() -> Result<()>
{
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;
    let user = admin()?;

    clear(&sql);
    let unknown = refusal(&api, &cat, Some(&user), &[("nope", "eq.1")]).await;
    assert!(unknown.contains("has no field `nope`"), "{unknown}");

    let operator = refusal(&api, &cat, Some(&user), &[("published", "between.2020")]).await;
    assert!(
        operator.contains("`between` is not a comparison"),
        "{operator}"
    );

    let inner = refusal(
        &api,
        &cat,
        Some(&user),
        &[("select", "title,author!inner(name)")],
    )
    .await;
    assert!(inner.contains("`!inner`"), "{inner}");

    let one_to_many = refusal(&api, &cat, Some(&user), &[("select", "title,books(title)")]).await;
    assert!(one_to_many.contains("one-to-many embed"), "{one_to_many}");

    // None of them reached the database: a refused query issues nothing.
    assert!(recorded(&sql).is_empty(), "{:#?}", recorded(&sql));
    Ok(())
}

// ---------------------------------------------------------------------------
// Authorization — each of these fails open if its rule is dropped
// ---------------------------------------------------------------------------

/// `books` readable by anyone but owned by an email, `authors` and `publishers`
/// left admin-only.
async fn own_books(cat: &Catalog, rls: bool) -> Result<()> {
    bootstrap_table_meta(cat).await?;
    let mut books = TableMeta::new("books");
    books.set_ownership_formula(Some("owner === user.email"));
    if rls {
        books.set_rls_enabled(true);
    }
    save_table_meta(cat, &books).await?;
    if rls {
        enable_rls(cat, &cat.require("books")?).await?;
    }
    Ok(())
}

#[tokio::test]
async fn an_ownership_formula_still_bounds_a_filtered_ordered_and_paged_read() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    own_books(&cat, false).await?;
    let api = provider(&cat)?;
    let ada = caller("ada@example.com")?;

    // Ada owns Emma (2019), Anonymous (2020) and Persuasion (2021). The filter,
    // the ordering and the page are hers; which rows they apply to is not.
    let got = rows(
        &api,
        &cat,
        Some(&ada),
        &[
            ("select", "title"),
            ("published", "gte.2020-01-01"),
            ("order", "published.desc"),
            ("limit", "5"),
        ],
    )
    .await?;
    assert_eq!(
        got,
        vec![
            json!({ "title": "Persuasion" }),
            json!({ "title": "Anonymous" })
        ]
    );

    // The page walks her rows, not the table's: an offset past her two is empty
    // rather than showing somebody else's third.
    let paged = rows(
        &api,
        &cat,
        Some(&ada),
        &[
            ("select", "title"),
            ("order", "published.asc"),
            ("offset", "3"),
        ],
    )
    .await?;
    assert!(paged.is_empty(), "{paged:?}");
    Ok(())
}

#[tokio::test]
async fn row_level_security_still_bounds_a_filtered_ordered_and_paged_read() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    own_books(&cat, true).await?;
    let api = provider(&cat)?;
    let bo = caller("bo@example.com")?;

    let got = rows(
        &api,
        &cat,
        Some(&bo),
        &[
            ("select", "title"),
            ("published", "gte.2022-06-01"),
            ("order", "published.desc"),
        ],
    )
    .await?;
    assert_eq!(got, vec![json!({ "title": "Peer Gynt" })]);

    // The policies decide, so a filter that would match somebody else's rows
    // matches nothing at all.
    let others = rows(
        &api,
        &cat,
        Some(&bo),
        &[("select", "title"), ("title", "eq.Emma")],
    )
    .await?;
    assert!(others.is_empty(), "{others:?}");
    Ok(())
}

#[tokio::test]
async fn an_embed_into_a_table_the_caller_may_not_read_is_refused_by_name() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    // `books` is open to everyone through its ownership formula; `authors` keeps
    // the default admin-only read floor.
    own_books(&cat, false).await?;
    let api = provider(&cat)?;
    let ada = caller("ada@example.com")?;

    // Reading her own books is fine…
    assert!(
        !rows(&api, &cat, Some(&ada), &[("select", "title")])
            .await?
            .is_empty()
    );

    // …and `select` is not a way around the floor on the *other* table: a caller
    // who may not list `authors` may not read one through a key either. Without
    // `join_guard` this would answer with the author's name, one column at a
    // time, and nothing would say so.
    let refused = denial(
        &api,
        &cat,
        Some(&ada),
        &[("select", "title,author(name,country)")],
    )
    .await;
    assert!(refused.contains("authors"), "{refused}");

    // The same rule for the second hop, reached through an author the caller
    // could not read either.
    let deep = denial(
        &api,
        &cat,
        Some(&ada),
        &[("select", "title,author(publisher(city))")],
    )
    .await;
    assert!(deep.contains("authors"), "{deep}");
    Ok(())
}

#[tokio::test]
async fn an_embed_whose_target_is_owned_by_a_formula_is_refused_rather_than_answered() -> Result<()>
{
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    bootstrap_table_meta(&cat).await?;
    // Both tables readable through a formula. A join subquery has no `WHERE`
    // this provider owns, so the formula cannot decide *which* author rows a
    // caller reaches — and answering anyway is the quietest possible leak.
    let mut books = TableMeta::new("books");
    books.set_ownership_formula(Some("owner === user.email"));
    save_table_meta(&cat, &books).await?;
    let mut authors = TableMeta::new("authors");
    authors.set_ownership_formula(Some("country === 'GB'"));
    save_table_meta(&cat, &authors).await?;

    let api = provider(&cat)?;
    let ada = caller("ada@example.com")?;
    let refused = refusal(&api, &cat, Some(&ada), &[("select", "title,author(name)")]).await;
    assert!(
        refused.contains("cannot be read through a key by you"),
        "{refused}"
    );
    // Reading the table directly still works and still applies the formula.
    let direct = api
        .handle(ApiRequest::get("/api/authors"), &cat, Some(&ada))
        .await?;
    assert_eq!(direct.status, 200, "{}", direct.body);
    assert_eq!(
        direct.body.as_array().expect("rows").len(),
        1,
        "{}",
        direct.body
    );
    Ok(())
}

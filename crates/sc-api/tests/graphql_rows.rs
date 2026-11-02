//! Reading rows over GraphQL, against a real Postgres (TODO "GraphQL API"
//! Phase 3).
//!
//! The unit tests in `sc-api`'s `graphql::args` prove what a `BoolExp` *lowers
//! to*; these prove what comes back — that a filtered, ordered, paged GraphQL
//! query returns exactly what the same question asked in SQL returns, and that
//! it costs what the design says it costs.
//!
//! The cost claim is the one that needs a real database and cannot be measured
//! with a stopwatch, so it is measured by **recording statements**: the catalog
//! is driven through a wrapper that keeps the SQL of everything run through it,
//! and the tests assert how many there were and which columns the one statement
//! asked for. A joinfield that quietly became a second query would still be fast
//! on four rows and wrong on four thousand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_api::{ApiProvider, ApiRequest, GraphqlProvider, Method};
use sc_auth::User;
use sc_catalog::Catalog;
use sc_db::{DatabaseDriver, DbCapabilities, PhysicalTable, RowStream, SchemaChange, Transaction};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{SqlDialect, Statement};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

/// A driver that records the SQL of every statement run through it, delegating
/// everything to the real one.
///
/// The only honest way to assert "one query per level": timing proves nothing on
/// a fixture this small, and a plan is not what we are claiming. Keeping the SQL
/// rather than just a count also lets a test say *which* columns the one
/// statement asked for.
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

/// Two departments, four employees, one of them with no department — and a
/// department whose manager is an employee, so a *second* Ⱶ-hop exists to ask
/// for.
const FIXTURE: &str = "
    CREATE TABLE departments (id bigint primary key, name text not null, manager bigint);
    CREATE TABLE employees (
        id bigint primary key,
        name text not null,
        salary bigint,
        active boolean,
        department bigint references departments(id));
    ALTER TABLE departments ADD FOREIGN KEY (manager) REFERENCES employees(id);
    INSERT INTO departments VALUES (1, 'Engineering', NULL), (2, 'Sales', NULL);
    INSERT INTO employees VALUES
        (1, 'Ada',   90000, true,  1),
        (2, 'Bror',  45000, true,  1),
        (3, 'Cleo',  60000, false, 2),
        (4, 'Dag',   30000, true,  NULL);
    UPDATE departments SET manager = 1 WHERE id = 1;
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

/// The statements run since the log was last cleared.
fn recorded(sql: &Sql) -> Vec<String> {
    sql.lock().expect("the sql log").clone()
}

/// Forget every statement run so far.
fn clear(sql: &Sql) {
    sql.lock().expect("the sql log").clear();
}

/// The provider over both tables, mounted where the default is.
fn provider(cat: &Catalog) -> Result<GraphqlProvider> {
    GraphqlProvider::project(
        "/graphql",
        &[cat.require("departments")?, cat.require("employees")?],
    )
}

/// A caller who meets both tables' default role floors.
fn admin() -> Result<User> {
    Ok(User::new(uuid::Uuid::new_v4(), Role::ADMIN))
}

/// POST one document as `admin` and return the response body.
async fn run(api: &GraphqlProvider, cat: &Arc<Catalog>, document: &str) -> Result<Json> {
    let user = admin()?;
    let resp = api
        .handle(
            ApiRequest::new(Method::Post, "/graphql").body(json!({ "query": document })),
            cat,
            Some(&user),
        )
        .await?;
    assert_eq!(resp.status, 200, "{}", resp.body);
    Ok(resp.body)
}

/// The `data` of a response, asserting there were no errors.
fn data(body: &Json) -> &Json {
    assert!(body.get("errors").is_none(), "{body}");
    &body["data"]
}

#[tokio::test]
async fn filters_ordering_and_paging_answer_what_the_sql_does() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;

    let body = run(
        &api,
        &cat,
        r#"{
            employees(
                where: { active: { eq: true }, salary: { gte: 40000 } },
                order_by: [{ salary: desc }]
            ) { name salary }
        }"#,
    )
    .await?;
    let rows = data(&body)["employees"].as_array().unwrap().clone();
    assert_eq!(
        rows,
        vec![
            json!({ "name": "Ada", "salary": 90000 }),
            json!({ "name": "Bror", "salary": 45000 }),
        ]
    );

    // The same question in SQL, by hand.
    let expected: Vec<String> = db
        .client()
        .await?
        .query(
            "SELECT name FROM employees WHERE active AND salary >= 40000 ORDER BY salary DESC",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    let names: Vec<String> = rows
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, expected);

    // `limit`/`offset` page through that same ordering.
    let body = run(
        &api,
        &cat,
        "{ employees(order_by: [{ salary: desc }], limit: 2, offset: 1) { name } }",
    )
    .await?;
    assert_eq!(
        data(&body)["employees"],
        json!([{ "name": "Cleo" }, { "name": "Bror" }])
    );
    Ok(())
}

#[tokio::test]
async fn a_joinfield_selection_is_one_query_and_only_the_leaves_asked_for() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    clear(&sql);
    let body = run(
        &api,
        &cat,
        r#"{
            employees(where: { department: { is_null: false } }, order_by: [{ name: asc }]) {
                name
                department { name }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["employees"],
        json!([
            { "name": "Ada",  "department": { "name": "Engineering" } },
            { "name": "Bror", "department": { "name": "Engineering" } },
            { "name": "Cleo", "department": { "name": "Sales" } },
        ])
    );
    // Three employees, three related departments, **one** statement: the
    // Ⱶ-join is a projected correlated subquery, not a second round trip.
    let statements = recorded(&sql);
    assert_eq!(
        statements.len(),
        1,
        "an outgoing key must be projected, not fetched: {statements:#?}"
    );
    // And only the leaf that was asked for: `departments.name` is projected;
    // `departments.manager`, which the caller did not name, is not.
    let one = &statements[0];
    assert!(one.contains("departmentⱵname"), "{one}");
    assert!(!one.contains("departmentⱵmanager"), "{one}");
    Ok(())
}

#[tokio::test]
async fn a_second_hop_composes_and_is_still_one_query() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    clear(&sql);
    let body = run(
        &api,
        &cat,
        r#"{
            employees(order_by: [{ name: asc }]) {
                name
                department { name manager { name } }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["employees"],
        json!([
            { "name": "Ada",  "department": { "name": "Engineering", "manager": { "name": "Ada" } } },
            { "name": "Bror", "department": { "name": "Engineering", "manager": { "name": "Ada" } } },
            // Sales has no manager: a null key on the far side of a join is
            // still `null`, not an object of nulls and not an error.
            { "name": "Cleo", "department": { "name": "Sales", "manager": null } },
            { "name": "Dag",  "department": null },
        ])
    );
    assert_eq!(recorded(&sql).len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_null_key_is_null_and_not_an_error() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;

    let body = run(
        &api,
        &cat,
        r#"{ employees(where: { name: { eq: "Dag" } }) { name department { name } } }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["employees"],
        json!([{ "name": "Dag", "department": null }])
    );
    Ok(())
}

#[tokio::test]
async fn by_pk_addresses_one_row_and_a_missing_one_is_null() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    clear(&sql);
    let body = run(
        &api,
        &cat,
        "{ employees_by_pk(id: 3) { name department { name } } }",
    )
    .await?;
    assert_eq!(
        data(&body)["employees_by_pk"],
        json!({ "name": "Cleo", "department": { "name": "Sales" } })
    );
    assert_eq!(recorded(&sql).len(), 1);

    let body = run(&api, &cat, "{ employees_by_pk(id: 99) { name } }").await?;
    assert_eq!(data(&body)["employees_by_pk"], Json::Null);
    Ok(())
}

#[tokio::test]
async fn the_row_cap_bounds_a_list_and_clamps_one_the_caller_asked_for() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    // A cap of two: a caller's `limit` is a request, not a permission.
    let api = provider(&cat)?.with_row_cap(2);

    let body = run(&api, &cat, "{ employees(order_by: [{ id: asc }]) { id } }").await?;
    assert_eq!(data(&body)["employees"].as_array().unwrap().len(), 2);

    let body = run(
        &api,
        &cat,
        "{ employees(order_by: [{ id: asc }], limit: 100) { id } }",
    )
    .await?;
    assert_eq!(data(&body)["employees"].as_array().unwrap().len(), 2);

    // A smaller bound than the cap is the caller's own.
    let body = run(
        &api,
        &cat,
        "{ employees(order_by: [{ id: asc }], limit: 1) { id } }",
    )
    .await?;
    assert_eq!(data(&body)["employees"], json!([{ "id": 1 }]));
    Ok(())
}

#[tokio::test]
async fn a_filter_naming_a_column_that_is_not_there_is_refused_by_name() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;

    // The schema itself refuses an unknown input field, which is the earliest
    // and cheapest place to refuse it: no statement is issued.
    let body = run(
        &api,
        &cat,
        "{ employees(where: { nope: { eq: 1 } }) { name } }",
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("nope"), "{body}");
    Ok(())
}

#[tokio::test]
async fn variables_carry_a_filter_and_an_ordering() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;

    let user = admin()?;
    let resp = api
        .handle(
            ApiRequest::new(Method::Post, "/graphql").body(json!({
                "query": "query ($min: BigInt, $dir: OrderDirection) {\
                            employees(where: { salary: { gte: $min } }, \
                                      order_by: [{ salary: $dir }]) { name } }",
                "variables": { "min": 60000, "dir": "desc" },
            })),
            &cat,
            Some(&user),
        )
        .await?;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(
        data(&resp.body)["employees"],
        json!([{ "name": "Ada" }, { "name": "Cleo" }])
    );
    Ok(())
}

#[tokio::test]
async fn a_caller_below_the_tables_floor_is_refused_rather_than_served() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    // The read goes through `ownership::read_row_values_as`, so the table's own
    // role floor decides — here for an anonymous caller, who is refused by name
    // before a statement is issued rather than handed an empty list.
    clear(&sql);
    let resp = api
        .handle(
            ApiRequest::new(Method::Post, "/graphql")
                .body(json!({ "query": "{ employees { name } }" })),
            &cat,
            None,
        )
        .await?;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let errors = resp.body["errors"].to_string();
    assert!(errors.contains("employees"), "{}", resp.body);
    assert!(recorded(&sql).is_empty());
    Ok(())
}

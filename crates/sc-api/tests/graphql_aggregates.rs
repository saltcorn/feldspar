//! Aggregates over GraphQL, against a real Postgres (TODO "GraphQL API"
//! Phase 5).
//!
//! The milestone's motivating question is "for each department, the number of
//! employees earning below 50 000", and this file is where it is answered. Two
//! kinds of claim are asserted, because the phase makes two:
//!
//! - **The numbers are the database's.** Every aggregate is checked against the
//!   SQL a person would have written by hand for the same question — including
//!   the empty-relation answers, where `sum` is `0` and `avg` is null, which is
//!   AGG_EXPRS.md's semantics table and not this provider's opinion.
//! - **The cost is one statement.** A child aggregate is a correlated subquery
//!   projected into the parent's own `SELECT`, so a query over N departments
//!   asks the database once. That is a claim about statements, so it is
//!   asserted by counting statements.
//!
//! And the refusals: an aggregate the caller may not see is an error naming the
//! table, never a plausible zero. A silent `0` is worse than a refusal because
//! nobody investigates it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_api::{ApiProvider, ApiRequest, GraphqlLimits, GraphqlProvider, Method};
use sc_auth::User;
use sc_catalog::{Catalog, TableMeta, bootstrap_table_meta, save_table_meta};
use sc_db::{DatabaseDriver, DbCapabilities, PhysicalTable, RowStream, SchemaChange, Transaction};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{SqlDialect, Statement};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

/// A driver that records the SQL of every statement run through it — the same
/// instrument the other two GraphQL suites use, and for the same reason.
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

/// Four departments, nine employees and a few tasks — one department with no
/// employees at all, one employee belonging to no department, and a third level
/// so that "the aggregate is projected into whichever read reaches the row" can
/// be asserted below the root as well as at it. The two edges of a correlated
/// aggregate (an empty relation, a null key) are in the fixture rather than in
/// a comment.
const FIXTURE: &str = "
    CREATE TABLE departments (id bigint primary key, name text not null);
    CREATE TABLE employees (
        id bigint primary key,
        name text not null,
        salary bigint,
        department bigint references departments(id));
    INSERT INTO departments VALUES
        (1, 'Engineering'), (2, 'Sales'), (3, 'Support'), (4, 'Empty');
    INSERT INTO employees VALUES
        (1, 'Ada',   90000, 1),
        (2, 'Bror',  45000, 1),
        (3, 'Cleo',  60000, 1),
        (4, 'Dag',   30000, 2),
        (5, 'Eve',   70000, 2),
        (6, 'Fritz', 20000, 3),
        (7, 'Gina',  80000, 3),
        (8, 'Hans',  50000, 3),
        (9, 'Iris',  10000, NULL);
    CREATE TABLE tasks (
        id bigint primary key,
        title text not null,
        hours bigint,
        employee bigint references employees(id));
    INSERT INTO tasks VALUES
        (1, 'Design', 3, 1),
        (2, 'Build',  5, 1),
        (3, 'Test',   2, 2);
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

/// The provider over the three tables, mounted where the default is, with the
/// aggregate fields **switched on** — they are off unless an application asks
/// for them, and this is the file about what they do when it has.
fn provider(cat: &Catalog) -> Result<GraphqlProvider> {
    GraphqlProvider::project_with(
        "/graphql",
        &[
            cat.require("departments")?,
            cat.require("employees")?,
            cat.require("tasks")?,
        ],
        GraphqlLimits::new().aggregates(true),
    )
}

/// A caller who meets every table's default role floor.
fn admin() -> Result<User> {
    Ok(User::new(uuid::Uuid::new_v4(), Role::ADMIN))
}

/// POST one document as `user` and return the response body.
async fn run_as(
    api: &GraphqlProvider,
    cat: &Arc<Catalog>,
    user: Option<&User>,
    document: &str,
) -> Result<Json> {
    let resp = api
        .handle(
            ApiRequest::new(Method::Post, "/graphql").body(json!({ "query": document })),
            cat,
            user,
        )
        .await?;
    assert_eq!(resp.status, 200, "{}", resp.body);
    Ok(resp.body)
}

/// POST one document as an admin.
async fn run(api: &GraphqlProvider, cat: &Arc<Catalog>, document: &str) -> Result<Json> {
    let user = admin()?;
    run_as(api, cat, Some(&user), document).await
}

/// The `data` of a response, asserting there were no errors.
fn data(body: &Json) -> &Json {
    assert!(body.get("errors").is_none(), "{body}");
    &body["data"]
}

#[tokio::test]
async fn the_motivating_query_is_one_statement_and_agrees_with_hand_written_sql() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    clear(&sql);
    let body = run(
        &api,
        &cat,
        r#"{
            departments(order_by: [{ id: asc }]) {
                name
                employees_aggregate(where: { salary: { lt: 50000 } }) { count }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees_aggregate": { "count": 1 } },
            { "name": "Sales",       "employees_aggregate": { "count": 1 } },
            { "name": "Support",     "employees_aggregate": { "count": 1 } },
            // An empty relation counts 0 — the one place a zero is the truth.
            { "name": "Empty",       "employees_aggregate": { "count": 0 } },
        ])
    );

    // The whole thing is **one** statement: the aggregate is a column of the
    // departments query, not a second read and not a read per department.
    let statements = recorded(&sql);
    assert_eq!(statements.len(), 1, "{statements:#?}");
    assert!(statements[0].contains("count(*)"), "{}", statements[0]);
    // The bound rode in as a parameter rather than as text in the statement.
    assert!(!statements[0].contains("50000"), "{}", statements[0]);

    // And the numbers are the ones a person would have written by hand.
    let rows = db
        .client()
        .await?
        .query(
            "SELECT d.name, count(e.id) FILTER (WHERE e.salary < 50000) AS n
               FROM departments d LEFT JOIN employees e ON e.department = d.id
              GROUP BY d.id, d.name ORDER BY d.id",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let by_hand: Vec<Json> = rows
        .iter()
        .map(|r| {
            json!({
                "name": r.get::<_, String>(0),
                "employees_aggregate": { "count": r.get::<_, i64>(1) },
            })
        })
        .collect();
    assert_eq!(data(&body)["departments"], Json::Array(by_hand));
    Ok(())
}

#[tokio::test]
async fn a_root_aggregate_computes_every_function_over_the_filtered_rows() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    clear(&sql);
    let body = run(
        &api,
        &cat,
        r#"{
            employees_aggregate(where: { department: { eq: 1 } }) {
                count
                sum { salary }
                avg { salary }
                min { salary name }
                max { salary }
            }
        }"#,
    )
    .await?;
    let agg = &data(&body)["employees_aggregate"];
    assert_eq!(agg["count"], json!(3));
    // A sum of `bigint` is `numeric` in Postgres, and the schema promised
    // `BigInt` — so it reaches the wire as the number the SDL says it is.
    assert_eq!(agg["sum"]["salary"], json!(195_000));
    assert_eq!(agg["min"]["salary"], json!(45_000));
    assert_eq!(agg["min"]["name"], json!("Ada"));
    assert_eq!(agg["max"]["salary"], json!(90_000));
    // `avg` is exact rather than a rounded `Float`, so it rides as a string.
    let avg: f64 = agg["avg"]["salary"]
        .as_str()
        .expect("an exact decimal")
        .parse()
        .expect("a number");
    assert!((avg - 65_000.0).abs() < 1e-9, "{avg}");

    // Five aggregates, one statement.
    assert_eq!(recorded(&sql).len(), 1, "{:#?}", recorded(&sql));
    Ok(())
}

#[tokio::test]
async fn sum_over_no_rows_is_zero_and_avg_over_no_rows_is_null() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;

    // The semantics table of docs/AGG_EXPRS.md, exactly: a JS programmer's
    // `[].reduce((a, b) => a + b, 0)` is 0, and there is no mean of nothing.
    let body = run(
        &api,
        &cat,
        r#"{
            employees_aggregate(where: { department: { eq: 4 } }) {
                count sum { salary } avg { salary } min { salary } max { salary }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["employees_aggregate"],
        json!({
            "count": 0,
            "sum": { "salary": 0 },
            "avg": { "salary": null },
            "min": { "salary": null },
            "max": { "salary": null },
        })
    );

    // And the same through the correlated path, on the department that has no
    // employees: one implementation, one answer.
    let body = run(
        &api,
        &cat,
        r#"{
            departments(where: { id: { eq: 4 } }) {
                employees_aggregate { count sum { salary } avg { salary } }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"][0]["employees_aggregate"],
        json!({ "count": 0, "sum": { "salary": 0 }, "avg": { "salary": null } })
    );
    Ok(())
}

#[tokio::test]
async fn one_relation_aggregated_twice_is_two_subqueries_and_two_answers() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    clear(&sql);
    let body = run(
        &api,
        &cat,
        r#"{
            departments(where: { id: { eq: 1 } }) {
                cheap: employees_aggregate(where: { salary: { lt: 50000 } }) { count }
                rich:  employees_aggregate(where: { salary: { gte: 50000 } }) {
                    count
                    total: sum { salary }
                }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"][0],
        json!({
            "cheap": { "count": 1 },
            "rich": { "count": 2, "total": { "salary": 150_000 } },
        })
    );

    // Still one statement, carrying three correlated subqueries with three
    // distinct aliases — two questions about one relation are two subqueries,
    // and an alias each is what keeps them apart.
    let statements = recorded(&sql);
    assert_eq!(statements.len(), 1, "{statements:#?}");
    for alias in ["_fd_g1", "_fd_g2", "_fd_g3"] {
        assert!(statements[0].contains(alias), "{}", statements[0]);
    }
    Ok(())
}

#[tokio::test]
async fn an_aggregate_over_a_filtered_parent_list_is_over_that_filter() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    // The parent's `WHERE` is not dropped to make room for the aggregate's:
    // two departments asked for, two answered.
    clear(&sql);
    let body = run(
        &api,
        &cat,
        r#"{
            departments(where: { name: { like: "S%" } }, order_by: [{ id: asc }]) {
                name
                employees_aggregate { count }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Sales",   "employees_aggregate": { "count": 2 } },
            { "name": "Support", "employees_aggregate": { "count": 3 } },
        ])
    );
    let statements = recorded(&sql);
    assert_eq!(statements.len(), 1, "{statements:#?}");
    assert!(statements[0].contains("LIKE"), "{}", statements[0]);
    Ok(())
}

#[tokio::test]
async fn count_distinct_counts_values_and_ignores_nulls() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;

    let body = run(
        &api,
        &cat,
        r#"{
            employees_aggregate {
                rows: count
                departments: count(distinct: department)
            }
        }"#,
    )
    .await?;
    // Nine employees; three departments among them, and Iris' null key is not
    // a fourth.
    assert_eq!(
        data(&body)["employees_aggregate"],
        json!({ "rows": 9, "departments": 3 })
    );
    Ok(())
}

#[tokio::test]
async fn an_aggregate_over_a_table_the_caller_may_not_read_is_refused_not_zeroed() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    // `departments` is readable by everybody; `employees` keeps the admin-only
    // default. A caller who may not read the employees may not count them
    // either — and the refusal names the table, because a `0` here is a leak
    // nobody would investigate.
    bootstrap_table_meta(&cat).await?;
    save_table_meta(
        &cat,
        &TableMeta::new("departments").access(Role::PUBLIC, Role::ADMIN),
    )
    .await?;
    let api = provider(&cat)?;
    let public = User::new(uuid::Uuid::new_v4(), Role::PUBLIC);

    let body = run_as(
        &api,
        &cat,
        Some(&public),
        "{ employees_aggregate { count } }",
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("employees"), "{body}");
    assert!(body["data"]["employees_aggregate"].is_null(), "{body}");

    // The same through the correlated path: the count is a column of the
    // departments read, so refusing it refuses that read rather than answering
    // it with a number.
    let body = run_as(
        &api,
        &cat,
        Some(&public),
        "{ departments { name employees_aggregate { count } } }",
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("employees"), "{body}");
    assert!(!errors.contains("count\": 0"), "{body}");

    // And the admin, over the same schema, still gets the numbers.
    let body = run(
        &api,
        &cat,
        "{ departments(where: { id: { eq: 2 } }) { employees_aggregate { count } } }",
    )
    .await?;
    assert_eq!(
        data(&body)["departments"][0]["employees_aggregate"]["count"],
        json!(2)
    );
    Ok(())
}

#[tokio::test]
async fn an_untranslatable_ownership_formula_refuses_the_aggregate_by_name() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    bootstrap_table_meta(&cat).await?;
    save_table_meta(
        &cat,
        &TableMeta::new("departments").access(Role::PUBLIC, Role::ADMIN),
    )
    .await?;
    // `.includes(…)` is a function call, so the formula cannot become SQL. A
    // read of rows can still answer it one row at a time in the evaluator; a
    // *count* cannot, because the database does the counting. The honest
    // answer is a refusal naming the table (docs/GRAPHQL_API.md §5).
    let mut employees = TableMeta::new("employees");
    employees.set_ownership_formula(Some("name.includes('a')"));
    save_table_meta(&cat, &employees).await?;
    let api = provider(&cat)?;
    let public = User::new(uuid::Uuid::new_v4(), Role::PUBLIC);

    for document in [
        "{ employees_aggregate { count } }",
        "{ departments { employees_aggregate { count } } }",
    ] {
        let body = run_as(&api, &cat, Some(&public), document).await?;
        let errors = body["errors"].to_string();
        assert!(errors.contains("employees"), "{document}: {body}");
        assert!(errors.contains("row by row"), "{document}: {body}");
    }
    Ok(())
}

#[tokio::test]
async fn an_ownership_formula_the_database_can_carry_bounds_the_count() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    bootstrap_table_meta(&cat).await?;
    save_table_meta(
        &cat,
        &TableMeta::new("departments").access(Role::PUBLIC, Role::ADMIN),
    )
    .await?;
    // Sub-floor access, granted by a formula the translator *can* carry: it
    // becomes a predicate inside the aggregate's own subquery, so the count is
    // over the rows this caller may read and no others.
    let mut employees = TableMeta::new("employees");
    employees.set_ownership_formula(Some("salary < 50000"));
    save_table_meta(&cat, &employees).await?;
    let api = provider(&cat)?;
    let public = User::new(uuid::Uuid::new_v4(), Role::PUBLIC);

    clear(&sql);
    let body = run_as(
        &api,
        &cat,
        Some(&public),
        "{ departments(order_by: [{ id: asc }]) { name employees_aggregate { count } } }",
    )
    .await?;
    // The admin's counts would be 3, 2, 3, 0. This caller may only see the
    // employees under 50 000, and the numbers say so.
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees_aggregate": { "count": 1 } },
            { "name": "Sales",       "employees_aggregate": { "count": 1 } },
            { "name": "Support",     "employees_aggregate": { "count": 1 } },
            { "name": "Empty",       "employees_aggregate": { "count": 0 } },
        ])
    );
    // Still one statement: the ownership predicate rode into the subquery's
    // WHERE rather than costing a read of its own.
    assert_eq!(recorded(&sql).len(), 1, "{:#?}", recorded(&sql));
    Ok(())
}

#[tokio::test]
async fn an_aggregate_below_the_root_rides_in_the_read_that_reaches_its_row() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    // The rule is not "the root query projects the aggregates" but "whichever
    // read reaches the row projects them" — so a batched child read carries
    // its own correlated subqueries, and a third level still costs no third
    // statement.
    clear(&sql);
    let body = run(
        &api,
        &cat,
        r#"{
            departments(where: { id: { eq: 1 } }) {
                employees(order_by: [{ id: asc }]) {
                    name
                    tasks_aggregate { count sum { hours } }
                }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"][0]["employees"],
        json!([
            { "name": "Ada",  "tasks_aggregate": { "count": 2, "sum": { "hours": 8 } } },
            { "name": "Bror", "tasks_aggregate": { "count": 1, "sum": { "hours": 2 } } },
            { "name": "Cleo", "tasks_aggregate": { "count": 0, "sum": { "hours": 0 } } },
        ])
    );
    // One statement for the departments and one for their employees — the
    // tasks were never read at all, only counted and summed.
    let statements = recorded(&sql);
    assert_eq!(statements.len(), 2, "{statements:#?}");
    assert!(
        statements[1].contains("\"tasks\" AS \"_fd_g1\""),
        "{}",
        statements[1]
    );
    Ok(())
}

#[tokio::test]
async fn an_aggregate_on_a_row_reached_through_a_join_says_it_was_not_computed() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;

    // A Ⱶ-joined row is projected out of its parent's query, one requested
    // leaf at a time; an aggregate correlated to *it* was never computed. That
    // is a refusal naming the table rather than a query per employee.
    let body = run(
        &api,
        &cat,
        r#"{
            employees(where: { id: { eq: 1 } }) {
                name
                department { name employees_aggregate { count } }
            }
        }"#,
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("employees"), "{body}");
    assert!(errors.contains("not computed"), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_switched_off_application_refuses_an_aggregate_before_issuing_a_statement() -> Result<()>
{
    // Aggregates are off unless an application switches them on, and off means
    // *absent from the schema*: the refusal is `async-graphql`'s own validation
    // over the document, so it costs nothing. Asserted as a statement count,
    // because "before a statement is issued" is a claim about statements.
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = GraphqlProvider::project_with(
        "/graphql",
        &[
            cat.require("departments")?,
            cat.require("employees")?,
            cat.require("tasks")?,
        ],
        GraphqlLimits::new(),
    )?;

    clear(&sql);
    let body = run(&api, &cat, "{ departments_aggregate { count } }").await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("departments_aggregate"), "{body}");
    assert!(recorded(&sql).is_empty(), "{:#?}", recorded(&sql));

    // The child field is gone with it, and the list it hangs off is not.
    let body = run(
        &api,
        &cat,
        "{ departments { name employees_aggregate { count } } }",
    )
    .await?;
    assert!(
        body["errors"].to_string().contains("employees_aggregate"),
        "{body}"
    );
    let body = run(
        &api,
        &cat,
        "{ departments(order_by: [{ id: asc }]) { name } }",
    )
    .await?;
    assert_eq!(data(&body)["departments"][0]["name"], json!("Engineering"));
    Ok(())
}

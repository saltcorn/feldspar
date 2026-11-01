//! Child lists over GraphQL, against a real Postgres (TODO "GraphQL API"
//! Phase 4).
//!
//! The claim this phase makes is a *cost* claim — a level of a query costs one
//! statement, not one per parent — so most of what is asserted here is the SQL
//! that was run. Timing would prove nothing on four departments; a `DataLoader`
//! that had quietly stopped batching would still be fast on this fixture and
//! ruinous on a real one.
//!
//! The other two claims are about who decides and what happens when nobody can:
//! a child read is the **child table's** business, and a child list nobody
//! bounded must say so rather than stream a table into a response.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_api::{ApiProvider, ApiRequest, GraphqlProvider, Method};
use sc_auth::User;
use sc_catalog::{Catalog, TableMeta, bootstrap_table_meta, save_table_meta};
use sc_db::{DatabaseDriver, DbCapabilities, PhysicalTable, RowStream, SchemaChange, Transaction};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_expr::{FormulaCall, JsEvaluator};
use sc_query::{SqlDialect, Statement, Value};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

/// A driver that records the SQL of every statement run through it — the same
/// instrument `graphql_rows.rs` uses, and for the same reason: "one query per
/// level" is a claim about statements, so it is asserted with statements.
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

/// Four departments and nine employees, so "one query, not N" has an N worth
/// counting and every department has enough children for a per-parent `limit`
/// to cut something off. One department has no employees at all.
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
async fn a_child_list_over_many_parents_is_one_query_not_one_per_parent() -> Result<()> {
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
                employees(order_by: [{ id: asc }]) { name }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees": [{ "name": "Ada" }, { "name": "Bror" }, { "name": "Cleo" }] },
            { "name": "Sales",       "employees": [{ "name": "Dag" }, { "name": "Eve" }] },
            { "name": "Support",     "employees": [{ "name": "Fritz" }, { "name": "Gina" }, { "name": "Hans" }] },
            // A parent with no children is an empty list, not a null and not an
            // error.
            { "name": "Empty",       "employees": [] },
        ])
    );

    // Four parents, one statement for them and **one** for all their children.
    let statements = recorded(&sql);
    assert_eq!(
        statements.len(),
        2,
        "a child list must be batched across siblings: {statements:#?}"
    );
    // And that one child statement asks for every parent at once.
    let children = &statements[1];
    assert!(children.contains("\"department\" IN ("), "{children}");
    // The employee with no department is not in anybody's list, so the read
    // never had to consider them.
    assert!(!children.contains("row_number"), "{children}");
    Ok(())
}

#[tokio::test]
async fn a_per_parent_limit_takes_the_first_k_of_each_parent() -> Result<()> {
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
                employees(order_by: [{ salary: desc }], limit: 2) { name salary }
            }
        }"#,
    )
    .await?;
    // Two *each*, not two altogether — the whole reason `row_number()` had to
    // exist before this phase could.
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees": [
                { "name": "Ada", "salary": 90000 }, { "name": "Cleo", "salary": 60000 }] },
            { "name": "Sales", "employees": [
                { "name": "Eve", "salary": 70000 }, { "name": "Dag", "salary": 30000 }] },
            { "name": "Support", "employees": [
                { "name": "Gina", "salary": 80000 }, { "name": "Hans", "salary": 50000 }] },
            { "name": "Empty", "employees": [] },
        ])
    );

    let statements = recorded(&sql);
    assert_eq!(statements.len(), 2, "{statements:#?}");
    let children = &statements[1];
    assert!(
        children.contains("row_number() OVER (PARTITION BY \"department\""),
        "{children}"
    );
    // The numbering happens *inside* the read the filter applies to and is
    // compared outside it — which is what keeps a row the caller may not see
    // from taking a place in somebody's first two.
    let numbering = children.find("row_number()").expect("a window");
    let filter = children
        .find("\"department\" IN (")
        .expect("the correlation");
    let compared = children.rfind("\"_fd_rn\"").expect("the comparison");
    assert!(numbering < filter && filter < compared, "{children}");
    Ok(())
}

#[tokio::test]
async fn a_per_parent_offset_pages_within_each_parent() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;

    let body = run(
        &api,
        &cat,
        r#"{
            departments(where: { id: { in: [1, 3] } }, order_by: [{ id: asc }]) {
                name
                employees(order_by: [{ salary: desc }], limit: 1, offset: 1) { name }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees": [{ "name": "Cleo" }] },
            { "name": "Support", "employees": [{ "name": "Hans" }] },
        ])
    );
    Ok(())
}

#[tokio::test]
async fn the_child_where_is_the_childs_own_and_still_one_query() -> Result<()> {
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
                employees(where: { salary: { gte: 60000 } }, order_by: [{ id: asc }]) { name }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees": [{ "name": "Ada" }, { "name": "Cleo" }] },
            { "name": "Sales", "employees": [{ "name": "Eve" }] },
            { "name": "Support", "employees": [{ "name": "Gina" }] },
            { "name": "Empty", "employees": [] },
        ])
    );
    assert_eq!(recorded(&sql).len(), 2);
    Ok(())
}

#[tokio::test]
async fn one_relation_asked_twice_is_two_reads_and_two_answers() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    // Two different questions about the same relation cannot share a read —
    // and must not share an answer either.
    clear(&sql);
    let body = run(
        &api,
        &cat,
        r#"{
            departments(where: { id: { eq: 1 } }) {
                rich: employees(where: { salary: { gte: 60000 } }, order_by: [{ id: asc }]) { name }
                poor: employees(where: { salary: { lt: 60000 } }, order_by: [{ id: asc }]) { name }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([{
            "rich": [{ "name": "Ada" }, { "name": "Cleo" }],
            "poor": [{ "name": "Bror" }],
        }])
    );
    // One parent statement and one per distinct child question.
    assert_eq!(recorded(&sql).len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_child_list_behind_a_join_is_correlated_on_the_key_it_needs() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    // `department` is a Ⱶ-join — projected into the employees query rather than
    // fetched — so its own child list only works if the parent read knew to
    // bring the joined row's key back with it.
    clear(&sql);
    let body = run(
        &api,
        &cat,
        r#"{
            employees(where: { id: { in: [1, 4] } }, order_by: [{ id: asc }]) {
                name
                department { name employees(order_by: [{ id: asc }]) { name } }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["employees"],
        json!([
            { "name": "Ada", "department": { "name": "Engineering", "employees": [
                { "name": "Ada" }, { "name": "Bror" }, { "name": "Cleo" }] } },
            { "name": "Dag", "department": { "name": "Sales", "employees": [
                { "name": "Dag" }, { "name": "Eve" }] } },
        ])
    );
    // One for the employees (carrying the joined department), one for the
    // grandchildren of both.
    assert_eq!(recorded(&sql).len(), 2, "{:#?}", recorded(&sql));
    Ok(())
}

#[tokio::test]
async fn an_unbounded_child_list_that_reaches_the_cap_says_so() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    // A cap the fixture's children exceed. The cap is shared between parents
    // here, so truncating it silently would cut one department's list short
    // with nothing to say which.
    let api = provider(&cat)?.with_row_cap(4);

    let body = run(
        &api,
        &cat,
        "{ departments(order_by: [{ id: asc }]) { name employees { name } } }",
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("row cap"), "{body}");
    assert!(errors.contains("employees"), "{body}");
    // The parents are still answered: only the field that could not be bounded
    // is null.
    let departments = body["data"]["departments"].as_array().unwrap().clone();
    assert_eq!(departments.len(), 4);
    assert_eq!(departments[0]["name"], json!("Engineering"));
    assert_eq!(departments[0]["employees"], Json::Null);

    // The same query with a per-parent bound is answerable, because then the
    // bound is per parent rather than shared.
    let body = run(
        &api,
        &cat,
        "{ departments(order_by: [{ id: asc }]) { employees(order_by: [{ id: asc }], limit: 2) { name } } }",
    )
    .await?;
    assert_eq!(
        data(&body)["departments"][0]["employees"],
        json!([{ "name": "Ada" }, { "name": "Bror" }])
    );
    Ok(())
}

/// An evaluator that grants a row on its own, without JavaScript: this test is
/// about *when* the per-parent bound is applied, not about V8. It stands in for
/// the reified path, so it answers the way that path does — a verdict per row.
struct WellPaidOnly;

#[async_trait]
impl JsEvaluator for WellPaidOnly {
    async fn eval(&self, call: FormulaCall) -> Result<bool> {
        Ok(matches!(call.row.get("salary"), Some(Value::Int(n)) if *n >= 40000))
    }

    async fn eval_value(&self, _call: FormulaCall) -> Result<Json> {
        Err(sc_error::Error::invalid("not part of this test"))
    }

    async fn run_code(&self, _call: sc_expr::CodeCall<'_>) -> Result<Json> {
        Err(sc_error::Error::invalid("not part of this test"))
    }
}

#[tokio::test]
async fn a_per_parent_limit_waits_for_an_untranslatable_ownership_formula() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    bootstrap_table_meta(&cat).await?;
    save_table_meta(
        &cat,
        &TableMeta::new("departments").access(Role::PUBLIC, Role::ADMIN),
    )
    .await?;
    // `.includes(…)` is a function call, so the formula cannot become SQL and
    // the rows have to be decided one at a time in the evaluator.
    let mut employees = TableMeta::new("employees");
    employees.set_ownership_formula(Some("name.includes('')"));
    save_table_meta(&cat, &employees).await?;
    let api = provider(&cat)?.with_evaluator(Arc::new(WellPaidOnly));

    let public = User::new(uuid::Uuid::new_v4(), Role::PUBLIC);
    let body = run_as(
        &api,
        &cat,
        Some(&public),
        r#"{
            departments(order_by: [{ id: asc }]) {
                name
                employees(order_by: [{ salary: asc }], limit: 1) { name }
            }
        }"#,
    )
    .await?;
    // Sales is the discriminating case: its cheapest employee (Dag, 30 000) is
    // one the evaluator denies. A `LIMIT 1` per parent applied in the database
    // would have spent Sales' whole budget on him and returned an empty list —
    // "you may see one of these" answered with none of them.
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees": [{ "name": "Bror" }] },
            { "name": "Sales", "employees": [{ "name": "Eve" }] },
            { "name": "Support", "employees": [{ "name": "Hans" }] },
            { "name": "Empty", "employees": [] },
        ])
    );
    Ok(())
}

#[tokio::test]
async fn a_child_table_the_caller_may_not_read_is_an_error_on_that_field_alone() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    // `departments` is readable by everybody; `employees` keeps the admin-only
    // default. The child read is the child table's decision, so the parents
    // must still come back.
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
        "{ departments(order_by: [{ id: asc }]) { name employees { name } } }",
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("employees"), "{body}");
    let departments = body["data"]["departments"].as_array().unwrap().clone();
    assert_eq!(departments.len(), 4);
    assert_eq!(departments[0]["name"], json!("Engineering"));
    assert_eq!(departments[0]["employees"], Json::Null);

    // And the admin, over the same schema, still gets them.
    let body = run(
        &api,
        &cat,
        "{ departments(where: { id: { eq: 2 } }) { employees(order_by: [{ id: asc }]) { name } } }",
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([{ "employees": [{ "name": "Dag" }, { "name": "Eve" }] }])
    );
    Ok(())
}

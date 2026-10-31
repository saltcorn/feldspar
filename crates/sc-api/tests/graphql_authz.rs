//! Authorization, cost and the refusals over GraphQL, against a real Postgres
//! (TODO "GraphQL API" Phase 7).
//!
//! Every test here is shaped the same way, because the phase's own rule is:
//! **each authorization rule gets a test that would fail open if the rule were
//! dropped.** An aggregate is never asserted by checking that a number is
//! correct for an administrator — it is asserted by checking what a caller who
//! may *not* see the rows gets, which is either a refusal or the smaller number
//! their own rows make. A test that only checked the admin's number would pass
//! just as happily against a provider that had forgotten to authorize anything.
//!
//! Three groups:
//!
//! - **Row-level security across a correlated subquery.** A child table under
//!   RLS is counted inside the *parent's* statement, so the parent's statement
//!   has to be a caller-context transaction or the child's policies decide with
//!   no caller set. Phase 5 refused this case; Phase 7 answers it.
//! - **The limits.** Depth and complexity are refused before a statement is
//!   issued — asserted by counting statements, not by reading the message — and
//!   the statement budget is refused when it is spent. Introspection stays on.
//! - **What a caller writes must not reach SQL as an identifier.** Aliases,
//!   fragments and variables are the user's input; response keys come from the
//!   operation and column names come from the catalog.
//!
//! The non-RLS half of the ownership rule — the child's translated predicate
//! ANDed into the aggregate's subquery, and the refusal when it cannot be
//! translated — is asserted in `graphql_aggregates.rs`, where the aggregates it
//! bounds are.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_api::{ApiProvider, ApiRequest, GraphqlLimits, GraphqlProvider, Method};
use sc_auth::User;
use sc_catalog::{Catalog, TableMeta, bootstrap_table_meta, enable_rls, save_table_meta};
use sc_db::{DatabaseDriver, DbCapabilities, PhysicalTable, RowStream, SchemaChange, Transaction};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{SqlDialect, Statement, Value};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

/// A driver that records the SQL of every statement run through it.
///
/// The same instrument the other GraphQL suites use. It records the *pooled*
/// path only — a caller-context transaction goes through `begin()` and runs its
/// statements on the transaction — which is exactly right for what it is used
/// for here: proving that a refused query issued **nothing**.
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

/// Three departments and six employees, each employee owned by an email — which
/// is what an ownership formula and an RLS policy both read. Two owners with
/// employees in more than one department, so "the count a restricted caller
/// sees" is a different number per department and not just a smaller total.
const FIXTURE: &str = "
    CREATE TABLE departments (id bigint primary key, name text not null);
    CREATE TABLE employees (
        id bigint primary key,
        name text not null,
        owner text not null,
        salary bigint,
        department bigint references departments(id));
    INSERT INTO departments VALUES (1, 'Engineering'), (2, 'Sales'), (3, 'Empty');
    INSERT INTO employees VALUES
        (1, 'Ada',   'ada@example.com', 90000, 1),
        (2, 'Bror',  'ada@example.com', 45000, 1),
        (3, 'Cleo',  'bo@example.com',  60000, 1),
        (4, 'Dag',   'ada@example.com', 30000, 2),
        (5, 'Eve',   'bo@example.com',  70000, 2),
        (6, 'Fritz', 'bo@example.com',  20000, 2);
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

/// `departments` readable by everybody, `employees` protected by row-level
/// security on `owner === user.email` and otherwise admin-only.
///
/// This is the configuration Phase 5 carried forward as a refusal: the parent is
/// an ordinary table, the child's rule is the database's, and the child is
/// counted inside the parent's statement.
async fn protect_employees(cat: &Catalog) -> Result<()> {
    bootstrap_table_meta(cat).await?;
    save_table_meta(
        cat,
        &TableMeta::new("departments").access(Role::PUBLIC, Role::ADMIN),
    )
    .await?;
    let mut employees = TableMeta::new("employees");
    employees.set_ownership_formula(Some("owner === user.email"));
    employees.set_rls_enabled(true);
    save_table_meta(cat, &employees).await?;
    enable_rls(cat, &cat.require("employees")?).await
}

/// The statements run since the log was last cleared.
fn recorded(sql: &Sql) -> Vec<String> {
    sql.lock().expect("the sql log").clone()
}

/// Forget every statement run so far.
fn clear(sql: &Sql) {
    sql.lock().expect("the sql log").clear();
}

/// The provider over both tables, under the default limits **plus** the
/// aggregate fields: several rules below are about what an aggregate does with
/// rows the caller may not see, and a schema without the fields could not state
/// them at all.
fn provider(cat: &Catalog) -> Result<GraphqlProvider> {
    provider_with(cat, GraphqlLimits::new().aggregates(true))
}

/// The provider under limits of the test's choosing.
fn provider_with(cat: &Catalog, limits: GraphqlLimits) -> Result<GraphqlProvider> {
    GraphqlProvider::project_with(
        "/graphql",
        &[cat.require("departments")?, cat.require("employees")?],
        limits,
    )
}

/// A caller who meets every table's default role floor.
fn admin() -> Result<User> {
    Ok(User::new(uuid::Uuid::new_v4(), Role::ADMIN))
}

/// A caller at the public role, identified by the email the ownership formula
/// and the generated policy both read.
fn caller(email: &str) -> Result<User> {
    let mut user = User::new(uuid::Uuid::new_v4(), Role::PUBLIC);
    user = user.with_field("email".to_owned(), Value::Text(email.to_owned()));
    Ok(user)
}

/// POST one document as `user`, with variables, and return the response body.
async fn run_vars(
    api: &GraphqlProvider,
    cat: &Arc<Catalog>,
    user: Option<&User>,
    document: &str,
    variables: Json,
) -> Result<Json> {
    let resp = api
        .handle(
            ApiRequest::new(Method::Post, "/graphql")
                .body(json!({ "query": document, "variables": variables })),
            cat,
            user,
        )
        .await?;
    assert_eq!(resp.status, 200, "{}", resp.body);
    Ok(resp.body)
}

/// POST one document as `user`.
async fn run_as(
    api: &GraphqlProvider,
    cat: &Arc<Catalog>,
    user: Option<&User>,
    document: &str,
) -> Result<Json> {
    run_vars(api, cat, user, document, Json::Null).await
}

/// The `data` of a response, asserting there were no errors.
fn data(body: &Json) -> &Json {
    assert!(body.get("errors").is_none(), "{body}");
    &body["data"]
}

// ---------------------------------------------------------------------------
// Row-level security across a correlated subquery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_forced_child_tables_policies_bound_the_count_inside_the_parents_read() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    protect_employees(&cat).await?;
    let api = provider(&cat)?;

    // Ada owns two employees in Engineering and one in Sales. The count she
    // sees is *hers*, not the department's — and it is computed by the
    // database, inside the same statement that reads the departments, with the
    // employees' own SELECT policy applied to the subquery.
    let body = run_as(
        &api,
        &cat,
        Some(&caller("ada@example.com")?),
        "{ departments(order_by: [{ id: asc }]) { name employees_aggregate { count } } }",
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees_aggregate": { "count": 2 } },
            { "name": "Sales",       "employees_aggregate": { "count": 1 } },
            { "name": "Empty",       "employees_aggregate": { "count": 0 } },
        ])
    );

    // Bo's numbers are different, over the same query and the same schema —
    // which is the property a provider that had forgotten the caller could not
    // produce.
    let body = run_as(
        &api,
        &cat,
        Some(&caller("bo@example.com")?),
        "{ departments(order_by: [{ id: asc }]) { name employees_aggregate { count } } }",
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees_aggregate": { "count": 1 } },
            { "name": "Sales",       "employees_aggregate": { "count": 2 } },
            { "name": "Empty",       "employees_aggregate": { "count": 0 } },
        ])
    );

    // And the administrator, whom every policy's role floor admits, sees all
    // of them — so the smaller numbers above are the policy at work and not a
    // broken join.
    let body = run_as(
        &api,
        &cat,
        Some(&admin()?),
        "{ departments(order_by: [{ id: asc }]) { name employees_aggregate { count } } }",
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees_aggregate": { "count": 3 } },
            { "name": "Sales",       "employees_aggregate": { "count": 3 } },
            { "name": "Empty",       "employees_aggregate": { "count": 0 } },
        ])
    );
    Ok(())
}

#[tokio::test]
async fn an_anonymous_caller_counts_none_of_a_forced_child_table() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    protect_employees(&cat).await?;
    let api = provider(&cat)?;

    // Nobody is logged in, so `sc.user` is unset, so every policy's
    // `user === null` half decides — and a formula comparing a column to null
    // grants nothing. Zero here is the *truth*, and it is the fail-closed shape
    // §7.3 depends on rather than an accident: an unset GUC must never mean
    // "no restriction".
    let body = run_as(
        &api,
        &cat,
        None,
        "{ departments(order_by: [{ id: asc }]) { name employees_aggregate { count } } }",
    )
    .await?;
    assert_eq!(
        data(&body)["departments"],
        json!([
            { "name": "Engineering", "employees_aggregate": { "count": 0 } },
            { "name": "Sales",       "employees_aggregate": { "count": 0 } },
            { "name": "Empty",       "employees_aggregate": { "count": 0 } },
        ])
    );
    Ok(())
}

#[tokio::test]
async fn a_forced_child_tables_policies_also_bound_its_aggregated_values() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    protect_employees(&cat).await?;
    let api = provider(&cat)?;

    // `sum` is the aggregate a leak is hardest to see in: a total is plausible
    // whatever it is made of. Ada's Engineering total is her two salaries
    // (90 000 + 45 000), not the department's 195 000.
    let body = run_as(
        &api,
        &cat,
        Some(&caller("ada@example.com")?),
        r#"{
            departments(where: { id: { eq: 1 } }) {
                employees_aggregate { count sum { salary } max { salary } }
            }
        }"#,
    )
    .await?;
    assert_eq!(
        data(&body)["departments"][0]["employees_aggregate"],
        json!({ "count": 2, "sum": { "salary": 135000 }, "max": { "salary": 90000 } })
    );
    Ok(())
}

#[tokio::test]
async fn a_forced_child_list_and_its_aggregate_agree_about_which_rows_exist() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    protect_employees(&cat).await?;
    let api = provider(&cat)?;

    // The list is a read of the child table in its own right and the aggregate
    // is a subquery of the parent's read: two different statements, two
    // different code paths, one rule. If they disagreed, one of them would be
    // wrong — and the one that is wrong is the one nobody looks at.
    let body = run_as(
        &api,
        &cat,
        Some(&caller("bo@example.com")?),
        r#"{
            departments(order_by: [{ id: asc }]) {
                name
                employees(order_by: [{ id: asc }]) { name }
                employees_aggregate { count }
            }
        }"#,
    )
    .await?;
    let departments = &data(&body)["departments"];
    assert_eq!(
        departments,
        &json!([
            {
                "name": "Engineering",
                "employees": [{ "name": "Cleo" }],
                "employees_aggregate": { "count": 1 },
            },
            {
                "name": "Sales",
                "employees": [{ "name": "Eve" }, { "name": "Fritz" }],
                "employees_aggregate": { "count": 2 },
            },
            { "name": "Empty", "employees": [], "employees_aggregate": { "count": 0 } },
        ])
    );
    Ok(())
}

#[tokio::test]
async fn a_join_into_a_forced_table_reads_the_callers_own_row() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    // The other direction: an *outgoing* key into a protected table, projected
    // as a Ⱶ-join subquery of the child's own read. `departments` is the
    // protected one here, so an employee's `department { name }` runs the
    // departments' policy inside the employees' statement.
    bootstrap_table_meta(&cat).await?;
    save_table_meta(
        &cat,
        &TableMeta::new("employees").access(Role::PUBLIC, Role::ADMIN),
    )
    .await?;
    let mut departments = TableMeta::new("departments");
    departments.set_ownership_formula(Some("name === user.department"));
    departments.set_rls_enabled(true);
    save_table_meta(&cat, &departments).await?;
    enable_rls(&cat, &cat.require("departments")?).await?;
    let api = provider(&cat)?;

    let mut engineer = caller("ada@example.com")?;
    engineer = engineer.with_field("department".to_owned(), Value::Text("Engineering".into()));
    let body = run_as(
        &api,
        &cat,
        Some(&engineer),
        "{ employees(order_by: [{ id: asc }]) { name department { name } } }",
    )
    .await?;
    // Ada may see the Engineering department and no other, so the employees in
    // Sales come back with a nulled name rather than with "Sales". The
    // *employees* are all readable — this caller meets their floor — which is
    // what makes this a test of the join and not of the list.
    //
    // The relation is an object of nulls rather than a null object, and that is
    // the Ⱶ-join's shape rather than a decision taken here: a join projects one
    // subquery per requested *leaf*, so a hidden row and a row whose column is
    // genuinely null produce the same columns. What matters for this phase is
    // the value, and the value is not Sales.
    assert_eq!(
        data(&body)["employees"],
        json!([
            { "name": "Ada",   "department": { "name": "Engineering" } },
            { "name": "Bror",  "department": { "name": "Engineering" } },
            { "name": "Cleo",  "department": { "name": "Engineering" } },
            { "name": "Dag",   "department": { "name": null } },
            { "name": "Eve",   "department": { "name": null } },
            { "name": "Fritz", "department": { "name": null } },
        ])
    );
    Ok(())
}

#[tokio::test]
async fn a_key_is_not_a_way_around_the_targets_read_floor() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    bootstrap_table_meta(&cat).await?;
    // Employees are public; departments are admin-only, with no formula and no
    // policies. A list of departments is refused — and following the key into
    // one has to be refused for the same reason, or the floor means nothing:
    // `employees { department { name } }` would be a department reader.
    save_table_meta(
        &cat,
        &TableMeta::new("employees").access(Role::PUBLIC, Role::ADMIN),
    )
    .await?;
    save_table_meta(
        &cat,
        &TableMeta::new("departments").access(Role::ADMIN, Role::ADMIN),
    )
    .await?;
    let api = provider(&cat)?;
    let public = caller("ada@example.com")?;

    let body = run_as(&api, &cat, Some(&public), "{ departments { name } }").await?;
    assert!(body["errors"].to_string().contains("departments"), "{body}");

    let body = run_as(
        &api,
        &cat,
        Some(&public),
        "{ employees(limit: 1) { name department { name } } }",
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("departments"), "{body}");
    assert!(!errors.contains("Engineering"), "{body}");
    assert!(!body.to_string().contains("Engineering"), "{body}");

    // The key's own value is still theirs to read — it is a column of a row
    // they may read, and refusing it would be refusing their own data.
    let body = run_as(
        &api,
        &cat,
        Some(&public),
        "{ employees(order_by: [{ id: asc }], limit: 1) { name department_id: department { __typename } } }",
    )
    .await?;
    assert_eq!(data(&body)["employees"][0]["name"], json!("Ada"));
    Ok(())
}

#[tokio::test]
async fn a_key_into_a_table_owned_by_a_formula_is_refused_by_name() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    bootstrap_table_meta(&cat).await?;
    save_table_meta(
        &cat,
        &TableMeta::new("employees").access(Role::PUBLIC, Role::ADMIN),
    )
    .await?;
    // Sub-floor access granted by a formula: this caller may read *some*
    // departments. A Ⱶ-join cannot say which — its subquery is built from the
    // schema shape and has no `WHERE` this provider owns — so answering would
    // hand over a row the formula withholds, one column at a time. The refusal
    // names the table and says what to do instead.
    let mut departments = TableMeta::new("departments");
    departments.set_ownership_formula(Some("name === 'Engineering'"));
    save_table_meta(&cat, &departments).await?;
    let api = provider(&cat)?;
    let public = caller("ada@example.com")?;

    let body = run_as(
        &api,
        &cat,
        Some(&public),
        "{ employees(order_by: [{ id: asc }]) { name department { name } } }",
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("departments"), "{body}");
    assert!(errors.contains("ownership formula"), "{body}");
    assert!(!body.to_string().contains("Engineering"), "{body}");

    // Read directly and the formula does its job: Engineering and nothing else.
    let body = run_as(&api, &cat, Some(&public), "{ departments { name } }").await?;
    assert_eq!(
        data(&body)["departments"],
        json!([{ "name": "Engineering" }])
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_too_deep_query_is_refused_before_a_statement_is_issued() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider_with(&cat, GraphqlLimits::new().max_depth(4))?;

    // Two tables that reference each other are an unbounded query from a
    // document of a few hundred bytes. Depth is `async-graphql`'s own
    // validation, so the refusal happens over the parsed document — which is
    // the property worth having, and the one asserted here.
    clear(&sql);
    let body = run_as(
        &api,
        &cat,
        Some(&admin()?),
        "{ departments { employees { department { employees { name } } } } }",
    )
    .await?;
    let errors = body["errors"].to_string();
    // The refusal names the bound, because "Query is nested too deep" leaves
    // the person holding the query with nothing to change.
    assert!(errors.contains("4 levels deep"), "{body}");
    assert_eq!(recorded(&sql), Vec::<String>::new(), "{body}");

    // A query within the bound still runs, so the limit is a bound and not an
    // outage.
    clear(&sql);
    let body = run_as(&api, &cat, Some(&admin()?), "{ departments { name } }").await?;
    assert_eq!(data(&body)["departments"][0]["name"], json!("Engineering"));
    assert_eq!(recorded(&sql).len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_too_expensive_query_is_refused_before_a_statement_is_issued() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider_with(&cat, GraphqlLimits::new().max_complexity(8))?;

    // Complexity is the *width* a depth limit cannot see: this document is
    // three levels deep and names a dozen fields, once per alias, which is how
    // a shallow query is made enormous.
    clear(&sql);
    let body = run_as(
        &api,
        &cat,
        Some(&admin()?),
        r#"{
            a: departments { id name }
            b: departments { id name }
            c: departments { id name }
            d: departments { id name }
        }"#,
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("8 fields"), "{body}");
    assert_eq!(recorded(&sql), Vec::<String>::new(), "{body}");
    Ok(())
}

#[tokio::test]
async fn an_operation_that_spends_its_statement_budget_is_refused_rather_than_served() -> Result<()>
{
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    // Two statements: enough for a root read and one batched child level.
    let api = provider(&cat)?.with_statement_budget(2);

    clear(&sql);
    let body = run_as(
        &api,
        &cat,
        Some(&admin()?),
        "{ departments { employees { department { employees { name } } } } }",
    )
    .await?;
    let errors = body["errors"].to_string();
    assert!(errors.contains("database reads and writes"), "{body}");
    // Spent, not exceeded: the refusal replaced the third statement rather than
    // following it. The budget is the *work not done*.
    assert_eq!(recorded(&sql).len(), 2, "{:#?}", recorded(&sql));

    // The same query under the default budget is answered, so the refusal above
    // is the budget and not the query.
    let api = provider(&cat)?;
    let body = run_as(
        &api,
        &cat,
        Some(&admin()?),
        "{ departments(where: { id: { eq: 1 } }) { employees { name } } }",
    )
    .await?;
    assert_eq!(
        data(&body)["departments"][0]["employees"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );
    Ok(())
}

#[tokio::test]
async fn introspection_stays_on_under_the_default_limits() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    // The standard introspection query is deeper than any data query written by
    // hand — `__schema { types { fields { type { ofType … } } } }` unrolled
    // seven times — so a depth default chosen without it in mind would refuse
    // every GraphQL tool's very first request. This is that query's shape.
    clear(&sql);
    let body = run_as(
        &api,
        &cat,
        None,
        r#"{
            __schema {
                queryType { name }
                types {
                    name
                    fields {
                        name
                        type {
                            name kind
                            ofType { name kind
                                ofType { name kind
                                    ofType { name kind
                                        ofType { name kind
                                            ofType { name kind
                                                ofType { name kind } } } } } } }
                    }
                }
            }
        }"#,
    )
    .await?;
    let types = data(&body)["__schema"]["types"]
        .as_array()
        .expect("the type list")
        .clone();
    assert!(
        types.iter().any(|t| t["name"] == json!("Departments")),
        "{body}"
    );
    // And it cost nothing: introspection is answered from the schema, never
    // from the database.
    assert_eq!(recorded(&sql), Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn a_list_field_is_bounded_by_the_row_cap_the_caller_cannot_raise() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider_with(&cat, GraphqlLimits::new().row_cap(2))?;

    // A caller's `limit` is a request, not a permission: an absent one becomes
    // the cap and a larger one is clamped to it. Both, because "the default is
    // bounded" and "the argument is bounded" are different mistakes.
    for document in [
        "{ employees(order_by: [{ id: asc }]) { name } }",
        "{ employees(order_by: [{ id: asc }], limit: 100) { name } }",
    ] {
        let body = run_as(&api, &cat, Some(&admin()?), document).await?;
        assert_eq!(
            data(&body)["employees"],
            json!([{ "name": "Ada" }, { "name": "Bror" }]),
            "{document}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// What a caller writes must not reach SQL as an identifier
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_alias_changes_the_response_key_and_nothing_else() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    // A GraphQL alias is `/[_A-Za-z][_0-9A-Za-z]*/`, so it cannot *spell* a SQL
    // fragment — no quote, no space, no semicolon survives the parser. What it
    // can do is be an alias that looks like one, and reach a statement as a
    // column alias. This asserts the statement is the same either way, so the
    // response key is the only thing the caller chose.
    clear(&sql);
    let plain = run_as(
        &api,
        &cat,
        Some(&admin()?),
        "{ departments(where: { id: { eq: 1 } }) { name employees_aggregate { count } } }",
    )
    .await?;
    let plain_sql = recorded(&sql);

    clear(&sql);
    let aliased = run_as(
        &api,
        &cat,
        Some(&admin()?),
        r#"{
            departments_1__x: departments(where: { id: { eq: 1 } }) {
                DROP_TABLE_employees: name
                select_1_from_employees: employees_aggregate { count_x: count }
            }
        }"#,
    )
    .await?;
    let aliased_sql = recorded(&sql);

    // The answer moved, key for key…
    assert_eq!(
        data(&plain)["departments"],
        json!([{ "name": "Engineering", "employees_aggregate": { "count": 3 } }])
    );
    assert_eq!(
        data(&aliased)["departments_1__x"],
        json!([{
            "DROP_TABLE_employees": "Engineering",
            "select_1_from_employees": { "count_x": 3 },
        }])
    );
    // …and the only thing that changed in the statement is the aggregate's own
    // result alias, which is the response key by design (see `agg`'s module
    // doc). No alias became a table, a column or a clause: the `FROM` and the
    // `WHERE` are identical.
    assert_eq!(plain_sql.len(), 1);
    assert_eq!(aliased_sql.len(), 1);
    assert!(
        !aliased_sql[0].contains("DROP_TABLE_employees"),
        "an alias on a *column* reached the statement: {}",
        aliased_sql[0]
    );
    assert_eq!(
        plain_sql[0].replace("\"employees_aggregate.count\"", "X"),
        aliased_sql[0].replace("\"select_1_from_employees.count_x\"", "X"),
        "the statements differ by more than the aggregate's result alias"
    );
    Ok(())
}

#[tokio::test]
async fn a_fragment_resolves_exactly_as_the_selection_it_stands_for() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    // A fragment is a document-level indirection the executor flattens before a
    // resolver sees it. The claim is that the read behind it is the *same*
    // read — the projections come from the flattened selection set, so a
    // fragment cannot smuggle in a field the inline form would not have.
    clear(&sql);
    let inline = run_as(
        &api,
        &cat,
        Some(&admin()?),
        "{ departments(where: { id: { eq: 1 } }) { name employees(limit: 2) { name } } }",
    )
    .await?;
    let inline_sql = recorded(&sql);

    clear(&sql);
    let fragmented = run_as(
        &api,
        &cat,
        Some(&admin()?),
        r#"
        fragment DeptFields on Departments {
            name
            employees(limit: 2) { ...EmpFields }
        }
        fragment EmpFields on Employees { name }
        query { departments(where: { id: { eq: 1 } }) { ...DeptFields } }
        "#,
    )
    .await?;
    assert_eq!(data(&inline), data(&fragmented));
    assert_eq!(inline_sql, recorded(&sql));
    Ok(())
}

#[tokio::test]
async fn a_variable_is_bound_as_a_parameter_however_it_is_spelled() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, sql) = setup(&db).await?;
    let api = provider(&cat)?;

    // A variable is a *value*, and the row layer coerces it against the column
    // and the query layer binds it. Nothing about a quote or a semicolon in it
    // is special, because it never becomes text in a statement — which is what
    // this asserts, rather than asserting that some denylist rejected it.
    clear(&sql);
    let hostile = "'; DROP TABLE employees; --";
    let body = run_vars(
        &api,
        &cat,
        Some(&admin()?),
        "query ($name: String) { employees(where: { name: { eq: $name } }) { id } }",
        json!({ "name": hostile }),
    )
    .await?;
    assert_eq!(data(&body)["employees"], json!([]));
    let statements = recorded(&sql);
    assert_eq!(statements.len(), 1, "{statements:#?}");
    assert!(!statements[0].contains("DROP"), "{}", statements[0]);

    // And the table is still there, which is the blunt version of the same
    // claim.
    let body = run_as(
        &api,
        &cat,
        Some(&admin()?),
        "{ employees_aggregate { count } }",
    )
    .await?;
    assert_eq!(data(&body)["employees_aggregate"]["count"], json!(6));
    Ok(())
}

#[tokio::test]
async fn a_variable_naming_a_column_is_still_checked_against_the_catalog() -> Result<()> {
    let db = TestDb::new().await?;
    let (cat, _) = setup(&db).await?;
    let api = provider(&cat)?;

    // The one place a caller's input *is* about an identifier:
    // `count(distinct: Column)` takes an enum, and an enum through a variable
    // arrives as a string. The schema restricts it, and the aggregate checks it
    // against the table again anyway — because a column name is about to become
    // an identifier in a statement, and the catalog is the only thing that may
    // decide one.
    let body = run_vars(
        &api,
        &cat,
        Some(&admin()?),
        "query ($c: EmployeesSelectColumn!) { employees_aggregate { count(distinct: $c) } }",
        json!({ "c": "owner" }),
    )
    .await?;
    assert_eq!(data(&body)["employees_aggregate"]["count"], json!(2));

    // A name the catalog does not know is refused by the schema's own enum,
    // before anything is asked of the database.
    let body = run_vars(
        &api,
        &cat,
        Some(&admin()?),
        "query ($c: EmployeesSelectColumn!) { employees_aggregate { count(distinct: $c) } }",
        json!({ "c": "owner\") FROM employees; --" }),
    )
    .await?;
    assert!(body.get("errors").is_some(), "{body}");
    Ok(())
}

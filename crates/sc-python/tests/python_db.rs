//! Phase 2.3 of the Python code adapter: the `saltcorn` package's `db`.
//!
//! No database. What is under test here is the **lowering** — that a Python
//! chain produces exactly the plan the JavaScript chain produces, since that is
//! the whole of why the two languages cannot disagree about authority, budgets
//! or events. The host is a recorder: it keeps every plan it was handed and
//! answers whatever the test scripted. Phase 2.6 asks the same questions of a
//! real one.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_error::Result;
use sc_expr::{CodeCall, CodeHost};
use sc_python::PythonRuntime;
use serde_json::{Value as Json, json};

/// Keeps the plans and answers what it was told to, in order — the last answer
/// repeating once the script runs out, so a test that does not care how many
/// calls a terminal makes need not count them.
struct Recorder {
    plans: Mutex<Vec<Json>>,
    answers: Mutex<std::collections::VecDeque<Json>>,
    last: Mutex<Json>,
}

impl Recorder {
    fn answering(answers: Vec<Json>) -> Arc<Recorder> {
        let last = answers.last().cloned().unwrap_or(Json::Null);
        Arc::new(Recorder {
            plans: Mutex::new(Vec::new()),
            answers: Mutex::new(answers.into_iter().collect()),
            last: Mutex::new(last),
        })
    }

    /// A host for a read that answers no rows — enough for any test that only
    /// wants to see the plan.
    fn silent() -> Arc<Recorder> {
        Recorder::answering(vec![json!([])])
    }

    fn plans(&self) -> Vec<Json> {
        self.plans.lock().expect("not poisoned").clone()
    }

    /// The one plan this test made, which is the usual case.
    fn plan(&self) -> Json {
        let plans = self.plans();
        assert_eq!(plans.len(), 1, "expected one plan, got {plans:?}");
        plans.into_iter().next().expect("one plan")
    }
}

#[async_trait]
impl CodeHost for Recorder {
    async fn call(&self, request: Json) -> Result<Json> {
        self.plans.lock().expect("not poisoned").push(request);
        match self.answers.lock().expect("not poisoned").pop_front() {
            Some(answer) => answer,
            None => self.last.lock().expect("not poisoned").clone(),
        }
        .pipe_ok()
    }
}

/// `Ok(self)`, so the `match` above reads as one expression.
trait PipeOk: Sized {
    fn pipe_ok(self) -> Result<Self> {
        Ok(self)
    }
}
impl PipeOk for Json {}

/// Run `code` against `host` and answer what the body returned.
async fn run(host: &Arc<Recorder>, code: &str) -> Json {
    PythonRuntime::new()
        .run(CodeCall {
            code: code.to_owned(),
            host: Some(host.as_ref() as &dyn CodeHost),
            ..CodeCall::default()
        })
        .await
        .unwrap_or_else(|e| panic!("the body failed: {e}\n--- body ---\n{code}"))
}

/// Run `code` and answer the error it failed with.
async fn fails(host: &Arc<Recorder>, code: &str) -> String {
    PythonRuntime::new()
        .run(CodeCall {
            code: code.to_owned(),
            host: Some(host.as_ref() as &dyn CodeHost),
            ..CodeCall::default()
        })
        .await
        .expect_err("this body was supposed to fail")
        .to_string()
}

// ---------------------------------------------------------------------------
// The chain
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whole_chain_lowers_to_the_plan_the_javascript_chain_lowers_to() {
    let host = Recorder::silent();
    run(
        &host,
        r#"
return (db.invoices
    .where(paid=False, due__lt="2026-01-01")
    .where("amount > 10")
    .select("id", "amount", "customerⱵemail", chased="remindersↃinvoice.length")
    .order_by("due")
    .order_by("id", "desc")
    .limit(50)
    .offset(10)
    .rows())
"#,
    )
    .await;
    assert_eq!(
        host.plan(),
        json!({
            "op": "select",
            "table": "invoices",
            "authority": "default",
            "where": { "and": [
                { "paid": false, "due": { "lt": "2026-01-01" } },
                { "formula": "amount > 10" },
            ]},
            "select": [
                "id",
                "amount",
                "customerⱵemail",
                { "alias": "chased", "formula": "remindersↃinvoice.length" },
            ],
            "order": [
                { "field": "due", "dir": "asc" },
                { "field": "id", "dir": "desc" },
            ],
            "limit": 50,
            "offset": 10,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_two_where_spellings_and_the_keyword_operators_are_one_filter() {
    // Every operator, spelled as a keyword suffix.
    let host = Recorder::silent();
    run(
        &host,
        r#"
db.books.where(
    a__eq=1, b__ne=2, c__gt=3, d__gte=4, e__lt=5, f__lte=6,
    g__in=[1, 2], h__nin=[3], i__like="%x%", j__ilike="%y%", k__is_null=True,
).rows()
"#,
    )
    .await;
    assert_eq!(
        host.plan()["where"],
        json!({
            "a": { "eq": 1 }, "b": { "ne": 2 }, "c": { "gt": 3 }, "d": { "gte": 4 },
            "e": { "lt": 5 }, "f": { "lte": 6 }, "g": { "in": [1, 2] },
            "h": { "nin": [3] }, "i": { "like": "%x%" }, "j": { "ilike": "%y%" },
            "k": { "is_null": true },
        })
    );

    // A bare keyword is equality, and two comparisons on **one** field cannot
    // share a key — so they become the `and` the flat object would have meant.
    let host = Recorder::silent();
    run(
        &host,
        "db.books.where(title=\"Dune\", pages__gt=100, pages__lt=500).rows()",
    )
    .await;
    assert_eq!(
        host.plan()["where"],
        json!({ "and": [
            { "title": "Dune" },
            { "pages": { "gt": 100 } },
            { "pages": { "lt": 500 } },
        ]})
    );

    // The object DSL every other surface speaks, and the spelled-out
    // combinators over it.
    let host = Recorder::silent();
    run(
        &host,
        r#"
import saltcorn as sc
db.books.where(sc.or_({"pages": {"gt": 100}}, sc.not_("author == null"))).rows()
"#,
    )
    .await;
    assert_eq!(
        host.plan()["where"],
        json!({ "or": [
            { "pages": { "gt": 100 } },
            { "not": { "formula": "author == null" } },
        ]})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chain_is_pure_so_a_query_can_be_narrowed_two_ways() {
    let host = Recorder::silent();
    run(
        &host,
        r#"
base = db.books.where(pages__gt=100)
base.where(author="Herbert").rows()
base.where(author="Le Guin").rows()
"#,
    )
    .await;
    let plans = host.plans();
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0]["where"],
        json!({ "and": [{ "pages": { "gt": 100 } }, { "author": "Herbert" }] })
    );
    assert_eq!(
        plans[1]["where"],
        json!({ "and": [{ "pages": { "gt": 100 } }, { "author": "Le Guin" }] })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aggregate_with_a_group_and_a_having_is_one_plan() {
    let host = Recorder::answering(vec![json!([{ "author": "Herbert", "n": 2 }])]);
    let out = run(
        &host,
        r#"
return (db.books
    .group_by("author")
    .aggregate(n="count()", pages="sum(pages)")
    .having(n__gt=1)
    .rows())
"#,
    )
    .await;
    assert_eq!(
        host.plan(),
        json!({
            "op": "aggregate",
            "table": "books",
            "authority": "default",
            "group": ["author"],
            "aggregate": [
                { "alias": "n", "fn": "count", "arg": null },
                { "alias": "pages", "fn": "sum", "arg": "pages" },
            ],
            "having": { "n": { "gt": 1 } },
        })
    );
    assert_eq!(out, json!([{ "author": "Herbert", "n": 2 }]));

    // A grouped query answers groups, so a scalar terminal says so rather than
    // answering the first group's value.
    let host = Recorder::silent();
    let said = fails(&host, "return db.books.group_by(\"author\").count()").await;
    assert!(said.contains(".count() answers one value"), "{said}");
    assert!(said.contains("aggregate("), "{said}");
}

// ---------------------------------------------------------------------------
// The terminals
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_scalar_terminal_is_one_nameless_group_unwrapped() {
    for (call, func, arg) in [
        ("count()", "count", Json::Null),
        ("sum(\"pages\")", "sum", json!("pages")),
        ("avg(\"pages\")", "avg", json!("pages")),
        ("min(\"pages\")", "min", json!("pages")),
        ("max(\"pages\")", "max", json!("pages")),
    ] {
        let host = Recorder::answering(vec![json!({ "value": 42 })]);
        let out = run(&host, &format!("return db.books.{call}")).await;
        assert_eq!(out, json!(42), "{call}");
        assert_eq!(
            host.plan(),
            json!({
                "op": "aggregate",
                "table": "books",
                "authority": "default",
                "aggregate": [{ "alias": "value", "fn": func, "arg": arg }],
            }),
            "{call}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_get_and_exists_all_ask_for_one_row() {
    let host = Recorder::answering(vec![json!([{ "id": 1 }])]);
    assert_eq!(
        run(&host, "return db.books.first()").await,
        json!({"id": 1})
    );
    assert_eq!(host.plan()["limit"], json!(1));

    let host = Recorder::answering(vec![json!([])]);
    assert_eq!(run(&host, "return db.books.first()").await, Json::Null);

    let host = Recorder::answering(vec![json!([{ "id": 7 }])]);
    assert_eq!(run(&host, "return db.books.get(7)").await, json!({"id": 7}));
    assert_eq!(host.plan()["pk"], json!(7));

    let host = Recorder::answering(vec![json!([{ "id": 1 }])]);
    assert_eq!(run(&host, "return db.books.exists()").await, json!(true));
    let host = Recorder::answering(vec![json!([])]);
    assert_eq!(run(&host, "return db.books.exists()").await, json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_is_spelled_three_ways_and_means_the_same_plan() {
    for body in [
        "return db.books.insert(title=\"Dune\", pages=412)",
        "return db.books.insert({\"title\": \"Dune\", \"pages\": 412})",
    ] {
        let host = Recorder::answering(vec![json!({ "id": 1 })]);
        run(&host, body).await;
        assert_eq!(
            host.plan(),
            json!({
                "op": "insert",
                "table": "books",
                "authority": "default",
                "values": { "title": "Dune", "pages": 412 },
            }),
            "{body}"
        );
    }

    // A list inserts several, which is the host's own `many` case.
    let host = Recorder::answering(vec![json!([{ "id": 1 }, { "id": 2 }])]);
    run(
        &host,
        "return db.books.insert([{\"title\": \"a\"}, {\"title\": \"b\"}])",
    )
    .await;
    assert_eq!(
        host.plan()["values"],
        json!([{"title": "a"}, {"title": "b"}])
    );

    // An update carries its assignments and its filter.
    let host = Recorder::answering(vec![json!({ "updated": 1, "ids": [3] })]);
    let out = run(&host, "return db.books.where(id=3).update(pages=500)").await;
    assert_eq!(out, json!({ "updated": 1, "ids": [3] }));
    assert_eq!(
        host.plan(),
        json!({
            "op": "update",
            "table": "books",
            "authority": "default",
            "where": { "id": 3 },
            "values": { "pages": 500 },
        })
    );

    let host = Recorder::answering(vec![json!({ "deleted": 2, "ids": [1, 2] })]);
    run(&host, "return db.books.where(pages__lt=10).delete()").await;
    assert_eq!(host.plan()["op"], json!("delete"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whole_table_write_is_refused_where_the_author_can_see_it() {
    for (body, verb) in [
        ("db.books.update(pages=1)", "update"),
        ("db.books.delete()", "delete"),
    ] {
        let host = Recorder::silent();
        // Caught as a `DbError`, because it is the same refusal the host makes —
        // and refused **before** the plan is sent, so nothing was asked.
        let out = run(
            &host,
            &format!(
                "import saltcorn as sc\ntry:\n    {body}\nexcept sc.DbError as e:\n    \
                 return str(e)\nreturn None"
            ),
        )
        .await;
        let said = out.as_str().unwrap_or_default().to_owned();
        assert!(said.contains("would touch every row"), "{verb}: {said}");
        assert!(said.contains(&format!("db.books.{verb}()")), "{said}");
        assert!(host.plans().is_empty(), "{verb}: nothing should be sent");
    }
}

// ---------------------------------------------------------------------------
// `.iter()`
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn iter_walks_more_rows_than_one_batch_and_the_query_itself_is_the_same_walk() {
    let batches = || {
        vec![
            json!({ "rows": [{ "id": 1 }, { "id": 2 }], "cursor": [2] }),
            json!({ "rows": [{ "id": 3 }, { "id": 4 }], "cursor": [4] }),
            json!({ "rows": [{ "id": 5 }], "cursor": null }),
        ]
    };
    let host = Recorder::answering(batches());
    let out = run(
        &host,
        "return [row[\"id\"] for row in db.books.order_by(\"id\").iter(2)]",
    )
    .await;
    assert_eq!(out, json!([1, 2, 3, 4, 5]));
    let plans = host.plans();
    assert_eq!(plans.len(), 3, "one host call per batch");
    assert_eq!(plans[0]["cursor"], json!(true));
    assert_eq!(plans[0]["limit"], json!(2));
    assert!(
        plans[0].get("after").is_none(),
        "the first batch resumes nothing"
    );
    assert_eq!(plans[1]["after"], json!([2]));
    assert_eq!(plans[2]["after"], json!([4]));

    // Iterating the query itself is the same walk, so a `for` loop over a chain
    // needs no terminal at all.
    let host = Recorder::answering(batches());
    let out = run(
        &host,
        "return [row[\"id\"] for row in db.books.order_by(\"id\")]",
    )
    .await;
    assert_eq!(out, json!([1, 2, 3, 4, 5]));

    // A `.limit()` bounds the **iteration** and is spent by stopping: nothing is
    // fetched once it is reached, which is what makes an early `break` cheap.
    let host = Recorder::answering(batches());
    let out = run(
        &host,
        "return [row[\"id\"] for row in db.books.order_by(\"id\").limit(3).iter(2)]",
    )
    .await;
    assert_eq!(out, json!([1, 2, 3]));
    assert_eq!(host.plans().len(), 2, "the third batch is never asked for");
}

// ---------------------------------------------------------------------------
// Authority, and the body's own SQL
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_authority_is_the_runs_default_until_the_body_delegates() {
    let host = Recorder::silent();
    run(
        &host,
        r#"
db.books.rows()
db.as_user().books.rows()
db.books.as_user().rows()
db.as_user().books.as_admin().rows()
"#,
    )
    .await;
    let plans = host.plans();
    let authorities: Vec<&str> = plans
        .iter()
        .map(|p| p["authority"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(authorities, vec!["default", "user", "user", "admin"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bodys_own_sql_sends_its_text_and_its_binds_apart() {
    let host = Recorder::answering(vec![json!([{ "n": 1 }])]);
    let out = run(
        &host,
        "return db.sql(\"select * from books where pages > $1\", [200])",
    )
    .await;
    assert_eq!(out, json!([{ "n": 1 }]));
    assert_eq!(
        host.plan(),
        json!({
            "op": "sql",
            "authority": "default",
            "sql": "select * from books where pages > $1",
            "params": [200],
        })
    );

    // Delegated two ways, which are the same call.
    let host = Recorder::silent();
    run(
        &host,
        r#"
db.sql("select 1", [], as_user=True)
db.as_user().sql("select 1")
db.as_user().sql("select 1", as_user=False)
"#,
    )
    .await;
    let plans = host.plans();
    let authorities: Vec<&str> = plans
        .iter()
        .map(|p| p["authority"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(authorities, vec!["user", "user", "admin"]);
}

// ---------------------------------------------------------------------------
// The package itself
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_package_is_importable_and_the_handle_is_the_one_in_scope() {
    let host = Recorder::silent();
    let out = run(
        &host,
        r#"
import saltcorn
import saltcorn as sc
return {
    "same": saltcorn.db is db,
    "aliased": sc is saltcorn,
    "errors": [
        issubclass(sc.DbError, sc.SaltcornError),
        issubclass(sc.Timeout, BaseException) and not issubclass(sc.Timeout, Exception),
    ],
    "table": repr(db.table("customer orders")),
}
"#,
    )
    .await;
    assert_eq!(out["same"], json!(true));
    assert_eq!(out["aliased"], json!(true));
    assert_eq!(out["errors"], json!([true, true]));
    assert_eq!(
        out["table"],
        json!("<saltcorn query on `customer orders` as default>"),
        "a name that is not an identifier is reached with db.table(...)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_with_no_database_has_no_db_bound_at_all() {
    let error = PythonRuntime::new()
        .run(CodeCall {
            code: "return db.books.rows()".to_owned(),
            ..CodeCall::default()
        })
        .await
        .expect_err("a pure body has no database");
    let said = error.to_string();
    assert!(said.contains("NameError"), "{said}");
    assert!(said.contains("db"), "{said}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mistake_in_the_chain_is_a_python_error_and_not_a_refusal() {
    // An author's slip is a `TypeError`/`ValueError` — what a Python author
    // expects of a library, and what a bare `except DbError` in a retry loop
    // must not swallow. Nothing is sent for any of them.
    for (body, kind, hint) in [
        (
            "db.books.order_by(\"id\", \"sideways\").rows()",
            "ValueError",
            "asc",
        ),
        (
            "db.books.where().rows()",
            "TypeError",
            "where() needs a condition",
        ),
        (
            "db.books.where(12).rows()",
            "TypeError",
            "a condition is keywords",
        ),
        (
            "db.books.aggregate(n=\"sum\").rows()",
            "ValueError",
            "is not an aggregate",
        ),
        (
            "db.books.select(12).rows()",
            "TypeError",
            "select() takes field names",
        ),
        // `.iter()` is a generator, so its argument check lands at the first
        // `next()` rather than at the call — which is where the loop is, so the
        // author sees it in the same place either way.
        (
            "list(db.books.iter(0))",
            "ValueError",
            "how many rows to read at a time",
        ),
        (
            "db.books.group_by(\"author\").rows()",
            "ValueError",
            "needs an .aggregate",
        ),
    ] {
        let host = Recorder::silent();
        let said = fails(&host, body).await;
        assert!(said.contains(kind), "{body}: {said}");
        assert!(said.contains(hint), "{body}: {said}");
        assert!(host.plans().is_empty(), "{body}: nothing should be sent");
    }
}

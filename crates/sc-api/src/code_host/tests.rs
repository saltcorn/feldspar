//! What a plan becomes, and what it is refused for.
//!
//! Every assertion here is about the step between the guest and the database:
//! the plan arrives as JSON, and what leaves is a statement whose every
//! identifier came from the catalog and whose every literal is a bound
//! parameter. The catalog is real (introspected from a driver with no rows
//! behind it), so a join path is resolved through the same schema shape a
//! formula is validated against — and no statement is ever run, which is
//! precisely the point: a plan that reaches SQL wrongly is a bug that must be
//! visible before a database is involved.

use std::sync::Arc;

use sc_auth::ROLE_ADMIN;
use sc_catalog::Catalog;
use sc_db::PhysicalTable;
use sc_error::Result;
use sc_query::{OrderDir, Projection, Select, Source, SqlDialect, Statement, Value};
use serde_json::{Value as Json, json};

use super::plan::{self, Read};
use super::{Authority, HostLimits, Plan};
use crate::graphql::testing::{catalog_of, physical};

/// Postgres-flavoured rendering, as in `sc-query`'s own tests.
struct Pg;

impl SqlDialect for Pg {
    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
    fn placeholder(&self, position: usize) -> String {
        format!("${position}")
    }
}

/// `books` with an author key, `authors` behind it, and `reviews` in front of it
/// — the smallest schema that has a Ⱶ-path and a Ↄ-aggregation in it.
async fn library() -> Arc<Catalog> {
    let tables: Vec<PhysicalTable> = vec![
        physical(
            "books",
            &[
                ("id", "int8", false),
                ("title", "text", true),
                ("pages", "int8", true),
                ("published", "date", true),
                ("price", "numeric", true),
                ("author", "int8", true),
            ],
            &[("author", "authors", "id")],
        ),
        physical(
            "authors",
            &[
                ("id", "int8", false),
                ("name", "text", true),
                ("country", "text", true),
            ],
            &[],
        ),
        physical(
            "reviews",
            &[
                ("id", "int8", false),
                ("book", "int8", true),
                ("stars", "int8", true),
            ],
            &[("book", "books", "id")],
        ),
    ];
    catalog_of(tables).await
}

/// A role-40 reader, as the JSON object an event carries its caller as.
fn caller() -> Json {
    json!({ "id": uuid::Uuid::nil().to_string(), "email": "ada@example.com" })
}

/// Resolve one plan (as the guest would send it) into a read.
async fn read_of(plan: Json) -> Result<Read> {
    let cat = library().await;
    let plan: Plan = serde_json::from_value(plan).expect("a well-formed plan");
    plan::read(&cat, &plan, &HostLimits::default(), ROLE_ADMIN)
}

/// The message a plan is refused with.
async fn refusal(plan: Json) -> String {
    let cat = library().await;
    let parsed: std::result::Result<Plan, _> = serde_json::from_value(plan);
    match parsed {
        // A plan the seam does not describe is refused where every other malformed
        // request is: at the boundary, by serde, naming the field.
        Err(e) => format!("{e}"),
        Ok(plan) => match plan.op {
            super::Op::Aggregate => {
                plan::aggregate(&cat, &plan, &HostLimits::default(), ROLE_ADMIN)
                    .err()
                    .expect("this plan is refused")
                    .to_string()
            }
            super::Op::Insert => plan::insert(&cat, &plan, &HostLimits::default())
                .err()
                .expect("this plan is refused")
                .to_string(),
            super::Op::Update | super::Op::Delete => {
                plan::write(&cat, &plan, &HostLimits::default(), ROLE_ADMIN)
                    .err()
                    .expect("this plan is refused")
                    .to_string()
            }
            _ => plan::read(&cat, &plan, &HostLimits::default(), ROLE_ADMIN)
                .err()
                .expect("this plan is refused")
                .to_string(),
        },
    }
}

/// The `SELECT` a read lowers to, rendered — the row's own columns plus whatever
/// the plan projected, filtered, ordered and bounded.
fn rendered(read: &Read) -> (String, Vec<Value>) {
    let mut columns = vec![Projection::all()];
    columns.extend(read.query.extra.iter().cloned());
    let mut select = Select::from(Source::table(read.table.name.clone())).columns(columns);
    if let Some(filter) = read.query.filter.clone() {
        select = select.filter(filter);
    }
    select.order = read.query.order.clone();
    select.limit = read.query.limit;
    select.offset = read.query.offset;
    Pg.render(&Statement::Select(Box::new(select)))
        .expect("renders")
}

/// The `SELECT` an aggregate lowers to — the projections, the grouping, the
/// bound on the groups and the query's own order and limit.
fn rendered_aggregate(agg: &plan::Aggregate) -> (String, Vec<Value>) {
    let mut select =
        Select::from(Source::table(agg.table.name.clone())).columns(agg.projections.clone());
    select.filter = agg.query.filter.clone();
    select.group = agg.query.group.clone();
    select.having = agg.query.having.clone();
    select.order = agg.query.order.clone();
    select.limit = agg.query.limit;
    select.offset = agg.query.offset;
    Pg.render(&Statement::Select(Box::new(select)))
        .expect("renders")
}

/// The `WHERE` clause of a plan's filter, and its bound parameters — rendered
/// on its own, so the read's bound does not appear among them.
async fn where_of(filter: Json) -> Result<(String, Vec<Value>)> {
    let read = read_of(json!({ "op": "select", "table": "books", "where": filter })).await?;
    let select =
        Select::from(Source::table("books")).filter(read.query.filter.clone().expect("a filter"));
    let (sql, binds) = Pg
        .render(&Statement::Select(Box::new(select)))
        .expect("renders");
    Ok((
        sql.split_once(" WHERE ")
            .map(|(_, w)| w.to_owned())
            .unwrap_or_else(|| panic!("no WHERE in {sql}")),
        binds,
    ))
}

#[tokio::test]
async fn a_join_projection_is_a_correlated_subquery_aliased_by_the_path() {
    // `db.books.select("id", "authorⱵname")` — the Ⱶ-path is not a column of
    // `books`, so it becomes a subquery projected beside the row, under the path
    // itself as its key. Which is exactly how the guest reads it back:
    // `row.authorⱵname`, one identifier.
    let read = read_of(json!({
        "op": "select",
        "table": "books",
        "select": ["id", "title", "authorⱵname"],
    }))
    .await
    .expect("resolves");
    let (sql, _) = rendered(&read);
    assert!(
        sql.contains(
            "(SELECT \"_fd_j1\".\"name\" FROM \"authors\" AS \"_fd_j1\" \
             WHERE (\"_fd_j1\".\"id\" = \"books\".\"author\")) AS \"authorⱵname\""
        ),
        "{sql}"
    );
    // A column of the table is *not* projected twice: it is already in the row.
    assert_eq!(read.query.extra.len(), 1, "only the path is a projection");

    // And the answer carries exactly the keys the plan asked for, in its own
    // order, as the REST wire shape.
    let mut values = std::collections::BTreeMap::new();
    values.insert("id".to_owned(), Value::Int(3));
    values.insert("title".to_owned(), Value::Text("Orlando".into()));
    values.insert("authorⱵname".to_owned(), Value::Text("Woolf".into()));
    values.insert("pages".to_owned(), Value::Int(200));
    assert_eq!(
        read.row(&values),
        json!({ "id": 3, "title": "Orlando", "authorⱵname": "Woolf" }),
        "a projected read answers what it selected and not the whole row"
    );
}

#[tokio::test]
async fn a_formula_projection_carries_a_child_aggregation_into_the_same_statement() {
    // The milestone's own example, in miniature: `{ chased: "reviewsↃbook.length" }`
    // is one correlated aggregate, projected under the alias the body named.
    let read = read_of(json!({
        "op": "select",
        "table": "books",
        "select": [
            "id",
            { "alias": "reviews", "formula": "reviewsↃbook.length" },
            { "alias": "net", "formula": "price * 2" },
        ],
    }))
    .await
    .expect("resolves");
    let (sql, _) = rendered(&read);
    assert!(
        sql.contains("count(*) FROM \"reviews\"") && sql.contains("AS \"reviews\""),
        "{sql}"
    );
    assert!(sql.contains("(\"books\".\"price\" * $"), "{sql}");
}

#[tokio::test]
async fn every_comparison_and_the_combinators_lower_through_the_shared_vocabulary() {
    for (op, sql_op) in [
        ("eq", "="),
        ("ne", "<>"),
        ("gt", ">"),
        ("gte", ">="),
        ("lt", "<"),
        ("lte", "<="),
    ] {
        let (sql, binds) = where_of(json!({ "pages": { op: 300 } })).await.expect("ok");
        assert_eq!(sql, format!("(\"pages\" {sql_op} $1)"));
        assert_eq!(binds, vec![Value::Int(300)]);
    }
    let (sql, binds) = where_of(json!({ "pages": { "in": [1, 2] } }))
        .await
        .expect("ok");
    assert_eq!(sql, "(\"pages\" IN ($1, $2))");
    assert_eq!(binds, vec![Value::Int(1), Value::Int(2)]);
    let (sql, _) = where_of(json!({ "pages": { "nin": [1] } }))
        .await
        .expect("ok");
    assert_eq!(sql, "(NOT (\"pages\" IN ($1)))");
    let (sql, _) = where_of(json!({ "title": { "is_null": true } }))
        .await
        .expect("ok");
    assert_eq!(sql, "(\"title\" IS NULL)");
    let (sql, binds) = where_of(json!({ "title": { "ilike": "%woolf%" } }))
        .await
        .expect("ok");
    assert_eq!(sql, "(\"title\" ILIKE $1)");
    assert_eq!(binds, vec![Value::Text("%woolf%".into())]);

    // A date binds as a *date*, because the coercion goes through the column —
    // the same reason a REST query string's `gte.2020-01-01` does.
    let (_, binds) = where_of(json!({ "published": { "lt": "2026-08-17" } }))
        .await
        .expect("ok");
    assert!(
        matches!(binds.as_slice(), [Value::Date(_)]),
        "expected a bound date, got {binds:?}"
    );

    // The combinators, which this milestone added to the shared walk — so an
    // agent's `where` gained them at the same moment.
    let (sql, _) = where_of(json!({
        "or": [
            { "title": "Orlando" },
            { "and": [ { "pages": { "gt": 100 } }, { "not": { "title": { "is_null": true } } } ] },
        ]
    }))
    .await
    .expect("ok");
    assert_eq!(
        sql,
        "((\"title\" = $1) OR ((\"pages\" > $2) AND (NOT (\"title\" IS NULL))))"
    );
}

#[tokio::test]
async fn the_formula_spelling_of_a_filter_reaches_the_same_where() {
    // §3's two spellings. This one is what `update_rows`/`delete_rows` already
    // take, and it lowers through the same translator a calculated field does.
    let (sql, binds) = where_of(json!({ "formula": "pages > 100 && title !== null" }))
        .await
        .expect("ok");
    assert_eq!(
        sql,
        "((\"books\".\"pages\" > $1) AND (\"books\".\"title\" IS NOT NULL))"
    );
    assert_eq!(binds, vec![Value::Int(100)]);

    // A Ⱶ-path as a filter *key* compares the joined column, and the literal is
    // coerced against the column it belongs to — on `authors`, not on `books`.
    let (sql, binds) = where_of(json!({ "authorⱵcountry": "GB" }))
        .await
        .expect("ok");
    assert!(sql.contains("FROM \"authors\" AS \"_fd_j1\""), "{sql}");
    assert_eq!(binds, vec![Value::Text("GB".into())]);

    // The two spellings **mix**, which is what two `.where()` calls produce:
    // the prelude ANDs them, so the formula arrives nested inside a combinator
    // and has to be recognised there rather than only at the top.
    let (sql, binds) = where_of(json!({
        "and": [ { "title": "Orlando" }, { "formula": "pages > 100" } ]
    }))
    .await
    .expect("ok");
    assert_eq!(sql, "((\"title\" = $1) AND (\"books\".\"pages\" > $2))");
    assert_eq!(binds, vec![Value::Text("Orlando".into()), Value::Int(100)]);
}

#[tokio::test]
async fn order_limit_and_offset_are_the_plans_own() {
    let read = read_of(json!({
        "op": "select",
        "table": "books",
        "order": [ { "field": "published", "dir": "desc" }, { "field": "authorⱵname" } ],
        "limit": 10,
        "offset": 20,
    }))
    .await
    .expect("resolves");
    assert_eq!(read.query.order.len(), 2);
    assert_eq!(read.query.order[0].dir, OrderDir::Desc);
    assert_eq!(read.query.order[1].dir, OrderDir::Asc);
    assert_eq!(read.query.limit, Some(10));
    assert_eq!(read.query.offset, Some(20));
    // An ordering by a Ⱶ-path is the same correlated subquery a projection is.
    let (sql, _) = rendered(&read);
    assert!(
        sql.contains("ORDER BY \"published\" DESC, (SELECT \"_fd_j1\".\"name\""),
        "{sql}"
    );
    assert!(sql.contains("LIMIT $1 OFFSET $2"), "{sql}");
}

#[tokio::test]
async fn a_read_with_no_limit_asks_for_one_row_more_than_the_cap() {
    // How "more than you may hold" is told apart from "exactly the cap": the
    // statement asks for one more, and the host refuses when it arrives.
    let read = read_of(json!({ "op": "select", "table": "books" }))
        .await
        .expect("resolves");
    assert_eq!(read.query.limit, Some(super::DEFAULT_MAX_ROWS + 1));
    // With no `select`, the answer is the whole row — the table's own fields.
    let mut values = std::collections::BTreeMap::new();
    values.insert("id".to_owned(), Value::Int(1));
    values.insert("title".to_owned(), Value::Text("Emma".into()));
    let row = read.row(&values);
    assert_eq!(row["id"], json!(1));
    assert_eq!(row["title"], json!("Emma"));
}

#[tokio::test]
async fn an_aggregate_plan_is_one_projection_per_value_over_the_filtered_rows() {
    let cat = library().await;
    let plan: Plan = serde_json::from_value(json!({
        "op": "aggregate",
        "table": "books",
        "where": { "author": 3 },
        "aggregate": [
            { "alias": "value", "fn": "sum", "arg": "pages" },
            { "alias": "n", "fn": "count", "arg": Json::Null },
            { "alias": "worth", "fn": "max", "arg": "price * 2" },
        ],
    }))
    .expect("a plan");
    let agg = plan::aggregate(&cat, &plan, &HostLimits::default(), ROLE_ADMIN).expect("resolves");
    assert!(!agg.grouped, "nothing to group by is one row");
    let (sql, _) = rendered_aggregate(&agg);
    assert!(
        sql.contains("COALESCE(sum(\"pages\"), $1) AS \"value\""),
        "{sql}"
    );
    assert!(sql.contains("count(*) AS \"n\""), "{sql}");
    assert!(sql.contains("max((\"books\".\"price\" * $"), "{sql}");
    assert!(sql.contains("WHERE (\"author\" = $"), "{sql}");
}

#[tokio::test]
async fn a_grouped_aggregate_projects_its_keys_beside_its_values_and_bounds_the_groups() {
    // `db.books.where({ pages: { gt: 100 } }).groupBy("authorⱵname")
    //     .aggregate({ n: "count()", pages: "sum(pages)" })
    //     .having({ n: { gt: 1 } }).orderBy("n", "desc").limit(5).rows()`
    let cat = library().await;
    let plan: Plan = serde_json::from_value(json!({
        "op": "aggregate",
        "table": "books",
        "where": { "pages": { "gt": 100 } },
        "group": ["authorⱵname"],
        "aggregate": [
            { "alias": "n", "fn": "count", "arg": Json::Null },
            { "alias": "pages", "fn": "sum", "arg": "pages" },
        ],
        "having": { "n": { "gt": 1 } },
        "order": [ { "field": "n", "dir": "desc" } ],
        "limit": 5,
    }))
    .expect("a plan");
    let agg = plan::aggregate(&cat, &plan, &HostLimits::default(), ROLE_ADMIN).expect("resolves");
    assert!(agg.grouped);
    assert_eq!(agg.keys, vec!["authorⱵname", "n", "pages"], "keys in order");
    let (sql, binds) = rendered_aggregate(&agg);

    // The group key is a Ⱶ-path, so it is the same correlated subquery a
    // projection or an ordering would be — projected under the path itself and
    // grouped by the expression, because an output alias is not in scope there.
    let joined = "(SELECT \"_fd_j1\".\"name\" FROM \"authors\" AS \"_fd_j1\" \
                  WHERE (\"_fd_j1\".\"id\" = \"books\".\"author\"))";
    assert!(
        sql.contains(&format!("{joined} AS \"authorⱵname\"")),
        "{sql}"
    );
    assert!(sql.contains(&format!("GROUP BY {joined}")), "{sql}");
    // The values, the bound on the groups (the aggregate repeated, not the
    // alias), the ordering by one of them, and the limit.
    assert!(sql.contains("count(*) AS \"n\""), "{sql}");
    assert!(sql.contains("AS \"pages\""), "{sql}");
    assert!(sql.contains("HAVING (count(*) > $"), "{sql}");
    assert!(sql.contains("ORDER BY count(*) DESC"), "{sql}");
    assert!(sql.contains("LIMIT $"), "{sql}");
    assert!(
        binds.contains(&Value::Int(100)) && binds.contains(&Value::Int(1)),
        "{binds:?}"
    );
}

#[tokio::test]
async fn a_grouped_read_is_bounded_and_its_having_names_only_its_own_values() {
    // Without a `.limit()` a grouped aggregate asks for one group more than the
    // cap, exactly as a row read does — the groups are materialised too.
    let cat = library().await;
    let plan: Plan = serde_json::from_value(json!({
        "op": "aggregate", "table": "books", "group": ["author"],
        "aggregate": [ { "alias": "n", "fn": "count" } ],
    }))
    .expect("a plan");
    let agg = plan::aggregate(&cat, &plan, &HostLimits::default(), ROLE_ADMIN).expect("resolves");
    assert_eq!(agg.query.limit, Some(super::DEFAULT_MAX_ROWS + 1));

    // A `having` on a column is a condition on the rows, which `.where()` says
    // for less; it is refused naming the values that are there.
    let refused = refusal(json!({
        "op": "aggregate", "table": "books", "group": ["author"],
        "aggregate": [ { "alias": "n", "fn": "count" } ],
        "having": { "pages": { "gt": 10 } },
    }))
    .await;
    assert!(
        refused.contains("`pages`") && refused.contains("`n`") && refused.contains(".where()"),
        "{refused}"
    );

    // Two values under one name is one value lost on the way back through JSON.
    let twice = refusal(json!({
        "op": "aggregate", "table": "books",
        "aggregate": [
            { "alias": "n", "fn": "count" },
            { "alias": "n", "fn": "sum", "arg": "pages" },
        ],
    }))
    .await;
    assert!(twice.contains("asked for twice"), "{twice}");
}

#[tokio::test]
async fn every_refusal_names_what_was_wrong_with_the_plan() {
    // An unknown table.
    let unknown_table = refusal(json!({ "op": "select", "table": "nope" })).await;
    assert!(
        unknown_table.contains("`nope` is not in the catalog"),
        "{unknown_table}"
    );

    // An unknown column, in each of the places a name can appear.
    for plan in [
        json!({ "op": "select", "table": "books", "select": ["nope"] }),
        json!({ "op": "select", "table": "books", "order": [ { "field": "nope" } ] }),
        json!({ "op": "select", "table": "books", "where": { "nope": 1 } }),
    ] {
        let message = refusal(plan).await;
        assert!(message.contains("nope"), "{message}");
    }

    // A path through something that is not a key: named as what it is, because
    // "no such field `titleⱵname`" would send the author looking for a typo.
    // As a projection the formula language says it first…
    let unjoinable = refusal(json!({
        "op": "select", "table": "books", "select": ["titleⱵname"],
    }))
    .await;
    assert!(
        unjoinable.contains("titleⱵname") && unjoinable.contains("not a Key field"),
        "{unjoinable}"
    );
    // …and as a filter key, where the host walks the path itself to find the
    // table a literal is coerced against, the host says it.
    let unjoinable = refusal(json!({
        "op": "select", "table": "books", "where": { "titleⱵname": "x" },
    }))
    .await;
    assert!(
        unjoinable.contains("is not a key to another table"),
        "{unjoinable}"
    );

    // A plan carrying something the seam does not describe.
    let malformed = refusal(json!({
        "op": "select", "table": "books", "sql": "drop table books",
    }))
    .await;
    assert!(malformed.contains("sql"), "{malformed}");

    // A bound above the row cap: refused, never silently lowered to it.
    let too_many = refusal(json!({ "op": "select", "table": "books", "limit": 5000 })).await;
    assert!(
        too_many.contains("1000 rows") && too_many.contains("5000"),
        "{too_many}"
    );

    // A formula the translator cannot lower says where to compute it instead.
    let untranslatable = refusal(json!({
        "op": "select", "table": "books",
        "select": [ { "alias": "x", "formula": "title.padStart(3)" } ],
    }))
    .await;
    assert!(
        untranslatable.contains("compute it in your code body"),
        "{untranslatable}"
    );

    // A formula reaching for the event's bindings: they are the code body's, and
    // the body can splice a value into the plan itself.
    let ambient = refusal(json!({
        "op": "select", "table": "books", "where": { "formula": "pages > user.id" },
    }))
    .await;
    assert!(ambient.contains("`user`"), "{ambient}");

    // Grouping shapes an aggregate and nothing else, so a row read carrying one
    // is refused rather than quietly answering every row.
    let grouped = refusal(json!({ "op": "select", "table": "books", "group": ["author"] })).await;
    assert!(
        grouped.contains(".groupBy()") && grouped.contains("aggregate"),
        "{grouped}"
    );
    let having = refusal(json!({
        "op": "select", "table": "books", "having": { "n": { "gt": 1 } },
    }))
    .await;
    assert!(having.contains(".having()"), "{having}");
}

#[tokio::test]
async fn an_update_resolves_its_rows_by_key_before_it_writes_any_of_them() {
    // `db.books.where({ author: 1 }).update({ shelf: 3 })` — the plan becomes a
    // *read*, because §4's rule is that the matched rows are written one at a
    // time through the row layer, which is what gives each one its own event.
    let cat = library().await;
    let plan: Plan = serde_json::from_value(json!({
        "op": "update", "table": "books",
        "where": { "author": 1 },
        "values": { "title": "Orlando" },
    }))
    .expect("a plan");
    let write = plan::write(&cat, &plan, &HostLimits::default(), ROLE_ADMIN).expect("resolves");
    assert_eq!(write.pk, "id", "each matched row is addressed by its key");
    assert_eq!(write.values, Some(json!({ "title": "Orlando" })));
    // The matched read is the whole row (no `select` to narrow it) under the
    // plan's own filter, bounded by the row cap like any other read.
    let (sql, binds) = rendered(&write.matched);
    assert!(sql.contains("WHERE (\"author\" = $1)"), "{sql}");
    assert_eq!(binds[0], Value::Int(1));
    assert_eq!(write.matched.query.limit, Some(super::DEFAULT_MAX_ROWS + 1));
}

#[tokio::test]
async fn a_write_plan_is_refused_for_what_a_write_cannot_mean() {
    // §4: an omitted `.where()` must not be able to rewrite or empty a table.
    // The prelude refuses this in front of the author; the host is the rule, so
    // a plan that arrives without one — from a tampered prelude, or from §15's
    // next guest language — is refused here too.
    for op in ["update", "delete"] {
        let message = refusal(json!({
            "op": op, "table": "books", "values": { "title": "x" },
        }))
        .await;
        assert!(
            message.contains("every row of `books`") && message.contains(".where()"),
            "{message}"
        );
    }

    // A chain method that only shapes a read is refused rather than ignored: a
    // body that wrote one meant something the write cannot do.
    let shaped = refusal(json!({
        "op": "update", "table": "books",
        "where": { "id": 1 }, "select": ["title"], "values": { "title": "x" },
    }))
    .await;
    assert!(
        shaped.contains(".select()") && shaped.contains("update"),
        "{shaped}"
    );
    let filtered_insert = refusal(json!({
        "op": "insert", "table": "books", "where": { "id": 1 }, "values": { "title": "x" },
    }))
    .await;
    assert!(filtered_insert.contains(".where()"), "{filtered_insert}");

    // The values, checked against the columns **before any row is written** —
    // there is no transaction here (§6), so a bad third row found on the third
    // INSERT would leave the first two written and their events already out.
    let unknown = refusal(json!({
        "op": "insert", "table": "books",
        "values": [ { "title": "Orlando" }, { "shelf": 3 } ],
    }))
    .await;
    assert!(
        unknown.contains("no field `shelf`") && unknown.contains("insert"),
        "{unknown}"
    );
    let ill_typed = refusal(json!({
        "op": "update", "table": "books", "where": { "id": 1 }, "values": { "pages": "lots" },
    }))
    .await;
    assert!(ill_typed.contains("`pages`"), "{ill_typed}");
    let nothing = refusal(json!({
        "op": "update", "table": "books", "where": { "id": 1 }, "values": {},
    }))
    .await;
    assert!(nothing.contains("names no field"), "{nothing}");

    // A bulk insert is bounded too: the rows are written one statement each.
    let cat = library().await;
    let many: Vec<Json> = (0..3)
        .map(|i| json!({ "title": format!("b{i}") }))
        .collect();
    let plan: Plan =
        serde_json::from_value(json!({ "op": "insert", "table": "books", "values": many }))
            .expect("a plan");
    let message = plan::insert(
        &cat,
        &plan,
        &HostLimits {
            max_rows: 2,
            max_calls: 10,
        },
    )
    .err()
    .expect("refused")
    .to_string();
    assert!(
        message.contains("3 rows into `books`") && message.contains("more than the 2"),
        "{message}"
    );
}

#[tokio::test]
async fn a_delegated_plan_resolves_its_names_at_the_callers_own_role() {
    let plan: Json = json!({
        "op": "select", "table": "books", "authority": "user",
        "select": ["title", "authorⱵname"],
    });
    let parsed = |plan: &Json| -> Plan { serde_json::from_value(plan.clone()).expect("a plan") };

    // The admin resolves the Ⱶ-path, because they may read `authors`.
    let cat = library().await;
    let mut admin = plan.clone();
    admin["authority"] = json!("admin");
    plan::read(&cat, &parsed(&admin), &HostLimits::default(), ROLE_ADMIN)
        .expect("the admin reads through a key");

    // Delegated, the same plan is resolved at the *event's* role — so the join
    // goes through `ownership::join_guard` as a caller, and a role-40 reader who
    // may not read `authors` is refused by name rather than handed its columns
    // one at a time through a key.
    let host = super::TableHost::new(&cat).caused_by(40, Some(caller()));
    let refused = host
        .run(&parsed(&plan))
        .await
        .expect_err("the caller may not read `authors`");
    assert!(refused.to_string().contains("authors"), "{refused}");
}

#[tokio::test]
async fn delegation_needs_a_caller_it_can_name_and_public_is_a_valid_answer() {
    let cat = library().await;
    let plan: Plan = serde_json::from_value(json!({
        "op": "select", "table": "books", "authority": "user",
    }))
    .expect("a plan");

    // An event whose caller object is not a user is refused **before** any
    // statement: acting as somebody requires knowing who, and the admin's rows
    // are the one answer that must never be the fallback.
    let host = super::TableHost::new(&cat).caused_by(40, Some(json!({ "id": 7 })));
    let refused = host.run(&plan).await.expect_err("not a caller");
    assert!(refused.to_string().contains("uuid"), "{refused}");

    // An event with no caller at all — a scheduled or startup trigger — is not an
    // error: it delegates to the public role, which on an admin-only table with
    // no ownership formula may read nothing. (The refusal is the *table's*, which
    // is the point: `asUser()` there is honest rather than broken.)
    let host = super::TableHost::new(&cat);
    let public = host
        .run(&plan)
        .await
        .expect_err("public may not read books");
    assert!(public.to_string().contains("may not read"), "{public}");
}

#[tokio::test]
async fn the_four_authorities_are_the_four_spellings_and_nothing_else_is_one() {
    // §4: v1's `Table` says whose view of the data this is with an argument, so
    // the seam grew the two forms that argument means — the public role, and one
    // **named** user. An authority this server does not understand must be a
    // sentence naming the four, never a silent fall back to the default: the
    // default is admin, which is the one wrong answer here that would matter.
    let of = |json: Json| serde_json::from_value::<Authority>(json);
    assert_eq!(of(json!("admin")).expect("admin"), Authority::Admin);
    assert_eq!(of(json!("user")).expect("user"), Authority::User);
    assert_eq!(of(json!("public")).expect("public"), Authority::Public);
    assert_eq!(
        of(json!({ "user": "0d4e1e1e-0000-4000-8000-000000000001" })).expect("named"),
        Authority::Named(json!("0d4e1e1e-0000-4000-8000-000000000001"))
    );
    for bad in [json!("root"), json!({ "role": 40 }), json!(40), json!(null)] {
        let refused = of(bad.clone()).expect_err("not an authority").to_string();
        assert!(
            refused.contains("is not an authority") && refused.contains("public"),
            "{bad}: {refused}"
        );
    }
}

#[tokio::test]
async fn a_named_user_is_loaded_where_users_live_and_a_bad_name_is_refused() {
    let cat = library().await;
    // A user id is a uuid here, because that is what a user is identified by on
    // this server — v1's integer ids have no counterpart, and a plugin handing
    // one over is told so rather than left with an empty answer. Both refusals
    // happen before any statement runs.
    for (id, wanted) in [
        (json!(7), "not a user id"),
        (json!("ada@example.com"), "not a user id"),
    ] {
        let plan: Plan = serde_json::from_value(json!({
            "op": "select", "table": "books", "authority": { "user": id },
        }))
        .expect("a plan");
        let host = super::TableHost::new(&cat);
        let refused = host.run(&plan).await.expect_err("no such user").to_string();
        assert!(refused.contains(wanted), "{refused}");
    }

    // `public` is the public role and nobody, which the ownership rule can
    // answer without a lookup — and does: `books` is admin-only here, so the
    // refusal is the table's own.
    let plan: Plan = serde_json::from_value(json!({
        "op": "select", "table": "books", "authority": "public",
    }))
    .expect("a plan");
    let refused = super::TableHost::new(&cat)
        .caused_by(ROLE_ADMIN, Some(caller()))
        .run(&plan)
        .await
        .expect_err("public may not read books")
        .to_string();
    assert!(
        refused.contains("may not read"),
        "an admin's run asked to be treated as the public and was: {refused}"
    );
}

/// A view's host (TODO "Saltcorn UI" 7.2): the viewer's authority is the ceiling.
/// A plan that says nothing — which in a code body is the admin's — runs as the
/// viewer, and `forUser` may name the viewer and nobody else.
#[tokio::test]
async fn a_viewers_host_runs_every_plan_as_the_viewer() {
    let cat = library().await;
    let plan = |authority: Option<Json>| -> Plan {
        let mut plan = json!({ "op": "select", "table": "books" });
        if let Some(authority) = authority {
            plan["authority"] = authority;
        }
        serde_json::from_value(plan).expect("a plan")
    };

    // The same unmarked read: the admin's in a code body, which passes every
    // floor and gets as far as the statement (this catalog has no database to
    // run it on)…
    let admin = super::TableHost::new(&cat)
        .caused_by(40, Some(caller()))
        .run(&plan(None))
        .await
        .expect_err("no database here")
        .to_string();
    assert!(
        admin.contains("no database") && !admin.contains("may not read"),
        "a code body reads as admin: {admin}"
    );
    // …and the viewer's in a view, where role 40 may not read `books`.
    let viewer = super::TableHost::new(&cat)
        .caused_by(40, Some(caller()))
        .viewer_only();
    for authority in [None, Some(json!("admin")), Some(json!("user"))] {
        let refused = viewer
            .run(&plan(authority.clone()))
            .await
            .expect_err("the viewer may not read books")
            .to_string();
        assert!(refused.contains("may not read"), "{authority:?}: {refused}");
    }
    // `forUser: req.user` is the viewer, answered without looking them up
    // (there is no users table here to find them in).
    let own = viewer
        .run(&plan(Some(
            json!({ "user": uuid::Uuid::nil().to_string() }),
        )))
        .await
        .expect_err("still the viewer")
        .to_string();
    assert!(own.contains("may not read"), "{own}");
    // Anybody else is refused by name, before any lookup.
    let other = "0d4e1e1e-0000-4000-8000-000000000001";
    let borrowed = viewer
        .run(&plan(Some(json!({ "user": other }))))
        .await
        .expect_err("not the viewer")
        .to_string();
    assert!(
        borrowed.contains(other) && borrowed.contains("only the viewer"),
        "{borrowed}"
    );
    // An anonymous viewer has nobody to name at all.
    let anonymous = super::TableHost::new(&cat).viewer_only();
    let refused = anonymous
        .run(&plan(Some(
            json!({ "user": uuid::Uuid::nil().to_string() }),
        )))
        .await
        .expect_err("nobody to be")
        .to_string();
    assert!(refused.contains("only the viewer"), "{refused}");
}

#[tokio::test]
async fn the_call_budget_is_spent_once_per_plan_and_then_refused() {
    let cat = library().await;
    let host = super::TableHost::new(&cat).with_limits(HostLimits {
        max_rows: 10,
        max_calls: 2,
    });
    // The plans themselves reach a driver that refuses to run anything, so what
    // is asserted is the budget: two calls get past it, the third does not.
    let plan = json!({ "op": "select", "table": "books" });
    for _ in 0..2 {
        let e = sc_expr::CodeHost::call(&host, plan.clone())
            .await
            .expect_err("no database behind this catalog");
        assert!(!e.to_string().contains("database calls"), "{e}");
    }
    let spent = sc_expr::CodeHost::call(&host, plan)
        .await
        .expect_err("the budget is spent");
    assert!(
        spent.to_string().contains("more than 2 database calls"),
        "{spent}"
    );
}

// ---------------------------------------------------------------------------
// The body's own SQL
// ---------------------------------------------------------------------------

/// The statement one `db.sql(…)` lowers to, or the message it is refused with.
async fn sql_of(request: Json) -> Result<(String, Vec<Value>)> {
    let cat = library().await;
    let plan: super::SqlPlan = serde_json::from_value(request)
        .map_err(|e| sc_error::Error::invalid(format!("not a sql plan: {e}")))?;
    let statement = plan::statement(&cat, &plan)?;
    Ok(Pg.render(&statement).expect("renders"))
}

#[tokio::test]
async fn the_bodys_own_sql_runs_as_written_and_its_arguments_are_binds() {
    let (sql, binds) = sql_of(json!({
        "op": "sql",
        "sql": "select owner, count(*) as n from books where pages > $1 group by owner",
        "params": [200],
    }))
    .await
    .expect("one statement");

    // The text is the author's, unchanged — that is what `db.sql()` is for. What
    // the server put in it is nothing.
    assert_eq!(
        sql,
        "select owner, count(*) as n from books where pages > $1 group by owner"
    );
    // And the argument never became part of it: a value is a value even when it
    // spells SQL, which is the whole guarantee that survives the escape hatch.
    assert_eq!(binds, vec![Value::Int(200)]);

    let (_, binds) = sql_of(json!({
        "op": "sql",
        "sql": "select * from books where title = $1",
        "params": ["'; drop table books; --"],
    }))
    .await
    .expect("one statement");
    assert_eq!(
        binds,
        vec![Value::Text("'; drop table books; --".to_owned())]
    );
}

#[tokio::test]
async fn a_second_statement_and_a_named_parameter_are_refused_saying_what_to_do() {
    // Two statements: a `Raw` is prepared and bound, so the pair is either
    // refused by the backend or half-run. Refused here, where the message can
    // say what to do instead.
    let refused = sql_of(json!({
        "op": "sql", "sql": "insert into books (title) values ($1); delete from books",
        "params": ["Dune"],
    }))
    .await
    .expect_err("two statements");
    assert!(refused.to_string().contains("2 statements"), "{refused}");
    assert!(refused.to_string().contains("call it twice"), "{refused}");

    // A `;` inside a literal or a comment separates nothing — the same scanner a
    // custom SQL query is read by, so `db.sql()` cannot be broken by punctuation
    // in a string.
    sql_of(json!({ "op": "sql", "sql": "select 'a; b' as t -- ; not a statement" }))
        .await
        .expect("one statement");

    // `:name` is §13.4's spelling, where the admin also declares each parameter's
    // type. Here the arguments are positional, and saying so beats a syntax error
    // pointing at a colon.
    let refused = sql_of(json!({
        "op": "sql", "sql": "select * from books where title = :title", "params": ["Dune"],
    }))
    .await
    .expect_err("a named parameter");
    assert!(refused.to_string().contains("`:title`"), "{refused}");
    assert!(refused.to_string().contains("$1"), "{refused}");

    // A cast is not a parameter, for the same reason it is not one anywhere else.
    sql_of(json!({ "op": "sql", "sql": "select pages::text from books" }))
        .await
        .expect("a cast is a cast");

    let empty = sql_of(json!({ "op": "sql", "sql": "  -- nothing at all\n" }))
        .await
        .expect_err("no SQL");
    assert!(empty.to_string().contains("no SQL"), "{empty}");
}

#[tokio::test]
async fn a_sql_request_is_told_apart_from_a_plan_by_its_op_and_keeps_its_own_words() {
    let cat = library().await;

    // The dispatch: `op: "sql"` is read as a `SqlPlan`, so a request that names a
    // table the way a plan does is refused as the *sql* shape it said it was —
    // rather than as a plan with a missing field.
    let refused = sc_expr::CodeHost::call(
        &super::TableHost::new(&cat),
        json!({ "op": "sql", "table": "books", "sql": "select 1" }),
    )
    .await
    .expect_err("`table` is not a field of a sql request");
    assert!(refused.to_string().contains("table"), "{refused}");

    // And the other way: a plan is still a plan.
    let refused = sc_expr::CodeHost::call(
        &super::TableHost::new(&cat),
        json!({ "op": "select", "table": "books", "sql": "select 1" }),
    )
    .await
    .expect_err("`sql` is not a field of a plan");
    assert!(refused.to_string().contains("sql"), "{refused}");
}

// ---------------------------------------------------------------------------
// Streaming: `.iter()`
// ---------------------------------------------------------------------------

/// The `WHERE` a resumed batch adds, on its own — the clause that decides
/// whether streaming is correct, read without the ordering and the bound around
/// it.
async fn resumed_where(order: Json, after: Json) -> String {
    let read = read_of(json!({
        "op": "select",
        "table": "books",
        "cursor": true,
        "order": order,
        "after": after,
    }))
    .await
    .expect("resolves");
    let select = Select::from(Source::table("books"))
        .filter(read.query.filter.clone().expect("a resume predicate"));
    let (sql, _) = Pg
        .render(&Statement::Select(Box::new(select)))
        .expect("renders");
    sql.split_once(" WHERE ")
        .map(|(_, w)| w.to_owned())
        .unwrap_or_else(|| panic!("no WHERE in {sql}"))
}

#[tokio::test]
async fn a_streamed_read_sorts_by_a_total_order_and_says_where_the_nulls_go() {
    // `db.books.orderBy("published", "desc").iter()`. Two things are added to
    // what `.rows()` would do, and both are what makes the next batch able to
    // resume: the primary key ends the ordering, so no two rows tie, and null
    // placement is stated rather than inherited from the dialect.
    let read = read_of(json!({
        "op": "select",
        "table": "books",
        "cursor": true,
        "order": [{ "field": "published", "dir": "desc" }],
    }))
    .await
    .expect("resolves");
    let (sql, _) = rendered(&read);
    assert!(
        sql.contains(r#"ORDER BY "published" DESC NULLS FIRST, "id" ASC"#),
        "{sql}"
    );
    // The batch is the row cap when the guest named no size of its own — not the
    // cap plus one, which is `.rows()`'s way of telling "exactly the cap" from
    // "more than we may answer". A short batch is how a stream ends instead.
    assert_eq!(read.query.limit, Some(1000));
    // Every sort value is projected under a reserved alias, because the cursor
    // cannot be read off the answer: a `.select()` narrows what comes back, and a
    // Ⱶ-path is no column of this table at all.
    assert!(
        sql.contains(r#""published" AS "__sc_cursor_0""#)
            && sql.contains(r#""id" AS "__sc_cursor_1""#),
        "{sql}"
    );
}

#[tokio::test]
async fn an_ordering_that_already_ends_at_the_primary_key_is_not_given_a_second_one() {
    let read = read_of(json!({
        "op": "select",
        "table": "books",
        "cursor": true,
        "order": [{ "field": "id", "dir": "asc" }],
    }))
    .await
    .expect("resolves");
    let (sql, _) = rendered(&read);
    assert_eq!(read.query.order.len(), 1, "{sql}");
    assert!(sql.contains(r#"ORDER BY "id" ASC"#), "{sql}");
}

#[tokio::test]
async fn resuming_is_the_lexicographic_rule_and_each_direction_answers_its_own_nulls() {
    // Ascending: the rows after a value are the greater ones **and the nulls**,
    // which sort last — and a tie on the value is broken by the primary key.
    assert_eq!(
        resumed_where(
            json!([{ "field": "published", "dir": "asc" }]),
            json!(["2020-01-01", 7]),
        )
        .await,
        r#"((("published" > $1) OR ("published" IS NULL)) OR (("published" = $2) AND ("id" > $3)))"#
    );

    // Ascending from a **null**: the nulls are last, so no row sorts after this
    // one by this key at all — only the tie-break can tell them apart. A term
    // that could never be true is not emitted; it is the absence of one.
    assert_eq!(
        resumed_where(
            json!([{ "field": "published", "dir": "asc" }]),
            json!([null, 7]),
        )
        .await,
        r#"(("published" IS NULL) AND ("id" > $1))"#
    );

    // Descending: the nulls came first, so they are behind us and the rule is
    // plain.
    assert_eq!(
        resumed_where(
            json!([{ "field": "published", "dir": "desc" }]),
            json!(["2020-01-01", 7]),
        )
        .await,
        r#"(("published" < $1) OR (("published" = $2) AND ("id" > $3)))"#
    );

    // Descending from a null: everything that is not null is still to come.
    assert_eq!(
        resumed_where(
            json!([{ "field": "published", "dir": "desc" }]),
            json!([null, 7]),
        )
        .await,
        r#"(("published" IS NOT NULL) OR (("published" IS NULL) AND ("id" > $1)))"#
    );

    // Two keys: the chain is as long as the ordering, and the primary key ends
    // it. `pages` is nullable here; `id` is not, which is why its own term
    // carries no `IS NULL` disjunct.
    assert_eq!(
        resumed_where(
            json!([
                { "field": "published", "dir": "asc" },
                { "field": "pages", "dir": "desc" },
            ]),
            json!(["2020-01-01", 300, 7]),
        )
        .await,
        r#"((("published" > $1) OR ("published" IS NULL)) OR \
((("published" = $2) AND ("pages" < $3)) OR \
((("published" = $4) AND ("pages" = $5)) AND ("id" > $6))))"#
            .replace("\\\n", "")
            .as_str()
    );

    // No ordering at all: the primary key is the whole of it, and resuming is the
    // one comparison everybody expects.
    assert_eq!(resumed_where(json!([]), json!([7])).await, r#"("id" > $1)"#);
}

#[tokio::test]
async fn a_cursor_value_is_bound_and_coerced_against_the_column_it_sorts_on() {
    let read = read_of(json!({
        "op": "select",
        "table": "books",
        "cursor": true,
        "order": [{ "field": "published", "dir": "asc" }],
        "after": ["2020-01-01", 7],
    }))
    .await
    .expect("resolves");
    let (_, binds) = rendered(&read);
    // A date reaches the statement as a date and the key as an integer: the
    // cursor is the host's own answer coming back, and it is typed on the way in
    // exactly as a `.where()` literal is.
    assert!(
        matches!(binds.first(), Some(Value::Date(_))),
        "the date is a date: {binds:?}"
    );
    // The date is bound twice — once for "sorts after it", once for the tie it
    // breaks — because every literal is its own parameter.
    assert_eq!(binds.get(2), Some(&Value::Int(7)));

    // And a value the column cannot hold is refused, rather than reaching the
    // database as something it will argue with.
    let refused = refusal(json!({
        "op": "select",
        "table": "books",
        "cursor": true,
        "order": [{ "field": "published", "dir": "asc" }],
        "after": ["not a date", 7],
    }))
    .await;
    assert!(refused.contains("published"), "{refused}");
}

#[tokio::test]
async fn a_streamed_read_can_sort_by_a_join_path_and_types_its_cursor_at_the_far_end() {
    let read = read_of(json!({
        "op": "select",
        "table": "books",
        "cursor": true,
        "order": [{ "field": "authorⱵname", "dir": "asc" }],
        "after": ["Woolf", 7],
    }))
    .await
    .expect("resolves");
    let (sql, binds) = rendered(&read);
    // The very same correlated subquery the ordering already used, now also in
    // the `WHERE` and projected as the cursor's own value.
    assert!(sql.contains(r#"AS "__sc_cursor_0""#), "{sql}");
    assert_eq!(binds.first(), Some(&Value::Text("Woolf".to_owned())));
    // A joined value can be absent however `NOT NULL` the far column is — the row
    // it points at may not exist — so the ascending rule keeps its null disjunct.
    assert!(sql.contains("IS NULL"), "{sql}");
}

#[tokio::test]
async fn what_a_streamed_read_refuses_and_why() {
    // An expression has no column to type a cursor value against.
    let refused = refusal(json!({
        "op": "select",
        "table": "books",
        "cursor": true,
        "order": [{ "field": "pages * price", "dir": "asc" }],
    }))
    .await;
    assert!(refused.contains("resume"), "{refused}");
    assert!(refused.contains(".rows()"), "{refused}");

    // A grouped answer has no primary key to break a tie on.
    let refused = refusal(json!({
        "op": "aggregate",
        "table": "books",
        "cursor": true,
        "group": ["author"],
        "aggregate": [{ "alias": "n", "fn": "count" }],
    }))
    .await;
    assert!(refused.contains("streams"), "{refused}");

    // An `.offset()` is skipped once, at the start of the iteration; applied
    // again to a resumed batch it would skip rows in the middle of the stream.
    let refused = refusal(json!({
        "op": "select",
        "table": "books",
        "cursor": true,
        "offset": 10,
        "after": [7],
    }))
    .await;
    assert!(refused.contains("skipped once"), "{refused}");

    // A cursor is the host's own answer handed back: one that does not fit the
    // ordering is refused rather than padded out to it.
    let refused = refusal(json!({
        "op": "select",
        "table": "books",
        "cursor": true,
        "order": [{ "field": "published", "dir": "asc" }],
        "after": [7],
    }))
    .await;
    assert!(refused.contains("cursor of 1 value(s)"), "{refused}");

    // And streaming a write is a shape the seam does not have.
    let refused = refusal(json!({
        "op": "delete",
        "table": "books",
        "cursor": true,
        "where": { "id": 1 },
    }))
    .await;
    assert!(refused.contains("`.iter()` has no meaning"), "{refused}");
}

#[tokio::test]
async fn a_table_with_no_primary_key_cannot_be_streamed() {
    // Nothing to break a tie on, so nothing to resume from. The refusal is the
    // row layer's own, which is where "a table this server can address rows in"
    // is already defined.
    let cat = catalog_of(vec![physical(
        "logs",
        &[("at", "timestamptz", true), ("msg", "text", true)],
        &[],
    )])
    .await;
    let plan: Plan = serde_json::from_value(json!({
        "op": "select", "table": "logs", "cursor": true,
    }))
    .expect("a well-formed plan");
    let refused = plan::read(&cat, &plan, &HostLimits::default(), ROLE_ADMIN)
        .err()
        .expect("no primary key")
        .to_string();
    assert!(refused.contains("no primary key"), "{refused}");
}

#[tokio::test]
async fn the_batch_size_is_clamped_where_a_rows_bound_is_refused() {
    // The two are different questions. `.limit()` on a `.rows()` is the answer,
    // so asking for more than the cap is refused — a body handed 1000 of 5000
    // rows would compute a wrong answer out of a right-looking one. A batch size
    // is how many round trips the same rows arrive in, so it is clamped: the
    // iteration yields exactly what it would have either way.
    let refused = refusal(json!({ "op": "select", "table": "books", "limit": 5000 })).await;
    assert!(refused.contains("may read 1000 rows at once"), "{refused}");

    let read = read_of(json!({
        "op": "select", "table": "books", "cursor": true, "limit": 5000,
    }))
    .await
    .expect("resolves");
    assert_eq!(read.query.limit, Some(1000));
}

// ---------------------------------------------------------------------------
// A model handle's requests (milestone 31 §3)
// ---------------------------------------------------------------------------

/// A model host that records what it was asked and answers each row by what
/// identifies it: a key as `key:<k>`, a literal row as `title:<title>`.
#[derive(Default)]
struct RecordingModels {
    calls: std::sync::Mutex<Vec<ModelCall>>,
}

/// One call a [`RecordingModels`] was asked: model, fit, table, `keys` or
/// `values`, and the rows.
type ModelCall = (String, Option<String>, String, &'static str, Vec<Json>);

#[async_trait::async_trait]
impl sc_catalog::ModelHost for RecordingModels {
    async fn predict(
        &self,
        model: &str,
        fit: Option<&str>,
        table: &str,
        rows: sc_catalog::PredictRows<'_>,
        detail: bool,
    ) -> Result<Vec<Json>> {
        let (kind, rows) = match rows {
            sc_catalog::PredictRows::Keys(keys) => ("keys", keys.to_vec()),
            sc_catalog::PredictRows::Values(values) => ("values", values.to_vec()),
        };
        self.calls.lock().unwrap().push((
            model.to_owned(),
            fit.map(str::to_owned),
            table.to_owned(),
            kind,
            rows.clone(),
        ));
        Ok(rows
            .iter()
            .map(|row| {
                let value = match kind {
                    "keys" => json!(format!("key:{row}")),
                    _ => json!(format!("title:{}", row["title"].as_str().unwrap_or("?"))),
                };
                if detail {
                    json!({ "value": value })
                } else {
                    value
                }
            })
            .collect())
    }

    async fn describe(&self, _model: &str) -> Result<sc_catalog::ModelSummary> {
        Err(sc_error::Error::invalid("not described here"))
    }
}

#[tokio::test]
async fn a_batch_of_rows_is_one_call_for_the_keyed_and_one_for_the_literal_in_row_order() {
    let cat = library().await;
    let books = cat.require("books").unwrap();
    let models = RecordingModels::default();
    let rows = vec![
        json!({ "id": 3, "title": "Emma" }),
        json!({ "title": "Unwritten", "pages": 10 }),
        json!({ "id": 1 }),
        // A null key is no key: the row is taken as it is.
        json!({ "id": null, "title": "Draft" }),
    ];
    let answer = super::models::predict_through(&models, "Pages", "fit-1", &books, &rows, false)
        .await
        .unwrap();
    assert_eq!(
        answer,
        json!(["key:3", "title:Unwritten", "key:1", "title:Draft"])
    );
    let calls = models.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    // Both calls name the fit the handle resolved, not the model's active fit.
    assert_eq!(calls[0].0, "Pages");
    assert_eq!(calls[0].1.as_deref(), Some("fit-1"));
    assert_eq!(calls[0].2, "books");
    assert_eq!(calls[0].3, "keys");
    assert_eq!(calls[0].4, vec![json!(3), json!(1)]);
    assert_eq!(calls[1].3, "values");
    assert_eq!(calls[1].4.len(), 2);

    // Only keyed rows: no literal call at all. `detail` reaches the host.
    let models = RecordingModels::default();
    let answer = super::models::predict_through(
        &models,
        "Pages",
        "fit-1",
        &books,
        &[json!({ "id": 2 })],
        true,
    )
    .await
    .unwrap();
    assert_eq!(answer, json!([{ "value": "key:2" }]));
    assert_eq!(models.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_row_that_is_not_an_object_is_refused_naming_it() {
    let cat = library().await;
    let books = cat.require("books").unwrap();
    let models = RecordingModels::default();
    let err = super::models::predict_through(
        &models,
        "Pages",
        "fit-1",
        &books,
        &[json!({ "id": 1 }), json!(7)],
        false,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("predict() takes a row object or an array of them, and row 2 is 7"),
        "{err}"
    );
    assert!(models.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_models_request_is_refused_before_any_model_is_read_when_it_is_malformed() {
    let cat = library().await;
    let host = super::TableHost::new(&cat);
    let refused = |request: Json| {
        let host = &host;
        async move {
            sc_expr::CodeHost::call(host, request)
                .await
                .unwrap_err()
                .to_string()
        }
    };
    // The flat functions of the Stan milestone are gone.
    let err = refused(json!({ "op": "models", "what": "instance", "model": "Radon" })).await;
    assert!(
        err.contains(
            "a model handle has no `instance`; it has predict, draws, summary and \
             writePosterior"
        ),
        "{err}"
    );
    // Every request after `get` names the fit `get` resolved.
    let err = refused(json!({ "op": "models", "what": "draws", "model": "Radon",
                              "variable": "alpha" }))
    .await;
    assert!(
        err.contains("a models `draws` request names the fit `models.get` resolved"),
        "{err}"
    );
    // A field the seam does not describe, by serde, naming it.
    let err = refused(json!({ "op": "models", "what": "get", "model": "Radon",
                              "instance": "x" }))
    .await;
    assert!(err.contains("unknown field `instance`"), "{err}");
    // An authority that is none of the four spellings.
    let err = refused(
        json!({ "op": "models", "what": "write_posterior", "model": "Radon",
                              "fit": "f", "authority": "root" }),
    )
    .await;
    assert!(err.contains("not one the server understands"), "{err}");
}

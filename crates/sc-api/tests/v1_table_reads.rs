//! Saltcorn 1's `Table` **reading a real database** (the milestone "the v1
//! `Table` API", phase 3).
//!
//! The lowering itself is proved in `sc-expr`: those tests put a spy behind the
//! sender and assert the plan each v1 method sends. What is proved here is the
//! half they cannot — that the plan then *answers*, through the very same
//! `TableHost` a `db` chain goes through, with the rows a v1 plugin expects to
//! read back.
//!
//! The `Table` is built here the way a code body will build it once phase 6
//! binds it as a run parameter: over the run's own sender (`db.__scSend`) and
//! the run's own schema snapshot. That is the whole seam — one sender, one call
//! budget, one authority default — so what these tests exercise is the same
//! object the trigger author will get.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_types::Role;
use std::sync::Arc;

use sc_api::code_host::{TableHost, schema_snapshot};
use sc_catalog::{Catalog, TableMeta, bootstrap_table_meta, save_table_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_expr::{CodeCall, CodeHost, CodeRuntime};
use sc_test_harness::TestDb;
use serde_json::{Value as Json, json};

/// A small library: books with an author key, and reviews pointing back at
/// them — enough for a Ⱶ-path, an inverse relation and every aggregate v1 can
/// spell over one.
async fn setup(db: &TestDb) -> Result<Arc<Catalog>> {
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE authors (id bigint primary key, name text, country text);
             CREATE TABLE books (id bigint primary key, title text, pages bigint,
                 published date, author bigint references authors(id));
             CREATE TABLE reviews (id bigint primary key, book bigint references books(id),
                 stars bigint, critic text, posted date);
             INSERT INTO authors VALUES (1, 'Woolf', 'GB'), (2, 'Herbert', 'US');
             INSERT INTO books VALUES
                 (1, 'Orlando', 333, '1928-10-11', 1),
                 (2, 'The Waves', 297, '1931-10-08', 1),
                 (3, 'Dune', 412, '1965-08-01', 2);
             INSERT INTO reviews VALUES
                 (1, 1, 5, 'ada', '2020-01-01'),
                 (2, 1, 4, 'bob', '2021-01-01'),
                 (3, 3, 3, 'ada', '2019-01-01');",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Ok(Arc::new(
        Catalog::init(driver as Arc<dyn DatabaseDriver>).await?,
    ))
}

/// Run `body` with the v1 `Table` and `Field` over this catalog — the api a
/// code body will be handed, built here by hand until phase 6 hands it over.
async fn v1(catalog: &Catalog, body: &str) -> Result<Json> {
    let host = TableHost::new(catalog);
    let snapshot = schema_snapshot(catalog)?;
    let runtime = CodeRuntime::with_workers(1);
    runtime
        .run(CodeCall {
            code: format!(
                "const {{ Table, Field }} = __scMakeV1Api(db.__scSend, __scSchema({}));\n{body}",
                snapshot.generation()
            ),
            host: Some(&host),
            schema: Some(&snapshot),
            ..CodeCall::default()
        })
        .await
}

#[tokio::test]
async fn the_definition_of_dones_read_answers_from_a_real_table() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    // The milestone's own six lines, minus the write: `Table.findOne` answers
    // synchronously, and the read that follows is one round trip.
    let out = v1(
        &catalog,
        r#"const books = Table.findOne({ name: "books" });
           const recent = await books.getRows({ published: { gt: "1930-01-01" } },
                                              { orderBy: "title", limit: 10 });
           return {
             pk: books.pk_name,
             fkey: books.getField("author").is_fkey,
             titles: recent.map((b) => b.title),
             whole_row: recent[0],
           };"#,
    )
    .await?;
    assert_eq!(out["pk"], json!("id"), "no host call answered this");
    assert_eq!(out["fkey"], json!(true));
    assert_eq!(out["titles"], json!(["Dune", "The Waves"]));
    // A row is the whole row, as the wire shape has it: v1's `getRows` answers
    // every column and so does this.
    assert_eq!(
        out["whole_row"],
        json!({ "id": 3, "title": "Dune", "pages": 412, "published": "1965-08-01", "author": 2 })
    );
    Ok(())
}

#[tokio::test]
async fn v1s_wheres_and_selopts_reach_the_database_meaning_what_they_said() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    let out = v1(
        &catalog,
        r#"const books = Table.findOne("books");
           return {
             between: (await books.getRows({ pages: { gt: 300, lt: 400 } })).map((b) => b.title),
             ilike:   (await books.getRows({ title: { ilike: "he wav" } })).map((b) => b.title),
             in:      (await books.getRows({ id: { in: [1, 3] } }, { orderBy: "id" }))
                        .map((b) => b.title),
             not_in:  (await books.getRows({ id: { not: { in: [1, 3] } } })).map((b) => b.title),
             or:      (await books.getRows({ or: [{ pages: 297 }, { pages: 412 }] },
                                           { orderBy: "pages" })).map((b) => b.title),
             null:    (await books.getRows({ author: null })).length,
             none:    (await books.getRows({ _false: true })).length,
             desc:    (await books.getRows({}, { orderBy: "pages", orderDesc: true, limit: 1 }))
                        .map((b) => b.title),
             fields:  await books.getRows({ id: 1 }, { fields: ["title"] }),
             offset:  (await books.getRows({}, { orderBy: "id", offset: 2 })).map((b) => b.id),
             one:     (await books.getRow({ title: "Dune" })).pages,
             missing: await books.getRow({ title: "nope" }),
             count:   await books.countRows({ pages: { gt: 300 } }),
             all:     await books.countRows(),
             authors: await books.distinctValues("author"),
           };"#,
    )
    .await?;

    assert_eq!(out["between"], json!(["Orlando"]));
    // v1's implicit `%…%`, which is what every v1 search box means by it.
    assert_eq!(out["ilike"], json!(["The Waves"]));
    assert_eq!(out["in"], json!(["Orlando", "Dune"]));
    assert_eq!(out["not_in"], json!(["The Waves"]));
    assert_eq!(out["or"], json!(["The Waves", "Dune"]));
    assert_eq!(out["null"], json!(0));
    assert_eq!(
        out["none"],
        json!(0),
        "`_false` matches nothing, as it says"
    );
    assert_eq!(out["desc"], json!(["Dune"]));
    assert_eq!(
        out["fields"],
        json!([{ "title": "Orlando" }]),
        "`fields` narrows the row to what was asked for"
    );
    assert_eq!(out["offset"], json!([3]));
    assert_eq!(out["one"], json!(412));
    // v1's `getRow` answers null rather than undefined, which is what a plugin
    // tests with `if (row)`.
    assert_eq!(out["missing"], Json::Null);
    assert_eq!(out["count"], json!(2));
    assert_eq!(out["all"], json!(3));
    assert_eq!(out["authors"], json!([1, 2]));
    Ok(())
}

#[tokio::test]
async fn an_aggregation_query_answers_one_object_and_a_group_per_row() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    let out = v1(
        &catalog,
        r#"const books = Table.findOne("books");
           const flat = await books.aggregationQuery({
             n: { aggregate: "count" },
             longest: { field: "pages", aggregate: "max" },
             shortest: { field: "pages", aggregate: "min" },
             total: { field: "pages", aggregate: "sum" },
           });
           const grouped = await books.aggregationQuery(
             { n: { aggregate: "count" }, total: { field: "pages", aggregate: "sum" } },
             { groupBy: "author" }
           );
           return { flat: flat, grouped: grouped };"#,
    )
    .await?;

    // A `sum` over a bigint column is `numeric` in Postgres, and this server
    // carries a Decimal to the wire as a **string** rather than through a JSON
    // number that cannot hold all of them. That is the wire shape everywhere
    // else on this server, and a v1 method is not the place to invent a second.
    assert_eq!(
        out["flat"],
        json!({ "n": 3, "longest": 412, "shortest": 297, "total": "1042" }),
        "ungrouped, v1 answers one object — and so does this"
    );
    let grouped = out["grouped"].as_array().expect("a row per group");
    assert_eq!(grouped.len(), 2);
    assert!(
        grouped.contains(&json!({ "author": 1, "n": 2, "total": "630" })),
        "the group key rides back beside the values: {out}"
    );
    Ok(())
}

#[tokio::test]
async fn a_joined_read_brings_the_path_and_the_relation_back_on_the_row() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    // §6: `joinFields` is a Ⱶ-path projection and `aggregations` an inverse
    // relation, so all of this is **one** statement — the same one a
    // `db.books.select(...)` would send.
    let out = v1(
        &catalog,
        r#"const books = Table.findOne("books");
           const rows = await books.getJoinedRows({
             where: { pages: { gt: 300 } },
             joinFields: {
               writer: { ref: "author", target: "name" },
               where_from: { ref: "author", target: "country" },
             },
             aggregations: {
               reviews: { table: "reviews", ref: "book", aggregate: "count" },
               stars: { table: "reviews", ref: "book", field: "stars", aggregate: "avg" },
               best: { table: "reviews", ref: "book", field: "stars", aggregate: "max" },
               critics: { table: "reviews", ref: "book", field: "critic",
                          aggregate: "count distinct" },
               latest: { table: "reviews", ref: "book", field: "stars",
                         aggregate: "Latest posted" },
             },
             orderBy: "title",
           });
           const one = await books.getJoinedRow({
             where: { id: 2 },
             aggregations: { reviews: { table: "reviews", ref: "book", aggregate: "count" } },
           });
           return {
             rows: rows.map((r) => ({
               title: r.title, pages: r.pages, writer: r.writer, from: r.where_from,
               reviews: r.reviews, stars: Number(r.stars), best: r.best,
               critics: r.critics, latest: r.latest,
             })),
             one: { title: one.title, reviews: one.reviews },
           };"#,
    )
    .await?;

    assert_eq!(
        out["rows"],
        json!([
            {
                "title": "Dune", "pages": 412, "writer": "Herbert", "from": "US",
                "reviews": 1, "stars": 3, "best": 3, "critics": 1, "latest": 3,
            },
            {
                "title": "Orlando", "pages": 333, "writer": "Woolf", "from": "GB",
                // Two reviews by two critics; the latest by `posted` is bob's 4.
                "reviews": 2, "stars": 4.5, "best": 5, "critics": 2, "latest": 4,
            },
        ]),
        "the row's own columns, the joined values and the child aggregates in one read"
    );
    assert_eq!(out["one"], json!({ "title": "The Waves", "reviews": 0 }));
    Ok(())
}

#[tokio::test]
async fn the_joined_query_answers_the_statement_this_server_will_not_run() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;

    let out = v1(
        &catalog,
        r#"const books = Table.findOne("books");
           const query = await books.getJoinedQuery({
             joinFields: { writer: { ref: "author", target: "name" } },
             aggregations: { reviews: { table: "reviews", ref: "book", aggregate: "count" } },
             orderBy: "title",
           });
           const bound = await books.getJoinedQuery({ where: { pages: { gt: 300 } } });
           return { query: query, bound: bound };"#,
    )
    .await?;

    let sql = out["query"]["sql"].as_str().expect("v1's own shape");
    assert!(sql.to_lowercase().starts_with("select"), "{sql}");
    assert!(sql.contains("\"books\""), "{sql}");
    assert!(
        sql.contains("\"authors\""),
        "the join is in the statement: {sql}"
    );
    assert!(sql.contains("count("), "the aggregation is too: {sql}");
    // The one value a read with no `where` still binds is **this server's row
    // cap**: the statement is the statement this server would run, cap and all,
    // rather than an idealised one that would fetch a table.
    assert_eq!(out["query"]["values"], json!([1001]));
    // Every literal is a **bind**, here as everywhere: the value the body
    // filtered on is in `values` and not in the text.
    assert_eq!(out["bound"]["values"], json!([300, 1001]));
    assert!(
        !out["bound"]["sql"]
            .as_str()
            .unwrap_or_default()
            .contains("300"),
        "{out}"
    );

    // And it is real SQL, which is the only claim worth making about text this
    // server hands back and will not run: the database itself takes it, with
    // its bind, and answers the three books.
    let rows = db
        .client()
        .await?
        .query(sql, &[&1001_i64])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert_eq!(rows.len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_rendered_read_carries_the_readers_own_rule_or_says_it_may_not_have_one() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    bootstrap_table_meta(&catalog).await?;
    // `books` is readable by nobody below role 40, and by its owner's country
    // above it — a formula the translator can lower, which is what lets it be
    // carried *inside* a statement at all.
    let mut books = TableMeta::new("books").access(Role::of(40), Role::of(40));
    books.set_ownership_formula(Some("authorⱵcountry === user.country"));
    save_table_meta(&catalog, &books).await?;
    // `reviews`, by contrast, is the administrator's alone: no floor a reader
    // meets and no formula to extend it.
    save_table_meta(
        &catalog,
        &TableMeta::new("reviews").access(Role::ADMIN, Role::ADMIN),
    )
    .await?;
    catalog.reload().await?;

    let reader = |country: &str| {
        json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "role": 80,
            "email": "reader@example.com",
            "country": country,
        })
    };

    // A caller whose access comes from the formula gets the statement with the
    // formula in it — the same predicate `ownership::aggregate_guard` hands an
    // aggregate, because a rendered statement decides once for all its rows
    // exactly as an aggregate does.
    let host = TableHost::new(&catalog).caused_by(Role::of(80), Some(reader("GB")));
    let out = host
        .call(json!({
            "op": "select", "table": "books", "authority": "user", "render": true,
            "select": ["id", "title"],
        }))
        .await?;
    let sql = out["sql"].as_str().expect("the statement");
    assert!(
        sql.contains("\"authors\""),
        "the formula is in the where: {sql}"
    );
    assert_eq!(
        out["values"],
        json!(["GB", 1001]),
        "and its value is a bind"
    );

    // A caller the floor does not admit and no formula extends gets v1's own
    // answer rather than a statement they could take elsewhere.
    let refused = host
        .call(json!({
            "op": "select", "table": "reviews", "authority": "user", "render": true,
        }))
        .await?;
    assert_eq!(refused, json!({ "notAuthorized": true }));

    // And the admin authority a trigger runs at renders the read unrestricted,
    // which is what it would have run.
    let admin = TableHost::new(&catalog)
        .call(json!({ "op": "select", "table": "books", "render": true }))
        .await?;
    assert!(
        !admin["sql"]
            .as_str()
            .unwrap_or_default()
            .contains("\"authors\""),
        "{admin}"
    );
    Ok(())
}

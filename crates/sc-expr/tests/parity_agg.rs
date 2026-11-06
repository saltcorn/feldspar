//! Aggregation parity (TODO Phase 7): the symbolic translation of a Ↄ-relation
//! chain executed by real Postgres and the reified evaluation over the same
//! prefetched child rows in real V8 must agree — case by case over the
//! semantics table in `docs/AGG_EXPRS.md`, including the empty-relation, null
//! and tie corners.
//!
//! Each case seeds the *same* child rows two ways — inserted into the `lines`
//! table for the SQL correlated subquery, and bound as a JSON array under the
//! relation identifier for the prelude — so both evaluators see one world.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use sc_db_postgres::PgParam;
use sc_expr::{
    DenoEvaluator, Env, Formula, FormulaCall, JsEvaluator, Operation, SchemaShape, TableShape,
    UserEnv, translate,
};
use sc_query::{Expr as QExpr, Select, Source, SqlDialect, Statement, Value};
use sc_test_harness::TestDb;
use tokio_postgres::types::ToSql;

struct Pg;

impl SqlDialect for Pg {
    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
    fn placeholder(&self, position: usize) -> String {
        format!("${position}")
    }
}

/// orders(id text PK); lines(id int8 PK, "order" text → orders.id, qty int8,
/// price int8, status text).
fn shape() -> SchemaShape {
    SchemaShape::new()
        .table("orders", TableShape::new().primary_key("id").field("id"))
        .table(
            "lines",
            TableShape::new()
                .primary_key("id")
                .field("id")
                .key_field("order", "orders", "id")
                .field("qty")
                .field("price")
                .field("status"),
        )
        .user_fields(["id"])
}

async fn create_schema(client: &tokio_postgres::Client) {
    client
        .batch_execute(
            r#"CREATE TABLE orders (id text PRIMARY KEY);
               CREATE TABLE lines (id int8 PRIMARY KEY, "order" text REFERENCES orders(id),
                   qty int8, price int8, status text);
               INSERT INTO orders VALUES ('o1');"#,
        )
        .await
        .expect("create schema");
}

/// One child row, seeded both into `lines` and into the reified array.
#[derive(Clone)]
struct Line {
    id: i64,
    qty: Option<i64>,
    price: Option<i64>,
    status: Option<&'static str>,
}

impl Line {
    fn new(id: i64, qty: Option<i64>, price: Option<i64>, status: Option<&'static str>) -> Line {
        Line {
            id,
            qty,
            price,
            status,
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "qty": self.qty,
            "price": self.price,
            "status": self.status,
        })
    }
}

async fn sql_verdict(client: &tokio_postgres::Client, pred: QExpr) -> bool {
    let stmt: Statement = Select::from(Source::table("orders")).filter(pred).into();
    let (sql, binds) = Pg.render(&stmt).expect("render");
    let wrapped = format!("SELECT EXISTS({sql})");
    // Bind through the production `PgParam` so an integer literal lands in a
    // `numeric` aggregate context the same way it would in a real query.
    let params: Vec<PgParam> = binds.iter().map(PgParam).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let row = client.query_one(&wrapped, &refs).await.expect("query");
    row.get(0)
}

async fn seed(client: &tokio_postgres::Client, lines: &[Line]) {
    client
        .execute("DELETE FROM lines", &[])
        .await
        .expect("clear");
    for l in lines {
        client
            .execute(
                r#"INSERT INTO lines (id, "order", qty, price, status) VALUES ($1, 'o1', $2, $3, $4)"#,
                &[&l.id, &l.qty, &l.price, &l.status],
            )
            .await
            .expect("insert line");
    }
}

fn relation_binding(lines: &[Line]) -> BTreeMap<String, Value> {
    let arr = serde_json::Value::Array(lines.iter().map(Line::json).collect());
    BTreeMap::from([("linesↃorder".to_string(), Value::Json(arr))])
}

/// Assert three-way agreement for one aggregation case.
async fn check(
    client: &tokio_postgres::Client,
    ev: &DenoEvaluator,
    src: &str,
    lines: &[Line],
    user: Option<&[(&str, Value)]>,
    expect: bool,
) {
    seed(client, lines).await;
    let formula = Formula::parse(src).expect(src);
    let user_map = user.map(|u| u.iter().map(|(k, v)| (k.to_string(), v.clone())).collect());

    let env = UserEnv::Inline(user_map.clone());
    let pred = translate(
        &formula,
        Operation::Read,
        &Env::new(&env),
        &shape(),
        "orders",
    )
    .unwrap_or_else(|e| panic!("{src}: translate: {e}"));
    let symbolic = sql_verdict(client, pred).await;

    let reified = ev
        .eval(FormulaCall {
            formula,
            op: Operation::Read,
            row: relation_binding(lines),
            user: user_map,
            ambient: Default::default(),
            now: chrono::Utc::now(),
        })
        .await
        .unwrap_or_else(|e| panic!("{src}: reified: {e}"));

    assert_eq!(symbolic, reified, "{src}: evaluators disagree");
    assert_eq!(symbolic, expect, "{src}: verdict");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn symbolic_and_reified_agree_on_aggregations() {
    let db = TestDb::new().await.expect("test db");
    let client = db.client().await.expect("client");
    create_schema(&client).await;
    let ev = DenoEvaluator::new();

    // qty 1,2,3 (one null qty ignored by value aggregates); statuses vary.
    let three = [
        Line::new(1, Some(1), Some(10), Some("shipped")),
        Line::new(2, Some(2), Some(20), Some("pending")),
        Line::new(3, Some(3), Some(30), Some("shipped")),
    ];
    let with_null = [
        Line::new(1, Some(1), Some(10), Some("shipped")),
        Line::new(2, None, Some(20), Some("pending")),
        Line::new(3, Some(3), Some(30), Some("shipped")),
    ];
    let empty: [Line; 0] = [];

    // length / count.
    check(&client, &ev, "linesↃorder.length === 3", &three, None, true).await;
    check(&client, &ev, "linesↃorder.length === 0", &empty, None, true).await;

    // sum: null ignored, empty → 0 (the coalesce). `sum` widens to numeric in
    // Postgres, and the integer literal binds through it via `PgParam`.
    check(
        &client,
        &ev,
        "linesↃorder.sum(\"qty\") === 6",
        &three,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.sum(\"qty\") === 4",
        &with_null,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.sum(\"qty\") === 0",
        &empty,
        None,
        true,
    )
    .await;
    // sum with an arrow selector: 1*10 + 2*20 + 3*30 = 140.
    check(
        &client,
        &ev,
        "linesↃorder.sum(r => r.qty * r.price) === 140",
        &three,
        None,
        true,
    )
    .await;

    // avg / min / max, empty → null grants nothing.
    check(
        &client,
        &ev,
        "linesↃorder.avg(\"qty\") > 1.5",
        &three,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.avg(\"qty\") === null",
        &empty,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.min(\"qty\") === 1",
        &three,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.max(\"qty\") === 3",
        &three,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.max(\"qty\") === null",
        &empty,
        None,
        true,
    )
    .await;

    // some / every, including the null-predicate corners.
    check(
        &client,
        &ev,
        "linesↃorder.some(r => r.qty > 2)",
        &three,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.some(r => r.qty > 9)",
        &three,
        None,
        false,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.some(r => r.qty > 0)",
        &empty,
        None,
        false,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.every(r => r.qty > 0)",
        &three,
        None,
        true,
    )
    .await;
    // A null qty is not provenly > 0, so `every` fails.
    check(
        &client,
        &ev,
        "linesↃorder.every(r => r.qty > 0)",
        &with_null,
        None,
        false,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.every(r => r.qty > 0)",
        &empty,
        None,
        true,
    )
    .await;

    // filter folds into the WHERE (counted via length → int8).
    check(
        &client,
        &ev,
        "linesↃorder.filter(r => r.status === \"shipped\").length === 2",
        &three,
        None,
        true,
    )
    .await;

    // includes over a map; membership against a user field.
    let u1 = [("id", Value::Text("shipped".into()))];
    check(
        &client,
        &ev,
        "linesↃorder.map(r => r.status).includes(\"pending\")",
        &three,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.map(r => r.status).includes(user.id)",
        &three,
        Some(&u1),
        true,
    )
    .await;

    // distinct count.
    check(
        &client,
        &ev,
        "linesↃorder.distinct(\"status\").length === 2",
        &three,
        None,
        true,
    )
    .await;

    // maxBy / minBy member access, empty → null.
    check(
        &client,
        &ev,
        "linesↃorder.maxBy(\"qty\").status === \"shipped\"",
        &three,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.minBy(\"qty\").status === \"shipped\"",
        &three,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.maxBy(\"qty\").status === null",
        &empty,
        None,
        true,
    )
    .await;

    // maxBy tie-break by primary key: two rows share qty 5; the greater id
    // wins on both evaluators (SQL `ORDER BY qty DESC, id DESC`, prelude id
    // tie-break), so its status is the answer.
    let tie = [
        Line::new(1, Some(5), None, Some("low_id")),
        Line::new(2, Some(5), None, Some("high_id")),
    ];
    check(
        &client,
        &ev,
        "linesↃorder.maxBy(\"qty\").status === \"high_id\"",
        &tie,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.minBy(\"qty\").status === \"low_id\"",
        &tie,
        None,
        true,
    )
    .await;

    // join: string_agg is unordered, so only the deterministic single-element
    // and empty cases are asserted for parity.
    let one = [Line::new(1, Some(1), None, Some("only"))];
    check(
        &client,
        &ev,
        "linesↃorder.map(r => r.status).join(\", \") === \"only\"",
        &one,
        None,
        true,
    )
    .await;
    check(
        &client,
        &ev,
        "linesↃorder.map(r => r.status).join(\", \") === \"\"",
        &empty,
        None,
        true,
    )
    .await;
}

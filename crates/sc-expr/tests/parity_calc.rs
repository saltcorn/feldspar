//! Calculated-field parity (TODO Phase 8): a non-stored calc field's value
//! computed two ways must agree — the symbolic `translate_value` projected into
//! a `SELECT` and executed by Postgres, and the reified `eval_value` in V8 —
//! over a plain expression, a Ⱶ-join, a Ↄ-aggregation, and a calc field that
//! reads another calc field (inlined).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use sc_db_postgres::PgParam;
use sc_expr::{
    CalcFields, DenoEvaluator, Env, Formula, FormulaCall, JsEvaluator, Operation, SchemaShape,
    TableShape, UserEnv, translate_value,
};
use sc_query::{Expr as QExpr, Projection, Select, Source, SqlDialect, Statement, Value};
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

fn shape() -> SchemaShape {
    SchemaShape::new()
        .table(
            "books",
            TableShape::new()
                .primary_key("id")
                .field("id")
                .field("pages")
                .key_field("publisher", "publishers", "id"),
        )
        .table(
            "publishers",
            TableShape::new()
                .primary_key("id")
                .field("id")
                .field("name"),
        )
        .table(
            "reviews",
            TableShape::new()
                .primary_key("id")
                .field("id")
                .key_field("book", "books", "id"),
        )
}

async fn create_schema(client: &tokio_postgres::Client) {
    client
        .batch_execute(
            "CREATE TABLE publishers (id bigint primary key, name text);
             CREATE TABLE books (id bigint primary key, pages bigint,
                 publisher bigint references publishers(id));
             CREATE TABLE reviews (id bigint primary key, book bigint references books(id));
             INSERT INTO publishers VALUES (1, 'ACME');
             INSERT INTO books VALUES (1, 100, 1);
             INSERT INTO reviews VALUES (1, 1), (2, 1);",
        )
        .await
        .expect("schema");
}

/// The reified bindings for book 1 (the same world the database holds): its own
/// fields, the prefetched Ⱶ-join value, the prefetched Ↄ-relation, and any
/// earlier calc field already computed into the row.
fn bindings(extra: &[(&str, Value)]) -> BTreeMap<String, Value> {
    let mut row = BTreeMap::new();
    row.insert("pages".into(), Value::Int(100));
    row.insert("publisherⱵname".into(), Value::Text("ACME".into()));
    row.insert(
        "reviewsↃbook".into(),
        Value::Json(serde_json::json!([{ "id": 1 }, { "id": 2 }])),
    );
    for (k, v) in extra {
        row.insert((*k).into(), v.clone());
    }
    row
}

/// The symbolic value: project `(expr)::text` for book 1 and read it back.
async fn sql_value(client: &tokio_postgres::Client, calc: &CalcFields, src: &str) -> String {
    let formula = Formula::parse(src).unwrap();
    let user = UserEnv::Inline(None);
    let value = translate_value(
        &formula,
        &Env::new(&user).with_calc(calc),
        &shape(),
        "books",
    )
    .unwrap_or_else(|e| panic!("{src}: translate_value: {e}"));
    let as_text = QExpr::Cast {
        expr: Box::new(value),
        type_name: "text".into(),
    };
    let select = Select::from(Source::table("books"))
        .columns(vec![Projection::expr(as_text)])
        .filter(QExpr::col("id").eq(QExpr::lit(1_i64)));
    let stmt: Statement = select.into();
    let (sql, binds) = Pg.render(&stmt).unwrap();
    let params: Vec<PgParam> = binds.iter().map(PgParam).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let row = client.query_one(&sql, &refs).await.expect("query");
    row.get::<_, Option<String>>(0).unwrap_or_default()
}

/// The reified value, as the same text form.
async fn reified_value(ev: &DenoEvaluator, src: &str, extra: &[(&str, Value)]) -> String {
    let json = ev
        .eval_value(FormulaCall {
            formula: Formula::parse(src).unwrap(),
            op: Operation::Read,
            row: bindings(extra),
            user: None,
            ambient: Default::default(),
        })
        .await
        .unwrap_or_else(|e| panic!("{src}: eval_value: {e}"));
    match json {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[tokio::test]
async fn calc_field_values_agree_symbolic_and_reified() {
    let db = TestDb::new().await.expect("db");
    let client = db.client().await.expect("client");
    create_schema(&client).await;
    let ev = DenoEvaluator::new();
    let empty = CalcFields::new();

    // Plain arithmetic.
    assert_eq!(sql_value(&client, &empty, "pages * 2").await, "200");
    assert_eq!(reified_value(&ev, "pages * 2", &[]).await, "200");

    // A Ⱶ-join.
    assert_eq!(sql_value(&client, &empty, "publisherⱵname").await, "ACME");
    assert_eq!(reified_value(&ev, "publisherⱵname", &[]).await, "ACME");

    // A Ↄ-aggregation.
    assert_eq!(sql_value(&client, &empty, "reviewsↃbook.length").await, "2");
    assert_eq!(reified_value(&ev, "reviewsↃbook.length", &[]).await, "2");

    // A calc field reading another: `quad = double * 2`, `double = pages * 2`.
    // Symbolic inlines `double`; reified reads the earlier value from the row.
    let mut calc = CalcFields::new();
    calc.insert("double".into(), Formula::parse("pages * 2").unwrap());
    assert_eq!(sql_value(&client, &calc, "double * 2").await, "400");
    assert_eq!(
        reified_value(&ev, "double * 2", &[("double", Value::Int(200))]).await,
        "400"
    );

    // An array aggregate: Postgres's jsonb text spaces its elements, so the
    // two are compared as JSON.
    let json = |text: String| serde_json::from_str::<serde_json::Value>(&text).unwrap();
    let src = "reviewsↃbook.map(r => r.id)";
    assert_eq!(
        json(sql_value(&client, &empty, src).await),
        serde_json::json!([1, 2])
    );
    assert_eq!(
        json(reified_value(&ev, src, &[]).await),
        serde_json::json!([1, 2])
    );
}

//! Vectors on Postgres (TODO-agent-skills AS2.1–AS2.3): pgvector's `vector(n)`
//! read and written as JSON arrays through the driver's binary codec,
//! introspected with its dimensions, ordered by cosine distance, and the
//! sentence a database without pgvector answers with.

use sc_db::{ColumnDef, SchemaChange};
use sc_db_postgres::PgDriver;
use sc_query::{
    Expr, Insert, OrderBy, Projection, Select, Source, Statement, Value, VectorDistance,
};
use sc_test_harness::TestDb;
use serde_json::json;

fn vector(values: serde_json::Value) -> Expr {
    Expr::Cast {
        expr: Box::new(Expr::lit(Value::Json(values))),
        type_name: "vector(3)".into(),
    }
}

#[tokio::test]
async fn a_vector_column_round_trips_and_orders_by_cosine_distance() -> sc_error::Result<()> {
    let Some(db) = TestDb::with_pgvector().await? else {
        return Ok(());
    };
    let driver = PgDriver::from_pool(db.pool().clone());
    assert!(driver.detect_vector().await?.is_available());
    driver
        .apply_schema(&SchemaChange::CreateTable {
            name: "doc".into(),
            columns: vec![
                ColumnDef::new("id", "int8").not_null(),
                ColumnDef::new("embedding", "vector(3)"),
            ],
            primary_key: vec!["id".into()],
            unlogged: false,
        })
        .await?;
    let tables = driver.introspect().await?;
    let doc = tables.iter().find(|t| t.name == "doc").expect("doc");
    let column = doc
        .columns
        .iter()
        .find(|c| c.name == "embedding")
        .expect("embedding");
    assert_eq!(column.sql_type, "vector(3)");

    for (id, v) in [
        (1_i64, json!([1, 0, 0])),
        (2, json!([0.1, 1, 0])),
        (3, json!([0.9, 0.1, 0])),
    ] {
        let insert = Insert::row(
            "doc",
            vec!["id".into(), "embedding".into()],
            vec![Expr::lit(id), vector(v)],
        );
        driver
            .query(&Statement::from(insert))
            .await?
            .try_collect()
            .await?;
    }
    // The wrong length is pgvector's refusal.
    let wrong = Insert::row(
        "doc",
        vec!["id".into(), "embedding".into()],
        vec![Expr::lit(4_i64), vector(json!([1, 2]))],
    );
    let err = match driver.query(&Statement::from(wrong)).await {
        Ok(rows) => rows.try_collect().await.map(|_| ()).unwrap_err(),
        Err(e) => e,
    };
    assert!(err.to_string().contains("dimensions"), "{err}");

    let distance = Expr::VectorDistance(Box::new(VectorDistance {
        column: Expr::col("embedding"),
        to: vec![1.0, 0.0, 0.0],
    }));
    let mut select = Select::from(Source::table("doc"))
        .columns(vec![
            Projection::expr(Expr::col("id")),
            Projection::expr(Expr::col("embedding")),
            Projection::expr_as(distance.clone(), "d"),
        ])
        .limit(2);
    select.order = vec![OrderBy::asc(distance)];
    let rows = driver
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    let ids: Vec<_> = rows.iter().map(|r| r.get("id").cloned()).collect();
    assert_eq!(ids, vec![Some(Value::Int(1)), Some(Value::Int(3))]);
    assert_eq!(
        rows[0].get("embedding"),
        Some(&Value::Json(json!([1.0, 0.0, 0.0])))
    );
    assert_eq!(
        rows[1].get("embedding"),
        Some(&Value::Json(json!([0.9, 0.1, 0.0])))
    );
    match rows[0].get("d") {
        Some(Value::Float(d)) => assert!(d.abs() < 1e-6, "{d}"),
        other => panic!("a distance, not {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn a_database_without_pgvector_says_how_to_get_it() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = PgDriver::from_pool(db.pool().clone());
    let found = driver.detect_vector().await?;
    if found.is_available() {
        // This machine's default template has pgvector; nothing to refuse.
        return Ok(());
    }
    let reason = found.require().unwrap_err();
    assert!(
        reason.contains("pgvector")
            && (reason.contains("CREATE EXTENSION vector") || reason.contains("-pgvector")),
        "{reason}"
    );
    Ok(())
}

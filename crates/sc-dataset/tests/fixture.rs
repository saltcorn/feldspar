//! The tables every dataset test reads, on either backend.
//!
//! Small enough that the expected rows of each operation can be worked out by
//! hand, and shaped like the Analytics UI's demo data: neighbourhoods, the
//! houses in them, viewings of the houses, and a rate that changes over time
//! for the as-of join.

use std::sync::Arc;

use chrono::NaiveDate;
use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId, TableId};
use sc_dataset::{DatasetDef, Page, StagePage, read_stage, value_json};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::Value as Json;

/// A catalog on one backend, holding the fixture. The Postgres database is
/// kept alive for as long as the catalog is used.
pub struct Fixture {
    pub cat: Catalog,
    pub backend: &'static str,
    _db: Option<TestDb>,
}

fn column(name: &str, ty: BasicType) -> DataField {
    DataField::plain(name, TypeRef::Basic(ty))
}

fn id() -> DataField {
    column("id", BasicType::Int).required().primary_key()
}

fn key(name: &str, table: &str) -> DataField {
    let mut field = column(name, BasicType::Int);
    field.kind = DataFieldKind::Key {
        target_table: TableId(table.to_owned()),
        target_field: FieldId("id".to_owned()),
        summary_field: None,
    };
    field
}

fn date(s: &str) -> Value {
    Value::Date(NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date"))
}

async fn insert(cat: &Catalog, table: &str, columns: &[&str], rows: Vec<Vec<Value>>) -> Result<()> {
    let insert = Insert {
        table: table.to_owned(),
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        rows: rows
            .into_iter()
            .map(|r| r.into_iter().map(Expr::Lit).collect())
            .collect(),
        returning: Vec::new(),
    };
    cat.primary()
        .query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

async fn fill(cat: &Catalog) -> Result<()> {
    sc_dataset::bootstrap_datasets(cat).await?;
    cat.create_table("neighbourhoods", &[id(), column("name", BasicType::Text)])
        .await?;
    cat.create_table(
        "houses",
        &[
            id(),
            column("price", BasicType::Float),
            column("area", BasicType::Float),
            key("neighbourhood", "neighbourhoods"),
            column("year_built", BasicType::Int),
            column("sold", BasicType::Bool),
        ],
    )
    .await?;
    cat.create_table(
        "viewings",
        &[
            id(),
            key("house", "houses"),
            column("viewed_on", BasicType::Date),
            column("attended", BasicType::Bool),
        ],
    )
    .await?;
    cat.create_table(
        "rates",
        &[
            id(),
            column("valid_from", BasicType::Date),
            column("rate", BasicType::Float),
        ],
    )
    .await?;
    cat.create_table(
        "sales",
        &[
            id(),
            column("region", BasicType::Text),
            column("quarter", BasicType::Text),
            column("amount", BasicType::Int),
        ],
    )
    .await?;
    let t = |s: &str| Value::Text(s.to_owned());
    insert(
        cat,
        "neighbourhoods",
        &["id", "name"],
        vec![
            vec![Value::Int(1), t("North")],
            vec![Value::Int(2), t("South")],
            vec![Value::Int(3), t("East")],
        ],
    )
    .await?;
    let house = |id: i64, price: f64, area: f64, hood: i64, year: i64, sold: Option<bool>| {
        vec![
            Value::Int(id),
            Value::Float(price),
            Value::Float(area),
            Value::Int(hood),
            Value::Int(year),
            sold.map_or(Value::Null, Value::Bool),
        ]
    };
    insert(
        cat,
        "houses",
        &["id", "price", "area", "neighbourhood", "year_built", "sold"],
        vec![
            house(1, 200_000.0, 100.0, 1, 1990, Some(true)),
            house(2, 150_000.0, 50.0, 1, 2000, Some(false)),
            house(3, 90_000.0, 60.0, 2, 1985, Some(true)),
            house(4, 300_000.0, 120.0, 2, 2010, Some(true)),
            house(5, 120_000.0, 40.0, 1, 1990, None),
        ],
    )
    .await?;
    insert(
        cat,
        "viewings",
        &["id", "house", "viewed_on", "attended"],
        vec![
            vec![
                Value::Int(1),
                Value::Int(1),
                date("2024-01-05"),
                Value::Bool(true),
            ],
            vec![
                Value::Int(2),
                Value::Int(1),
                date("2024-02-10"),
                Value::Bool(false),
            ],
            vec![
                Value::Int(3),
                Value::Int(3),
                date("2024-01-20"),
                Value::Bool(true),
            ],
            vec![
                Value::Int(4),
                Value::Int(4),
                date("2024-03-01"),
                Value::Bool(true),
            ],
        ],
    )
    .await?;
    insert(
        cat,
        "rates",
        &["id", "valid_from", "rate"],
        vec![
            vec![Value::Int(1), date("2024-01-01"), Value::Float(0.03)],
            vec![Value::Int(2), date("2024-02-01"), Value::Float(0.04)],
            vec![Value::Int(3), date("2024-03-01"), Value::Float(0.05)],
        ],
    )
    .await?;
    let sale = |id: i64, region: &str, quarter: &str, amount: i64| {
        vec![Value::Int(id), t(region), t(quarter), Value::Int(amount)]
    };
    insert(
        cat,
        "sales",
        &["id", "region", "quarter", "amount"],
        vec![
            sale(1, "north", "q1", 10),
            sale(2, "north", "q2", 20),
            sale(3, "south", "q1", 5),
            sale(4, "north", "q1", 1),
        ],
    )
    .await?;
    Ok(())
}

/// The fixture on Postgres.
pub async fn postgres() -> Result<Fixture> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    fill(&cat).await?;
    Ok(Fixture {
        cat,
        backend: "postgres",
        _db: Some(db),
    })
}

/// The fixture on SQLite.
pub async fn sqlite() -> Result<Fixture> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let cat = Catalog::init(driver).await?;
    fill(&cat).await?;
    Ok(Fixture {
        cat,
        backend: "sqlite",
        _db: None,
    })
}

/// Both backends, one after the other.
pub async fn both() -> Result<Vec<Fixture>> {
    Ok(vec![postgres().await?, sqlite().await?])
}

/// Every row of the stage after `upto` operations (all when `None`), as
/// JSON, with its column names.
pub async fn read(fx: &Fixture, def: &DatasetDef, upto: Option<usize>) -> Result<StagePage> {
    read_stage(&fx.cat, def, upto, Page::first(1000)).await
}

/// The rows of a page, as JSON.
pub fn json_rows(page: &StagePage) -> Vec<Vec<Json>> {
    page.rows
        .iter()
        .map(|r| r.iter().map(value_json).collect())
        .collect()
}

/// The column names of a page.
pub fn names(page: &StagePage) -> Vec<String> {
    page.columns.iter().map(|c| c.name.clone()).collect()
}

/// Assert two tables of JSON are equal, numbers compared as numbers (so `3`
/// and `3.0`, and two floats a rounding apart, are equal).
#[track_caller]
pub fn assert_rows(backend: &str, actual: &[Vec<Json>], expected: &[Vec<Json>]) {
    let same = actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(a, e)| {
            a.len() == e.len()
                && a.iter()
                    .zip(e)
                    .all(|(x, y)| match (x.as_f64(), y.as_f64()) {
                        (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * (1.0 + y.abs()),
                        _ => x == y,
                    })
        });
    assert!(
        same,
        "on {backend}:\n  got      {}\n  expected {}",
        serde_json::to_string(actual).unwrap_or_default(),
        serde_json::to_string(expected).unwrap_or_default()
    );
}

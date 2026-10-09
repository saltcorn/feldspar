//! Stat cards worked out on both backends (analytics TODO A6.2), against
//! numbers counted by hand from eight sales:
//!
//! | id | sold_on    | amount | region |
//! |----|------------|--------|--------|
//! | 1  | 2023-12-31 | 5      | north  |
//! | 2  | 2024-01-05 | 10     | north  |
//! | 3  | 2024-01-20 | 20     | south  |
//! | 4  | 2024-02-03 | 30     | north  |
//! | 5  | 2024-02-14 | 40     | north  |
//! | 6  | 2024-02-28 | 50     | south  |
//! | 7  | 2024-03-01 | 60     | north  |
//! | 8  | 2024-03-15 | —      | south  |
//!
//! `at` is `sold_on` at noon UTC, a timestamp.

use std::sync::Arc;

use chrono::{NaiveDate, TimeZone, Utc};
use sc_analytics::card::{CardData, RenderedCard, StatCard, render_card};
use sc_catalog::{Catalog, DataField};
use sc_dataset::{Base, DatasetDef, DatasetId, save_dataset};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};

struct Fixture {
    cat: Catalog,
    backend: &'static str,
    sales: DatasetId,
    _db: Option<TestDb>,
}

fn column(name: &str, ty: BasicType) -> DataField {
    DataField::plain(name, TypeRef::Basic(ty))
}

fn day(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("a date")
}

async fn fill(cat: &Catalog) -> Result<DatasetId> {
    sc_dataset::bootstrap_datasets(cat).await?;
    cat.create_table(
        "sales",
        &[
            column("id", BasicType::Int).required().primary_key(),
            column("sold_on", BasicType::Date),
            column("at", BasicType::Timestamp),
            column("amount", BasicType::Float),
            column("region", BasicType::Text),
        ],
    )
    .await?;
    let rows: [(i64, NaiveDate, Option<f64>, &str); 8] = [
        (1, day(2023, 12, 31), Some(5.), "north"),
        (2, day(2024, 1, 5), Some(10.), "north"),
        (3, day(2024, 1, 20), Some(20.), "south"),
        (4, day(2024, 2, 3), Some(30.), "north"),
        (5, day(2024, 2, 14), Some(40.), "north"),
        (6, day(2024, 2, 28), Some(50.), "south"),
        (7, day(2024, 3, 1), Some(60.), "north"),
        (8, day(2024, 3, 15), None, "south"),
    ];
    let insert = Insert {
        table: "sales".into(),
        columns: ["id", "sold_on", "at", "amount", "region"]
            .iter()
            .map(|c| (*c).to_owned())
            .collect(),
        rows: rows
            .iter()
            .map(|(id, d, amount, region)| {
                let noon = Utc.from_utc_datetime(&d.and_hms_opt(12, 0, 0).expect("noon"));
                vec![
                    Value::Int(*id),
                    Value::Date(*d),
                    Value::Timestamp(noon),
                    amount.map_or(Value::Null, Value::Float),
                    Value::Text((*region).into()),
                ]
                .into_iter()
                .map(Expr::Lit)
                .collect()
            })
            .collect(),
        returning: Vec::new(),
    };
    cat.primary()
        .query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    let def = DatasetDef {
        id: DatasetId::new(),
        name: "Sales".into(),
        description: String::new(),
        base: Base::Table {
            table: "sales".into(),
        },
        operations: Vec::new(),
    };
    save_dataset(cat, &def).await?;
    Ok(def.id)
}

async fn both() -> Result<Vec<Fixture>> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let pg = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    let pg_sales = fill(&pg).await?;
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let lite = Catalog::init(driver).await?;
    let lite_sales = fill(&lite).await?;
    Ok(vec![
        Fixture {
            cat: pg,
            backend: "postgres",
            sales: pg_sales,
            _db: Some(db),
        },
        Fixture {
            cat: lite,
            backend: "sqlite",
            sales: lite_sales,
            _db: None,
        },
    ])
}

fn card(fx: &Fixture, mut raw: Json) -> StatCard {
    raw["dataset"] = json!(fx.sales);
    serde_json::from_value(raw).expect("a card")
}

async fn numbers(fx: &Fixture, raw: Json) -> CardData {
    match render_card(&fx.cat, &card(fx, raw)).await.expect("renders") {
        RenderedCard::Card(data) => data,
        RenderedCard::Refused { problems, .. } => {
            panic!("on {}: refused: {problems:?}", fx.backend)
        }
    }
}

async fn refused(fx: &Fixture, raw: Json) -> String {
    match render_card(&fx.cat, &card(fx, raw)).await.expect("answers") {
        RenderedCard::Refused { error, .. } => error,
        RenderedCard::Card(data) => panic!("on {}: drew {data:?}", fx.backend),
    }
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-9)
}

#[tokio::test]
async fn a_card_without_periods_summarises_every_row() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let sum = numbers(
            &fx,
            json!({ "value": { "function": "sum", "column": "amount" } }),
        )
        .await;
        assert_eq!(sum.value, Some(215.0), "{b}");
        assert_eq!(sum.label, "total of amount");
        assert!(sum.period.is_none() && sum.comparison.is_none() && sum.sparkline.is_empty());

        let mean = numbers(
            &fx,
            json!({ "value": { "function": "mean", "column": "amount" } }),
        )
        .await;
        assert!(close(mean.value, 215.0 / 7.0), "{b}: {:?}", mean.value);
        let median = numbers(
            &fx,
            json!({ "value": { "function": "median", "column": "amount" } }),
        )
        .await;
        assert_eq!(median.value, Some(30.0), "{b}");
        let rows = numbers(&fx, json!({ "value": { "function": "count" } })).await;
        assert_eq!(rows.value, Some(8.0), "{b}");
        let amounts = numbers(
            &fx,
            json!({ "value": { "function": "count", "column": "amount" } }),
        )
        .await;
        assert_eq!(
            amounts.value,
            Some(7.0),
            "{b}: a missing amount is not counted"
        );
        let regions = numbers(
            &fx,
            json!({ "value": { "function": "count_distinct", "column": "region" } }),
        )
        .await;
        assert_eq!(regions.value, Some(2.0), "{b}");
        let low = numbers(
            &fx,
            json!({ "value": { "function": "min", "column": "amount" } }),
        )
        .await;
        assert_eq!(low.value, Some(5.0), "{b}");

        // Filtered, and compared with every row.
        let north = numbers(
            &fx,
            json!({ "value": { "function": "sum", "column": "amount" },
                    "filter": "region == \"north\"", "comparison": "unfiltered" }),
        )
        .await;
        assert_eq!(north.value, Some(145.0), "{b}");
        let c = north.comparison.expect("compared");
        assert_eq!(c.value, Some(215.0), "{b}");
        assert!(close(c.ratio, 145.0 / 215.0), "{b}");
        assert_eq!(c.change, Some(-70.0), "{b}");
    }
    Ok(())
}

#[tokio::test]
async fn a_card_compares_the_latest_period_with_the_one_before() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        // March 2024 holds the latest sale: two sales, against three in February.
        let month = numbers(
            &fx,
            json!({ "value": { "function": "count" },
                    "time": { "column": "sold_on", "period": "month" },
                    "comparison": "previous_period" }),
        )
        .await;
        assert_eq!(month.value, Some(2.0), "{b}");
        let period = month.period.expect("a period");
        assert_eq!(
            (period.start, period.end),
            (day(2024, 3, 1), day(2024, 4, 1)),
            "{b}"
        );
        let c = month.comparison.expect("compared");
        assert_eq!(c.value, Some(3.0), "{b}");
        assert_eq!(c.change, Some(-1.0), "{b}");
        assert!(close(c.ratio, 2.0 / 3.0), "{b}");
        let previous = c.period.expect("the previous period");
        assert_eq!(
            (previous.start, previous.end),
            (day(2024, 2, 1), day(2024, 3, 1)),
            "{b}"
        );

        // The sparkline: November (nothing) to March, oldest first.
        let spark = numbers(
            &fx,
            json!({ "value": { "function": "count" },
                    "time": { "column": "sold_on", "period": "month" },
                    "comparison": "previous_period", "sparkline": true, "periods": 5 }),
        )
        .await;
        let points: Vec<(NaiveDate, Option<f64>)> =
            spark.sparkline.iter().map(|p| (p.start, p.value)).collect();
        assert_eq!(
            points,
            vec![
                (day(2023, 11, 1), Some(0.0)),
                (day(2023, 12, 1), Some(1.0)),
                (day(2024, 1, 1), Some(2.0)),
                (day(2024, 2, 1), Some(3.0)),
                (day(2024, 3, 1), Some(2.0)),
            ],
            "{b}"
        );
        assert_eq!(spark.comparison.expect("compared").value, Some(3.0), "{b}");

        // A mean of nothing is missing, not 0; a median per period.
        let medians = numbers(
            &fx,
            json!({ "value": { "function": "median", "column": "amount" },
                    "time": { "column": "sold_on", "period": "month" },
                    "sparkline": true, "periods": 3 }),
        )
        .await;
        let values: Vec<Option<f64>> = medians.sparkline.iter().map(|p| p.value).collect();
        assert_eq!(values, vec![Some(15.0), Some(40.0), Some(60.0)], "{b}");
        let means = numbers(
            &fx,
            json!({ "value": { "function": "mean", "column": "amount" },
                    "time": { "column": "sold_on", "period": "month" },
                    "sparkline": true, "periods": 6 }),
        )
        .await;
        assert_eq!(means.sparkline[0].value, None, "{b}: October had no sales");
        assert_eq!(means.value, Some(60.0), "{b}");

        // Quarters, and the card's filter choosing the latest period.
        let quarter = numbers(
            &fx,
            json!({ "value": { "function": "sum", "column": "amount" },
                    "filter": "region == \"north\"",
                    "time": { "column": "sold_on", "period": "quarter" },
                    "comparison": "previous_period" }),
        )
        .await;
        assert_eq!(quarter.value, Some(140.0), "{b}");
        assert_eq!(
            quarter.comparison.expect("compared").value,
            Some(5.0),
            "{b}"
        );

        // Against every row, over the same period.
        let share = numbers(
            &fx,
            json!({ "value": { "function": "count" }, "filter": "region == \"north\"",
                    "time": { "column": "sold_on", "period": "month" },
                    "comparison": "unfiltered" }),
        )
        .await;
        assert_eq!(share.value, Some(1.0), "{b}");
        let c = share.comparison.expect("compared");
        assert_eq!((c.value, c.ratio), (Some(2.0), Some(0.5)), "{b}");
    }
    Ok(())
}

#[tokio::test]
async fn weeks_of_a_timestamp_and_today_s_period() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        // The latest sale is on Friday 15 March 2024: its week began on the
        // 11th, and the week before had no sales.
        let week = numbers(
            &fx,
            json!({ "value": { "function": "count" },
                    "time": { "column": "at", "period": "week" },
                    "comparison": "previous_period" }),
        )
        .await;
        assert_eq!(week.value, Some(1.0), "{b}");
        assert_eq!(
            week.period.expect("a period").start,
            day(2024, 3, 11),
            "{b}"
        );
        let c = week.comparison.expect("compared");
        assert_eq!(
            (c.value, c.change, c.ratio),
            (Some(0.0), Some(1.0), None),
            "{b}"
        );

        // Today's month has no sales: 0, and a period that is today's.
        let today = numbers(
            &fx,
            json!({ "value": { "function": "count" },
                    "time": { "column": "sold_on", "period": "month", "anchor": "today" } }),
        )
        .await;
        assert_eq!(today.value, Some(0.0), "{b}");
        let start = today.period.expect("a period").start;
        assert!(start > day(2024, 3, 1), "{b}: {start}");
    }
    Ok(())
}

#[tokio::test]
async fn a_card_that_does_not_read_says_why() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let err = refused(
            &fx,
            json!({ "value": { "function": "sum", "column": "region" } }),
        )
        .await;
        assert!(
            err.contains("needs numbers, and `region` is a text"),
            "{b}: {err}"
        );
        let err = refused(
            &fx,
            json!({ "value": { "function": "count" },
                    "time": { "column": "amount", "period": "month" } }),
        )
        .await;
        assert!(err.contains("periods are taken from a date"), "{b}: {err}");
        let err = refused(
            &fx,
            json!({ "value": { "function": "count" }, "filter": "nope > 1" }),
        )
        .await;
        assert!(
            err.starts_with("the card's filter does not read"),
            "{b}: {err}"
        );
        assert!(err.contains("nope"), "{b}: {err}");

        let mut gone = card(&fx, json!({ "value": { "function": "count" } }));
        gone.dataset = DatasetId::new();
        match render_card(&fx.cat, &gone).await? {
            RenderedCard::Refused { error, .. } => assert!(error.contains("gone"), "{b}: {error}"),
            RenderedCard::Card(_) => panic!("{b}: drew a card of nothing"),
        }
    }
    Ok(())
}

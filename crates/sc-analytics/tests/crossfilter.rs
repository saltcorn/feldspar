//! Cross-filtering on both backends (analytics TODO A6.3–A6.4): panels drawn
//! with a dashboard's conditions, against rows counted by hand.
//!
//! `districts`: 1 North, 2 South, 3 East.
//!
//! `incidents`:
//!
//! | id | district | category  | occurred_on |
//! |----|----------|-----------|-------------|
//! | 1  | 1        | burglary  | 2025-01-10  |
//! | 2  | 1        | theft     | 2025-02-03  |
//! | 3  | 2        | burglary  | 2025-02-15  |
//! | 4  | 2        | burglary  | 2025-03-01  |
//! | 5  | 3        | vandalism | 2025-03-20  |
//! | 6  | 1        | burglary  | 2025-03-28  |
//! | 7  | —        | theft     | 2025-04-02  |
//!
//! `at` is `occurred_on` at noon UTC, a timestamp.
//!
//! `patrols` (only a `district` foreign key in common with the incidents):
//! 1 → district 1, 5 hours; 2 → 1, 3 hours; 3 → 2, 4 hours; 4 → 3, 2 hours.

use std::sync::Arc;

use chrono::{NaiveDate, TimeZone, Utc};
use sc_analytics::crossfilter::Condition;
use sc_analytics::map::render_map_in;
use sc_analytics::panel::{Panel, render_panel_in};
use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId, TableId};
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
    incidents: DatasetId,
    districts: DatasetId,
    patrols: DatasetId,
    by_district: DatasetId,
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

async fn dataset(cat: &Catalog, name: &str, table: &str, operations: Json) -> Result<DatasetId> {
    let def = DatasetDef {
        id: DatasetId::new(),
        name: name.into(),
        description: String::new(),
        base: Base::Table {
            table: table.into(),
        },
        operations: serde_json::from_value(operations).expect("operations"),
    };
    save_dataset(cat, &def).await?;
    Ok(def.id)
}

async fn fill(cat: Catalog, backend: &'static str, db: Option<TestDb>) -> Result<Fixture> {
    sc_dataset::bootstrap_datasets(&cat).await?;
    cat.create_table("districts", &[id(), column("name", BasicType::Text)])
        .await?;
    cat.create_table(
        "incidents",
        &[
            id(),
            key("district", "districts"),
            column("category", BasicType::Text),
            column("occurred_on", BasicType::Date),
            column("at", BasicType::Timestamp),
        ],
    )
    .await?;
    cat.create_table(
        "patrols",
        &[
            id(),
            key("district", "districts"),
            column("hours", BasicType::Float),
        ],
    )
    .await?;
    let text = |s: &str| Value::Text(s.into());
    insert(
        &cat,
        "districts",
        &["id", "name"],
        vec![
            vec![Value::Int(1), text("North")],
            vec![Value::Int(2), text("South")],
            vec![Value::Int(3), text("East")],
        ],
    )
    .await?;
    let incidents: [(i64, Option<i64>, &str, (i32, u32, u32)); 7] = [
        (1, Some(1), "burglary", (2025, 1, 10)),
        (2, Some(1), "theft", (2025, 2, 3)),
        (3, Some(2), "burglary", (2025, 2, 15)),
        (4, Some(2), "burglary", (2025, 3, 1)),
        (5, Some(3), "vandalism", (2025, 3, 20)),
        (6, Some(1), "burglary", (2025, 3, 28)),
        (7, None, "theft", (2025, 4, 2)),
    ];
    insert(
        &cat,
        "incidents",
        &["id", "district", "category", "occurred_on", "at"],
        incidents
            .iter()
            .map(|(id, district, category, (y, m, d))| {
                let day = NaiveDate::from_ymd_opt(*y, *m, *d).expect("a date");
                let noon = Utc.from_utc_datetime(&day.and_hms_opt(12, 0, 0).expect("noon"));
                vec![
                    Value::Int(*id),
                    district.map_or(Value::Null, Value::Int),
                    text(category),
                    Value::Date(day),
                    Value::Timestamp(noon),
                ]
            })
            .collect(),
    )
    .await?;
    insert(
        &cat,
        "patrols",
        &["id", "district", "hours"],
        [(1, 1, 5.0), (2, 1, 3.0), (3, 2, 4.0), (4, 3, 2.0)]
            .iter()
            .map(|(id, d, h)| vec![Value::Int(*id), Value::Int(*d), Value::Float(*h)])
            .collect(),
    )
    .await?;
    let incidents = dataset(&cat, "Incidents", "incidents", json!([])).await?;
    let districts = dataset(&cat, "Districts", "districts", json!([])).await?;
    let patrols = dataset(&cat, "Patrols", "patrols", json!([])).await?;
    let by_district = dataset(
        &cat,
        "Incidents by district",
        "incidents",
        json!([{ "id": "g", "enabled": true, "kind": "aggregate", "params": {
            "group_by": [{ "name": "district", "formula": "district" }],
            "summaries": [{ "name": "n", "function": "count" }]
        } }]),
    )
    .await?;
    Ok(Fixture {
        cat,
        backend,
        incidents,
        districts,
        patrols,
        by_district,
        _db: db,
    })
}

async fn both() -> Result<Vec<Fixture>> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let pg = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let lite = Catalog::init(driver).await?;
    Ok(vec![
        fill(pg, "postgres", Some(db)).await?,
        fill(lite, "sqlite", None).await?,
    ])
}

fn condition(raw: Json) -> Condition {
    serde_json::from_value(raw).expect("a condition")
}

fn card(dataset: DatasetId, value: Json, extra: Json) -> Panel {
    let mut content = json!({ "dataset": dataset, "value": value });
    if let (Some(c), Some(e)) = (content.as_object_mut(), extra.as_object()) {
        c.extend(e.clone());
    }
    Panel::from_json(&json!({
        "id": uuid::Uuid::new_v4(), "kind": "stat_card", "content": content
    }))
    .expect("a card")
}

fn count(dataset: DatasetId) -> Panel {
    card(dataset, json!({ "function": "count" }), json!({}))
}

/// The panel drawn with `conditions`, as the API answers it.
async fn drawn(fx: &Fixture, panel: &Panel, conditions: &[Condition]) -> Json {
    let rendered = render_panel_in(&fx.cat, panel, conditions)
        .await
        .unwrap_or_else(|e| panic!("on {}: {e}", fx.backend));
    serde_json::to_value(&rendered).expect("serialises")
}

/// A card's value with `conditions`.
async fn value(fx: &Fixture, panel: &Panel, conditions: &[Condition]) -> Option<f64> {
    let answer = drawn(fx, panel, conditions).await;
    assert!(
        answer["card"]["error"].is_null(),
        "on {}: {}",
        fx.backend,
        answer["card"]
    );
    answer["card"]["value"].as_f64()
}

#[tokio::test]
async fn selecting_a_district_filters_a_dataset_that_only_shares_its_key() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        // A district clicked on a map of the districts table: the rows
        // themselves, by their key.
        let north = condition(json!({ "id": "map", "dataset": fx.districts, "values": [1] }));
        let hours = card(
            fx.patrols,
            json!({ "function": "sum", "column": "hours" }),
            json!({}),
        );
        assert_eq!(value(&fx, &hours, &[]).await, Some(14.0), "{b}");
        assert_eq!(
            value(&fx, &hours, std::slice::from_ref(&north)).await,
            Some(8.0),
            "{b}: patrols of North"
        );
        assert_eq!(
            value(&fx, &count(fx.incidents), std::slice::from_ref(&north)).await,
            Some(3.0),
            "{b}: incidents in North"
        );
        assert_eq!(
            value(&fx, &count(fx.districts), std::slice::from_ref(&north)).await,
            Some(1.0),
            "{b}: North itself"
        );
        let answer = drawn(&fx, &hours, std::slice::from_ref(&north)).await;
        assert_eq!(
            answer["filters"],
            json!([{ "id": "map", "dataset": fx.patrols, "column": "district" }]),
            "{b}"
        );

        // A bar of an aggregated dataset, grouped by the key: the same.
        let south = condition(json!({
            "id": "bar", "dataset": fx.by_district, "column": "district", "values": [2]
        }));
        assert_eq!(
            value(&fx, &hours, std::slice::from_ref(&south)).await,
            Some(4.0),
            "{b}"
        );
        // Two conditions both hold: South's incidents that are burglaries.
        let burglary = condition(json!({
            "id": "cat", "dataset": fx.incidents, "column": "category", "values": ["burglary"]
        }));
        assert_eq!(
            value(
                &fx,
                &count(fx.incidents),
                &[south.clone(), burglary.clone()]
            )
            .await,
            Some(2.0),
            "{b}"
        );
        // A category is not a key: it does not reach the patrols, and says so.
        let answer = drawn(&fx, &hours, std::slice::from_ref(&burglary)).await;
        assert_eq!(answer["card"]["value"], json!(14.0), "{b}");
        assert_eq!(
            answer["filters"][0]["skipped"],
            json!(
                "`category` is a column of `Incidents`, and neither refers to the rows of a \
                 table that `Patrols` refers to"
            ),
            "{b}"
        );
        // A missing district is a value too.
        let nowhere = condition(json!({
            "id": "none", "dataset": fx.incidents, "column": "district", "values": [null]
        }));
        assert_eq!(
            value(&fx, &count(fx.incidents), &[nowhere]).await,
            Some(1.0),
            "{b}"
        );
        // A value that is not one of the column's is skipped with a sentence.
        let wrong = condition(json!({
            "id": "w", "dataset": fx.incidents, "column": "district", "values": ["abc"]
        }));
        let answer = drawn(&fx, &count(fx.incidents), &[wrong]).await;
        assert_eq!(answer["card"]["value"], json!(7.0), "{b}");
        assert_eq!(
            answer["filters"][0]["skipped"],
            json!("\"abc\" is not a value of `district`, an integer column"),
            "{b}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_brushed_range_of_dates_filters_the_panels_on_its_dataset() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let range = |min: Json, max: Json, exclusive: bool| {
            condition(json!({
                "id": "brush", "dataset": fx.incidents, "column": "occurred_on",
                "range": { "min": min, "max": max, "max_exclusive": exclusive }
            }))
        };
        let incidents = count(fx.incidents);
        assert_eq!(
            value(
                &fx,
                &incidents,
                &[range(json!("2025-02-01"), json!("2025-03-01"), false)]
            )
            .await,
            Some(3.0),
            "{b}: 2, 3 and 4"
        );
        assert_eq!(
            value(
                &fx,
                &incidents,
                &[range(json!("2025-02-01"), json!("2025-03-01"), true)]
            )
            .await,
            Some(2.0),
            "{b}: 2 and 3"
        );
        // A time axis brushes in milliseconds, and a timestamp's bound is an
        // instant: from 2025-03-01T00:00Z.
        let from_march = condition(json!({
            "id": "t", "dataset": fx.incidents, "column": "at",
            "range": { "min": 1_740_787_200_000_i64 }
        }));
        assert_eq!(
            value(&fx, &incidents, &[from_march]).await,
            Some(4.0),
            "{b}: 4 to 7"
        );

        // A plot of the same dataset: the bars of what is left.
        let bars = Panel::from_json(&json!({
            "id": uuid::Uuid::new_v4(), "kind": "plot", "content": { "spec": {
                "data": { "kind": "dataset", "dataset": fx.incidents },
                "layers": [{ "mark": "bar", "stat": { "kind": "count" },
                             "encoding": { "x": { "field": "category" } } }]
            } }
        }))
        .expect("a plot");
        let answer = drawn(
            &fx,
            &bars,
            &[range(json!("2025-02-01"), json!("2025-03-01"), false)],
        )
        .await;
        let layer = &answer["plot"]["layers"][0];
        let x = layer["columns"]
            .as_array()
            .and_then(|c| c.iter().position(|c| c == "x"))
            .expect("x");
        let y = layer["columns"]
            .as_array()
            .and_then(|c| c.iter().position(|c| c == "y"))
            .expect("y");
        let mut bars: Vec<(String, f64)> = layer["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|r| {
                (
                    r[x].as_str().unwrap_or_default().to_owned(),
                    r[y].as_f64().unwrap_or_default(),
                )
            })
            .collect();
        bars.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            bars,
            vec![("burglary".to_owned(), 2.0), ("theft".to_owned(), 1.0)],
            "{b}"
        );

        // A summary table counts what is left.
        let table = Panel::from_json(&json!({
            "id": uuid::Uuid::new_v4(), "kind": "summary_table", "content": { "spec": {
                "data": { "kind": "dataset", "dataset": fx.incidents },
                "rows": [{ "field": "category" }]
            } }
        }))
        .expect("a table");
        let answer = drawn(
            &fx,
            &table,
            &[range(json!("2025-02-01"), json!("2025-03-01"), false)],
        )
        .await;
        assert_eq!(answer["table"]["total"], json!(3), "{b}");
    }
    Ok(())
}

#[tokio::test]
async fn a_cards_unfiltered_comparison_leaves_the_selections_out() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let share = card(
            fx.incidents,
            json!({ "function": "count" }),
            json!({ "comparison": "unfiltered" }),
        );
        let burglary = condition(json!({
            "id": "cat", "dataset": fx.incidents, "column": "category", "values": ["burglary"]
        }));
        let answer = drawn(&fx, &share, &[burglary]).await;
        assert_eq!(answer["card"]["value"], json!(4.0), "{b}");
        assert_eq!(answer["card"]["comparison"]["value"], json!(7.0), "{b}");
    }
    Ok(())
}

#[tokio::test]
async fn a_map_layer_carries_the_conditions_in_its_filter() -> Result<()> {
    for fx in both().await? {
        let b = fx.backend;
        let spec = serde_json::from_value(json!({ "layers": [{
            "dataset": fx.incidents,
            "geometry": { "kind": "lon_lat", "longitude": "id", "latitude": "id" },
            "filter": "id > 1"
        }] }))
        .expect("a map");
        let burglary = condition(json!({
            "id": "cat", "dataset": fx.incidents, "column": "category", "values": ["burglary"]
        }));
        let scope = sc_analytics::crossfilter::scope(
            &fx.cat,
            &[burglary],
            &std::iter::once(fx.incidents).collect(),
        )
        .await?;
        // The request — what the tiles' URL carries — has both filters,
        // whether or not this database can draw it.
        let drawn = render_map_in(&fx.cat, &spec, &scope).await?;
        assert_eq!(
            drawn.layers[0].layer.filter.as_deref(),
            Some("(id > 1) && (category == \"burglary\")"),
            "{b}"
        );
    }
    Ok(())
}

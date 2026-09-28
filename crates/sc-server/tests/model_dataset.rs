//! A dataset materialised through the row layer (TODO "Predictive models",
//! Phase 1.5).
//!
//! `sc-model` cannot read a row — it is at layer 6 so a module can supply a
//! model provider — so it declares `DatasetSource` and this crate fills it in
//! over `sc_api::rows`. What that buys is what this file pins:
//!
//!   - the four things GOALS asks a dataset for are **one language**: a plain
//!     field, an arithmetic expression, a Ⱶ-join path and a Ↄ-aggregation all
//!     come back as columns of the frame, with no second vocabulary behind them;
//!   - the frame is **typed from the data** — a `numeric` column is a float
//!     column, a `text` one is a string column — because a provider's form is
//!     built against those types;
//!   - the filter restricts the rows, folded into the `WHERE`;
//!   - the row cap is asked as a `COUNT(*)` **before** the rows and refused by
//!     name, so a runaway dataset costs one count and not the process;
//!   - the split key rides along, so the frame divides into train and test — and
//!     a table with no single primary key still *reads*, and refuses only the
//!     split.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_model::{
    Column, ColumnType, Dataset, DatasetOrder, DatasetShape, DatasetSource, Read, Split,
};
use sc_server::CatalogDatasetSource;
use sc_test_harness::TestDb;

/// Four houses in two neighbourhoods, with viewings hanging off them, and a
/// keyless `readings` table beside them.
async fn setup(db: &TestDb) -> Result<Catalog> {
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE neighbourhoods (id bigint primary key, average_income numeric);
             CREATE TABLE houses (id bigint primary key, price numeric, bedrooms bigint,
                 sold boolean, region text,
                 neighbourhood bigint references neighbourhoods(id));
             CREATE TABLE viewings (id bigint primary key,
                 house bigint references houses(id));
             CREATE TABLE readings (sensor text, value numeric);
             INSERT INTO neighbourhoods VALUES (1, 40000), (2, 90000);
             INSERT INTO houses VALUES
                 (1, 100000, 2, true,  'north', 1),
                 (2, 250000, 4, true,  'south', 2),
                 (3, 175000, 3, false, 'north', 2),
                 (4, 320000, 5, true,  'south', 1);
             INSERT INTO viewings VALUES (1, 1), (2, 1), (3, 2);
             INSERT INTO readings VALUES ('a', 1), ('b', 2);",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    catalog.reload().await?;
    Ok(catalog)
}

/// The `houses` dataset of the milestone's own definition of done.
fn house_prices() -> Dataset {
    Dataset::new("houses")
        .column("price", "price")
        .column("bedrooms", "bedrooms")
        .column("region", "region")
        .column("income", "neighbourhoodⱵaverage_income")
        .column("viewings", "viewingsↃhouse.length")
}

#[tokio::test]
async fn a_dataset_is_one_language_for_fields_joins_and_aggregations() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let source = CatalogDatasetSource::new(Arc::new(catalog));

    let frame = source.materialise(&house_prices(), 1000).await?;

    assert_eq!(frame.rows, 4);
    assert_eq!(
        frame.names(),
        vec!["price", "bedrooms", "region", "income", "viewings"]
    );
    // Typed from the data, which is what a provider's form is built against.
    let shape = DatasetShape::of_frame("houses", &frame);
    assert_eq!(shape.column("price"), Some(ColumnType::Float));
    assert_eq!(shape.column("bedrooms"), Some(ColumnType::Int));
    assert_eq!(shape.column("region"), Some(ColumnType::Str));
    assert_eq!(shape.column("income"), Some(ColumnType::Float));
    assert_eq!(
        shape.numeric_columns(),
        vec!["price", "bedrooms", "income", "viewings"]
    );

    // The join path and the aggregation answered per row, in row order.
    let by_key = |name: &str| {
        let column = frame.column(name).cloned().unwrap();
        frame
            .keys
            .iter()
            .cloned()
            .zip((0..frame.rows).map(move |i| match &column {
                Column::Float(v) => v[i].map(|x| x.to_string()),
                Column::Int(v) => v[i].map(|x| x.to_string()),
                Column::Str(v) => v[i].clone(),
                _ => None,
            }))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let income = by_key("income");
    assert_eq!(income["int:1"].as_deref(), Some("40000"));
    assert_eq!(income["int:2"].as_deref(), Some("90000"));
    let viewings = by_key("viewings");
    assert_eq!(viewings["int:1"].as_deref(), Some("2"));
    assert_eq!(viewings["int:3"].as_deref(), Some("0"));
    Ok(())
}

#[tokio::test]
async fn the_filter_restricts_the_rows_and_the_split_divides_them() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let source = CatalogDatasetSource::new(Arc::new(catalog));

    let sold = house_prices().filtered("sold === true");
    let frame = source.materialise(&sold, 1000).await?;
    assert_eq!(frame.rows, 3);
    assert_eq!(frame.keys.len(), 3);

    // Every row lands on exactly one side, and the counts add up to the frame.
    let splits = frame.split(&Split::default().seeded(11))?;
    let counts = splits.counts;
    assert_eq!(counts.train + counts.validation + counts.test, 3);
    assert_eq!(splits.train.rows, counts.train);
    assert_eq!(splits.test.rows, counts.test);
    // Every split frame keeps every column.
    assert_eq!(splits.train.names(), frame.names());
    Ok(())
}

#[tokio::test]
async fn the_row_cap_is_refused_by_name_before_the_rows_are_read() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let source = CatalogDatasetSource::new(Arc::new(catalog));

    let err = source
        .materialise(&house_prices(), 2)
        .await
        .expect_err("four rows over a cap of two");
    let said = err.to_string();
    assert!(said.contains("more than 2 rows"), "{said}");
    assert!(said.contains("--model-max-rows"), "{said}");

    // A filter that brings it under the cap is the remedy the sentence names.
    let filtered = house_prices().filtered("bedrooms > 3");
    assert_eq!(source.materialise(&filtered, 2).await?.rows, 2);
    Ok(())
}

#[tokio::test]
async fn a_keyless_table_reads_but_cannot_be_split() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let source = CatalogDatasetSource::new(Arc::new(catalog));

    let ds = Dataset::new("readings")
        .column("sensor", "sensor")
        .column("value", "value");
    let frame = source.materialise(&ds, 1000).await?;
    // The read is unaffected …
    assert_eq!(frame.rows, 2);
    assert!(frame.keys.is_empty());
    // … the split is not, and it says why.
    let err = frame.split(&Split::default()).expect_err("no primary key");
    assert!(err.to_string().contains("nothing stable to hash"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_frame_comes_back_in_the_declared_order_with_ties_broken_by_the_key() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let source = CatalogDatasetSource::new(Arc::new(catalog));

    // `region` ties in pairs (north: 1, 3; south: 2, 4), so the key decides
    // within each — ascending, whichever way the region sorts.
    let ds = house_prices().ordered(DatasetOrder::desc("region"));
    let frame = source.materialise(&ds, 1000).await?;
    assert_eq!(frame.keys, vec!["int:2", "int:4", "int:1", "int:3"]);

    // A join path sorts like a column: neighbourhood 1 (40 000) before 2.
    let ds = house_prices()
        .ordered(DatasetOrder::asc("neighbourhoodⱵaverage_income"))
        .ordered(DatasetOrder::desc("price"));
    let frame = source.materialise(&ds, 1000).await?;
    assert_eq!(frame.keys, vec!["int:4", "int:1", "int:2", "int:3"]);

    // And a preview's `LIMIT` takes the first rows of *that* order.
    let first = source.read(&ds, &Read::all(1000).first(2)).await?;
    assert_eq!(first.keys, vec!["int:4", "int:1"]);
    Ok(())
}

#[tokio::test]
async fn a_filter_matching_nothing_is_an_empty_frame_and_not_an_error() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let source = CatalogDatasetSource::new(Arc::new(catalog));

    let ds = house_prices().filtered("bedrooms > 99");
    let frame = source.materialise(&ds, 1000).await?;
    assert_eq!(frame.rows, 0);
    // The columns survive: a frame with none would fail later as a shape error
    // rather than here as an empty fit.
    assert_eq!(frame.names().len(), 5);
    Ok(())
}

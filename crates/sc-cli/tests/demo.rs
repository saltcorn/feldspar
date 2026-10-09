//! `feldspar demo analytics` (analytics TODO A1.18, A2.15, A5.14): the demo's
//! tables and datasets, the same rows on every run and on both backends, and
//! nothing touched without `--replace`; the map demo's districts and incidents
//! where the database has PostGIS, and a sentence where it has not.

use std::sync::Arc;

use sc_analytics::demo::{
    DEMO_DATASETS, DEMO_DISTRICTS, DEMO_EVENTS, DEMO_EXTENT, DEMO_HOUSES, DEMO_INCIDENTS,
    DEMO_MAP_DATASETS, DEMO_PATIENTS, demo_analytics,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, OrderBy, Projection, Select, Source, Statement, Value};
use sc_test_harness::TestDb;

/// Every house, in id order, as text — what two runs must agree on.
async fn houses(cat: &Catalog) -> Result<Vec<String>> {
    let mut select = Select::from(Source::table("houses"));
    select.order = vec![OrderBy::asc(Expr::col("id"))];
    let rows = cat
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    Ok(rows.iter().map(|r| format!("{:?}", r.values())).collect())
}

/// The events, summarised: how many of each kind, and the sums of their
/// durations, sizes and hours — what the two backends' SQL must agree on.
async fn events(cat: &Catalog) -> Result<Vec<String>> {
    let sum = |c: &str| {
        Projection::expr(Expr::Agg {
            func: "sum".into(),
            distinct: false,
            args: vec![Expr::col(c)],
        })
    };
    let mut select = Select::from(Source::table("events")).columns(vec![
        Projection::expr(Expr::col("kind")),
        Projection::expr(Expr::Agg {
            func: "count".into(),
            distinct: false,
            args: Vec::new(),
        }),
        sum("duration_ms"),
        sum("size_kb"),
        sum("hour"),
        Projection::expr(Expr::Agg {
            func: "min".into(),
            distinct: false,
            args: vec![Expr::col("duration_ms")],
        }),
        Projection::expr(Expr::Agg {
            func: "max".into(),
            distinct: false,
            args: vec![Expr::col("duration_ms")],
        }),
    ]);
    select.group = vec![Expr::col("kind")];
    select.order = vec![OrderBy::asc(Expr::col("kind"))];
    let rows = cat
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            r.values()
                .iter()
                .map(|v| match v {
                    // Sums of a million tenths, rounded to compare; a sum of
                    // integers is a decimal on Postgres.
                    Value::Float(f) => format!("{f:.1}"),
                    Value::Decimal(d) => format!("{d:.1}"),
                    Value::Int(n) => format!("{n}.0"),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect())
}

async fn count(cat: &Catalog, table: &str) -> Result<i64> {
    let select = Select::from(Source::table(table)).columns(vec![Projection::expr(Expr::Agg {
        func: "count".into(),
        distinct: false,
        args: Vec::new(),
    })]);
    let rows = cat
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    match rows.first().and_then(|r| r.get_index(0)) {
        Some(Value::Int(n)) => Ok(*n),
        other => panic!("a count, got {other:?}"),
    }
}

async fn the_demo(cat: &Catalog, backend: &str) -> Result<(Vec<String>, Vec<String>)> {
    let started = std::time::Instant::now();
    let report = demo_analytics(cat, false).await?;
    eprintln!("{backend}: the demo took {:?}", started.elapsed());
    assert!(report.replaced.is_empty());
    // No PostGIS here (`TestDb::new` has none, SQLite cannot): the map demo
    // is left out with the reason, and the rest made.
    let skipped = report.skipped.clone().expect("no PostGIS, no districts");
    assert!(
        skipped.contains("`districts`, `incidents`") && skipped.contains("PostGIS"),
        "{backend}: {skipped}"
    );
    assert!(cat.get("districts")?.is_none());
    assert_eq!(report.tables[1], ("houses".to_owned(), DEMO_HOUSES));
    assert_eq!(count(cat, "neighbourhoods").await?, 5);
    assert_eq!(count(cat, "houses").await?, DEMO_HOUSES as i64);
    let viewings = report.tables[2].1 as i64;
    assert!(viewings > 300, "{backend}: {viewings} viewings");
    assert_eq!(count(cat, "viewings").await?, viewings);
    assert_eq!(count(cat, "patients").await?, DEMO_PATIENTS as i64);
    assert_eq!(count(cat, "measurements").await?, DEMO_PATIENTS as i64);
    assert_eq!(count(cat, "events").await?, DEMO_EVENTS as i64);

    // The datasets the Data explorer reads, each reading.
    assert_eq!(report.datasets, DEMO_DATASETS.map(str::to_owned).to_vec());
    assert!(report.kept.is_empty());
    let library = sc_dataset::load_library(cat).await?;
    let schema = sc_dataset::Schema::of_catalog(cat)?;
    for name in DEMO_DATASETS {
        let def = sc_dataset::load_dataset_by_name(cat, name)
            .await?
            .expect("the demo's dataset");
        let compiled = sc_dataset::compile(&schema, &library, &def, Default::default());
        let stage = compiled
            .last()
            .unwrap_or_else(|e| panic!("{backend}: `{name}` does not read: {e}"));
        if name == "Measurements" {
            let columns: Vec<String> = stage.shape().columns.into_iter().map(|c| c.name).collect();
            assert!(
                columns.ends_with(&["treatment".to_owned(), "change".to_owned()]),
                "{backend}: {columns:?}"
            );
        }
    }

    // Keys and types as the models tutorial has them.
    let houses_table = cat.require("houses")?;
    let is_key =
        |f: &sc_catalog::DataField| matches!(f.kind, sc_catalog::DataFieldKind::Key { .. });
    assert!(houses_table.field("neighbourhood").is_some_and(is_key));
    assert!(cat.require("viewings")?.field("house").is_some_and(is_key));

    // Nothing touched without `--replace`, and the refusal names the tables.
    let before = houses(cat).await?;
    let err = demo_analytics(cat, false)
        .await
        .expect_err("tables are there");
    assert!(
        err.to_string().contains(
            "`neighbourhoods`, `houses`, `viewings`, `patients`, `measurements`, `events`"
        ) && err.to_string().contains("--replace"),
        "{err}"
    );
    assert_eq!(houses(cat).await?, before);
    let events_before = events(cat).await?;
    assert_eq!(events_before.len(), 3, "{backend}: {events_before:?}");

    // With it, the same rows again; the datasets are kept, not made twice.
    let again = demo_analytics(cat, true).await?;
    assert_eq!(again.replaced.len(), 6);
    assert!(again.datasets.is_empty());
    assert_eq!(again.kept, DEMO_DATASETS.map(str::to_owned).to_vec());
    assert_eq!(
        houses(cat).await?,
        before,
        "{backend}: the rows are deterministic"
    );
    assert_eq!(events(cat).await?, events_before, "{backend}");

    // A row added later is numbered after the demo's.
    let insert = Insert::row(
        "neighbourhoods",
        vec!["name".into()],
        vec![Expr::lit("Newtown")],
    );
    cat.primary()
        .query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    assert_eq!(count(cat, "neighbourhoods").await?, 6);
    Ok((before, events_before))
}

#[tokio::test]
async fn the_demo_is_deterministic_and_leaves_existing_tables_alone() -> Result<()> {
    let db = TestDb::new().await?;
    let pg =
        Catalog::init(Arc::new(PgDriver::from_pool(db.pool().clone())) as Arc<dyn DatabaseDriver>)
            .await?;
    let (on_postgres, pg_events) = the_demo(&pg, "postgres").await?;

    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let sqlite = Catalog::init(driver).await?;
    let (on_sqlite, sqlite_events) = the_demo(&sqlite, "sqlite").await?;
    // One generator, so one set of houses — as far as the two backends'
    // readings of a row agree (SQLite has no boolean of its own).
    assert_eq!(on_postgres.len(), on_sqlite.len());
    // One statement, so one set of events: the same counts and sums.
    assert_eq!(pg_events, sqlite_events);
    Ok(())
}

/// Where the database has PostGIS, the map demo (A5.14): twelve districts
/// that tile the city, and the incidents inside it, each in one district; two
/// datasets over them; and the same rows again on `--replace`.
#[tokio::test]
async fn the_map_demo_makes_districts_and_incidents_where_there_is_postgis() -> Result<()> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(());
    };
    let cat =
        Catalog::init(Arc::new(PgDriver::from_pool(db.pool().clone())) as Arc<dyn DatabaseDriver>)
            .await?;
    let report = demo_analytics(&cat, false).await?;
    assert_eq!(report.skipped, None);
    assert_eq!(
        &report.tables[6..],
        [
            ("districts".to_owned(), DEMO_DISTRICTS),
            ("incidents".to_owned(), DEMO_INCIDENTS)
        ]
    );
    assert_eq!(count(&cat, "districts").await?, DEMO_DISTRICTS as i64);
    assert_eq!(count(&cat, "incidents").await?, DEMO_INCIDENTS as i64);
    let mut expected: Vec<String> = DEMO_DATASETS.map(str::to_owned).to_vec();
    expected.extend(DEMO_MAP_DATASETS.map(str::to_owned));
    assert_eq!(report.datasets, expected);

    let client = db.client().await?;
    let facts = |sql: &'static str| {
        let client = &client;
        async move { client.query_one(sql, &[]).await.expect(sql) }
    };
    // The districts tile the extent: their union is its rectangle, and no two
    // overlap.
    let [w, s, e, n] = DEMO_EXTENT;
    let row = facts(
        "SELECT ST_XMin(u), ST_YMin(u), ST_XMax(u), ST_YMax(u), ST_Area(u), \
         (SELECT count(*) FROM districts a JOIN districts b ON a.id < b.id \
          AND ST_Area(ST_Intersection(a.outline, b.outline)) > 1e-12) \
         FROM (SELECT ST_Union(outline) AS u FROM districts) AS t",
    )
    .await;
    let bounds: [f64; 5] = std::array::from_fn(|i| row.get(i));
    assert!(
        (bounds[0] - w).abs() < 1e-6 && (bounds[1] - s).abs() < 1e-6,
        "{bounds:?}"
    );
    assert!(
        (bounds[2] - e).abs() < 1e-6 && (bounds[3] - n).abs() < 1e-6,
        "{bounds:?}"
    );
    assert!((bounds[4] - (e - w) * (n - s)).abs() < 1e-9, "{bounds:?}");
    assert_eq!(row.get::<_, i64>(5), 0, "districts overlap");
    // Every incident is in exactly one district, and they are not spread
    // evenly: the busiest district has several times the quietest's.
    let row = facts(
        "SELECT count(*), min(c), max(c) FROM (SELECT d.id, count(i.id) AS c FROM districts d \
         LEFT JOIN incidents i ON ST_Within(i.location, d.outline) GROUP BY d.id) AS t",
    )
    .await;
    assert_eq!(row.get::<_, i64>(0), DEMO_DISTRICTS as i64);
    let (fewest, most): (i64, i64) = (row.get(1), row.get(2));
    let within = facts(
        "SELECT count(*) FROM incidents i JOIN districts d ON ST_Within(i.location, d.outline)",
    )
    .await;
    assert_eq!(within.get::<_, i64>(0), DEMO_INCIDENTS as i64);
    assert!(most > 3 * fewest.max(1), "{fewest} to {most}");
    let categories = facts("SELECT count(DISTINCT category) FROM incidents").await;
    assert_eq!(categories.get::<_, i64>(0), 5);

    // The datasets read, each with its geometry column.
    let library = sc_dataset::load_library(&cat).await?;
    let schema = sc_dataset::Schema::of_catalog(&cat)?;
    for (name, column) in [("Districts", "outline"), ("Incidents", "location")] {
        let def = sc_dataset::load_dataset_by_name(&cat, name)
            .await?
            .expect("the demo's dataset");
        let compiled = sc_dataset::compile(&schema, &library, &def, Default::default());
        let stage = compiled
            .last()
            .unwrap_or_else(|e| panic!("`{name}` does not read: {e}"));
        assert!(
            stage.shape().columns.iter().any(|c| c.name == column),
            "{name}"
        );
    }

    // The same rows on every run.
    let text = "SELECT string_agg(category || reported_on || ST_AsText(location), ';' ORDER BY id) \
                FROM incidents";
    let before: String = facts(text).await.get(0);
    let again = demo_analytics(&cat, true).await?;
    assert_eq!(again.replaced.len(), 8);
    assert_eq!(again.kept.len(), 5);
    let after: String = facts(text).await.get(0);
    assert_eq!(before, after);
    Ok(())
}

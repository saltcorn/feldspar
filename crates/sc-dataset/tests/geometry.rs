//! The `Geo` functions in a dataset's operations, computed by PostGIS
//! (analytics TODO A5.3), and their refusal on a database without it.
//!
//! The expected numbers are worked out independently of PostGIS: distances by
//! the haversine formula and areas from the size of a degree, both within a
//! fraction of a percent of the spheroid's answer.

use std::sync::Arc;

use sc_catalog::{Catalog, DataField};
use sc_dataset::{
    AggregateOp, ColType, DatasetDef, GroupKey, Library, Op, Options, Schema, Summary, compile,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, GeometryKind, TypeRef};
use serde_json::{Value as Json, json};

use crate::fixture::{both, json_rows, names, read};

/// London stations, and two parks drawn as 0.01° squares.
const STATIONS: &[(i64, &str, f64, f64)] = &[
    (1, "King's Cross", -0.1246, 51.5308),
    (2, "Waterloo", -0.1131, 51.5031),
    (3, "Paddington", -0.1759, 51.5154),
    (4, "Stratford", -0.0035, 51.5419),
    (5, "Wimbledon", -0.2064, 51.4214),
];

/// Trafalgar Square.
const CENTRE: (f64, f64) = (-0.1276, 51.5072);

pub(crate) fn point(lon: f64, lat: f64) -> Value {
    Value::Json(json!({"type": "Point", "coordinates": [lon, lat]}))
}

pub(crate) fn square(lon: f64, lat: f64) -> Value {
    Value::Json(json!({"type": "Polygon", "coordinates": [[
        [lon, lat], [lon + 0.01, lat], [lon + 0.01, lat + 0.01], [lon, lat + 0.01], [lon, lat]
    ]]}))
}

/// Great-circle distance in metres on a sphere of the mean Earth radius.
pub(crate) fn haversine((lon1, lat1): (f64, f64), (lon2, lat2): (f64, f64)) -> f64 {
    let r = 6_371_008.8;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

pub(crate) async fn insert(
    cat: &Catalog,
    table: &str,
    columns: &[&str],
    rows: Vec<Vec<Value>>,
) -> Result<()> {
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

/// The stations and parks in a database with PostGIS, or `None` where there is
/// none to be had.
async fn london() -> Result<Option<(Catalog, TestDb)>> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(None);
    };
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    sc_catalog::bootstrap_spatial(&cat).await?;
    sc_dataset::bootstrap_datasets(&cat).await?;
    let column = |name: &str, ty: BasicType| DataField::plain(name, TypeRef::Basic(ty));
    let id = || column("id", BasicType::Int).required().primary_key();
    cat.create_table(
        "stations",
        &[
            id(),
            column("name", BasicType::Text),
            column("location", BasicType::Geometry(GeometryKind::Point)),
        ],
    )
    .await?;
    cat.create_table(
        "parks",
        &[
            id(),
            column("outline", BasicType::Geometry(GeometryKind::Polygon)),
        ],
    )
    .await?;
    insert(
        &cat,
        "stations",
        &["id", "name", "location"],
        STATIONS
            .iter()
            .map(|(id, name, lon, lat)| {
                vec![
                    Value::Int(*id),
                    Value::Text((*name).into()),
                    point(*lon, *lat),
                ]
            })
            .collect(),
    )
    .await?;
    insert(
        &cat,
        "parks",
        &["id", "outline"],
        vec![vec![Value::Int(1), square(-0.17, 51.50)]],
    )
    .await?;
    Ok(Some((cat, db)))
}

pub(crate) async fn rows(cat: &Catalog, def: &DatasetDef) -> Result<(Vec<String>, Vec<Vec<Json>>)> {
    let page = sc_dataset::read_stage(cat, def, None, sc_dataset::Page::first(1000)).await?;
    Ok((
        page.columns.iter().map(|c| c.name.clone()).collect(),
        page.rows
            .iter()
            .map(|r| r.iter().map(sc_dataset::value_json).collect())
            .collect(),
    ))
}

#[tokio::test]
async fn geo_functions_compute_metres_cells_and_conditions() -> Result<()> {
    let Some((cat, _db)) = london().await? else {
        return Ok(());
    };
    let (lon, lat) = CENTRE;
    let def = DatasetDef::over_table("d", "stations")
        .then(Op::calculated(
            "from_centre",
            format!("Geo.distance(location, Geo.point({lon}, {lat}))"),
        ))
        .then(Op::calculated("cell", "Geo.hexCell(location, 1000)"))
        .then(Op::calculated("square", "Geo.squareCell(location, 1000)"))
        .then(Op::calculated(
            "nearby",
            "Geo.within(location, Geo.buffer(Geo.point(-0.1276, 51.5072), 3000))",
        ));

    // The types follow from the functions.
    let schema = Schema::of_catalog(&cat)?;
    let compiled = compile(&schema, &Library::default(), &def, Options::default());
    let shape = compiled.last().expect("valid").shape();
    let ty = |n: &str| shape.columns.iter().find(|c| c.name == n).map(|c| c.ty);
    assert_eq!(ty("location"), Some(ColType::Geometry));
    assert_eq!(ty("from_centre"), Some(ColType::Float));
    assert_eq!(ty("cell"), Some(ColType::Geometry));
    assert_eq!(ty("nearby"), Some(ColType::Bool));

    let (columns, got) = rows(&cat, &def).await?;
    let col = |n: &str| columns.iter().position(|c| c == n).expect(n);
    for (row, (_, name, slon, slat)) in got.iter().zip(STATIONS) {
        // The location reads back as the GeoJSON it was written as.
        assert_eq!(
            row[col("location")],
            json!({"type": "Point", "coordinates": [slon, slat]})
        );
        // Metres on the spheroid, within half a percent of the sphere's.
        let metres = row[col("from_centre")].as_f64().expect("a number");
        let expected = haversine((*slon, *slat), CENTRE);
        assert!(
            (metres - expected).abs() / expected < 0.005,
            "{name}: {metres} m, expected about {expected}"
        );
        assert_eq!(row[col("nearby")], json!(expected < 3000.0), "{name}");
        // A hexagon of six sides around the station, in WGS84.
        let cell = &row[col("cell")];
        assert_eq!(cell["type"], "Polygon", "{name}: {cell}");
        assert_eq!(
            cell["coordinates"][0].as_array().map(Vec::len),
            Some(7),
            "{cell}"
        );
        let square = &row[col("square")];
        assert_eq!(
            square["coordinates"][0].as_array().map(Vec::len),
            Some(5),
            "{square}"
        );
    }

    // A Filter by a Geo condition, and an Aggregate grouped by a cell: the
    // three central stations within 3 km share no 1 km cell, so three groups;
    // at 20 km they all share one or two.
    let near = DatasetDef::over_table("near", "stations")
        .then(Op::filter(format!(
            "Geo.distance(location, Geo.point({lon}, {lat})) < 3000"
        )))
        .then(Op::Aggregate(AggregateOp {
            group_by: vec![GroupKey {
                name: "cell".into(),
                formula: "Geo.squareCell(location, 1000)".into(),
            }],
            summaries: vec![Summary::count("n")],
        }));
    let (_, groups) = rows(&cat, &near).await?;
    let total: f64 = groups.iter().map(|g| g[1].as_f64().unwrap_or(0.0)).sum();
    assert_eq!(total, 2.0, "King's Cross and Waterloo: {groups:?}");

    // An area in square metres: 0.01° by 0.01° at 51.5° N.
    let parks = DatasetDef::over_table("p", "parks")
        .then(Op::calculated("m2", "Geo.area(outline)"))
        .then(Op::calculated("centre", "Geo.centroid(outline)"));
    let (columns, got) = rows(&cat, &parks).await?;
    let m2 = got[0][columns.iter().position(|c| c == "m2").expect("m2")]
        .as_f64()
        .expect("a number");
    let degree = 111_195.0; // metres per degree of latitude on the mean sphere
    let expected = (0.01 * degree) * (0.01 * degree * 51.505f64.to_radians().cos());
    assert!(
        (m2 - expected).abs() / expected < 0.01,
        "{m2} against {expected}"
    );
    let centre = &got[0][columns.iter().position(|c| c == "centre").expect("centre")];
    let c = centre["coordinates"].as_array().expect("a point");
    assert!(
        (c[0].as_f64().unwrap_or(0.0) + 0.165).abs() < 1e-3,
        "{centre}"
    );
    assert!(
        (c[1].as_f64().unwrap_or(0.0) - 51.505).abs() < 1e-3,
        "{centre}"
    );
    Ok(())
}

#[tokio::test]
async fn a_geo_function_is_refused_where_the_database_cannot_compute_it() -> Result<()> {
    for fx in both().await? {
        if fx.cat.primary().spatial().is_available() {
            continue;
        }
        let schema = Schema::of_catalog(&fx.cat)?;
        let def = DatasetDef::over_table("d", "houses")
            .then(Op::calculated("at", "Geo.point(price, area)"));
        let compiled = compile(&schema, &Library::default(), &def, Options::default());
        let error = compiled.operations[0].error.clone().unwrap_or_default();
        assert!(
            error.contains("`Geo.point` is computed by the database") && error.contains("PostGIS"),
            "on {}: {error}",
            fx.backend
        );
        if fx.backend == "sqlite" {
            assert!(error.contains("SQLite"), "{error}");
        }
        // The stage before it still reads.
        let page = read(&fx, &def, Some(0)).await?;
        assert!(names(&page).contains(&"price".to_owned()));
        assert_eq!(json_rows(&page).len(), 5);
    }
    Ok(())
}

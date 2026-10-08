//! The Spatial join operation and the geometry union summary (analytics TODO
//! A5.4), computed by PostGIS, and their refusal on a database without it.
//!
//! Two zones side by side, each a 0.01° square, and four spots: two in the
//! first zone, one in the second, one 0.03° east of both. Every expected
//! count is worked out by hand from that picture, and every expected distance
//! by the haversine formula.

use std::sync::Arc;

use sc_catalog::{Catalog, DataField};
use sc_dataset::{
    AggregateOp, ColType, DatasetDef, ForeignKey, Grain, GroupKey, JoinKind, Library, Op, Options,
    Other, Schema, SpatialJoinOp, SpatialRelation, Summary, SummaryFunction, compile,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::Value;
use sc_test_harness::TestDb;
use sc_types::{BasicType, GeometryKind, TypeRef};
use serde_json::{Value as Json, json};

use crate::fixture::{both, json_rows, names, read};
use crate::geometry::{haversine, insert, point, rows, square};

/// The spots: id, longitude, latitude.
const SPOTS: &[(i64, f64, f64)] = &[
    (1, 0.005, 51.505),
    (2, 0.002, 51.502),
    (3, 0.015, 51.505),
    (4, 0.050, 51.505),
];

async fn zones() -> Result<Option<(Catalog, TestDb)>> {
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
        "zones",
        &[
            id(),
            column("name", BasicType::Text),
            column("outline", BasicType::Geometry(GeometryKind::Polygon)),
        ],
    )
    .await?;
    cat.create_table(
        "spots",
        &[id(), column("at", BasicType::Geometry(GeometryKind::Point))],
    )
    .await?;
    insert(
        &cat,
        "zones",
        &["id", "name", "outline"],
        vec![
            vec![Value::Int(1), Value::Text("A".into()), square(0.0, 51.5)],
            vec![Value::Int(2), Value::Text("B".into()), square(0.01, 51.5)],
        ],
    )
    .await?;
    insert(
        &cat,
        "spots",
        &["id", "at"],
        SPOTS
            .iter()
            .map(|(id, lon, lat)| vec![Value::Int(*id), point(*lon, *lat)])
            .collect(),
    )
    .await?;
    Ok(Some((cat, db)))
}

fn join(with: &str, relation: SpatialRelation, left: &str, right: &str) -> SpatialJoinOp {
    SpatialJoinOp {
        with: Other::Table { table: with.into() },
        kind: JoinKind::Left,
        relation,
        left: left.into(),
        right: right.into(),
        distance: None,
        distance_column: None,
        columns: None,
        suffix: "_right".into(),
    }
}

/// The values of `column` in `rows`, in order.
fn column(names: &[String], rows: &[Vec<Json>], column: &str) -> Vec<Json> {
    let i = names.iter().position(|n| n == column).expect(column);
    rows.iter().map(|r| r[i].clone()).collect()
}

fn float(v: &Json) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

#[tokio::test]
async fn a_spatial_join_matches_points_to_the_region_they_are_in() -> Result<()> {
    let Some((cat, _db)) = zones().await? else {
        return Ok(());
    };
    let schema = Schema::of_catalog(&cat)?;

    // Within, kept left: every spot, with its zone's columns beside it; the
    // zone's `id` is taken, so it comes across as `id_right` — a reference
    // to the zone, so its name is a step away.
    let def = DatasetDef::over_table("in zones", "spots")
        .then(Op::SpatialJoin(join(
            "zones",
            SpatialRelation::Within,
            "at",
            "outline",
        )))
        .then(Op::calculated("zone", "id_rightⱵname"));
    let compiled = compile(&schema, &Library::default(), &def, Options::default());
    let shape = compiled.stage(1).expect("valid").shape();
    let names_after: Vec<&str> = shape.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names_after, ["id", "at", "id_right", "name", "outline"]);
    let id_right = shape.columns.iter().find(|c| c.name == "id_right");
    assert_eq!(
        id_right.and_then(|c| c.key.clone()),
        Some(ForeignKey {
            table: "zones".into(),
            field: "id".into()
        })
    );
    // A spot might be in two regions, so a row is no longer a spot's.
    assert_eq!(shape.grain, Grain::Derived);
    let (n, got) = rows(&cat, &def).await?;
    assert_eq!(
        column(&n, &got, "id"),
        [json!(1), json!(2), json!(3), json!(4)]
    );
    assert_eq!(
        column(&n, &got, "zone"),
        [json!("A"), json!("A"), json!("B"), Json::Null]
    );

    // Counted per zone: the Spatial join and the Aggregate that "count per
    // region" is. Grouped by the zone's key, a row is a zone's again.
    let counted = DatasetDef::over_table("per zone", "spots")
        .then(Op::SpatialJoin(SpatialJoinOp {
            kind: JoinKind::Inner,
            columns: Some(vec!["id".into()]),
            ..join("zones", SpatialRelation::Within, "at", "outline")
        }))
        .then(Op::Aggregate(AggregateOp {
            group_by: vec![GroupKey {
                name: "zone".into(),
                formula: "id_right".into(),
            }],
            summaries: vec![Summary::count("spots")],
        }))
        .then(Op::calculated("zone_name", "zoneⱵname"));
    let compiled = compile(&schema, &Library::default(), &counted, Options::default());
    assert!(compiled.is_valid(), "{:?}", compiled.first_error());
    let (n, got) = rows(&cat, &counted).await?;
    assert_eq!(column(&n, &got, "zone_name"), [json!("A"), json!("B")]);
    assert_eq!(column(&n, &got, "spots"), [json!(2), json!(1)]);

    // Contains, the other way round: each zone with the spots inside it.
    let containing =
        DatasetDef::over_table("zones with spots", "zones").then(Op::SpatialJoin(SpatialJoinOp {
            kind: JoinKind::Inner,
            columns: Some(vec!["id".into()]),
            suffix: "_spot".into(),
            ..join("spots", SpatialRelation::Contains, "outline", "at")
        }));
    let (n, got) = rows(&cat, &containing).await?;
    let mut pairs: Vec<(i64, i64)> = column(&n, &got, "id")
        .iter()
        .zip(column(&n, &got, "id_spot"))
        .map(|(z, s)| (z.as_i64().unwrap_or(0), s.as_i64().unwrap_or(0)))
        .collect();
    pairs.sort_unstable();
    assert_eq!(pairs, [(1, 1), (1, 2), (2, 3)]);

    // Intersects: the zones share an edge, so each meets itself and the
    // other.
    let meeting = DatasetDef::over_table("meeting", "zones").then(Op::SpatialJoin(SpatialJoinOp {
        kind: JoinKind::Inner,
        columns: Some(vec!["name".into()]),
        ..join("zones", SpatialRelation::Intersects, "outline", "outline")
    }));
    let (_, got) = rows(&cat, &meeting).await?;
    assert_eq!(got.len(), 4);
    Ok(())
}

#[tokio::test]
async fn within_a_distance_and_nearest_measure_metres() -> Result<()> {
    let Some((cat, _db)) = zones().await? else {
        return Ok(());
    };
    let schema = Schema::of_catalog(&cat)?;
    let spot = |id: i64| {
        let (_, lon, lat) = SPOTS.iter().find(|s| s.0 == id).copied().expect("a spot");
        (lon, lat)
    };

    // Within 500 m of each other: every spot of itself, and spots 1 and 2
    // (about 390 m apart) of each other. Spot 3 is about 690 m from spot 1.
    assert!(haversine(spot(1), spot(2)) < 500.0);
    assert!(haversine(spot(1), spot(3)) > 500.0);
    let near = DatasetDef::over_table("near", "spots").then(Op::SpatialJoin(SpatialJoinOp {
        kind: JoinKind::Inner,
        distance: Some(500.0),
        distance_column: Some("metres".into()),
        suffix: "_other".into(),
        ..join("spots", SpatialRelation::WithinDistance, "at", "at")
    }));
    let (n, got) = rows(&cat, &near).await?;
    let mut pairs: Vec<(i64, i64, f64)> = got
        .iter()
        .map(|r| {
            let at = |c: &str| r[n.iter().position(|x| x == c).expect(c)].clone();
            (
                at("id").as_i64().unwrap_or(0),
                at("id_other").as_i64().unwrap_or(0),
                float(&at("metres")),
            )
        })
        .collect();
    pairs.sort_by_key(|p| (p.0, p.1));
    let ids: Vec<(i64, i64)> = pairs.iter().map(|p| (p.0, p.1)).collect();
    assert_eq!(ids, [(1, 1), (1, 2), (2, 1), (2, 2), (3, 3), (4, 4)]);
    for (a, b, metres) in &pairs {
        let expected = haversine(spot(*a), spot(*b));
        assert!(
            (metres - expected).abs() <= 0.005 * expected.max(1.0),
            "{a}–{b}: {metres} m, expected about {expected}"
        );
    }

    // The nearest zone to each spot, and how far: none for the spots inside
    // one, about 2.1 km for spot 4, east of zone B's edge at 0.02°.
    let nearest = DatasetDef::over_table("nearest", "spots").then(Op::SpatialJoin(SpatialJoinOp {
        distance_column: Some("metres".into()),
        columns: Some(vec!["name".into()]),
        ..join("zones", SpatialRelation::Nearest, "at", "outline")
    }));
    let compiled = compile(&schema, &Library::default(), &nearest, Options::default());
    let stage = compiled.last().expect("valid");
    // One match at most, so a row is still a spot's, and keyed by it.
    assert!(stage.has_row_key());
    assert_eq!(
        stage.shape().grain,
        Grain::Table {
            table: "spots".into(),
            key: "id".into()
        }
    );
    let (n, got) = rows(&cat, &nearest).await?;
    assert_eq!(
        column(&n, &got, "name"),
        [json!("A"), json!("A"), json!("B"), json!("B")]
    );
    let metres: Vec<f64> = column(&n, &got, "metres").iter().map(float).collect();
    assert_eq!(&metres[..3], [0.0, 0.0, 0.0]);
    let expected = haversine(spot(4), (0.02, 51.505));
    assert!(
        (metres[3] - expected).abs() / expected < 0.005,
        "{} m, expected about {expected}",
        metres[3]
    );

    // Looking no further than a kilometre, spot 4 has no nearest zone: kept
    // with nothing beside it by a left join, dropped by an inner one.
    let within_km = |kind| {
        DatasetDef::over_table("nearest within 1 km", "spots").then(Op::SpatialJoin(
            SpatialJoinOp {
                kind,
                distance: Some(1000.0),
                columns: Some(vec!["name".into()]),
                ..join("zones", SpatialRelation::Nearest, "at", "outline")
            },
        ))
    };
    let (n, got) = rows(&cat, &within_km(JoinKind::Left)).await?;
    assert_eq!(
        column(&n, &got, "name"),
        [json!("A"), json!("A"), json!("B"), Json::Null]
    );
    let (_, got) = rows(&cat, &within_km(JoinKind::Inner)).await?;
    assert_eq!(got.len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_union_dissolves_regions_into_one_geometry() -> Result<()> {
    let Some((cat, _db)) = zones().await? else {
        return Ok(());
    };
    let dissolved = DatasetDef::over_table("dissolved", "zones")
        .then(Op::calculated("m2", "Geo.area(outline)"))
        .then(Op::Aggregate(AggregateOp {
            group_by: Vec::new(),
            summaries: vec![
                Summary::of("merged", SummaryFunction::Union, "outline"),
                Summary::of("total_m2", SummaryFunction::Sum, "m2"),
            ],
        }))
        .then(Op::calculated("merged_m2", "Geo.area(merged)"));
    let schema = Schema::of_catalog(&cat)?;
    let compiled = compile(&schema, &Library::default(), &dissolved, Options::default());
    let shape = compiled.last().expect("valid").shape();
    let ty = |n: &str| shape.columns.iter().find(|c| c.name == n).map(|c| c.ty);
    assert_eq!(ty("merged"), Some(ColType::Geometry));
    let (n, got) = rows(&cat, &dissolved).await?;
    assert_eq!(got.len(), 1);
    // The two squares share an edge, so they dissolve into one rectangle of
    // their combined area.
    let merged = &column(&n, &got, "merged")[0];
    assert_eq!(merged["type"], "Polygon", "{merged}");
    let (total, area) = (
        float(&column(&n, &got, "total_m2")[0]),
        float(&column(&n, &got, "merged_m2")[0]),
    );
    assert!(
        (total - area).abs() / total < 1e-6,
        "{total} against {area}"
    );
    Ok(())
}

#[tokio::test]
async fn a_spatial_join_says_what_it_cannot_do() -> Result<()> {
    if let Some((cat, _db)) = zones().await? {
        let schema = Schema::of_catalog(&cat)?;
        let error = |op: SpatialJoinOp| {
            let def = DatasetDef::over_table("d", "spots").then(Op::SpatialJoin(op));
            let compiled = compile(&schema, &Library::default(), &def, Options::default());
            compiled.operations[0].error.clone().unwrap_or_default()
        };
        let within = join("zones", SpatialRelation::Within, "at", "outline");
        let e = error(SpatialJoinOp {
            kind: JoinKind::Full,
            ..within.clone()
        });
        assert!(e.contains("choose inner or left"), "{e}");
        let e = error(join("zones", SpatialRelation::Within, "id", "outline"));
        assert!(
            e.contains("`id` is integer") && e.contains("geometry column"),
            "{e}"
        );
        let e = error(join("zones", SpatialRelation::Within, "at", "name"));
        assert!(e.contains("`name` is text"), "{e}");
        let e = error(join("zones", SpatialRelation::Within, "at", "shape"));
        assert!(
            e.contains("`shape` is not a column of what is joined") && e.contains("`outline`"),
            "{e}"
        );
        let e = error(join(
            "zones",
            SpatialRelation::WithinDistance,
            "at",
            "outline",
        ));
        assert!(e.contains("needs the distance, in metres"), "{e}");
        let e = error(SpatialJoinOp {
            distance: Some(-5.0),
            ..join("zones", SpatialRelation::Nearest, "at", "outline")
        });
        assert!(e.contains("give a positive number"), "{e}");
        let e = error(SpatialJoinOp {
            distance_column: Some("id".into()),
            ..within.clone()
        });
        assert!(e.contains("name the distance column something else"), "{e}");
        // A union of something that is not geometry.
        let def = DatasetDef::over_table("d", "zones").then(Op::Aggregate(AggregateOp {
            group_by: Vec::new(),
            summaries: vec![Summary::of("u", SummaryFunction::Union, "name")],
        }));
        let compiled = compile(&schema, &Library::default(), &def, Options::default());
        let e = compiled.operations[0].error.clone().unwrap_or_default();
        assert!(
            e.contains("`name` is text, and a union joins geometries"),
            "{e}"
        );
    }

    // Without PostGIS, both are refused with the reason, and the stage
    // before still reads.
    for fx in both().await? {
        if fx.cat.primary().spatial().is_available() {
            continue;
        }
        let schema = Schema::of_catalog(&fx.cat)?;
        let def = DatasetDef::over_table("d", "houses").then(Op::SpatialJoin(join(
            "neighbourhoods",
            SpatialRelation::Intersects,
            "price",
            "name",
        )));
        let compiled = compile(&schema, &Library::default(), &def, Options::default());
        let e = compiled.operations[0].error.clone().unwrap_or_default();
        assert!(
            e.contains("a spatial join is computed by the database") && e.contains("PostGIS"),
            "on {}: {e}",
            fx.backend
        );
        let def = DatasetDef::over_table("d", "houses").then(Op::Aggregate(AggregateOp {
            group_by: Vec::new(),
            summaries: vec![Summary::of("u", SummaryFunction::Union, "price")],
        }));
        let compiled = compile(&schema, &Library::default(), &def, Options::default());
        let e = compiled.operations[0].error.clone().unwrap_or_default();
        assert!(!e.is_empty(), "on {}", fx.backend);
        let page = read(&fx, &def, Some(0)).await?;
        assert!(names(&page).contains(&"price".to_owned()));
        assert_eq!(json_rows(&page).len(), 5);
    }
    Ok(())
}

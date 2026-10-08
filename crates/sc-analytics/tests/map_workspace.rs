//! The Map workspace (analytics TODO A5.8–A5.12): styles and their classes,
//! the attribute table, selection and saving it, and the toolbox — over the
//! sites and regions of `layers.rs`, on PostGIS; each test returns early where
//! there is none.
//!
//! The fixture: regions 1 "west" and 2 "east", two circles about 700 m
//! across; sites a and b in the west, c in the east, each about 1.3 km from
//! the next.

use sc_analytics::classify::Classification;
use sc_analytics::layer::{GeometrySource, LayerData, LayerRequest, SKETCH_VALUES, layer_sketch};
use sc_analytics::map::{MapEncoding, MapLayer, MapSpec, Style, render_map};
use sc_analytics::plot::FieldDef;
use sc_analytics::selection::{SelectBy, layer_rows, save_selection, select_features};
use sc_analytics::tools::{ToolArgs, run_tool};
use sc_dataset::{
    AggregateOp, DatasetDef, GroupKey, Op, Operation, Page, SortKey, Summary, read_stage,
    save_dataset,
};
use sc_error::Result;
use sc_query::Value;
use serde_json::{Value as Json, json};

use crate::layers::{insert, sites};

fn at() -> GeometrySource {
    GeometrySource::Column {
        column: "at".into(),
    }
}

fn outline() -> GeometrySource {
    GeometrySource::Column {
        column: "outline".into(),
    }
}

fn colour(field: &str) -> MapEncoding {
    MapEncoding {
        color: Some(FieldDef::of(field)),
        ..MapEncoding::default()
    }
}

/// The sites counted per region: rows that are not a table's, keyed by the
/// region.
async fn per_region(
    cat: &sc_catalog::Catalog,
    sites_id: sc_dataset::DatasetId,
) -> Result<DatasetDef> {
    let def = DatasetDef::new("Sites per region", sc_dataset::Base::dataset(sites_id)).then(
        Op::Aggregate(AggregateOp {
            group_by: vec![GroupKey::column("region")],
            summaries: vec![Summary::count("count")],
        }),
    );
    save_dataset(cat, &def).await?;
    Ok(def)
}

fn refused(data: &LayerData) -> &str {
    match data {
        LayerData::Refused { error } => error,
        other => panic!("not refused: {other:?}"),
    }
}

#[tokio::test]
async fn graduated_colours_come_with_their_breaks_and_proportional_symbols_are_points() -> Result<()>
{
    let Some((cat, _db, sites_id, regions_id)) = sites().await? else {
        return Ok(());
    };
    let graduated = |method, classes| MapLayer {
        encoding: colour("lon"),
        style: Style::Graduated { method, classes },
        ..MapLayer::new(sites_id, at())
    };
    let spec = MapSpec::of(vec![
        graduated(Classification::NaturalBreaks, 3),
        graduated(Classification::Quantile, 2),
        graduated(Classification::EqualInterval, 4),
        // Regions drawn as circles at their centres, sized by their id.
        MapLayer {
            encoding: MapEncoding {
                size: Some(FieldDef::of("id")),
                ..MapEncoding::default()
            },
            style: Style::Proportional,
            ..MapLayer::new(regions_id, outline())
        },
    ]);
    let drawn = render_map(&cat, &spec).await?;
    let classes = |i: usize| drawn.layers[i].classes.clone().expect("classes");
    // Three distinct longitudes, three classes, the last of the largest alone.
    assert_eq!(classes(0), vec![0.01, 0.02, 0.03, 0.03]);
    assert_eq!(classes(1), vec![0.01, 0.02, 0.03]);
    let equal = classes(2);
    assert_eq!(equal.len(), 5);
    assert!((equal[1] - 0.015).abs() < 1e-12, "{equal:?}");
    // Its classes are its colour scale: no range besides.
    assert!(!drawn.layers[0].domains.contains_key("color"));

    let regions = &drawn.layers[3];
    assert!(regions.layer.points);
    match &regions.data {
        LayerData::Geojson { geometry, .. } => assert_eq!(geometry, &["point"]),
        other => panic!("not GeoJSON: {other:?}"),
    }
    assert_eq!(regions.domains["size"].max, Some(json!(2.0)));
    assert_eq!(regions.classes, None);

    // What a style needs, said with a sentence; the other layers still drawn.
    let spec = MapSpec::of(vec![
        MapLayer {
            encoding: colour("name"),
            style: Style::Graduated {
                method: Classification::Quantile,
                classes: 4,
            },
            ..MapLayer::new(sites_id, at())
        },
        MapLayer {
            style: Style::Categories,
            ..MapLayer::new(sites_id, at())
        },
        MapLayer {
            style: Style::Proportional,
            ..MapLayer::new(sites_id, at())
        },
        MapLayer {
            popup: vec!["name".into(), "colour".into()],
            ..MapLayer::new(sites_id, at())
        },
        MapLayer {
            style: Style::Graduated {
                method: Classification::Quantile,
                classes: 12,
            },
            encoding: colour("lon"),
            ..MapLayer::new(sites_id, at())
        },
        MapLayer {
            encoding: colour("region"),
            style: Style::Categories,
            ..MapLayer::new(sites_id, at())
        },
    ]);
    let drawn = render_map(&cat, &spec).await?;
    assert_eq!(
        refused(&drawn.layers[0].data),
        "graduated colours need a number on Color, and `name` is text; choose categories instead"
    );
    assert!(refused(&drawn.layers[1].data).contains("put one there"));
    assert!(refused(&drawn.layers[2].data).contains("number on Size"));
    assert_eq!(
        refused(&drawn.layers[3].data),
        "`colour` in the popup is not a column of `Sites`"
    );
    assert!(refused(&drawn.layers[4].data).contains("from 2 to 7 classes"));
    // A key on Color as categories: its values.
    assert_eq!(
        drawn.layers[5].domains["color"].values,
        Some(vec![json!(1), json!(2)])
    );
    Ok(())
}

#[tokio::test]
async fn the_sketch_is_every_value_of_a_small_layer_and_rank_spaced_of_a_large_one() -> Result<()> {
    let Some((cat, _db, sites_id, _)) = sites().await? else {
        return Ok(());
    };
    let small = layer_sketch(&cat, &LayerRequest::new(sites_id, at()), "lat")
        .await?
        .expect("reads");
    assert_eq!(small, vec![51.5, 51.51, 51.52]);

    // 2,500 points, the value of each its number: half of them read.
    cat.create_table(
        "many",
        &[
            crate::layers::column("id", sc_types::BasicType::Int)
                .required()
                .primary_key(),
            crate::layers::column("v", sc_types::BasicType::Float),
            crate::layers::column(
                "at",
                sc_types::BasicType::Geometry(sc_types::GeometryKind::Point),
            ),
        ],
    )
    .await?;
    let rows: Vec<Vec<Value>> = (1..=2500)
        .map(|i| {
            vec![
                Value::Int(i),
                // In reverse, so the sketch's order is the value's, not the key's.
                Value::Float(f64::from(2501 - i as i32)),
                Value::Json(json!({ "type": "Point", "coordinates": [0.001 * i as f64, 51.0] })),
            ]
        })
        .collect();
    insert(&cat, "many", &["id", "v", "at"], rows).await?;
    let many = DatasetDef::over_table("Many", "many");
    save_dataset(&cat, &many).await?;
    let sketch = layer_sketch(&cat, &LayerRequest::new(many.id, at()), "v")
        .await?
        .expect("reads");
    assert!(
        sketch.len() <= 2 * SKETCH_VALUES as usize + 1,
        "{}",
        sketch.len()
    );
    assert!(sketch.len() >= SKETCH_VALUES as usize, "{}", sketch.len());
    assert_eq!(sketch.first(), Some(&1.0));
    assert_eq!(sketch.last(), Some(&2500.0));
    assert!(sketch.windows(2).all(|w| w[0] < w[1]));
    // Read again, the same values.
    let again = layer_sketch(&cat, &LayerRequest::new(many.id, at()), "v")
        .await?
        .expect("reads");
    assert_eq!(sketch, again);
    Ok(())
}

#[tokio::test]
async fn the_attribute_table_has_each_rows_feature_id() -> Result<()> {
    let Some((cat, _db, sites_id, _)) = sites().await? else {
        return Ok(());
    };
    let req = LayerRequest::new(sites_id, at());
    let rows = layer_rows(&cat, &req, None, 100).await?.expect("reads");
    let names: Vec<&str> = rows.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "lon", "lat", "region", "notes"]);
    assert_eq!(rows.ids, vec![json!(1), json!(2), json!(3)]);
    assert_eq!(rows.total, 3);
    assert!(rows.keyed && rows.sorted);
    assert_eq!(rows.rows[0][1], json!("a"));

    // Sorted on the server while rows are a table's: the ids go with them.
    let by_name = SortKey {
        formula: "name".into(),
        descending: true,
    };
    let rows = layer_rows(&cat, &req, Some(&by_name), 100)
        .await?
        .expect("reads");
    assert_eq!(rows.ids, vec![json!(3), json!(2), json!(1)]);
    assert_eq!(rows.rows[0][1], json!("c"));
    // A layer's filter, and a page.
    let filtered = LayerRequest {
        filter: Some("region == 1".into()),
        ..req.clone()
    };
    let rows = layer_rows(&cat, &filtered, None, 1).await?.expect("reads");
    assert_eq!((rows.ids.clone(), rows.total), (vec![json!(1)], 2));

    // Rows that are not a table's: ids are places in the order, and the
    // table sorts them itself.
    let counted = per_region(&cat, sites_id).await?;
    let geometry = GeometrySource::Key {
        column: "region".into(),
        geometry: "outline".into(),
    };
    let rows = layer_rows(
        &cat,
        &LayerRequest::new(counted.id, geometry),
        Some(&by_name),
        100,
    )
    .await?
    .expect("reads");
    assert_eq!(rows.ids, vec![json!(1), json!(2)]);
    assert!(!rows.keyed && !rows.sorted);
    assert_eq!(
        rows.rows,
        vec![vec![json!(1), json!(2)], vec![json!(2), json!(1)]]
    );
    Ok(())
}

#[tokio::test]
async fn features_are_selected_by_condition_shape_and_distance() -> Result<()> {
    let Some((cat, _db, sites_id, regions_id)) = sites().await? else {
        return Ok(());
    };
    let req = LayerRequest::new(sites_id, at());
    let select = |by: SelectBy| {
        let (cat, req) = (&cat, &req);
        async move { select_features(cat, req, &by).await }
    };
    let ids = |found: &sc_analytics::selection::Selected| found.ids.clone();

    let found = select(SelectBy::Condition {
        formula: "name != \"a\"".into(),
    })
    .await?
    .expect("selects");
    assert_eq!(ids(&found), vec![json!(2), json!(3)]);
    assert_eq!((found.count, found.truncated), (2, false));
    assert_eq!(found.condition, "name != \"a\"");

    // Within a distance of a point: a itself; then a and b, 1.3 km apart.
    let near = |distance: f64| SelectBy::NearPoint {
        longitude: 0.010,
        latitude: 51.500,
        distance,
    };
    assert_eq!(
        ids(&select(near(100.0)).await?.expect("selects")),
        vec![json!(1)]
    );
    let found = select(near(2000.0)).await?.expect("selects");
    assert_eq!(ids(&found), vec![json!(1), json!(2)]);
    assert!(
        found
            .condition
            .starts_with("Geo.distance(at, Geo.point(0.01, 51.5)) <= 2000"),
        "{}",
        found.condition
    );

    // A lasso around b.
    let lasso = json!({ "type": "Polygon", "coordinates": [[
        [0.015, 51.505], [0.025, 51.505], [0.025, 51.515], [0.015, 51.515], [0.015, 51.505] ]] });
    let found = select(SelectBy::Shape { geometry: lasso })
        .await?
        .expect("selects");
    assert_eq!(ids(&found), vec![json!(2)]);
    assert!(
        found
            .condition
            .starts_with("Geo.intersects(at, Geo.fromGeoJSON(")
    );

    // Within 1 m of the east region, selected in its own layer: c, inside it.
    let found = select(SelectBy::NearFeatures {
        layer: LayerRequest::new(regions_id, outline()),
        ids: vec![json!(2)],
        distance: 1.0,
    })
    .await?
    .expect("selects");
    assert_eq!(ids(&found), vec![json!(3)]);

    // Rows that are not a table's: their places in the order.
    let counted = per_region(&cat, sites_id).await?;
    let by_region = LayerRequest::new(
        counted.id,
        GeometrySource::Key {
            column: "region".into(),
            geometry: "outline".into(),
        },
    );
    let found = select_features(
        &cat,
        &by_region,
        &SelectBy::Condition {
            formula: "count > 1".into(),
        },
    )
    .await?
    .expect("selects");
    assert_eq!(found.ids, vec![json!(1)]);

    // Refusals.
    let e = select(SelectBy::Condition {
        formula: "height > 2".into(),
    })
    .await?
    .expect_err("no such column");
    assert!(
        e.starts_with("the selection's condition does not work"),
        "{e}"
    );
    let e = select(near(0.0)).await?.expect_err("no distance");
    assert!(e.contains("positive"), "{e}");
    let e = select(SelectBy::Shape {
        geometry: json!({ "type": "Point", "coordinates": [0, 0] }),
    })
    .await?
    .expect_err("not a polygon");
    assert!(e.contains("polygon"), "{e}");
    Ok(())
}

/// The rows of a stored dataset, each as JSON.
async fn rows_of(cat: &sc_catalog::Catalog, def: &DatasetDef) -> Result<Vec<Vec<Json>>> {
    let page = read_stage(cat, def, None, Page::first(100)).await?;
    Ok(page
        .rows
        .iter()
        .map(|r| r.iter().map(sc_dataset::value_json).collect())
        .collect())
}

#[tokio::test]
async fn a_selection_is_saved_as_a_dataset_over_the_layers() -> Result<()> {
    let Some((cat, _db, sites_id, _)) = sites().await? else {
        return Ok(());
    };
    // Clicked features of a table's rows: by their keys, after the layer's
    // own filter.
    let req = LayerRequest {
        filter: Some("lat > 51.505".into()),
        ..LayerRequest::new(sites_id, at())
    };
    let saved = save_selection(&cat, &req, "Picked", &[json!(1), json!(3)], None).await?;
    assert_eq!(saved.base, sc_dataset::Base::dataset(sites_id));
    let filters: Vec<String> = saved
        .operations
        .iter()
        .map(|o| match &o.op {
            Op::Filter(f) => f.formula.clone(),
            other => panic!("not a filter: {other:?}"),
        })
        .collect();
    assert_eq!(filters, vec!["lat > 51.505", "(id == 1) || (id == 3)"]);
    let stored = sc_dataset::require_dataset(&cat, saved.id).await?;
    let rows = rows_of(&cat, &stored).await?;
    assert_eq!(rows.len(), 1, "a is outside the layer's filter");
    assert_eq!(rows[0][1], json!("c"));

    // A selection made by a condition keeps the condition.
    let saved = save_selection(
        &cat,
        &LayerRequest::new(sites_id, at()),
        "Near a",
        &[],
        Some("Geo.distance(at, Geo.point(0.01, 51.5)) <= 2000"),
    )
    .await?;
    assert_eq!(rows_of(&cat, &saved).await?.len(), 2);

    // Clicked rows of an aggregated dataset: by the group keys.
    let counted = per_region(&cat, sites_id).await?;
    let by_region = LayerRequest::new(
        counted.id,
        GeometrySource::Key {
            column: "region".into(),
            geometry: "outline".into(),
        },
    );
    let saved = save_selection(&cat, &by_region, "East", &[json!(2)], None).await?;
    match &saved.operations.last().expect("a filter").op {
        Op::Filter(f) => assert_eq!(f.formula, "region == 2"),
        other => panic!("not a filter: {other:?}"),
    }
    assert_eq!(rows_of(&cat, &saved).await?, vec![vec![json!(2), json!(1)]]);

    // Rows that nothing tells apart cannot be saved by click; the name is
    // the store's to check.
    let mut stacked = DatasetDef::new("Stacked", sc_dataset::Base::dataset(sites_id));
    stacked.operations.push(Operation::new(
        "join",
        Op::SpatialJoin(sc_dataset::SpatialJoinOp {
            with: sc_dataset::Other::Table {
                table: "regions".into(),
            },
            kind: sc_dataset::JoinKind::Left,
            relation: sc_dataset::SpatialRelation::Intersects,
            left: "at".into(),
            right: "outline".into(),
            distance: None,
            distance_column: None,
            columns: Some(vec!["name".into()]),
            suffix: "_right".into(),
        }),
    ));
    save_dataset(&cat, &stacked).await?;
    let e = save_selection(
        &cat,
        &LayerRequest::new(stacked.id, at()),
        "Nope",
        &[json!(1)],
        None,
    )
    .await
    .expect_err("nothing tells them apart");
    assert!(
        e.to_string().contains("nothing that tells them apart"),
        "{e}"
    );
    let e = save_selection(&cat, &req, "Picked", &[json!(2)], None)
        .await
        .expect_err("the name is taken");
    assert!(e.to_string().contains("Picked"), "{e}");
    Ok(())
}

/// A layer as a tool's form gives it.
fn layer_arg(layer: &MapLayer) -> Json {
    serde_json::to_value(layer).unwrap()
}

#[tokio::test]
async fn the_toolbox_makes_datasets_and_layers() -> Result<()> {
    let Some((cat, _db, sites_id, regions_id)) = sites().await? else {
        return Ok(());
    };
    // A third region far from every site.
    insert(
        &cat,
        "regions",
        &["id", "name", "outline"],
        vec![vec![
            Value::Int(3),
            Value::Text("north".into()),
            Value::Json(json!({ "type": "Polygon", "coordinates": [[
                [0.0, 52.0], [0.01, 52.0], [0.01, 52.01], [0.0, 52.01], [0.0, 52.0] ]] })),
        ]],
    )
    .await?;
    let sites_layer = MapLayer::new(sites_id, at());
    let regions_layer = MapLayer::new(regions_id, outline());
    let args = |pairs: &[(&str, Json)]| {
        ToolArgs(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    };

    // Count per region: every region, the one with no site 0.
    let run = run_tool(
        &cat,
        "count_per_region",
        &args(&[
            ("layer", layer_arg(&sites_layer)),
            ("regions", layer_arg(&regions_layer)),
        ]),
        None,
    )
    .await?;
    assert_eq!(run.dataset.name, "Sites per Regions");
    let kinds: Vec<&str> = run.dataset.operations.iter().map(|o| o.op.kind()).collect();
    assert_eq!(kinds, ["spatial_join", "aggregate", "complete"]);
    let stored = sc_dataset::require_dataset(&cat, run.dataset.id).await?;
    assert_eq!(
        rows_of(&cat, &stored).await?,
        vec![
            vec![json!(1), json!(2)],
            vec![json!(2), json!(1)],
            vec![json!(3), json!(0)]
        ]
    );
    assert_eq!(
        run.layer.geometry,
        GeometrySource::Key {
            column: "region".into(),
            geometry: "outline".into()
        }
    );
    assert_eq!(run.layer.dataset, run.dataset.id);
    // Its layer draws, with its classes.
    let drawn = render_map(&cat, &MapSpec::of(vec![run.layer.clone()])).await?;
    match &drawn.layers[0].data {
        LayerData::Geojson { count, .. } => assert_eq!(*count, 3),
        other => panic!("not drawn: {other:?}"),
    }
    assert_eq!(drawn.layers[0].classes, Some(vec![0.0, 1.0, 2.0, 2.0]));
    // Run again: a name of its own.
    let again = run_tool(
        &cat,
        "count_per_region",
        &args(&[
            ("layer", layer_arg(&sites_layer)),
            ("regions", layer_arg(&regions_layer)),
        ]),
        None,
    )
    .await?;
    assert_eq!(again.dataset.name, "Sites per Regions 2");

    // Sum per region, of the longitude.
    let run = run_tool(
        &cat,
        "sum_per_region",
        &args(&[
            ("layer", layer_arg(&sites_layer)),
            ("regions", layer_arg(&regions_layer)),
            ("column", json!("lon")),
        ]),
        Some("Longitude per region"),
    )
    .await?;
    let rows = rows_of(&cat, &run.dataset).await?;
    assert_eq!(rows[1], vec![json!(2), json!(1), json!(0.03)]);
    assert_eq!(rows[2], vec![json!(3), json!(0), json!(0.0)]);

    // Buffer: a polygon per site.
    let run = run_tool(
        &cat,
        "buffer",
        &args(&[("layer", layer_arg(&sites_layer)), ("distance", json!(250))]),
        None,
    )
    .await?;
    assert_eq!(run.dataset.name, "Sites within 250 m");
    let drawn = render_map(&cat, &MapSpec::of(vec![run.layer.clone()])).await?;
    match &drawn.layers[0].data {
        LayerData::Geojson {
            geometry, count, ..
        } => {
            assert_eq!((geometry.as_slice(), *count), (&["polygon"][..], 3));
        }
        other => panic!("not drawn: {other:?}"),
    }

    // Distance to the nearest region: inside one, 0.
    let run = run_tool(
        &cat,
        "distance_to_nearest",
        &args(&[
            ("layer", layer_arg(&sites_layer)),
            ("to", layer_arg(&regions_layer)),
        ]),
        None,
    )
    .await?;
    let rows = rows_of(&cat, &run.dataset).await?;
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().all(|r| r.last() == Some(&json!(0.0))),
        "{rows:?}"
    );

    // Within 1 km of a region: every site, each inside one; within 1 km of
    // the sites filtered to b, and the regions near a site.
    let run = run_tool(
        &cat,
        "within_distance",
        &args(&[
            ("layer", layer_arg(&sites_layer)),
            ("of", layer_arg(&regions_layer)),
            ("distance", json!(1000)),
        ]),
        None,
    )
    .await?;
    assert_eq!(rows_of(&cat, &run.dataset).await?.len(), 3);
    let only_north = run_tool(
        &cat,
        "within_distance",
        &args(&[
            ("layer", layer_arg(&regions_layer)),
            ("of", layer_arg(&sites_layer)),
            ("distance", json!(1000)),
        ]),
        None,
    )
    .await?;
    // The regions within 1 km of a site: west and east, not the far north.
    assert_eq!(rows_of(&cat, &only_north.dataset).await?.len(), 2);

    // Spatial join: each site with its region's columns.
    let run = run_tool(
        &cat,
        "spatial_join",
        &args(&[
            ("layer", layer_arg(&sites_layer)),
            ("with", layer_arg(&regions_layer)),
            ("relation", json!("within")),
        ]),
        None,
    )
    .await?;
    let page = read_stage(&cat, &run.dataset, None, Page::first(10)).await?;
    assert!(page.columns.iter().any(|c| c.name == "name_right"));
    assert_eq!(page.total, 3);

    // Intersection: the sites' points where they meet the regions.
    let run = run_tool(
        &cat,
        "intersection",
        &args(&[
            ("layer", layer_arg(&sites_layer)),
            ("with", layer_arg(&regions_layer)),
        ]),
        None,
    )
    .await?;
    assert_eq!(
        run.layer.geometry,
        GeometrySource::Column {
            column: "intersection".into()
        }
    );
    assert_eq!(rows_of(&cat, &run.dataset).await?.len(), 3);

    // Dissolve: the sites merged by region.
    let run = run_tool(
        &cat,
        "dissolve",
        &args(&[("layer", layer_arg(&sites_layer)), ("by", json!("region"))]),
        None,
    )
    .await?;
    let page = read_stage(&cat, &run.dataset, None, Page::first(10)).await?;
    assert_eq!(page.total, 2);
    let drawn = render_map(&cat, &MapSpec::of(vec![run.layer.clone()])).await?;
    assert!(matches!(drawn.layers[0].data, LayerData::Geojson { .. }));

    // Refusals: a layer whose geometry is two columns cannot be joined to,
    // and nothing is stored.
    let before = sc_dataset::list_datasets(&cat).await?.len();
    let lonlat = MapLayer::new(
        regions_id,
        GeometrySource::LonLat {
            longitude: "lon".into(),
            latitude: "lat".into(),
        },
    );
    let e = run_tool(
        &cat,
        "count_per_region",
        &args(&[
            ("layer", layer_arg(&sites_layer)),
            ("regions", layer_arg(&lonlat)),
        ]),
        None,
    )
    .await
    .expect_err("not joinable");
    assert!(e.to_string().contains("Count per region: the layer"), "{e}");
    assert!(e.to_string().contains("two columns"), "{e}");
    let e = run_tool(
        &cat,
        "buffer",
        &args(&[("layer", layer_arg(&sites_layer))]),
        None,
    )
    .await
    .expect_err("no distance");
    assert!(e.to_string().contains("distance"), "{e}");
    let e = run_tool(&cat, "teleport", &args(&[]), None)
        .await
        .expect_err("no such tool");
    assert!(e.to_string().contains("teleport"), "{e}");
    assert_eq!(sc_dataset::list_datasets(&cat).await?.len(), before);
    Ok(())
}

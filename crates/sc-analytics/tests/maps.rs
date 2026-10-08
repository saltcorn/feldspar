//! Map panels (analytics TODO A5.6–A5.7): the geometry source the explorer
//! chooses, the map spec it makes of the drop zones, and what a map is drawn
//! from — each layer's features with the kinds of geometry among them and the
//! domains its colour, size and shape scales are drawn over.
//!
//! Over the sites and regions of `layers.rs`, on PostGIS; each test returns
//! early where there is none.

use sc_analytics::layer::{GeometrySource, LayerData, Spread, layer_domains};
use sc_analytics::map::{MapEncoding, MapLayer, MapSpec, render_map, suggest_map};
use sc_analytics::plot::{Assignment, FieldDef};
use sc_dataset::{DatasetDef, save_dataset};
use sc_error::Result;
use sc_types::BasicType;
use serde_json::json;

use crate::layers::{column, request, sites};

fn on(color: Option<&str>, size: Option<&str>, shape: Option<&str>) -> Assignment {
    Assignment {
        color: color.map(FieldDef::of),
        size: size.map(FieldDef::of),
        shape: shape.map(FieldDef::of),
        ..Assignment::default()
    }
}

#[tokio::test]
async fn the_explorer_offers_every_source_and_draws_the_first() -> Result<()> {
    let Some((cat, _db, sites_id, _)) = sites().await? else {
        return Ok(());
    };
    let answer = suggest_map(&cat, sites_id, &Assignment::default(), None).await?;
    assert_eq!(answer.error, None);
    let labels: Vec<&str> = answer.sources.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        ["`at`", "`lon` and `lat`", "`region` → `regions.outline`"]
    );
    let spec = answer.spec.expect("a map");
    assert_eq!(spec.layers.len(), 1);
    assert_eq!(
        spec.layers[0].geometry,
        GeometrySource::Column {
            column: "at".into()
        }
    );

    // Another source when asked for one of the dataset's; the drop zones
    // become the encoding, without their bins, and X and Y are not a map's.
    let by_region = GeometrySource::Key {
        column: "region".into(),
        geometry: "outline".into(),
    };
    let mut assignment = on(Some("name"), Some("lat"), None);
    assignment.color = Some(FieldDef::binned("name"));
    assignment.x = Some(FieldDef::of("lon"));
    let answer = suggest_map(&cat, sites_id, &assignment, Some(&by_region)).await?;
    let layer = &answer.spec.expect("a map").layers[0];
    assert_eq!(layer.geometry, by_region);
    assert_eq!(
        serde_json::to_value(&layer.encoding).unwrap(),
        json!({ "color": { "field": "name" }, "size": { "field": "lat" } })
    );

    // A source the dataset does not have is not drawn: the first is.
    let gone = GeometrySource::Column {
        column: "gone".into(),
    };
    let answer = suggest_map(&cat, sites_id, &Assignment::default(), Some(&gone)).await?;
    assert_eq!(
        answer.spec.expect("a map").layers[0].geometry,
        answer.sources[0].source
    );

    // A channel that cannot be drawn is a sentence, with the sources still
    // offered.
    let answer = suggest_map(&cat, sites_id, &on(None, Some("name"), None), None).await?;
    assert_eq!(
        answer.error.as_deref(),
        Some("Size on a map needs a number, and `name` is text; put it on Color or Shape")
    );
    assert_eq!(answer.sources.len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_dataset_with_nothing_to_map_says_so() -> Result<()> {
    let Some((cat, _db, _, _)) = sites().await? else {
        return Ok(());
    };
    cat.create_table(
        "notes",
        &[
            column("id", BasicType::Int).required().primary_key(),
            column("text", BasicType::Text),
            column("longitude", BasicType::Float),
        ],
    )
    .await?;
    let notes = DatasetDef::over_table("Notes", "notes");
    save_dataset(&cat, &notes).await?;
    let answer = suggest_map(&cat, notes.id, &Assignment::default(), None).await?;
    assert!(answer.spec.is_none());
    assert!(answer.sources.is_empty());
    assert_eq!(
        answer.error.as_deref(),
        Some(
            "`Notes` has nothing to put on a map: no geometry column, no longitude and \
             latitude columns, and no key to a table with a geometry column"
        )
    );
    Ok(())
}

#[tokio::test]
async fn a_map_is_drawn_with_its_domains_and_geometry_kinds() -> Result<()> {
    let Some((cat, _db, sites_id, regions_id)) = sites().await? else {
        return Ok(());
    };
    let spec = MapSpec::of(vec![
            // The regions, coloured by name.
            MapLayer {
                encoding: MapEncoding {
                    color: Some(FieldDef::of("name")),
                    ..MapEncoding::default()
                },
                ..MapLayer::new(
                    regions_id,
                    GeometrySource::Column {
                        column: "outline".into(),
                    },
                )
            },
            // The sites over them, sized by latitude, a shape per region, and
            // a filter leaving two.
            MapLayer {
                encoding: MapEncoding {
                    color: Some(FieldDef::of("lon")),
                    size: Some(FieldDef::of("lat")),
                    shape: Some(FieldDef::of("region")),
                    label: Some(FieldDef::of("name")),
                },
                filter: Some("name !== \"c\"".into()),
                ..MapLayer::new(
                    sites_id,
                    GeometrySource::LonLat {
                        longitude: "lon".into(),
                        latitude: "lat".into(),
                    },
                )
            },
        ]);
    let drawn = render_map(&cat, &spec).await?;
    assert_eq!(drawn.layers.len(), 2);

    let regions = &drawn.layers[0];
    match &regions.data {
        LayerData::Geojson {
            count, geometry, ..
        } => {
            assert_eq!(*count, 2);
            assert_eq!(geometry, &["polygon"]);
        }
        other => panic!("not GeoJSON: {other:?}"),
    }
    let color = serde_json::to_value(&regions.domains["color"]).unwrap();
    assert_eq!(
        color,
        json!({ "kind": "discrete", "min": "east", "max": "west", "values": ["east", "west"] })
    );

    let sites = &drawn.layers[1];
    match &sites.data {
        LayerData::Geojson {
            count, geometry, ..
        } => {
            assert_eq!(*count, 2, "the filter leaves two");
            assert_eq!(geometry, &["point"]);
        }
        other => panic!("not GeoJSON: {other:?}"),
    }
    // A number on Color and on Size is a range; the key on Shape its values;
    // a label has no domain. Every domain is over the filtered features.
    assert_eq!(sites.domains["color"].kind, "continuous");
    assert_eq!(sites.domains["color"].min, Some(json!(0.01)));
    assert_eq!(sites.domains["color"].max, Some(json!(0.02)));
    assert_eq!(sites.domains["size"].min, Some(json!(51.5)));
    assert_eq!(sites.domains["size"].max, Some(json!(51.51)));
    assert_eq!(sites.domains["shape"].values, Some(vec![json!(1)]));
    assert!(!sites.domains.contains_key("label"));
    assert_eq!(sites.layer.filter.as_deref(), Some("name !== \"c\""));

    // A layer that cannot be drawn says why, and the others are still drawn.
    let mut broken = spec.clone();
    broken.layers[1].encoding.size = Some(FieldDef::of("name"));
    let drawn = render_map(&cat, &broken).await?;
    assert!(matches!(drawn.layers[0].data, LayerData::Geojson { .. }));
    match &drawn.layers[1].data {
        LayerData::Refused { error } => assert!(error.starts_with("Size on a map needs a number")),
        other => panic!("not refused: {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn domains_are_read_over_the_features_with_a_geometry() -> Result<()> {
    let Some((cat, _db, sites_id, _)) = sites().await? else {
        return Ok(());
    };
    // Through the key, every site has a region's outline; the values come in
    // order, once each.
    let req = request(
        sites_id,
        GeometrySource::Key {
            column: "region".into(),
            geometry: "outline".into(),
        },
    );
    let domains = layer_domains(
        &cat,
        &req,
        &[
            ("region".into(), Spread::Values),
            ("lat".into(), Spread::Range),
        ],
    )
    .await?
    .expect("reads");
    assert_eq!(domains["region"].values, Some(vec![json!(1), json!(2)]));
    assert_eq!(domains["lat"].min, Some(json!(51.5)));
    assert_eq!(domains["lat"].max, Some(json!(51.52)));
    // A layer that cannot be drawn has the layer's sentence.
    let bad = request(
        sites_id,
        GeometrySource::Column {
            column: "name".into(),
        },
    );
    let refused = layer_domains(&cat, &bad, &[("lat".into(), Spread::Range)]).await?;
    assert!(refused.unwrap_err().contains("not a geometry"));
    Ok(())
}

//! Layer data for the browser (analytics TODO A5.5): GeoJSON for small layers
//! and Mapbox vector tiles for large ones, each geometry source, and the
//! refusals.
//!
//! The tiles are read back by a small protobuf reader below — the tile's
//! layers, their names, keys and features, each feature's id and the length
//! of its geometry commands — so the test checks what MapLibre will be given
//! rather than that bytes came back.

use std::sync::Arc;

use sc_analytics::layer::{
    GeometrySource, LayerData, LayerRequest, Limits, SOURCE_LAYER, layer_data, layer_tile,
};
use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId, TableId};
use sc_dataset::{DatasetDef, DatasetId, save_dataset};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, GeometryKind, TypeRef};
use serde_json::{Value as Json, json};

/// Three sites: id, name, longitude, latitude, region.
const SITES: &[(i64, &str, f64, f64, i64)] = &[
    (1, "a", 0.010, 51.500, 1),
    (2, "b", 0.020, 51.510, 1),
    (3, "c", 0.030, 51.520, 2),
];

pub(crate) fn column(name: &str, ty: BasicType) -> DataField {
    DataField::plain(name, TypeRef::Basic(ty))
}

/// A polygon of `n` vertices around a centre — what simplification thins.
fn circle(lon: f64, lat: f64, radius: f64, n: usize) -> Json {
    let mut ring: Vec<Json> = (0..n)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            json!([lon + radius * a.cos(), lat + radius * a.sin()])
        })
        .collect();
    ring.push(ring[0].clone());
    json!({ "type": "Polygon", "coordinates": [ring] })
}

pub(crate) async fn insert(cat: &Catalog, table: &str, columns: &[&str], rows: Vec<Vec<Value>>) -> Result<()> {
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

/// Regions and sites in a database with PostGIS, and a dataset over each;
/// `None` where there is no PostGIS.
pub(crate) async fn sites() -> Result<Option<(Catalog, TestDb, DatasetId, DatasetId)>> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(None);
    };
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    sc_catalog::bootstrap_spatial(&cat).await?;
    sc_dataset::bootstrap_datasets(&cat).await?;
    let id = || column("id", BasicType::Int).required().primary_key();
    cat.create_table(
        "regions",
        &[
            id(),
            column("name", BasicType::Text),
            column("outline", BasicType::Geometry(GeometryKind::Polygon)),
        ],
    )
    .await?;
    let mut region = column("region", BasicType::Int);
    region.kind = DataFieldKind::Key {
        target_table: TableId("regions".into()),
        target_field: FieldId("id".into()),
        summary_field: None,
    };
    cat.create_table(
        "sites",
        &[
            id(),
            column("name", BasicType::Text),
            column("lon", BasicType::Float),
            column("lat", BasicType::Float),
            region,
            column("at", BasicType::Geometry(GeometryKind::Point)),
            column("notes", BasicType::Json),
        ],
    )
    .await?;
    insert(
        &cat,
        "regions",
        &["id", "name", "outline"],
        vec![
            vec![
                Value::Int(1),
                Value::Text("west".into()),
                Value::Json(circle(0.015, 51.505, 0.01, 400)),
            ],
            vec![
                Value::Int(2),
                Value::Text("east".into()),
                Value::Json(circle(0.035, 51.525, 0.01, 400)),
            ],
        ],
    )
    .await?;
    insert(
        &cat,
        "sites",
        &["id", "name", "lon", "lat", "region", "at", "notes"],
        SITES
            .iter()
            .map(|(id, name, lon, lat, region)| {
                vec![
                    Value::Int(*id),
                    Value::Text((*name).into()),
                    Value::Float(*lon),
                    Value::Float(*lat),
                    Value::Int(*region),
                    Value::Json(json!({"type": "Point", "coordinates": [lon, lat]})),
                    Value::Json(json!({"seen": true})),
                ]
            })
            .collect(),
    )
    .await?;
    let sites = DatasetDef::over_table("Sites", "sites");
    save_dataset(&cat, &sites).await?;
    let regions = DatasetDef::over_table("Regions", "regions");
    save_dataset(&cat, &regions).await?;
    Ok(Some((cat, db, sites.id, regions.id)))
}

pub(crate) fn request(dataset: DatasetId, geometry: GeometrySource) -> LayerRequest {
    LayerRequest::new(dataset, geometry)
}

fn at() -> GeometrySource {
    GeometrySource::Column {
        column: "at".into(),
    }
}

/// The GeoJSON a small layer answers, or a panic saying what came instead.
async fn geojson(cat: &Catalog, req: &LayerRequest) -> (u64, Option<[f64; 4]>, Vec<String>, Json) {
    match layer_data(cat, req, Limits::default())
        .await
        .expect("reads")
    {
        LayerData::Geojson {
            count,
            bounds,
            properties,
            data,
            ..
        } => (
            count,
            bounds,
            properties.into_iter().map(|p| p.name).collect(),
            data,
        ),
        other => panic!("not GeoJSON: {other:?}"),
    }
}

fn refusal(data: LayerData) -> String {
    match data {
        LayerData::Refused { error } => error,
        other => panic!("not refused: {other:?}"),
    }
}

#[tokio::test]
async fn a_small_layer_is_geojson_keyed_by_its_rows() -> Result<()> {
    let Some((cat, _db, sites_id, _)) = sites().await? else {
        return Ok(());
    };

    // A geometry column: each site a feature, its id the row's key, carrying
    // every column that is not geometry or JSON.
    let (count, bounds, properties, data) = geojson(&cat, &request(sites_id, at())).await;
    assert_eq!(count, 3);
    assert_eq!(properties, ["id", "name", "lon", "lat", "region"]);
    let [w, s, e, n] = bounds.expect("bounds");
    assert!(
        (w - 0.01).abs() < 1e-9 && (e - 0.03).abs() < 1e-9,
        "{bounds:?}"
    );
    assert!(
        (s - 51.5).abs() < 1e-9 && (n - 51.52).abs() < 1e-9,
        "{bounds:?}"
    );
    assert_eq!(data["type"], "FeatureCollection");
    let features = data["features"].as_array().expect("features");
    assert_eq!(features.len(), 3);
    assert_eq!(features[1]["id"], json!(2));
    assert_eq!(
        features[1]["geometry"],
        json!({"type": "Point", "coordinates": [0.02, 51.51]})
    );
    assert_eq!(features[1]["properties"]["name"], "b");
    assert!(features[1]["properties"].get("at").is_none());

    // Longitude and latitude columns make the same points.
    let (_, _, _, from_columns) = geojson(
        &cat,
        &request(
            sites_id,
            GeometrySource::LonLat {
                longitude: "lon".into(),
                latitude: "lat".into(),
            },
        ),
    )
    .await;
    let points = |d: &Json| -> Vec<Json> {
        d["features"]
            .as_array()
            .expect("features")
            .iter()
            .map(|f| f["geometry"].clone())
            .collect()
    };
    assert_eq!(points(&from_columns), points(&data));

    // Along the foreign key, each site drawn as its region's outline; a
    // filter keeps some rows, and the properties are the ones asked for.
    let req = LayerRequest {
        properties: Some(vec!["name".into()]),
        filter: Some("region === 1".into()),
        ..request(
            sites_id,
            GeometrySource::Key {
                column: "region".into(),
                geometry: "outline".into(),
            },
        )
    };
    let (count, _, properties, data) = geojson(&cat, &req).await;
    assert_eq!(count, 2);
    assert_eq!(properties, ["name"]);
    let features = data["features"].as_array().expect("features");
    assert_eq!(features[0]["geometry"]["type"], "Polygon");
    assert_eq!(
        features[0]["properties"],
        json!({ "name": "a" }),
        "only what was asked for"
    );
    Ok(())
}

#[tokio::test]
async fn a_large_layer_is_tiles_simplified_by_zoom() -> Result<()> {
    let Some((cat, _db, sites_id, regions_id)) = sites().await? else {
        return Ok(());
    };
    // Over the limit, the answer is to fetch tiles.
    let small = Limits {
        features: 2,
        vertices: 1_000_000,
    };
    match layer_data(&cat, &request(sites_id, at()), small).await? {
        LayerData::Tiles {
            count,
            source_layer,
            keyed,
            properties,
            geometry,
            ..
        } => {
            assert_eq!(count, 3);
            // So a map draws them as points and adds no fill or line layer.
            assert_eq!(geometry, ["point"]);
            assert_eq!(source_layer, SOURCE_LAYER);
            assert!(keyed);
            assert_eq!(properties.len(), 5);
        }
        other => panic!("not tiles: {other:?}"),
    }
    // Two regions of 400 vertices each are over the default vertex limit's
    // share when the limit is low.
    let few_vertices = Limits {
        features: 100,
        vertices: 500,
    };
    let regions = request(
        regions_id,
        GeometrySource::Column {
            column: "outline".into(),
        },
    );
    assert!(matches!(
        layer_data(&cat, &regions, few_vertices).await?,
        LayerData::Tiles { vertices: 802, .. }
    ));

    // The whole world at zoom 0: one layer of three point features, each with
    // its row's key and the properties as keys.
    let tile = Tile::read(&layer_tile(&cat, &request(sites_id, at()), 0, 0, 0).await?);
    assert_eq!(tile.layers.len(), 1);
    let layer = &tile.layers[0];
    assert_eq!(layer.name, SOURCE_LAYER);
    assert_eq!(layer.extent, 4096);
    for key in ["id", "name", "lon", "lat", "region"] {
        assert!(
            layer.keys.iter().any(|k| k == key),
            "{key} in {:?}",
            layer.keys
        );
    }
    let mut ids: Vec<u64> = layer.features.iter().filter_map(|f| f.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2, 3]);
    assert!(layer.features.iter().all(|f| f.kind == 1), "points");

    // The tile at zoom 14 that holds the sites (longitude 0.01–0.03 at
    // 51.5° N is column 8192, row 5450), and one far from them.
    let (x, y) = tile_of(0.02, 51.51, 14);
    let near = Tile::read(&layer_tile(&cat, &request(sites_id, at()), 14, x, y).await?);
    assert!(!near.layers.is_empty() && !near.layers[0].features.is_empty());
    let far = Tile::read(&layer_tile(&cat, &request(sites_id, at()), 14, 0, 0).await?);
    assert!(far.layers.iter().all(|l| l.features.is_empty()));

    // A 400-vertex outline keeps its vertices close in and loses most of them
    // zoomed out.
    let commands = |tile: &Tile| -> usize {
        tile.layers
            .iter()
            .flat_map(|l| &l.features)
            .map(|f| f.geometry_len)
            .max()
            .unwrap_or(0)
    };
    let (x, y) = tile_of(0.015, 51.505, 12);
    let close = Tile::read(&layer_tile(&cat, &regions, 12, x, y).await?);
    let (x, y) = tile_of(0.015, 51.505, 5);
    let wide = Tile::read(&layer_tile(&cat, &regions, 5, x, y).await?);
    assert!(
        commands(&close) > 4 * commands(&wide),
        "zoom 12: {} commands, zoom 5: {}",
        commands(&close),
        commands(&wide)
    );
    Ok(())
}

#[tokio::test]
async fn a_layer_says_why_it_cannot_be_drawn() -> Result<()> {
    if let Some((cat, _db, sites_id, _)) = sites().await? {
        let data = |req: LayerRequest| {
            let cat = &cat;
            async move { layer_data(cat, &req, Limits::default()).await }
        };
        let e = refusal(
            data(request(
                sites_id,
                GeometrySource::Column {
                    column: "name".into(),
                },
            ))
            .await?,
        );
        assert!(e.contains("`name` is text, not a geometry"), "{e}");
        let e = refusal(
            data(request(
                sites_id,
                GeometrySource::Column {
                    column: "where".into(),
                },
            ))
            .await?,
        );
        assert!(e.contains("`where` is not a column of `Sites`"), "{e}");
        let e = refusal(
            data(LayerRequest {
                properties: Some(vec!["colour".into()]),
                ..request(sites_id, at())
            })
            .await?,
        );
        assert!(e.contains("`colour` is not a column"), "{e}");
        let e = refusal(
            data(LayerRequest {
                filter: Some("height > 3".into()),
                ..request(sites_id, at())
            })
            .await?,
        );
        assert!(e.starts_with("the layer's filter does not work"), "{e}");
        let e = refusal(
            data(request(
                sites_id,
                GeometrySource::Key {
                    column: "name".into(),
                    geometry: "outline".into(),
                },
            ))
            .await?,
        );
        assert!(e.starts_with("the layer's geometry does not work"), "{e}");
        let e = refusal(data(request(DatasetId::new(), at())).await?);
        assert!(e.contains("is gone"), "{e}");
        // A tile outside the grid.
        let e = layer_tile(&cat, &request(sites_id, at()), 2, 4, 0)
            .await
            .expect_err("no such tile");
        assert!(e.to_string().contains("no tile 4/0 at zoom 2"), "{e}");
        let e = layer_tile(&cat, &request(sites_id, at()), 30, 0, 0)
            .await
            .expect_err("too deep");
        assert!(e.to_string().contains("deeper than tiles go"), "{e}");
    }

    // Without PostGIS, the sentence says so.
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    if !cat.primary().spatial().is_available() {
        sc_dataset::bootstrap_datasets(&cat).await?;
        let e =
            refusal(layer_data(&cat, &request(DatasetId::new(), at()), Limits::default()).await?);
        assert!(e.contains("PostGIS"), "{e}");
    }
    Ok(())
}

/// The Web Mercator tile holding a point at zoom `z`.
fn tile_of(lon: f64, lat: f64, z: u32) -> (u32, u32) {
    let n = f64::from(1_u32 << z);
    let x = ((lon + 180.0) / 360.0 * n).floor();
    let lat = lat.to_radians();
    let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n).floor();
    (x as u32, y as u32)
}

// --- reading a vector tile -------------------------------------------------------

/// What the test reads of a Mapbox vector tile (spec 2.1): its layers.
#[derive(Debug, Default)]
struct Tile {
    layers: Vec<TileLayer>,
}

#[derive(Debug, Default)]
struct TileLayer {
    name: String,
    keys: Vec<String>,
    extent: u64,
    features: Vec<TileFeature>,
}

#[derive(Debug, Default)]
struct TileFeature {
    id: Option<u64>,
    /// 1 point, 2 line, 3 polygon.
    kind: u64,
    /// How many integers its geometry commands are.
    geometry_len: usize,
}

/// A protobuf message's fields: number, and either a varint or bytes.
fn fields(mut bytes: &[u8]) -> Vec<(u64, Result<u64, Vec<u8>>)> {
    fn varint(bytes: &mut &[u8]) -> u64 {
        let mut out = 0_u64;
        let mut shift = 0;
        while let Some((&b, rest)) = bytes.split_first() {
            *bytes = rest;
            out |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        out
    }
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let key = varint(&mut bytes);
        let (field, wire) = (key >> 3, key & 7);
        match wire {
            0 => out.push((field, Ok(varint(&mut bytes)))),
            2 => {
                let len = varint(&mut bytes) as usize;
                let (value, rest) = bytes.split_at(len.min(bytes.len()));
                out.push((field, Err(value.to_vec())));
                bytes = rest;
            }
            5 => bytes = &bytes[4.min(bytes.len())..],
            1 => bytes = &bytes[8.min(bytes.len())..],
            other => panic!("wire type {other} in a vector tile"),
        }
    }
    out
}

/// How many varints a packed field holds.
fn packed_len(bytes: &[u8]) -> usize {
    bytes.iter().filter(|b| *b & 0x80 == 0).count()
}

impl Tile {
    fn read(bytes: &[u8]) -> Tile {
        let mut tile = Tile::default();
        for (field, value) in fields(bytes) {
            if let (3, Err(layer)) = (field, value) {
                tile.layers.push(TileLayer::read(&layer));
            }
        }
        tile
    }
}

impl TileLayer {
    fn read(bytes: &[u8]) -> TileLayer {
        let mut layer = TileLayer::default();
        for (field, value) in fields(bytes) {
            match (field, value) {
                (1, Err(name)) => layer.name = String::from_utf8_lossy(&name).into_owned(),
                (2, Err(feature)) => {
                    let mut f = TileFeature::default();
                    for (field, value) in fields(&feature) {
                        match (field, value) {
                            (1, Ok(id)) => f.id = Some(id),
                            (3, Ok(kind)) => f.kind = kind,
                            (4, Err(geometry)) => f.geometry_len = packed_len(&geometry),
                            _ => {}
                        }
                    }
                    layer.features.push(f);
                }
                (3, Err(key)) => layer.keys.push(String::from_utf8_lossy(&key).into_owned()),
                (5, Ok(extent)) => layer.extent = extent,
                _ => {}
            }
        }
        layer
    }
}

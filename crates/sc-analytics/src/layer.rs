//! **Layer data for the browser** (analytics TODO A5.5): a dataset's rows as
//! map features, sent as GeoJSON while a layer is small and as Mapbox vector
//! tiles once it is not.
//!
//! A layer is a dataset, a **geometry source** and the columns each feature
//! carries (goals document, "Map workspace": "a layer is a dataset, a geometry
//! source and a style. The map never computes anything itself"). The geometry
//! comes from a geometry column, from longitude and latitude columns, or along
//! a foreign key to a table that has a geometry column — each of them a
//! formula (`location`, `Geo.point(lon, lat)`, `districtⱵoutline`) added to
//! the dataset as one more Calculated column, so the dataset compiler does the
//! work and the rules of formulas apply.
//!
//! **Small or large.** [`layer_data`] counts the features and their vertices
//! in one aggregate query, with the extent. Under both of [`Limits`] the
//! features come back as a GeoJSON FeatureCollection, whole; over either, the
//! answer says to fetch tiles, and [`layer_tile`] makes each one with
//! `ST_AsMVT`. A tile's geometries are put in Web Mercator, simplified to the
//! tile's resolution (one unit of its 4096 grid, so a polygon's vertices are
//! thinned as the zoom goes out), clipped to the tile with a margin, and
//! quantised by `ST_AsMVTGeom`.
//!
//! **Feature ids.** While a dataset's rows are rows of a table keyed by an
//! integer, each feature's id is the row's key, in GeoJSON and in tiles alike —
//! what A5.10 links the attribute table and the map by. Otherwise a GeoJSON
//! feature is numbered by its place in the dataset's order and a tile's
//! features have no id.
//!
//! Needs PostGIS, as every spatial feature does; elsewhere it answers the
//! sentence saying why.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};

use sc_catalog::Catalog;
use sc_dataset::{
    ColType, DatasetDef, DatasetId, Op, OpStatus, Operation, Options, ROW_KEY, Schema, Stage,
    StageColumn, compile, read_rows, value_json,
};
use sc_error::{Error, Result};
use sc_query::{Expr, Projection, Select, Source, Statement, UnOp, Value};

/// The most features a layer sends as GeoJSON.
pub const GEOJSON_FEATURES: u64 = 5_000;
/// The most vertices, over all its features, a layer sends as GeoJSON.
pub const GEOJSON_VERTICES: u64 = 250_000;
/// The size of a vector tile's grid.
pub const TILE_EXTENT: i64 = 4096;
/// How far past its edge, in grid units, a tile keeps geometry — so a line or
/// a symbol on the edge is drawn whole on both sides.
pub const TILE_BUFFER: i64 = 64;
/// The deepest zoom a tile is made for.
pub const MAX_ZOOM: u32 = 24;
/// The name of the one layer inside each tile (MapLibre's `source-layer`).
pub const SOURCE_LAYER: &str = "features";
/// Half the width of the Web Mercator world, in metres.
const MERCATOR_HALF: f64 = 20_037_508.342_789_244;

/// Where a layer's geometry comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GeometrySource {
    /// A geometry column of the dataset.
    Column {
        /// The column.
        column: String,
    },
    /// A point made from two number columns, in degrees.
    LonLat {
        /// The longitude column.
        longitude: String,
        /// The latitude column.
        latitude: String,
    },
    /// The geometry of the row a foreign key refers to: `districtⱵoutline`.
    Key {
        /// The foreign-key column of the dataset.
        column: String,
        /// The geometry column of the table it refers to.
        geometry: String,
    },
}

impl GeometrySource {
    /// The formula computing the geometry.
    pub fn formula(&self) -> String {
        match self {
            GeometrySource::Column { column } => column.clone(),
            GeometrySource::LonLat {
                longitude,
                latitude,
            } => format!("Geo.point({longitude}, {latitude})"),
            GeometrySource::Key { column, geometry } => format!("{column}Ⱶ{geometry}"),
        }
    }
}

/// What a layer draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerRequest {
    /// The dataset, by id: tiles are fetched by URL, so a layer reads a stored
    /// dataset.
    pub dataset: DatasetId,
    /// Where the geometry comes from.
    pub geometry: GeometrySource,
    /// The columns each feature carries; every column that is not geometry,
    /// JSON or bytes when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<Vec<String>>,
    /// A condition over the dataset's rows, for a layer showing some of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

/// Where GeoJSON stops and tiles begin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most features sent as GeoJSON.
    pub features: u64,
    /// The most vertices sent as GeoJSON.
    pub vertices: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            features: GEOJSON_FEATURES,
            vertices: GEOJSON_VERTICES,
        }
    }
}

/// A layer's data, or how to fetch it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "delivery", rename_all = "snake_case")]
pub enum LayerData {
    /// Small enough to send whole.
    Geojson {
        /// The features with a geometry.
        count: u64,
        /// `[west, south, east, north]` in degrees; none for no features.
        bounds: Option<[f64; 4]>,
        /// The columns each feature carries, with their types.
        properties: Vec<StageColumn>,
        /// A GeoJSON FeatureCollection.
        data: Json,
    },
    /// Too large to send whole: fetch it tile by tile ([`layer_tile`]).
    Tiles {
        /// The features with a geometry.
        count: u64,
        /// Their vertices.
        vertices: u64,
        /// `[west, south, east, north]` in degrees.
        bounds: Option<[f64; 4]>,
        /// The columns each feature carries, with their types.
        properties: Vec<StageColumn>,
        /// The layer inside each tile.
        source_layer: String,
        /// Whether a feature's id is its row's key.
        keyed: bool,
    },
    /// Nothing to draw, and why.
    #[serde(rename = "none")]
    Refused {
        /// The sentence.
        error: String,
    },
}

/// A layer's stage, ready to read.
struct Prepared {
    stage: Stage,
    /// The geometry column of the stage.
    geometry: String,
    /// The columns each feature carries.
    properties: Vec<StageColumn>,
    /// Whether a feature's id is its row's key (an integer).
    keyed: bool,
}

/// The id of the operations a layer adds to its dataset.
const FILTER_OP: &str = "layer-filter";
const GEOMETRY_OP: &str = "layer-geometry";

/// The layer's stage, or the sentence saying why it cannot be drawn.
async fn prepare(
    catalog: &Catalog,
    req: &LayerRequest,
) -> Result<std::result::Result<Prepared, String>> {
    let schema = Schema::of_catalog(catalog)?;
    if let Err(reason) = &schema.spatial {
        return Ok(Err(format!(
            "a map layer is drawn from geometry the database computes, and {reason}"
        )));
    }
    let Some(def) = sc_dataset::load_dataset(catalog, req.dataset).await? else {
        return Ok(Err(
            "the dataset this layer reads is gone; pick another".to_owned()
        ));
    };
    let mut library = sc_dataset::load_library(catalog).await?;
    let plain = compile(&schema, &library, &def, Options::default());
    let names: Vec<String> = match plain.last() {
        Ok(stage) => stage.shape().columns.into_iter().map(|c| c.name).collect(),
        Err(e) => {
            return Ok(Err(format!(
                "the dataset `{}` does not read: {e}",
                def.name
            )));
        }
    };
    // A geometry column is drawn as it is; any other source is one more
    // column, under a name the dataset does not use.
    let mut layered: DatasetDef = def.clone();
    if let Some(filter) = req.filter.as_deref().filter(|f| !f.trim().is_empty()) {
        layered
            .operations
            .push(Operation::new(FILTER_OP, Op::filter(filter)));
    }
    let geometry = match &req.geometry {
        GeometrySource::Column { column } => column.clone(),
        source => {
            let name = unique_name("geometry", &names);
            layered.operations.push(Operation::new(
                GEOMETRY_OP,
                Op::calculated(name.clone(), source.formula()),
            ));
            name
        }
    };
    library.insert(layered.clone());
    let compiled = compile(&schema, &library, &layered, Options::default());
    for (op, report) in layered.operations.iter().zip(&compiled.operations) {
        if report.status != OpStatus::Invalid {
            continue;
        }
        let error = report.error.clone().unwrap_or_default();
        return Ok(Err(match op.id.as_str() {
            FILTER_OP => format!("the layer's filter does not work: {error}"),
            GEOMETRY_OP => format!("the layer's geometry does not work: {error}"),
            _ => format!("the dataset `{}` does not read: {error}", def.name),
        }));
    }
    let stage = match compiled.last() {
        Ok(stage) => stage.clone(),
        Err(e) => {
            return Ok(Err(format!(
                "the dataset `{}` does not read: {e}",
                def.name
            )));
        }
    };
    let shape = stage.shape();
    let Some(column) = shape.columns.iter().find(|c| c.name == geometry) else {
        return Ok(Err(format!(
            "`{geometry}` is not a column of `{}` (its columns are {})",
            def.name,
            names
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    if !matches!(column.ty, ColType::Geometry | ColType::Unknown) {
        return Ok(Err(format!(
            "`{geometry}` is {}, not a geometry, so it cannot be drawn on a map",
            column.ty.name()
        )));
    }
    let properties: Vec<StageColumn> = match &req.properties {
        Some(wanted) => {
            let mut out = Vec::with_capacity(wanted.len());
            for w in wanted {
                match shape.columns.iter().find(|c| &c.name == w) {
                    Some(c) => out.push(c.clone()),
                    None => {
                        return Ok(Err(format!(
                            "`{w}` is not a column of `{}`, so a feature cannot carry it",
                            def.name
                        )));
                    }
                }
            }
            out
        }
        None => shape
            .columns
            .iter()
            .filter(|c| {
                c.name != geometry
                    && !matches!(c.ty, ColType::Geometry | ColType::Json | ColType::Bytes)
            })
            .cloned()
            .collect(),
    };
    let keyed = stage.row_key_type() == Some(ColType::Int);
    Ok(Ok(Prepared {
        stage,
        geometry,
        properties,
        keyed,
    }))
}

/// `name`, or `name_2`, `name_3`… — whichever `taken` does not have.
fn unique_name(name: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == name) {
        return name.to_owned();
    }
    (2..)
        .map(|n| format!("{name}_{n}"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or_else(|| name.to_owned())
}

fn func(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Func {
        name: name.to_owned(),
        args,
    }
}

fn agg(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Agg {
        func: name.to_owned(),
        distinct: false,
        args,
    }
}

/// A literal cast to a SQL type, so Postgres knows what the placeholder is.
fn lit_as(value: Value, type_name: &str) -> Expr {
    Expr::Cast {
        expr: Box::new(Expr::Lit(value)),
        type_name: type_name.to_owned(),
    }
}

/// The alias a layer's features are read from.
const FEATURES: &str = "_fd_l";

async fn run(catalog: &Catalog, select: Select) -> Result<Vec<sc_db::Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

fn number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::Decimal(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

/// A layer's data: its features as GeoJSON while it is small, else what a
/// map needs to fetch it as tiles.
pub async fn layer_data(
    catalog: &Catalog,
    req: &LayerRequest,
    limits: Limits,
) -> Result<LayerData> {
    let layer = match prepare(catalog, req).await? {
        Ok(layer) => layer,
        Err(error) => return Ok(LayerData::Refused { error }),
    };
    let features = layer.stage.features_query().map_err(Error::invalid)?;
    let g = Expr::qcol(FEATURES, layer.geometry.clone());
    let extent = || agg("ST_Extent", vec![g.clone()]);
    let summary = Select::from(Source::subquery(features, FEATURES))
        .columns(vec![
            Projection::expr_as(agg("count", Vec::new()), "n"),
            Projection::expr_as(agg("sum", vec![func("ST_NPoints", vec![g.clone()])]), "v"),
            Projection::expr_as(func("ST_XMin", vec![extent()]), "w"),
            Projection::expr_as(func("ST_YMin", vec![extent()]), "s"),
            Projection::expr_as(func("ST_XMax", vec![extent()]), "e"),
            Projection::expr_as(func("ST_YMax", vec![extent()]), "nn"),
        ])
        .filter(Expr::unary(UnOp::IsNotNull, g));
    let rows = run(catalog, summary).await?;
    let row = rows.first();
    let at = |i: usize| row.and_then(|r| r.get_index(i));
    let count = number(at(0)).unwrap_or(0.0) as u64;
    let vertices = number(at(1)).unwrap_or(0.0) as u64;
    let bounds = match (number(at(2)), number(at(3)), number(at(4)), number(at(5))) {
        (Some(w), Some(s), Some(e), Some(n)) => Some([w, s, e, n]),
        _ => None,
    };
    if count > limits.features || vertices > limits.vertices {
        return Ok(LayerData::Tiles {
            count,
            vertices,
            bounds,
            properties: layer.properties,
            source_layer: SOURCE_LAYER.to_owned(),
            keyed: layer.keyed,
        });
    }
    let rows = read_rows(catalog, &layer.stage, None, None).await?;
    let index = |name: &str| rows.columns.iter().position(|c| c.name == name);
    let geometry = index(&layer.geometry);
    let carried: Vec<(String, Option<usize>)> = layer
        .properties
        .iter()
        .map(|p| (p.name.clone(), index(&p.name)))
        .collect();
    let mut out = Vec::with_capacity(rows.rows.len());
    for (i, row) in rows.rows.iter().enumerate() {
        let shape = geometry
            .and_then(|g| row.get(g))
            .map_or(Json::Null, value_json);
        if shape.is_null() {
            continue;
        }
        let mut properties = Map::new();
        for (name, at) in &carried {
            properties.insert(
                name.clone(),
                at.and_then(|a| row.get(a)).map_or(Json::Null, value_json),
            );
        }
        let id = match rows.keys.get(i) {
            Some(Value::Int(key)) if layer.keyed => json!(key),
            _ => json!(i + 1),
        };
        out.push(json!({
            "type": "Feature",
            "id": id,
            "geometry": shape,
            "properties": properties,
        }));
    }
    Ok(LayerData::Geojson {
        count,
        bounds,
        properties: layer.properties,
        data: json!({ "type": "FeatureCollection", "features": out }),
    })
}

/// One Mapbox vector tile of a layer, at zoom `z`, column `x` and row `y` of
/// the Web Mercator tile grid; empty where the layer has nothing. A layer that
/// cannot be drawn, or a tile outside the grid, is refused with the sentence.
pub async fn layer_tile(
    catalog: &Catalog,
    req: &LayerRequest,
    z: u32,
    x: u32,
    y: u32,
) -> Result<Vec<u8>> {
    if z > MAX_ZOOM {
        return Err(Error::invalid(format!(
            "zoom {z} is deeper than tiles go (at most {MAX_ZOOM})"
        )));
    }
    let side = 1_u64 << z;
    if u64::from(x) >= side || u64::from(y) >= side {
        return Err(Error::invalid(format!(
            "there is no tile {x}/{y} at zoom {z}: each of them is below {side}"
        )));
    }
    let layer = prepare(catalog, req).await?.map_err(Error::invalid)?;
    let features = layer.stage.features_query().map_err(Error::invalid)?;
    let int = |n: u32| lit_as(Value::Int(i64::from(n)), "integer");
    let float = |f: f64| lit_as(Value::Float(f), "double precision");
    let world = || {
        func(
            "ST_MakeEnvelope",
            vec![
                float(-MERCATOR_HALF),
                float(-MERCATOR_HALF),
                float(MERCATOR_HALF),
                float(MERCATOR_HALF),
                lit_as(Value::Int(3857), "integer"),
            ],
        )
    };
    let envelope = |margin: f64| {
        func(
            "ST_TileEnvelope",
            vec![int(z), int(x), int(y), world(), float(margin)],
        )
    };
    let g = Expr::qcol(FEATURES, layer.geometry.clone());
    // One unit of the tile's grid, in metres: what the tile can show at all.
    let resolution = 2.0 * MERCATOR_HALF / side as f64 / TILE_EXTENT as f64;
    let geom = func(
        "ST_AsMVTGeom",
        vec![
            func(
                "ST_Simplify",
                vec![
                    func(
                        "ST_Transform",
                        vec![g.clone(), lit_as(Value::Int(3857), "integer")],
                    ),
                    float(resolution),
                    lit_as(Value::Bool(true), "boolean"),
                ],
            ),
            envelope(0.0),
            lit_as(Value::Int(TILE_EXTENT), "integer"),
            lit_as(Value::Int(TILE_BUFFER), "integer"),
            lit_as(Value::Bool(true), "boolean"),
        ],
    );
    const GEOM: &str = "_fd_geom";
    const TILE: &str = "_fd_tile";
    let mut columns = vec![Projection::expr_as(geom, GEOM)];
    if layer.keyed {
        columns.push(Projection::expr_as(Expr::qcol(FEATURES, ROW_KEY), ROW_KEY));
    }
    for p in &layer.properties {
        let value = Expr::qcol(FEATURES, p.name.clone());
        // A tile's values are strings, numbers and flags.
        let value = match p.ty {
            ColType::Int | ColType::Float | ColType::Bool | ColType::Text => value,
            ColType::Decimal => Expr::Cast {
                expr: Box::new(value),
                type_name: "double precision".to_owned(),
            },
            _ => Expr::Cast {
                expr: Box::new(value),
                type_name: "text".to_owned(),
            },
        };
        columns.push(Projection::expr_as(value, p.name.clone()));
    }
    let margin = TILE_BUFFER as f64 / TILE_EXTENT as f64;
    let inner = Select::from(Source::subquery(features, FEATURES))
        .columns(columns)
        .filter(
            // `ST_Intersects` brings its own bounding-box test, which an
            // index on the geometry answers.
            Expr::unary(UnOp::IsNotNull, g.clone()).and(func(
                "ST_Intersects",
                vec![
                    g,
                    func(
                        "ST_Transform",
                        vec![envelope(margin), lit_as(Value::Int(4326), "integer")],
                    ),
                ],
            )),
        );
    let text = |s: &str| lit_as(Value::Text(s.to_owned()), "text");
    let mut args = vec![
        Expr::col(TILE),
        text(SOURCE_LAYER),
        lit_as(Value::Int(TILE_EXTENT), "integer"),
        text(GEOM),
    ];
    if layer.keyed {
        args.push(text(ROW_KEY));
    }
    let tile = Select::from(Source::subquery(inner, TILE))
        .columns(vec![Projection::expr_as(agg("ST_AsMVT", args), "mvt")])
        .filter(Expr::unary(UnOp::IsNotNull, Expr::qcol(TILE, GEOM)));
    let rows = run(catalog, tile).await?;
    Ok(match rows.first().and_then(|r| r.get_index(0)) {
        Some(Value::Bytes(bytes)) => bytes.clone(),
        _ => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_geometry_source_is_a_formula() {
        let column = GeometrySource::Column {
            column: "location".into(),
        };
        assert_eq!(column.formula(), "location");
        let points = GeometrySource::LonLat {
            longitude: "lon".into(),
            latitude: "lat".into(),
        };
        assert_eq!(points.formula(), "Geo.point(lon, lat)");
        let key = GeometrySource::Key {
            column: "district".into(),
            geometry: "outline".into(),
        };
        assert_eq!(key.formula(), "districtⱵoutline");
        let back: GeometrySource =
            serde_json::from_value(json!({"kind": "lon_lat", "longitude": "x", "latitude": "y"}))
                .expect("a source");
        assert_eq!(
            back,
            GeometrySource::LonLat {
                longitude: "x".into(),
                latitude: "y".into()
            }
        );
    }

    #[test]
    fn a_name_the_dataset_has_is_numbered() {
        let taken = vec!["geometry".to_owned(), "geometry_2".to_owned()];
        assert_eq!(unique_name("geometry", &taken), "geometry_3");
        assert_eq!(unique_name("shape", &taken), "shape");
    }
}

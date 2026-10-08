//! **Map panels** (analytics TODO A5.6–A5.7): the map spec, the geometry
//! source chosen from a dataset's columns, and what a map is drawn from.
//!
//! A map is not a [`PlotSpec`](crate::plot::PlotSpec): a plot has one dataset
//! and positions on axes, and a map has layers, each a dataset of its own and
//! a geometry source over a base map (goals document, "Map workspace": "a
//! layer is a dataset, a geometry source and a style"). Its layers take the
//! encodings a plot layer has — colour, size, shape and label by column — so
//! the explorer's drop zones mean the same on a map as on a plot. The browser
//! compiles a spec to MapLibre layers (`ui/analytics/src/map/maplibre.ts`), and
//! nothing in the spec says which renderer draws it.
//!
//! ```json
//! { "layers": [
//!     { "dataset": "…uuid…",
//!       "geometry": { "kind": "column", "column": "location" },
//!       "encoding": { "color": { "field": "category" } } } ] }
//! ```
//!
//! **Where the geometry comes from** ([`geometry_sources`]): a geometry column
//! of the dataset; longitude and latitude columns, found by their names
//! (`lon` and `lat`, `pickup_longitude` and `pickup_latitude`); or a foreign
//! key to a table with a geometry column (`district` → `districts.outline`).
//! The explorer offers them in that order and draws the first until a person
//! picks another ([`suggest_map`]).
//!
//! **What is drawn** ([`render_map`]): each layer's features, as GeoJSON or
//! as vector tiles ([`layer_data`]), and the domains of its encoded columns
//! ([`layer_domains`]) — the range of a number on Color or Size, the values of
//! a category on Color or Shape — computed in SQL over every feature, so a
//! layer sent as tiles is coloured by the same scale at every zoom.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use sc_catalog::Catalog;
use sc_dataset::{ColType, DatasetId, Options, Schema, StageColumn, StageShape, compile};
use sc_error::Result;

use crate::layer::{
    GeometrySource, LayerData, LayerRequest, Limits, Spread, layer_data, layer_domains,
};
use crate::plot::{Assignment, Domain, FieldDef};

/// A map: layers over a base map, the first drawn at the bottom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapSpec {
    /// The layers, bottom first. At least one.
    pub layers: Vec<MapLayer>,
}

/// One layer of a map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapLayer {
    /// The dataset whose rows are the features.
    pub dataset: DatasetId,
    /// Where each row's geometry comes from.
    pub geometry: GeometrySource,
    /// Which columns colour, size, shape and label the features.
    #[serde(default, skip_serializing_if = "MapEncoding::is_empty")]
    pub encoding: MapEncoding,
    /// A condition over the dataset's rows, for a layer showing some of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

impl MapLayer {
    /// The request its features are read by: every column carried, so a
    /// feature's tooltip can show its row.
    pub fn request(&self) -> LayerRequest {
        LayerRequest {
            dataset: self.dataset,
            geometry: self.geometry.clone(),
            properties: None,
            filter: self.filter.clone(),
        }
    }
}

/// The channels a map layer encodes: a plot layer's, without the positions,
/// which the geometry is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MapEncoding {
    /// The fill of a polygon, the colour of a line or a point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<FieldDef>,
    /// The radius of a point, or the width of a line: a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<FieldDef>,
    /// The symbol of a point: a category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<FieldDef>,
    /// A text beside each feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<FieldDef>,
}

impl MapEncoding {
    fn is_empty(&self) -> bool {
        self.color.is_none() && self.size.is_none() && self.shape.is_none() && self.label.is_none()
    }

    /// Every channel with a column, by name, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &FieldDef)> {
        [
            ("color", self.color.as_ref()),
            ("size", self.size.as_ref()),
            ("shape", self.shape.as_ref()),
            ("label", self.label.as_ref()),
        ]
        .into_iter()
        .filter_map(|(c, f)| f.map(|f| (c, f)))
    }
}

/// Whether a column is a number a scale can spread over: a number that is
/// not a key.
fn is_measure(c: &StageColumn) -> bool {
    c.key.is_none() && matches!(c.ty, ColType::Int | ColType::Float | ColType::Decimal)
}

/// The encoding checked against the dataset's last stage: each refusal a
/// sentence. Empty when every channel can be drawn.
pub fn validate_encoding(shape: &StageShape, encoding: &MapEncoding, dataset: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (channel, f) in encoding.iter() {
        let name = channel_name(channel);
        let Some(c) = shape.column(&f.field) else {
            problems.push(format!(
                "`{}` on {name} is not a column of `{dataset}`",
                f.field
            ));
            continue;
        };
        if f.bin.is_some() {
            problems.push(format!(
                "`{}` is binned on {name}, and a map does not bin yet; take the bins off it",
                f.field
            ));
            continue;
        }
        match c.ty {
            ColType::Geometry => problems.push(format!(
                "`{}` is a geometry, which is the shape on a map rather than its {name}",
                f.field
            )),
            ColType::Json | ColType::Bytes => problems.push(format!(
                "`{}` is {}, which a map cannot show on {name}",
                f.field,
                c.ty.name()
            )),
            _ if channel == "size" && !is_measure(c) => problems.push(format!(
                "Size on a map needs a number, and `{}` is {}; put it on Color or Shape",
                f.field,
                if c.key.is_some() {
                    "a key".to_owned()
                } else {
                    c.ty.name().to_owned()
                }
            )),
            _ if channel == "shape" && is_measure(c) && c.ty != ColType::Int => {
                problems.push(format!(
                    "Shape on a map needs a category, and `{}` is a number; put it on Color or Size",
                    f.field
                ))
            }
            _ => {}
        }
    }
    problems
}

fn channel_name(channel: &str) -> &'static str {
    match channel {
        "color" => "Color",
        "size" => "Size",
        "shape" => "Shape",
        _ => "Label",
    }
}

/// One way a dataset's rows can be put on a map, as the explorer offers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceChoice {
    /// The source.
    pub source: GeometrySource,
    /// What it is, in words: "`location`", "`lon` and `lat`", "`district` →
    /// `districts.outline`".
    pub label: String,
}

/// The tokens that name a longitude, and the latitude each pairs with.
const LONGITUDES: [&str; 4] = ["longitude", "long", "lng", "lon"];
const LATITUDES: [&str; 2] = ["latitude", "lat"];

/// Where a token sits in a column name: the whole name, a prefix or a suffix,
/// with what is left beside it (the separator kept).
fn split_token<'a>(name: &'a str, token: &str) -> Option<(&'a str, bool)> {
    if name == token {
        return Some(("", false));
    }
    if let Some(stem) = name.strip_suffix(token)
        && (stem.ends_with('_') || stem.ends_with(' '))
    {
        return Some((stem, false));
    }
    if let Some(stem) = name.strip_prefix(token)
        && (stem.starts_with('_') || stem.starts_with(' '))
    {
        return Some((stem, true));
    }
    None
}

/// Every way `shape`'s rows can be put on a map, best first: its geometry
/// columns, its longitude and latitude columns, and its foreign keys to a
/// table with a geometry column (`geometry_of` names a table's geometry
/// columns).
pub fn geometry_sources(
    shape: &StageShape,
    geometry_of: impl Fn(&str) -> Vec<String>,
) -> Vec<SourceChoice> {
    let mut out = Vec::new();
    for c in &shape.columns {
        if c.ty == ColType::Geometry {
            out.push(SourceChoice {
                source: GeometrySource::Column {
                    column: c.name.clone(),
                },
                label: format!("`{}`", c.name),
            });
        }
    }
    let numbers: Vec<&StageColumn> = shape.columns.iter().filter(|c| is_measure(c)).collect();
    for lon in &numbers {
        let lower = lon.name.to_lowercase();
        let Some((stem, prefix)) = LONGITUDES.iter().find_map(|t| split_token(&lower, t)) else {
            continue;
        };
        let lat = numbers.iter().find(|c| {
            let other = c.name.to_lowercase();
            LATITUDES.iter().any(|t| {
                let wanted = if prefix {
                    format!("{t}{stem}")
                } else {
                    format!("{stem}{t}")
                };
                other == wanted
            })
        });
        if let Some(lat) = lat {
            out.push(SourceChoice {
                source: GeometrySource::LonLat {
                    longitude: lon.name.clone(),
                    latitude: lat.name.clone(),
                },
                label: format!("`{}` and `{}`", lon.name, lat.name),
            });
        }
    }
    for c in &shape.columns {
        let Some(key) = &c.key else { continue };
        for geometry in geometry_of(&key.table) {
            out.push(SourceChoice {
                label: format!("`{}` → `{}.{geometry}`", c.name, key.table),
                source: GeometrySource::Key {
                    column: c.name.clone(),
                    geometry,
                },
            });
        }
    }
    out
}

/// The geometry columns of each table, as `geometry_sources` asks for them.
fn table_geometry(schema: &Schema) -> impl Fn(&str) -> Vec<String> + '_ {
    |table| {
        schema
            .tables
            .get(table)
            .map(|t| {
                t.columns
                    .iter()
                    .filter(|c| c.ty == ColType::Geometry)
                    .map(|c| c.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// What [`suggest_map`] answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SuggestedMap {
    /// The map, when it can be drawn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<MapSpec>,
    /// Every way the dataset's rows can be put on a map, the one drawn first
    /// unless another was asked for.
    pub sources: Vec<SourceChoice>,
    /// Why nothing can be drawn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SuggestedMap {
    fn refuse(sources: Vec<SourceChoice>, error: impl Into<String>) -> SuggestedMap {
        SuggestedMap {
            spec: None,
            sources,
            error: Some(error.into()),
        }
    }
}

/// The map the explorer draws for `dataset` (A5.7): its rows over a base map,
/// the geometry from `geometry` when it is one of the dataset's sources, else
/// from the first ([`geometry_sources`]), and Color, Size, Shape and Label
/// from the drop zones (their bins dropped: a map does not bin yet). X, Y and
/// the facets are not a map's.
pub async fn suggest_map(
    catalog: &Catalog,
    dataset: DatasetId,
    assignment: &Assignment,
    geometry: Option<&GeometrySource>,
) -> Result<SuggestedMap> {
    let def = sc_dataset::require_dataset(catalog, dataset).await?;
    let schema = Schema::of_catalog(catalog)?;
    if let Err(reason) = &schema.spatial {
        return Ok(SuggestedMap::refuse(
            Vec::new(),
            format!("a map is drawn from geometry the database computes, and {reason}"),
        ));
    }
    let library = sc_dataset::load_library(catalog).await?;
    let compiled = compile(&schema, &library, &def, Options::default());
    let shape = match compiled.last() {
        Ok(stage) => stage.shape(),
        Err(e) => {
            return Ok(SuggestedMap::refuse(
                Vec::new(),
                format!("the dataset `{}` does not read: {e}", def.name),
            ));
        }
    };
    let sources = geometry_sources(&shape, table_geometry(&schema));
    let chosen = geometry
        .and_then(|g| sources.iter().find(|s| &s.source == g))
        .or_else(|| sources.first())
        .map(|s| s.source.clone());
    let Some(chosen) = chosen else {
        return Ok(SuggestedMap::refuse(
            sources,
            format!(
                "`{}` has nothing to put on a map: no geometry column, no longitude and \
                 latitude columns, and no key to a table with a geometry column",
                def.name
            ),
        ));
    };
    let unbinned = |f: &Option<FieldDef>| f.as_ref().map(|f| FieldDef::of(f.field.clone()));
    let encoding = MapEncoding {
        color: unbinned(&assignment.color),
        size: unbinned(&assignment.size),
        shape: unbinned(&assignment.shape),
        label: unbinned(&assignment.label),
    };
    if let Some(problem) = validate_encoding(&shape, &encoding, &def.name)
        .into_iter()
        .next()
    {
        return Ok(SuggestedMap::refuse(sources, problem));
    }
    Ok(SuggestedMap {
        spec: Some(MapSpec {
            layers: vec![MapLayer {
                dataset,
                geometry: chosen,
                encoding,
                filter: None,
            }],
        }),
        sources,
        error: None,
    })
}

/// One layer as [`render_map`] answers it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RenderedLayer {
    /// The request its features were read by — and its tiles are fetched by.
    pub layer: LayerRequest,
    /// Its features, or where to fetch them, or why there are none.
    pub data: LayerData,
    /// What each encoded column spans, by channel (`color`, `size`, `shape`).
    pub domains: BTreeMap<String, Domain>,
}

/// What [`render_map`] answers: one entry per layer of the spec, in order. A
/// layer that cannot be drawn is answered `delivery: "none"` and its
/// sentence, and the others are drawn.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RenderedMap {
    /// The layers, bottom first.
    pub layers: Vec<RenderedLayer>,
}

/// Each layer's features and the domains of its encoded columns.
pub async fn render_map(catalog: &Catalog, spec: &MapSpec) -> Result<RenderedMap> {
    let schema = Schema::of_catalog(catalog)?;
    let library = sc_dataset::load_library(catalog).await?;
    let mut layers = Vec::with_capacity(spec.layers.len());
    for layer in &spec.layers {
        let request = layer.request();
        let refuse = |error: String| RenderedLayer {
            layer: request.clone(),
            data: LayerData::Refused { error },
            domains: BTreeMap::new(),
        };
        // The encoding is checked against the dataset as it is now, so a
        // column renamed since the map was made is a sentence, not a failure.
        let shape = match library.get(layer.dataset) {
            None => None,
            Some(def) => match compile(&schema, &library, def, Options::default()).last() {
                Ok(stage) => Some((stage.shape(), def.name.clone())),
                Err(_) => None,
            },
        };
        let mut spreads = Vec::new();
        if let Some((shape, name)) = &shape {
            if let Some(problem) = validate_encoding(shape, &layer.encoding, name)
                .into_iter()
                .next()
            {
                layers.push(refuse(problem));
                continue;
            }
            for (channel, f) in layer.encoding.iter() {
                let Some(c) = shape.column(&f.field) else { continue };
                let spread = match channel {
                    "size" => Spread::Range,
                    "color" if is_measure(c) => Spread::Range,
                    "color" | "shape" => Spread::Values,
                    _ => continue,
                };
                spreads.push((channel, f.field.clone(), spread));
            }
        }
        let data = layer_data(catalog, &request, Limits::default()).await?;
        if matches!(data, LayerData::Refused { .. }) {
            layers.push(RenderedLayer {
                layer: request,
                data,
                domains: BTreeMap::new(),
            });
            continue;
        }
        let columns: Vec<(String, Spread)> = spreads
            .iter()
            .map(|(_, field, spread)| (field.clone(), *spread))
            .collect();
        let by_column = match layer_domains(catalog, &request, &columns).await? {
            Ok(d) => d,
            Err(error) => {
                layers.push(refuse(error));
                continue;
            }
        };
        let domains = spreads
            .iter()
            .filter_map(|(channel, field, _)| {
                by_column
                    .get(field)
                    .map(|d| ((*channel).to_owned(), d.clone()))
            })
            .collect();
        layers.push(RenderedLayer {
            layer: request,
            data,
            domains,
        });
    }
    Ok(RenderedMap { layers })
}

#[cfg(test)]
mod tests {
    use sc_dataset::{ForeignKey, Grain};
    use serde_json::json;

    use super::*;

    fn col(name: &str, ty: ColType) -> StageColumn {
        StageColumn {
            name: name.into(),
            ty,
            key: None,
        }
    }

    fn key(name: &str, table: &str) -> StageColumn {
        StageColumn {
            name: name.into(),
            ty: ColType::Int,
            key: Some(ForeignKey {
                table: table.into(),
                field: "id".into(),
            }),
        }
    }

    fn shape(columns: Vec<StageColumn>) -> StageShape {
        StageShape {
            columns,
            grain: Grain::Derived,
        }
    }

    fn districts(table: &str) -> Vec<String> {
        if table == "districts" {
            vec!["outline".into(), "centre".into()]
        } else {
            Vec::new()
        }
    }

    #[test]
    fn sources_come_geometry_first_then_coordinates_then_keys() {
        let s = shape(vec![
            key("district", "districts"),
            key("officer", "people"),
            col("lat", ColType::Float),
            col("lon", ColType::Float),
            col("location", ColType::Geometry),
            col("pickup_longitude", ColType::Float),
            col("pickup_latitude", ColType::Float),
            col("Lng_dropoff", ColType::Float),
            col("Lat_dropoff", ColType::Float),
            col("melon", ColType::Float),
        ]);
        let found = geometry_sources(&s, districts);
        let labels: Vec<&str> = found.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "`location`",
                "`lon` and `lat`",
                "`pickup_longitude` and `pickup_latitude`",
                "`Lng_dropoff` and `Lat_dropoff`",
                "`district` → `districts.outline`",
                "`district` → `districts.centre`",
            ]
        );
        assert_eq!(
            found[4].source,
            GeometrySource::Key {
                column: "district".into(),
                geometry: "outline".into()
            }
        );
    }

    #[test]
    fn coordinates_must_be_numbers_with_a_partner() {
        let s = shape(vec![
            col("lon", ColType::Text),
            col("lat", ColType::Text),
            col("longitude", ColType::Float),
            col("height", ColType::Float),
        ]);
        assert!(geometry_sources(&s, districts).is_empty());
    }

    #[test]
    fn each_channel_is_checked_with_a_sentence() {
        let s = shape(vec![
            col("price", ColType::Float),
            col("rooms", ColType::Int),
            col("kind", ColType::Text),
            col("location", ColType::Geometry),
            key("district", "districts"),
        ]);
        let check = |enc: serde_json::Value| -> Vec<String> {
            validate_encoding(&s, &serde_json::from_value(enc).unwrap(), "houses")
        };
        assert!(check(json!({ "color": { "field": "price" }, "size": { "field": "rooms" },
                              "shape": { "field": "kind" }, "label": { "field": "price" } }))
        .is_empty());
        // A key and a whole number are categories enough for a shape.
        assert!(check(json!({ "shape": { "field": "district" } })).is_empty());
        assert!(check(json!({ "shape": { "field": "rooms" } })).is_empty());
        assert_eq!(
            check(json!({ "size": { "field": "kind" } })),
            ["Size on a map needs a number, and `kind` is text; put it on Color or Shape"]
        );
        assert_eq!(
            check(json!({ "size": { "field": "district" } })),
            ["Size on a map needs a number, and `district` is a key; put it on Color or Shape"]
        );
        assert_eq!(
            check(json!({ "shape": { "field": "price" } })),
            ["Shape on a map needs a category, and `price` is a number; put it on Color or Size"]
        );
        assert_eq!(
            check(json!({ "color": { "field": "location" } })),
            ["`location` is a geometry, which is the shape on a map rather than its Color"]
        );
        assert_eq!(
            check(json!({ "label": { "field": "nope" } })),
            ["`nope` on Label is not a column of `houses`"]
        );
        assert!(check(json!({ "color": { "field": "price", "bin": {} } }))[0].contains("binned"));
    }

    #[test]
    fn a_spec_round_trips_as_json() {
        let raw = json!({ "layers": [ {
            "dataset": "6f1c0f9e-3a43-4d55-9f43-1e2f7d0c8a11",
            "geometry": { "kind": "lon_lat", "longitude": "lon", "latitude": "lat" },
            "encoding": { "color": { "field": "category" } },
            "filter": "year > 2020"
        } ] });
        let spec: MapSpec = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&spec).unwrap(), raw);
        let request = spec.layers[0].request();
        assert_eq!(request.filter.as_deref(), Some("year > 2020"));
        assert_eq!(request.properties, None);
    }
}

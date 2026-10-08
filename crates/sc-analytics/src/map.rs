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
//!
//! **The Map workspace** (A5.8–A5.13) keeps a map spec as its state, with
//! more on each layer: an id and a name, its [`Style`] (single symbol,
//! categories, graduated colours in classes the server computes, proportional
//! symbols, a heatmap), the fields its popup shows, whether it is shown, its
//! opacity and its legend. Beside the layers, [`ReferenceLayer`]s — tile and
//! map services drawn for context — and the [`MapViewport`] last looked at. A
//! whole map dragged into a report is the same spec as a panel
//! ([`crate::panel`]'s `map`), so a report shows what the workspace showed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use sc_catalog::Catalog;
use sc_dataset::{ColType, DatasetId, Options, Schema, StageColumn, StageShape, compile};
use sc_error::{Error, Result};

use crate::classify::{Classification, MAX_CLASSES, MIN_CLASSES, breaks};
use crate::layer::{
    GeometrySource, LayerData, LayerRequest, Limits, Spread, layer_data, layer_domains,
    layer_sketch,
};
use crate::plot::{Assignment, Domain, FieldDef};

/// A map: layers over a base map, the first drawn at the bottom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapSpec {
    /// The layers, bottom first. At least one to be drawn.
    #[serde(default)]
    pub layers: Vec<MapLayer>,
    /// Tile and map services drawn for context, over the base map and under
    /// the layers, bottom first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference: Vec<ReferenceLayer>,
    /// Where the map was last looked at; fitted to the data when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<MapViewport>,
}

impl MapSpec {
    /// A map of these layers, and nothing else.
    pub fn of(layers: Vec<MapLayer>) -> MapSpec {
        MapSpec {
            layers,
            reference: Vec::new(),
            view: None,
        }
    }

    /// What is wrong with it before anything is read: a layer's or a
    /// reference layer's settings, each refusal a sentence naming it. With
    /// `ids`, every layer must have an id of its own — a workspace's do, since
    /// its selection and attribute table name a layer by it.
    pub fn check(&self, ids: bool) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for (i, layer) in self.layers.iter().enumerate() {
            let which = layer.describe(i);
            match &layer.id {
                Some(id) if id.trim().is_empty() => {
                    return Err(Error::invalid(format!("{which} has an empty id")));
                }
                Some(id) if !seen.insert(id.as_str()) => {
                    return Err(Error::invalid(format!(
                        "{which} has the id `{id}`, which another layer has"
                    )));
                }
                None if ids => return Err(Error::invalid(format!("{which} has no id"))),
                _ => {}
            }
            if let Some(problem) = layer.static_problem() {
                return Err(Error::invalid(format!("{which}: {problem}")));
            }
        }
        for (i, r) in self.reference.iter().enumerate() {
            if let Err(problem) = r.check() {
                let name = if r.name.trim().is_empty() {
                    format!("reference layer {}", i + 1)
                } else {
                    format!("the reference layer \"{}\"", r.name)
                };
                return Err(Error::invalid(format!("{name}: {problem}")));
            }
            if !seen.insert(r.id.as_str()) {
                return Err(Error::invalid(format!(
                    "the reference layer \"{}\" has the id `{}`, which another layer has",
                    r.name, r.id
                )));
            }
        }
        if let Some(view) = &self.view
            && let Err(problem) = view.check()
        {
            return Err(Error::invalid(format!("the map's view: {problem}")));
        }
        Ok(())
    }

    /// The datasets its layers read, each with how many layers read it.
    pub fn datasets(&self) -> BTreeMap<DatasetId, usize> {
        let mut out = BTreeMap::new();
        for layer in &self.layers {
            *out.entry(layer.dataset).or_default() += 1;
        }
        out
    }
}

fn shown() -> bool {
    true
}

fn is_shown(v: &bool) -> bool {
    *v
}

fn opaque() -> f64 {
    1.0
}

fn is_opaque(v: &f64) -> bool {
    (*v - 1.0).abs() < f64::EPSILON
}

/// One layer of a map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapLayer {
    /// Its identity within the map: what a selection, the attribute table and
    /// the layer list name it by. A workspace's layers have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// What the layer list and the legend call it; the dataset's name when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// How the encoded columns are drawn (A5.9).
    #[serde(default, skip_serializing_if = "Style::is_auto")]
    pub style: Style,
    /// The columns a feature's popup shows, in order; every column when
    /// empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub popup: Vec<String>,
    /// Whether it is drawn.
    #[serde(default = "shown", skip_serializing_if = "is_shown")]
    pub visible: bool,
    /// From 0 (not seen) to 1.
    #[serde(default = "opaque", skip_serializing_if = "is_opaque")]
    pub opacity: f64,
    /// Whether the legend shows it.
    #[serde(default = "shown", skip_serializing_if = "is_shown")]
    pub legend: bool,
}

impl MapLayer {
    /// A layer drawing every row of `dataset` by `geometry`, as it is.
    pub fn new(dataset: DatasetId, geometry: GeometrySource) -> MapLayer {
        MapLayer {
            id: None,
            name: None,
            dataset,
            geometry,
            encoding: MapEncoding::default(),
            filter: None,
            style: Style::Auto,
            popup: Vec::new(),
            visible: true,
            opacity: 1.0,
            legend: true,
        }
    }

    /// The request its features are read by: every column carried, so a
    /// feature's tooltip can show its row; at their centres for a style that
    /// draws points.
    pub fn request(&self) -> LayerRequest {
        LayerRequest {
            filter: self.filter.clone().filter(|f| !f.trim().is_empty()),
            points: self.style.draws_points(),
            ..LayerRequest::new(self.dataset, self.geometry.clone())
        }
    }

    /// "the layer \"Incidents\"", or "layer 2" for one with no name.
    fn describe(&self, index: usize) -> String {
        match self.name.as_deref().filter(|n| !n.trim().is_empty()) {
            Some(name) => format!("the layer \"{name}\""),
            None => format!("layer {}", index + 1),
        }
    }

    /// What is wrong with its settings that no dataset is needed to see.
    fn static_problem(&self) -> Option<String> {
        if !(0.0..=1.0).contains(&self.opacity) || self.opacity.is_nan() {
            return Some(format!(
                "its opacity is {}, and an opacity is from 0 to 1",
                self.opacity
            ));
        }
        self.style.static_problem()
    }
}

/// How a layer's features are drawn (A5.9): the plot encodings, and the
/// map's own ways of classifying them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Style {
    /// As the explorer draws a map (A5.6): a number on Color along a ramp, a
    /// category by its values.
    #[default]
    Auto,
    /// Every feature in one colour; Color is not drawn.
    Single {
        /// `#rrggbb`; the palette's first colour when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
    },
    /// The column on Color as categories — a number too — one colour each.
    Categories,
    /// The number on Color cut into classes, one colour each, by `method`.
    Graduated {
        /// Where the classes break.
        method: Classification,
        /// How many classes.
        classes: u32,
    },
    /// Each feature a circle at its centre whose area is in proportion to
    /// the number on Size.
    Proportional,
    /// The features' density as a heat surface, each weighted by the number
    /// on Size when there is one.
    Heatmap {
        /// How far, in pixels, each feature spreads.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        radius: Option<f64>,
    },
}

impl Style {
    fn is_auto(&self) -> bool {
        matches!(self, Style::Auto)
    }

    /// Whether it draws each feature as a point at its centre.
    pub fn draws_points(&self) -> bool {
        matches!(self, Style::Proportional | Style::Heatmap { .. })
    }

    /// Its name in a sentence.
    pub fn describe(&self) -> &'static str {
        match self {
            Style::Auto => "automatic",
            Style::Single { .. } => "single symbol",
            Style::Categories => "categories",
            Style::Graduated { .. } => "graduated colours",
            Style::Proportional => "proportional symbols",
            Style::Heatmap { .. } => "heatmap",
        }
    }

    fn static_problem(&self) -> Option<String> {
        match self {
            Style::Single { color: Some(c) } if !is_hex_colour(c) => Some(format!(
                "`{c}` is not a colour; give one as #rrggbb"
            )),
            Style::Graduated { classes, .. } if !(MIN_CLASSES..=MAX_CLASSES).contains(classes) => {
                Some(format!(
                    "graduated colours have from {MIN_CLASSES} to {MAX_CLASSES} classes, not \
                     {classes}"
                ))
            }
            Style::Heatmap { radius: Some(r) } if !(1.0..=100.0).contains(r) => Some(format!(
                "a heatmap's radius is from 1 to 100 pixels, not {r}"
            )),
            _ => None,
        }
    }

    /// What is wrong with it over a dataset's columns and the layer's
    /// encoding: the channel it needs, with a sentence.
    fn problem(&self, shape: &StageShape, encoding: &MapEncoding) -> Option<String> {
        let what = self.describe();
        match self {
            Style::Categories if encoding.color.is_none() => Some(format!(
                "{what} colour by the column on Color; put one there"
            )),
            Style::Graduated { .. } => match &encoding.color {
                None => Some(format!(
                    "{what} colour by the number on Color; put one there"
                )),
                Some(f) => match shape.column(&f.field) {
                    Some(c) if !is_measure(c) => Some(format!(
                        "{what} need a number on Color, and `{}` is {}; choose categories instead",
                        f.field,
                        if c.key.is_some() { "a key" } else { c.ty.name() }
                    )),
                    _ => None,
                },
            },
            Style::Proportional if encoding.size.is_none() => Some(format!(
                "{what} are sized by the number on Size; put one there"
            )),
            _ => None,
        }
    }
}

/// Whether `s` is `#rrggbb`.
fn is_hex_colour(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// A tile or map service drawn for context (A5.11): not a dataset, so it
/// cannot be analysed, styled or selected from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceLayer {
    /// Its identity within the map.
    pub id: String,
    /// What the layer list calls it.
    pub name: String,
    /// Where its images come from.
    #[serde(flatten)]
    pub service: ReferenceService,
    /// From 0 to 1.
    #[serde(default = "opaque", skip_serializing_if = "is_opaque")]
    pub opacity: f64,
    /// Whether it is drawn.
    #[serde(default = "shown", skip_serializing_if = "is_shown")]
    pub visible: bool,
    /// The credit its provider asks for, shown in the map's corner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}

/// The kinds of service a reference layer is drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReferenceService {
    /// Raster tiles at a URL with `{z}`, `{x}` and `{y}` in it (an "XYZ"
    /// service: OpenStreetMap's, a satellite imagery provider's).
    Tiles {
        /// The template.
        url: String,
    },
    /// An OGC Web Map Service: its address and the layers to draw, asked for
    /// tile by tile in Web Mercator.
    Wms {
        /// The service's address.
        url: String,
        /// The service's layer names, comma-separated.
        layers: String,
    },
    /// An ArcGIS map service's cached tiles (`…/MapServer`).
    Arcgis {
        /// The service's address.
        url: String,
    },
}

impl ReferenceService {
    /// The service's address.
    pub fn url(&self) -> &str {
        match self {
            ReferenceService::Tiles { url }
            | ReferenceService::Wms { url, .. }
            | ReferenceService::Arcgis { url } => url,
        }
    }

    /// The origin its images come from — what Settings → Maps must name for
    /// the browser to load them.
    pub fn origin(&self) -> std::result::Result<String, String> {
        sc_config::origin_of(self.url())
    }
}

impl ReferenceLayer {
    fn check(&self) -> std::result::Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("it has no id".to_owned());
        }
        if !(0.0..=1.0).contains(&self.opacity) {
            return Err(format!(
                "its opacity is {}, and an opacity is from 0 to 1",
                self.opacity
            ));
        }
        let url = self.service.url().trim();
        if url.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '<') {
            return Err(format!("`{url}` is not a URL"));
        }
        self.service.origin()?;
        match &self.service {
            ReferenceService::Tiles { url } => {
                let has = |p: &str| url.contains(p);
                if !(has("{z}") && has("{x}") && has("{y}"))
                    && !has("{quadkey}")
                    && !has("{bbox-epsg-3857}")
                {
                    return Err(format!(
                        "`{url}` is not a tile template: it names no `{{z}}`, `{{x}}` and \
                         `{{y}}` for the tile's zoom, column and row"
                    ));
                }
            }
            ReferenceService::Wms { layers, .. } if layers.trim().is_empty() => {
                return Err("a map service needs the names of the layers to draw".to_owned());
            }
            _ => {}
        }
        Ok(())
    }
}

/// Where a map is looked at: its centre, in degrees, and its zoom.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MapViewport {
    /// `[longitude, latitude]`.
    pub center: [f64; 2],
    /// From 0 (the whole world) to 24.
    pub zoom: f64,
}

impl MapViewport {
    fn check(&self) -> std::result::Result<(), String> {
        let [lon, lat] = self.center;
        if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
            return Err(format!("[{lon}, {lat}] is not a longitude and a latitude"));
        }
        if !(0.0..=24.0).contains(&self.zoom) {
            return Err(format!("zoom {} is not from 0 to 24", self.zoom));
        }
        Ok(())
    }
}

/// A Map workspace's state as this crate reads it: its spec. The rest — the
/// selection, which layer's table is open — is the screen's.
pub fn spec_of_state(state: &serde_json::Value) -> Result<MapSpec> {
    serde_json::from_value(state.clone())
        .map_err(|e| Error::invalid(format!("the map's state does not read: {e}")))
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
        spec: Some(MapSpec::of(vec![MapLayer {
            encoding,
            ..MapLayer::new(dataset, chosen)
        }])),
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
    /// Graduated colours' breaks: the smallest value, each class's lower
    /// bound after the first, the largest ([`crate::classify`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classes: Option<Vec<f64>>,
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
            classes: None,
        };
        if let Some(problem) = layer.static_problem() {
            layers.push(refuse(problem));
            continue;
        }
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
                .or_else(|| layer.style.problem(shape, &layer.encoding))
                .or_else(|| {
                    layer
                        .popup
                        .iter()
                        .find(|p| shape.column(p).is_none())
                        .map(|p| format!("`{p}` in the popup is not a column of `{name}`"))
                })
            {
                layers.push(refuse(problem));
                continue;
            }
            for (channel, f) in layer.encoding.iter() {
                let Some(c) = shape.column(&f.field) else { continue };
                let spread = match (channel, &layer.style) {
                    // One colour, or a heat surface: nothing on Color is drawn.
                    ("color", Style::Single { .. } | Style::Heatmap { .. }) => continue,
                    // Its classes are its scale.
                    ("color", Style::Graduated { .. }) => continue,
                    ("color", Style::Categories) => Spread::Values,
                    ("size", _) => Spread::Range,
                    ("color", _) if is_measure(c) => Spread::Range,
                    ("color" | "shape", _) => Spread::Values,
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
                classes: None,
            });
            continue;
        }
        let classes = match (&layer.style, &layer.encoding.color) {
            (Style::Graduated { method, classes }, Some(f)) => {
                match layer_sketch(catalog, &request, &f.field).await? {
                    Ok(values) => Some(breaks(&values, *classes, *method)),
                    Err(error) => {
                        layers.push(refuse(error));
                        continue;
                    }
                }
            }
            _ => None,
        };
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
            classes,
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

    #[test]
    fn a_workspace_map_checks_its_layers_reference_layers_and_view() {
        let d = "6f1c0f9e-3a43-4d55-9f43-1e2f7d0c8a11";
        let state = json!({
            "layers": [
                { "id": "a", "name": "Incidents", "dataset": d,
                  "geometry": { "kind": "column", "column": "at" },
                  "style": { "kind": "graduated", "method": "natural_breaks", "classes": 5 },
                  "encoding": { "color": { "field": "count" } },
                  "popup": ["name"], "visible": false, "opacity": 0.4, "legend": false },
                { "id": "b", "dataset": d, "geometry": { "kind": "column", "column": "at" },
                  "style": { "kind": "single", "color": "#aa3300" } },
            ],
            "reference": [
                { "id": "osm", "name": "OSM", "kind": "tiles",
                  "url": "https://tile.openstreetmap.org/{z}/{x}/{y}.png", "opacity": 0.5 },
                { "id": "wms", "name": "Cadastre", "kind": "wms",
                  "url": "https://maps.example.com/wms", "layers": "parcels" },
            ],
            "view": { "center": [0.1, 51.5], "zoom": 11 },
            "selection": { "layer": "a", "ids": [1, 2] },
        });
        let spec = spec_of_state(&state).expect("reads");
        spec.check(true).expect("checks");
        assert!(!spec.layers[0].visible);
        assert_eq!(spec.reference[1].service.origin().as_deref(), Ok("https://maps.example.com"));
        // What the screen keeps beside the spec is not the spec's.
        let back = serde_json::to_value(&spec).unwrap();
        assert!(back.get("selection").is_none());
        assert_eq!(back["layers"][1].get("visible"), None, "defaults are left out");

        let refused = |change: &dyn Fn(&mut serde_json::Value), says: &str| {
            let mut s = state.clone();
            change(&mut s);
            let err = spec_of_state(&s)
                .and_then(|spec| spec.check(true))
                .expect_err(says);
            assert!(err.to_string().contains(says), "{err} should say {says}");
        };
        refused(&|s| s["layers"][1]["id"] = json!("a"), "which another layer has");
        refused(
            &|s| {
                s["layers"][1].as_object_mut().unwrap().remove("id");
            },
            "layer 2 has no id",
        );
        refused(&|s| s["layers"][0]["opacity"] = json!(1.5), "the layer \"Incidents\": its opacity");
        refused(&|s| s["layers"][1]["style"]["color"] = json!("red"), "not a colour");
        refused(&|s| s["layers"][0]["style"]["classes"] = json!(9), "from 2 to 7 classes");
        refused(
            &|s| s["reference"][0]["url"] = json!("https://tile.example.com/tiles.png"),
            "not a tile template",
        );
        refused(
            &|s| s["reference"][0]["url"] = json!("ftp://tile.example.com/{z}/{x}/{y}"),
            "not an http or https URL",
        );
        refused(
            &|s| s["reference"][0]["url"] = json!("https://a.example.com/{z}/{x}/{y}.png\"; script-src *"),
            "is not a URL",
        );
        refused(&|s| s["reference"][1]["layers"] = json!(" "), "names of the layers");
        refused(&|s| s["view"]["zoom"] = json!(30), "zoom 30");
        refused(&|s| s["layers"][0]["style"]["kind"] = json!("rainbow"), "does not read");
        // A panel's map has no ids to keep.
        let mut panel = spec.clone();
        panel.layers[0].id = None;
        panel.check(false).expect("a panel's layers need no id");
    }

    #[test]
    fn styles_say_what_they_draw() {
        assert!(Style::Proportional.draws_points());
        assert!(Style::Heatmap { radius: None }.draws_points());
        assert!(!Style::Categories.draws_points());
        let layer = MapLayer {
            style: Style::Heatmap { radius: None },
            filter: Some("  ".into()),
            ..MapLayer::new(DatasetId::new(), GeometrySource::Column { column: "at".into() })
        };
        let req = layer.request();
        assert!(req.points);
        assert_eq!(req.filter, None, "a blank filter is none");
        assert_eq!(req.geometry_formula(), "Geo.centroid(at)");
    }
}

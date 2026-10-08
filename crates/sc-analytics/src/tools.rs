//! **The Map workspace's toolbox** (analytics TODO A5.12): spatial analysis
//! as shortcuts to dataset operations.
//!
//! A tool is a form whose answer is an ordinary global dataset, added to the
//! map as a layer (goals document, "Map workspace": "a tool is a shortcut, not
//! a black box"). Nothing it makes is hidden: the dataset opens in the Dataset
//! editor, with the operations the tool chose, and can be changed there. The
//! map never computes anything itself.
//!
//! | group | tool | operations |
//! |---|---|---|
//! | Proximity | Buffer | a Calculated column `Geo.buffer(g, d)` |
//! | Proximity | Distance to nearest | a Spatial join to the nearest, keeping only the distance |
//! | Proximity | Within a distance | a Spatial join to the nearest within the distance, inner |
//! | Overlay | Spatial join | a Spatial join (intersects, within, contains) |
//! | Overlay | Intersection | a Spatial join and a Calculated column `Geo.intersection(g, h)` |
//! | Aggregate | Count per region, Sum per region | a Spatial join to the regions, an Aggregate by the region, and a Complete over every region with 0 |
//! | Aggregate | Dissolve | an Aggregate by a column with the union of the geometries |
//!
//! A tool's input layers are map layers — a dataset, its geometry source and
//! its filter — so a new dataset starts from the input layer's dataset, the
//! layer's filter as a Filter, and its geometry as a Calculated column when it
//! is not a column already.
//!
//! **Plugins** add tools. A Rust plugin implements [`MapTool`]; a module
//! declares one as data — its form and the operations it adds, with the
//! answers written in as `{{name}}` ([`TemplateTool`]) — under its
//! `maptools` export, and the server installs them on every module change
//! ([`install_plugin_tools`]).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json, json};

use sc_catalog::Catalog;
use sc_dataset::{
    AggregateOp, Base, ColType, CompleteColumn, CompleteOp, CompleteValues, DatasetDef, DatasetId,
    FillValue, Grain, GroupKey, JoinKind, Library, Op, OpStatus, Operation, Options, Other, Schema,
    SpatialJoinOp, SpatialRelation, StageShape, Summary, SummaryFunction, compile,
};
use sc_error::{Error, Result};

use crate::classify::Classification;
use crate::layer::{GeometrySource, unique_name};
use crate::map::{MapEncoding, MapLayer, Style};
use crate::plot::FieldDef;

/// What a tool's form asks for, and what a tool is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// Unique among the tools: a built-in's name, a plugin's prefixed with
    /// its module (`@acme/gis:hotspots`).
    pub id: String,
    /// The toolbox's group: Proximity, Overlay, Aggregate, or a plugin's own.
    pub group: String,
    /// The menu's label.
    pub label: String,
    /// One sentence on what it makes.
    #[serde(default)]
    pub description: String,
    /// The form, in order.
    #[serde(default)]
    pub params: Vec<ToolParam>,
    /// The module a plugin's tool comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
}

/// One field of a tool's form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolParam {
    /// The answer's key.
    pub name: String,
    /// The field's label.
    pub label: String,
    /// What it asks for.
    #[serde(flatten)]
    pub kind: ParamKind,
    /// What the form starts on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Json>,
    /// Whether it may be left empty.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

/// The kinds of field a tool's form has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamKind {
    /// One of the map's layers.
    Layer,
    /// A column of the layer another field picked.
    Column {
        /// That field's name.
        of: String,
        /// `number`, `text`, … — the column types offered; any when empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        types: Vec<String>,
    },
    /// A number, in `unit`.
    Number {
        /// `m` for metres.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
    /// One of a few choices.
    Choice {
        /// The choices.
        options: Vec<ChoiceOption>,
    },
    /// Text.
    Text,
}

/// One choice of a [`ParamKind::Choice`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    /// The answer.
    pub value: String,
    /// What the form shows.
    pub label: String,
}

impl ToolParam {
    fn new(name: &str, label: &str, kind: ParamKind) -> ToolParam {
        ToolParam {
            name: name.to_owned(),
            label: label.to_owned(),
            kind,
            default: None,
            optional: false,
        }
    }

    fn layer(name: &str, label: &str) -> ToolParam {
        ToolParam::new(name, label, ParamKind::Layer)
    }

    fn metres(name: &str, label: &str, default: f64) -> ToolParam {
        ToolParam {
            default: Some(json!(default)),
            ..ToolParam::new(
                name,
                label,
                ParamKind::Number {
                    unit: Some("m".to_owned()),
                },
            )
        }
    }

    fn column(name: &str, label: &str, of: &str, types: &[&str]) -> ToolParam {
        ToolParam::new(
            name,
            label,
            ParamKind::Column {
                of: of.to_owned(),
                types: types.iter().map(|t| (*t).to_owned()).collect(),
            },
        )
    }

    fn choice(name: &str, label: &str, options: &[(&str, &str)], default: &str) -> ToolParam {
        ToolParam {
            default: Some(json!(default)),
            ..ToolParam::new(
                name,
                label,
                ParamKind::Choice {
                    options: options
                        .iter()
                        .map(|(value, label)| ChoiceOption {
                            value: (*value).to_owned(),
                            label: (*label).to_owned(),
                        })
                        .collect(),
                },
            )
        }
    }
}

/// What a tool is given: the schema and the stored datasets, to look at its
/// input layers' columns.
pub struct ToolContext {
    schema: Schema,
    library: Library,
}

impl ToolContext {
    /// The context of `catalog` as it is now.
    pub async fn load(catalog: &Catalog) -> Result<ToolContext> {
        Ok(ToolContext {
            schema: Schema::of_catalog(catalog)?,
            library: sc_dataset::load_library(catalog).await?,
        })
    }

    /// The last stage's shape of a stored dataset, or why it has none.
    pub fn shape_of(&self, dataset: DatasetId) -> std::result::Result<StageShape, String> {
        let def = self
            .library
            .get(dataset)
            .ok_or("the dataset of one of the layers is gone")?;
        compile(&self.schema, &self.library, def, Options::default())
            .last()
            .map(|s| s.shape())
            .map_err(|e| format!("the dataset `{}` does not read: {e}", def.name))
    }

    /// A stored dataset's name.
    pub fn name_of(&self, dataset: DatasetId) -> String {
        self.library
            .get(dataset)
            .map_or_else(|| "the layer".to_owned(), |d| d.name.clone())
    }

    /// The geometry columns of a table.
    fn table_geometry(&self, table: &str) -> Vec<String> {
        self.schema
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

/// A form's answers.
#[derive(Debug, Clone, Default)]
pub struct ToolArgs(pub Map<String, Json>);

impl ToolArgs {
    fn get(&self, name: &str) -> Option<&Json> {
        self.0.get(name).filter(|v| !v.is_null())
    }

    /// The layer the field `name` picked.
    pub fn layer(&self, name: &str) -> std::result::Result<MapLayer, String> {
        let raw = self
            .get(name)
            .ok_or_else(|| format!("pick the layer for `{name}`"))?;
        serde_json::from_value(raw.clone()).map_err(|e| format!("`{name}` is not a layer: {e}"))
    }

    /// The number in the field `name`.
    pub fn number(&self, name: &str) -> std::result::Result<f64, String> {
        match self.get(name) {
            Some(Json::Number(n)) => n
                .as_f64()
                .ok_or_else(|| format!("`{name}` is not a number")),
            Some(Json::String(s)) => s
                .trim()
                .parse()
                .map_err(|_| format!("`{s}` is not a number for `{name}`")),
            _ => Err(format!("give a number for `{name}`")),
        }
    }

    /// The text in the field `name`, trimmed; `None` when empty.
    pub fn text(&self, name: &str) -> Option<String> {
        self.get(name)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// A distance in metres, positive.
    fn metres(&self, name: &str) -> std::result::Result<f64, String> {
        let d = self.number(name)?;
        if d.is_finite() && d > 0.0 {
            Ok(d)
        } else {
            Err(format!(
                "{d} metres is not a distance; give a positive number"
            ))
        }
    }
}

/// What a tool makes: a dataset (named and stored by [`run_tool`]) and the
/// layer that shows it.
#[derive(Debug, Clone)]
pub struct ToolBuild {
    /// What the dataset is called unless the person names it.
    pub name: String,
    /// The dataset's base and operations; its id and name are set when it is
    /// stored.
    pub dataset: DatasetDef,
    /// The layer, its `dataset` set when it is stored.
    pub layer: MapLayer,
}

/// A tool of the toolbox.
pub trait MapTool: Send + Sync {
    /// Its form, and what it is.
    fn descriptor(&self) -> ToolDescriptor;
    /// The dataset and layer the answers make, or the sentence saying why
    /// they make none.
    fn build(&self, ctx: &ToolContext, args: &ToolArgs) -> std::result::Result<ToolBuild, String>;
}

// --- helpers the built-ins share ----------------------------------------------------

/// A new dataset over a layer's rows: its dataset, its filter as a Filter,
/// and its geometry as a column — the column itself, or a Calculated column
/// of its formula. Answers the dataset and that column's name.
fn over_layer(
    ctx: &ToolContext,
    layer: &MapLayer,
) -> std::result::Result<(DatasetDef, String, Vec<String>), String> {
    let shape = ctx.shape_of(layer.dataset)?;
    let mut names: Vec<String> = shape.columns.iter().map(|c| c.name.clone()).collect();
    let mut def = DatasetDef::new("", Base::dataset(layer.dataset));
    if let Some(filter) = layer.filter.as_deref().filter(|f| !f.trim().is_empty()) {
        def.operations
            .push(Operation::new("filter", Op::filter(filter)));
    }
    let geometry = match &layer.geometry {
        GeometrySource::Column { column } => column.clone(),
        source => {
            let name = unique_name("geometry", &names);
            def.operations.push(Operation::new(
                "geometry",
                Op::calculated(name.clone(), source.formula()),
            ));
            names.push(name.clone());
            name
        }
    };
    Ok((def, geometry, names))
}

/// What a Spatial join can join a layer as: its dataset, by a geometry
/// column of it. A layer whose geometry is computed, or that shows some of
/// its rows, is refused with a sentence: the join would read every row of its
/// dataset, not what the layer shows.
fn joinable(ctx: &ToolContext, layer: &MapLayer) -> std::result::Result<(Other, String), String> {
    let name = layer
        .name
        .clone()
        .unwrap_or_else(|| ctx.name_of(layer.dataset));
    if layer
        .filter
        .as_deref()
        .is_some_and(|f| !f.trim().is_empty())
    {
        return Err(format!(
            "the layer \"{name}\" shows some of its dataset's rows; save those as a dataset of \
             their own to use them here"
        ));
    }
    match &layer.geometry {
        GeometrySource::Column { column } => Ok((
            Other::Dataset {
                dataset: layer.dataset,
            },
            column.clone(),
        )),
        GeometrySource::LonLat { .. } => Err(format!(
            "the layer \"{name}\" makes its points from two columns; add them as a geometry \
             column (Geo.point) to its dataset to use it here"
        )),
        GeometrySource::Key { column, .. } => Err(format!(
            "the layer \"{name}\" takes its geometry along `{column}` from another table; use a \
             layer of that table here"
        )),
    }
}

fn layer_name(ctx: &ToolContext, layer: &MapLayer) -> String {
    layer
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| ctx.name_of(layer.dataset))
}

/// A distance as a name says it: `500 m`, `1.5 km`.
fn distance_words(d: f64) -> String {
    if d >= 1000.0 {
        format!("{} km", d / 1000.0)
    } else {
        format!("{d} m")
    }
}

fn spatial_join(
    with: Other,
    kind: JoinKind,
    relation: SpatialRelation,
    left: &str,
    right: &str,
) -> SpatialJoinOp {
    SpatialJoinOp {
        with,
        kind,
        relation,
        left: left.to_owned(),
        right: right.to_owned(),
        distance: None,
        distance_column: None,
        columns: None,
        suffix: "_right".to_owned(),
    }
}

/// The name a joined column of the other's has after a join to rows that
/// already have `names`.
fn joined_name(column: &str, names: &[String]) -> String {
    if names.iter().any(|n| n == column) {
        format!("{column}_right")
    } else {
        column.to_owned()
    }
}

/// A layer showing a new dataset by the input layer's geometry source, or a
/// column of it.
fn result_layer(geometry: GeometrySource, style: Style, encoding: MapEncoding) -> MapLayer {
    MapLayer {
        style,
        encoding,
        // The dataset is set when it is stored.
        ..MapLayer::new(DatasetId::new(), geometry)
    }
}

fn colour_by(field: &str) -> MapEncoding {
    MapEncoding {
        color: Some(FieldDef::of(field)),
        ..MapEncoding::default()
    }
}

fn graduated() -> Style {
    Style::Graduated {
        method: Classification::NaturalBreaks,
        classes: 5,
    }
}

// --- the built-in tools ------------------------------------------------------------

struct Buffer;

impl MapTool for Buffer {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "buffer".into(),
            group: "Proximity".into(),
            label: "Buffer".into(),
            description: "The area within a distance of each feature.".into(),
            params: vec![
                ToolParam::layer("layer", "Layer"),
                ToolParam::metres("distance", "Distance", 500.0),
            ],
            module: None,
        }
    }

    fn build(&self, ctx: &ToolContext, args: &ToolArgs) -> std::result::Result<ToolBuild, String> {
        let layer = args.layer("layer")?;
        let d = args.metres("distance")?;
        let (mut dataset, g, names) = over_layer(ctx, &layer)?;
        let buffer = unique_name("buffer", &names);
        dataset.operations.push(Operation::new(
            "buffer",
            Op::calculated(buffer.clone(), format!("Geo.buffer({g}, {d})")),
        ));
        Ok(ToolBuild {
            name: format!("{} within {}", layer_name(ctx, &layer), distance_words(d)),
            dataset,
            layer: result_layer(
                GeometrySource::Column { column: buffer },
                Style::Single { color: None },
                MapEncoding::default(),
            ),
        })
    }
}

struct DistanceToNearest;

impl MapTool for DistanceToNearest {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "distance_to_nearest".into(),
            group: "Proximity".into(),
            label: "Distance to nearest".into(),
            description: "Each feature's distance in metres to the nearest feature of another \
                          layer."
                .into(),
            params: vec![
                ToolParam::layer("layer", "Layer"),
                ToolParam::layer("to", "Nearest of"),
            ],
            module: None,
        }
    }

    fn build(&self, ctx: &ToolContext, args: &ToolArgs) -> std::result::Result<ToolBuild, String> {
        let layer = args.layer("layer")?;
        let to = args.layer("to")?;
        let (other, right) = joinable(ctx, &to)?;
        let (mut dataset, g, names) = over_layer(ctx, &layer)?;
        let column = unique_name("distance", &names);
        dataset.operations.push(Operation::new(
            "nearest",
            Op::SpatialJoin(SpatialJoinOp {
                distance_column: Some(column.clone()),
                columns: Some(Vec::new()),
                ..spatial_join(other, JoinKind::Left, SpatialRelation::Nearest, &g, &right)
            }),
        ));
        Ok(ToolBuild {
            name: format!(
                "{} by distance to {}",
                layer_name(ctx, &layer),
                layer_name(ctx, &to)
            ),
            dataset,
            layer: result_layer(layer.geometry.clone(), graduated(), colour_by(&column)),
        })
    }
}

struct WithinDistance;

impl MapTool for WithinDistance {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "within_distance".into(),
            group: "Proximity".into(),
            label: "Within a distance".into(),
            description: "The features within a distance of any feature of another layer.".into(),
            params: vec![
                ToolParam::layer("layer", "Layer"),
                ToolParam::layer("of", "Of"),
                ToolParam::metres("distance", "Distance", 1000.0),
            ],
            module: None,
        }
    }

    fn build(&self, ctx: &ToolContext, args: &ToolArgs) -> std::result::Result<ToolBuild, String> {
        let layer = args.layer("layer")?;
        let of = args.layer("of")?;
        let d = args.metres("distance")?;
        let (other, right) = joinable(ctx, &of)?;
        let (mut dataset, g, names) = over_layer(ctx, &layer)?;
        let column = unique_name("distance", &names);
        dataset.operations.push(Operation::new(
            "within",
            Op::SpatialJoin(SpatialJoinOp {
                distance: Some(d),
                distance_column: Some(column),
                columns: Some(Vec::new()),
                ..spatial_join(other, JoinKind::Inner, SpatialRelation::Nearest, &g, &right)
            }),
        ));
        Ok(ToolBuild {
            name: format!(
                "{} within {} of {}",
                layer_name(ctx, &layer),
                distance_words(d),
                layer_name(ctx, &of)
            ),
            dataset,
            layer: result_layer(layer.geometry.clone(), layer.style.clone(), layer.encoding),
        })
    }
}

/// The relations an overlay offers.
const RELATIONS: [(&str, &str); 3] = [
    ("intersects", "intersects"),
    ("within", "is within"),
    ("contains", "contains"),
];

struct Join;

impl MapTool for Join {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "spatial_join".into(),
            group: "Overlay".into(),
            label: "Spatial join".into(),
            description: "Each feature with the columns of the features of another layer it \
                          meets."
                .into(),
            params: vec![
                ToolParam::layer("layer", "Layer"),
                ToolParam::layer("with", "With"),
                ToolParam::choice("relation", "Where each feature", &RELATIONS, "intersects"),
                ToolParam::choice(
                    "keep",
                    "Keep",
                    &[
                        ("left", "every feature"),
                        ("inner", "only the features that meet one"),
                    ],
                    "left",
                ),
            ],
            module: None,
        }
    }

    fn build(&self, ctx: &ToolContext, args: &ToolArgs) -> std::result::Result<ToolBuild, String> {
        let layer = args.layer("layer")?;
        let with = args.layer("with")?;
        let relation = match args.text("relation").as_deref().unwrap_or("intersects") {
            "intersects" => SpatialRelation::Intersects,
            "within" => SpatialRelation::Within,
            "contains" => SpatialRelation::Contains,
            other => return Err(format!("`{other}` is not a relation an overlay knows")),
        };
        let kind = match args.text("keep").as_deref().unwrap_or("left") {
            "inner" => JoinKind::Inner,
            _ => JoinKind::Left,
        };
        let (other, right) = joinable(ctx, &with)?;
        let (mut dataset, g, _) = over_layer(ctx, &layer)?;
        dataset.operations.push(Operation::new(
            "join",
            Op::SpatialJoin(spatial_join(other, kind, relation, &g, &right)),
        ));
        Ok(ToolBuild {
            name: format!(
                "{} with {}",
                layer_name(ctx, &layer),
                layer_name(ctx, &with)
            ),
            dataset,
            layer: result_layer(layer.geometry.clone(), Style::Auto, MapEncoding::default()),
        })
    }
}

struct Intersection;

impl MapTool for Intersection {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "intersection".into(),
            group: "Overlay".into(),
            label: "Intersection".into(),
            description: "The parts where the features of two layers overlap, with the columns \
                          of both."
                .into(),
            params: vec![
                ToolParam::layer("layer", "Layer"),
                ToolParam::layer("with", "With"),
            ],
            module: None,
        }
    }

    fn build(&self, ctx: &ToolContext, args: &ToolArgs) -> std::result::Result<ToolBuild, String> {
        let layer = args.layer("layer")?;
        let with = args.layer("with")?;
        let (other, right) = joinable(ctx, &with)?;
        let (mut dataset, g, mut names) = over_layer(ctx, &layer)?;
        let joined = joined_name(&right, &names);
        dataset.operations.push(Operation::new(
            "join",
            Op::SpatialJoin(spatial_join(
                other,
                JoinKind::Inner,
                SpatialRelation::Intersects,
                &g,
                &right,
            )),
        ));
        if let Ok(shape) = ctx.shape_of(with.dataset) {
            names.extend(shape.columns.into_iter().map(|c| c.name));
        }
        names.push(joined.clone());
        let part = unique_name("intersection", &names);
        dataset.operations.push(Operation::new(
            "intersection",
            Op::calculated(part.clone(), format!("Geo.intersection({g}, {joined})")),
        ));
        Ok(ToolBuild {
            name: format!(
                "{} intersecting {}",
                layer_name(ctx, &layer),
                layer_name(ctx, &with)
            ),
            dataset,
            layer: result_layer(
                GeometrySource::Column { column: part },
                Style::Auto,
                MapEncoding::default(),
            ),
        })
    }
}

/// Count or sum per region: points (or any features) joined to the regions
/// they meet, aggregated by the region, and completed over every region.
struct PerRegion {
    sum: bool,
}

impl MapTool for PerRegion {
    fn descriptor(&self) -> ToolDescriptor {
        let mut params = vec![
            ToolParam::layer("layer", "Features"),
            ToolParam::layer("regions", "Regions"),
        ];
        if self.sum {
            params.push(ToolParam::column("column", "Sum of", "layer", &["number"]));
        }
        ToolDescriptor {
            id: if self.sum {
                "sum_per_region"
            } else {
                "count_per_region"
            }
            .into(),
            group: "Aggregate".into(),
            label: if self.sum {
                "Sum per region"
            } else {
                "Count per region"
            }
            .into(),
            description: if self.sum {
                "Each region with the total of a column over the features in it."
            } else {
                "Each region with the number of features in it."
            }
            .into(),
            params,
            module: None,
        }
    }

    fn build(&self, ctx: &ToolContext, args: &ToolArgs) -> std::result::Result<ToolBuild, String> {
        let layer = args.layer("layer")?;
        let regions = args.layer("regions")?;
        let (other, right) = joinable(ctx, &regions)?;
        let regions_shape = ctx.shape_of(regions.dataset)?;
        let regions_name = layer_name(ctx, &regions);
        let Grain::Table { table, key } = &regions_shape.grain else {
            return Err(format!(
                "the regions of \"{regions_name}\" are not rows of a table, so features cannot \
                 be counted per region of them; use a layer of the regions' table"
            ));
        };
        if regions_shape.column(key).is_none() {
            return Err(format!(
                "\"{regions_name}\" has dropped its key `{key}`, which tells the regions apart"
            ));
        }
        // The map draws each region by its key, from the table's geometry.
        let table_geometry = ctx.table_geometry(table);
        let outline = if table_geometry.contains(&right) {
            right.clone()
        } else {
            table_geometry.first().cloned().ok_or_else(|| {
                format!("the table `{table}` has no geometry column to draw the regions by")
            })?
        };
        let column = if self.sum {
            let column = args
                .text("column")
                .ok_or("pick the column whose values are summed")?;
            let shape = ctx.shape_of(layer.dataset)?;
            match shape.column(&column) {
                Some(c)
                    if c.key.is_none()
                        && matches!(c.ty, ColType::Int | ColType::Float | ColType::Decimal) => {}
                Some(_) => return Err(format!("`{column}` is not a number to sum")),
                None => {
                    return Err(format!(
                        "`{column}` is not a column of \"{}\"",
                        layer_name(ctx, &layer)
                    ));
                }
            }
            Some(column)
        } else {
            None
        };
        let (mut dataset, g, names) = over_layer(ctx, &layer)?;
        let joined = joined_name(key, &names);
        dataset.operations.push(Operation::new(
            "join",
            Op::SpatialJoin(SpatialJoinOp {
                columns: Some(vec![key.clone()]),
                ..spatial_join(
                    other,
                    JoinKind::Inner,
                    SpatialRelation::Intersects,
                    &g,
                    &right,
                )
            }),
        ));
        let region = singular(table);
        let mut summaries = vec![Summary::count("count")];
        let mut fill = vec![FillValue {
            column: "count".into(),
            value: json!(0),
        }];
        let measure = match &column {
            Some(c) => {
                let name = unique_name(&format!("sum_{c}"), &[region.clone(), "count".into()]);
                summaries.push(Summary::of(name.clone(), SummaryFunction::Sum, c.clone()));
                fill.push(FillValue {
                    column: name.clone(),
                    value: json!(0),
                });
                name
            }
            None => "count".to_owned(),
        };
        dataset.operations.push(Operation::new(
            "aggregate",
            Op::Aggregate(AggregateOp {
                group_by: vec![GroupKey {
                    name: region.clone(),
                    formula: joined,
                }],
                summaries,
            }),
        ));
        // Every region, those with no feature counted 0.
        dataset.operations.push(Operation::new(
            "complete",
            Op::Complete(CompleteOp {
                columns: vec![CompleteColumn {
                    column: region.clone(),
                    values: CompleteValues::Table,
                }],
                fill,
            }),
        ));
        let what = match &column {
            Some(c) => format!("{c} of {}", layer_name(ctx, &layer)),
            None => layer_name(ctx, &layer),
        };
        Ok(ToolBuild {
            name: format!("{what} per {regions_name}"),
            dataset,
            layer: result_layer(
                GeometrySource::Key {
                    column: region,
                    geometry: outline,
                },
                graduated(),
                colour_by(&measure),
            ),
        })
    }
}

/// A table's name for one of its rows: `districts` → `district`.
fn singular(table: &str) -> String {
    let name = table
        .strip_suffix("ies")
        .map(|s| format!("{s}y"))
        .or_else(|| {
            table
                .strip_suffix('s')
                .filter(|s| s.len() > 2 && !s.ends_with('s'))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("{table}_row"));
    if name == "count" {
        "region".to_owned()
    } else {
        name
    }
}

struct Dissolve;

impl MapTool for Dissolve {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "dissolve".into(),
            group: "Aggregate".into(),
            label: "Dissolve".into(),
            description: "The features with the same value of a column merged into one.".into(),
            params: vec![
                ToolParam::layer("layer", "Layer"),
                ToolParam::column("by", "By", "layer", &[]),
            ],
            module: None,
        }
    }

    fn build(&self, ctx: &ToolContext, args: &ToolArgs) -> std::result::Result<ToolBuild, String> {
        let layer = args.layer("layer")?;
        let by = args
            .text("by")
            .ok_or("pick the column the features are merged by")?;
        let (mut dataset, g, names) = over_layer(ctx, &layer)?;
        if !names.contains(&by) {
            return Err(format!(
                "`{by}` is not a column of \"{}\"",
                layer_name(ctx, &layer)
            ));
        }
        let shape = unique_name("geometry", &[by.clone(), "count".into()]);
        dataset.operations.push(Operation::new(
            "dissolve",
            Op::Aggregate(AggregateOp {
                group_by: vec![GroupKey::column(by.clone())],
                summaries: vec![
                    Summary::of(shape.clone(), SummaryFunction::Union, g),
                    Summary::count("count"),
                ],
            }),
        ));
        Ok(ToolBuild {
            name: format!("{} dissolved by {by}", layer_name(ctx, &layer)),
            dataset,
            layer: result_layer(
                GeometrySource::Column { column: shape },
                Style::Categories,
                colour_by(&by),
            ),
        })
    }
}

/// The built-in tools, in the toolbox's order.
pub fn builtin_tools() -> Vec<Arc<dyn MapTool>> {
    vec![
        Arc::new(Buffer),
        Arc::new(DistanceToNearest),
        Arc::new(WithinDistance),
        Arc::new(Join),
        Arc::new(Intersection),
        Arc::new(PerRegion { sum: false }),
        Arc::new(PerRegion { sum: true }),
        Arc::new(Dissolve),
    ]
}

// --- tools declared as data ----------------------------------------------------------

/// A tool a module declares as data: its form, the operations it adds to the
/// dataset over its `base` layer, and the layer that shows the result, with
/// the form's answers written into each text as `{{name}}`.
///
/// ```json
/// { "id": "walk", "group": "Proximity", "label": "Walking distance",
///   "params": [ { "name": "layer", "label": "Layer", "kind": "layer" },
///               { "name": "minutes", "label": "Minutes", "kind": "number", "default": 10 } ],
///   "base": "layer",
///   "operations": [ { "kind": "calculated",
///                     "params": { "name": "walk", "formula": "Geo.buffer({{geometry}}, {{minutes}} * 80)" } } ],
///   "layer": { "geometry": { "kind": "column", "column": "walk" } },
///   "name": "{{layer.name}} within {{minutes}} minutes' walk" }
/// ```
///
/// `{{geometry}}` is the base layer's geometry column in the new dataset;
/// for a layer field `p`, `{{p.name}}`, `{{p.dataset}}` and `{{p.geometry}}`
/// (its geometry formula). A text that is only `{{name}}` becomes the answer
/// itself, so a number stays a number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolTemplate {
    /// The tool's id within its module.
    pub id: String,
    /// The toolbox's group.
    pub group: String,
    /// The menu's label.
    pub label: String,
    /// What it makes.
    #[serde(default)]
    pub description: String,
    /// The form.
    #[serde(default)]
    pub params: Vec<ToolParam>,
    /// The layer field whose dataset the new one is based on.
    pub base: String,
    /// The operations added after the base layer's filter and geometry.
    #[serde(default)]
    pub operations: Vec<Json>,
    /// The result's layer: `geometry`, `encoding` and `style`; the base
    /// layer's geometry source when it has none.
    #[serde(default)]
    pub layer: Option<Json>,
    /// The new dataset's name.
    #[serde(default)]
    pub name: Option<String>,
}

/// A [`ToolTemplate`] from a module, as a tool.
pub struct TemplateTool {
    module: String,
    template: ToolTemplate,
}

impl TemplateTool {
    /// The tool `module` declares as `raw`, or the sentence saying why it is
    /// not one.
    pub fn from_json(module: &str, raw: &Json) -> std::result::Result<TemplateTool, String> {
        let template: ToolTemplate = serde_json::from_value(raw.clone())
            .map_err(|e| format!("its map tool does not read: {e}"))?;
        if template.id.trim().is_empty() || template.label.trim().is_empty() {
            return Err("a map tool needs an id and a label".to_owned());
        }
        if !template
            .params
            .iter()
            .any(|p| p.name == template.base && p.kind == ParamKind::Layer)
        {
            return Err(format!(
                "the map tool `{}` is based on `{}`, which is not one of its layer fields",
                template.id, template.base
            ));
        }
        Ok(TemplateTool {
            module: module.to_owned(),
            template,
        })
    }
}

/// Write the answers into every text of `value`.
fn fill_in(value: &Json, vars: &BTreeMap<String, Json>) -> Json {
    match value {
        Json::String(s) => {
            let trimmed = s.trim();
            if let Some(name) = trimmed
                .strip_prefix("{{")
                .and_then(|r| r.strip_suffix("}}"))
                && !name.contains("{{")
                && let Some(v) = vars.get(name.trim())
            {
                return v.clone();
            }
            let mut out = s.clone();
            for (name, v) in vars {
                let text = match v {
                    Json::String(t) => t.clone(),
                    other => other.to_string(),
                };
                out = out.replace(&format!("{{{{{name}}}}}"), &text);
            }
            Json::String(out)
        }
        Json::Array(items) => Json::Array(items.iter().map(|v| fill_in(v, vars)).collect()),
        Json::Object(map) => Json::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), fill_in(v, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

impl MapTool for TemplateTool {
    fn descriptor(&self) -> ToolDescriptor {
        let t = &self.template;
        ToolDescriptor {
            id: format!("{}:{}", self.module, t.id),
            group: t.group.clone(),
            label: t.label.clone(),
            description: t.description.clone(),
            params: t.params.clone(),
            module: Some(self.module.clone()),
        }
    }

    fn build(&self, ctx: &ToolContext, args: &ToolArgs) -> std::result::Result<ToolBuild, String> {
        let t = &self.template;
        let mut vars: BTreeMap<String, Json> = BTreeMap::new();
        for p in &t.params {
            match &p.kind {
                ParamKind::Layer => {
                    let layer = args.layer(&p.name)?;
                    vars.insert(format!("{}.name", p.name), json!(layer_name(ctx, &layer)));
                    vars.insert(format!("{}.dataset", p.name), json!(layer.dataset));
                    vars.insert(
                        format!("{}.geometry", p.name),
                        json!(layer.geometry.formula()),
                    );
                }
                _ => match args.get(&p.name).or(p.default.as_ref()) {
                    Some(v) => {
                        vars.insert(p.name.clone(), v.clone());
                    }
                    None if p.optional => {}
                    None => return Err(format!("fill in `{}`", p.label)),
                },
            }
        }
        let base = args.layer(&t.base)?;
        let (mut dataset, g, _) = over_layer(ctx, &base)?;
        vars.insert("geometry".into(), json!(g));
        for (i, raw) in t.operations.iter().enumerate() {
            let filled = fill_in(raw, &vars);
            let mut object = match filled {
                Json::Object(o) => o,
                _ => {
                    return Err(format!(
                        "operation {} of the tool is not an operation",
                        i + 1
                    ));
                }
            };
            object
                .entry("id")
                .or_insert_with(|| json!(format!("tool-{}", i + 1)));
            let op: Operation = serde_json::from_value(Json::Object(object))
                .map_err(|e| format!("operation {} of the tool does not read: {e}", i + 1))?;
            dataset.operations.push(op);
        }
        let mut layer = MapLayer::new(DatasetId::new(), base.geometry.clone());
        if let Some(raw) = &t.layer {
            let filled = fill_in(raw, &vars);
            if let Some(g) = filled.get("geometry") {
                layer.geometry = serde_json::from_value(g.clone())
                    .map_err(|e| format!("the tool's layer geometry does not read: {e}"))?;
            }
            if let Some(e) = filled.get("encoding") {
                layer.encoding = serde_json::from_value(e.clone())
                    .map_err(|e| format!("the tool's layer encoding does not read: {e}"))?;
            }
            if let Some(s) = filled.get("style") {
                layer.style = serde_json::from_value(s.clone())
                    .map_err(|e| format!("the tool's layer style does not read: {e}"))?;
            }
        }
        let name = match &t.name {
            Some(n) => match fill_in(&json!(n), &vars) {
                Json::String(s) => s,
                other => other.to_string(),
            },
            None => format!("{} ({})", layer_name(ctx, &base), t.label),
        };
        Ok(ToolBuild {
            name,
            dataset,
            layer,
        })
    }
}

// --- the registry -------------------------------------------------------------------

/// The tools plugins installed, replaced whole on every module change.
static PLUGIN_TOOLS: RwLock<Vec<Arc<dyn MapTool>>> = RwLock::new(Vec::new());

/// Install the tools the modules supply, in place of the ones they supplied
/// before. A tool whose id a built-in or an earlier tool has is left out, and
/// said so in the answer, beside each declaration that does not read.
pub fn install_plugin_tools(declared: &[(String, Json)]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut taken: Vec<String> = builtin_tools().iter().map(|t| t.descriptor().id).collect();
    let mut tools: Vec<Arc<dyn MapTool>> = Vec::new();
    for (module, raw) in declared {
        match TemplateTool::from_json(module, raw) {
            Ok(tool) => {
                let id = tool.descriptor().id;
                if taken.contains(&id) {
                    problems.push(format!(
                        "the module {module} declares the map tool `{id}` twice"
                    ));
                    continue;
                }
                taken.push(id);
                tools.push(Arc::new(tool));
            }
            Err(e) => problems.push(format!("the module {module}: {e}")),
        }
    }
    if let Ok(mut installed) = PLUGIN_TOOLS.write() {
        *installed = tools;
    }
    problems
}

/// Add a tool written in Rust, after the installed ones.
pub fn register_map_tool(tool: Arc<dyn MapTool>) {
    if let Ok(mut installed) = PLUGIN_TOOLS.write() {
        installed.push(tool);
    }
}

/// Every tool: the built-ins, then the plugins'.
pub fn all_tools() -> Vec<Arc<dyn MapTool>> {
    let mut tools = builtin_tools();
    if let Ok(installed) = PLUGIN_TOOLS.read() {
        tools.extend(installed.iter().cloned());
    }
    tools
}

/// What the toolbox lists.
pub fn tool_descriptors() -> Vec<ToolDescriptor> {
    all_tools().iter().map(|t| t.descriptor()).collect()
}

/// What running a tool made.
#[derive(Debug, Clone, Serialize)]
pub struct ToolRun {
    /// The stored dataset.
    pub dataset: DatasetDef,
    /// The layer that shows it.
    pub layer: MapLayer,
}

/// Run the tool `id` on the form's answers: its dataset is checked —
/// refused with the first operation's sentence when it does not read, so
/// nothing is stored that would show an error — named (`name`, or the tool's
/// own name made unique), stored, and answered with its layer.
pub async fn run_tool(
    catalog: &Catalog,
    id: &str,
    args: &ToolArgs,
    name: Option<&str>,
) -> Result<ToolRun> {
    let tools = all_tools();
    let tool = tools
        .iter()
        .find(|t| t.descriptor().id == id)
        .ok_or_else(|| Error::not_found(format!("there is no map tool `{id}`")))?;
    let label = tool.descriptor().label;
    let ctx = ToolContext::load(catalog).await?;
    let built = tool
        .build(&ctx, args)
        .map_err(|e| Error::invalid(format!("{label}: {e}")))?;
    let mut dataset = built.dataset;
    dataset.id = DatasetId::new();
    dataset.name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_owned(),
        None => {
            let taken: Vec<String> = ctx.library.defs().map(|d| d.name.clone()).collect();
            let base = built.name;
            if taken.contains(&base) {
                (2..)
                    .map(|n| format!("{base} {n}"))
                    .find(|c| !taken.contains(c))
                    .unwrap_or(base)
            } else {
                base
            }
        }
    };
    let mut library = ctx.library;
    library.insert(dataset.clone());
    let compiled = compile(&ctx.schema, &library, &dataset, Options::default());
    if let Some(e) = &compiled.base.error {
        return Err(Error::invalid(format!("{label}: {e}")));
    }
    if let Some((op, report)) = dataset
        .operations
        .iter()
        .zip(&compiled.operations)
        .find(|(_, r)| r.status == OpStatus::Invalid)
    {
        return Err(Error::invalid(format!(
            "{label}: its {} operation (`{}`) does not work: {}",
            report.kind,
            op.id,
            report.error.as_deref().unwrap_or_default()
        )));
    }
    sc_dataset::save_dataset(catalog, &dataset).await?;
    let mut layer = built.layer;
    layer.dataset = dataset.id;
    layer.name = Some(dataset.name.clone());
    Ok(ToolRun { dataset, layer })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toolbox_has_the_three_groups_and_unique_ids() {
        let tools = tool_descriptors();
        let groups: Vec<&str> = tools.iter().map(|t| t.group.as_str()).collect();
        for g in ["Proximity", "Overlay", "Aggregate"] {
            assert!(groups.contains(&g), "{g}");
        }
        let mut ids: Vec<&str> = tools.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), tools.len());
        // Every column field names a layer field of its own form.
        for t in &tools {
            for p in &t.params {
                if let ParamKind::Column { of, .. } = &p.kind {
                    assert!(
                        t.params
                            .iter()
                            .any(|q| &q.name == of && q.kind == ParamKind::Layer),
                        "{}: {}",
                        t.id,
                        p.name
                    );
                }
            }
        }
    }

    #[test]
    fn a_table_names_its_rows() {
        assert_eq!(singular("districts"), "district");
        assert_eq!(singular("counties"), "county");
        assert_eq!(singular("glass"), "glass_row");
        assert_eq!(singular("zone"), "zone_row");
    }

    #[test]
    fn answers_are_written_into_a_template() {
        let vars = BTreeMap::from([
            ("minutes".to_owned(), json!(10)),
            ("geometry".to_owned(), json!("location")),
            ("layer.name".to_owned(), json!("Shops")),
        ]);
        assert_eq!(fill_in(&json!("{{minutes}}"), &vars), json!(10));
        assert_eq!(
            fill_in(&json!("Geo.buffer({{geometry}}, {{minutes}} * 80)"), &vars),
            json!("Geo.buffer(location, 10 * 80)")
        );
        assert_eq!(
            fill_in(&json!({ "a": ["{{layer.name}} walk"] }), &vars),
            json!({ "a": ["Shops walk"] })
        );
        // A name nobody answered is left as it is.
        assert_eq!(fill_in(&json!("{{nope}}"), &vars), json!("{{nope}}"));
    }

    #[test]
    fn a_declared_tool_is_checked_and_installed_beside_the_built_ins() {
        let walk = json!({
            "id": "walk", "group": "Proximity", "label": "Walking distance",
            "params": [ { "name": "layer", "label": "Layer", "kind": "layer" },
                        { "name": "minutes", "label": "Minutes", "kind": "number", "default": 10 } ],
            "base": "layer",
            "operations": [ { "kind": "calculated",
                              "params": { "name": "walk", "formula": "Geo.buffer({{geometry}}, {{minutes}} * 80)" } } ],
            "layer": { "geometry": { "kind": "column", "column": "walk" } }
        });
        let bad = json!({ "id": "x", "group": "G", "label": "X", "base": "nope" });
        let problems = install_plugin_tools(&[
            ("@acme/gis".to_owned(), walk.clone()),
            ("@acme/gis".to_owned(), walk),
            ("@acme/other".to_owned(), bad),
        ]);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("twice"), "{}", problems[0]);
        assert!(
            problems[1].contains("not one of its layer fields"),
            "{}",
            problems[1]
        );
        let ids: Vec<String> = tool_descriptors().into_iter().map(|t| t.id).collect();
        assert!(ids.contains(&"@acme/gis:walk".to_owned()), "{ids:?}");
        assert_eq!(ids.first().map(String::as_str), Some("buffer"));
        install_plugin_tools(&[]);
        assert!(!tool_descriptors().iter().any(|t| t.id.contains("walk")));
    }
}

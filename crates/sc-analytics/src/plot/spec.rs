//! The plot spec (analytics TODO A2.1; the goals document's "Plots and the
//! grammar of graphics").
//!
//! A plot is a declarative spec, a subset of Vega-Lite's ideas in Feldspar's
//! own JSON, so that its stats can be computed on the server and the renderer
//! (ECharts, A2.7) can be changed without changing what is stored:
//!
//! ```json
//! { "data": { "kind": "dataset", "dataset": "…uuid…" },
//!   "layers": [
//!     { "mark": "point",
//!       "encoding": { "x": { "field": "area" }, "y": { "field": "price" },
//!                     "color": { "field": "neighbourhood" } } },
//!     { "mark": "line", "stat": { "kind": "smooth", "method": "linear" },
//!       "encoding": { "x": { "field": "area" }, "y": { "field": "price" } } } ],
//!   "scales": { "y": { "kind": "log" } },
//!   "facet": { "wrap": { "field": "year_built", "bin": {} } },
//!   "references": [ { "channel": "y", "value": 100000 } ] }
//! ```
//!
//! Every type here is the JSON a workspace stores and the API carries. What a
//! spec *means* against a dataset — whether its columns exist and suit their
//! channels — is [`validate`](super::validate)'s business, and what it
//! *draws* is [`render_plot`](super::render_plot)'s.

use std::collections::BTreeMap;

use sc_dataset::DatasetId;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// Where a plot's rows come from.
///
/// One kind now; A3 adds a fit's output data, which is read from the instance
/// rather than through SQL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DataRef {
    /// The rows of a stored dataset after its last operation.
    Dataset {
        /// The dataset's id.
        dataset: DatasetId,
    },
}

/// A plot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlotSpec {
    /// Where the rows come from.
    pub data: DataRef,
    /// Several columns compared as one variable — the explorer's "several
    /// columns on Y", an implicit Stack applied before every layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fold: Option<Fold>,
    /// What is drawn, bottom first. At least one.
    pub layers: Vec<Layer>,
    /// How a channel's values become positions, colours or sizes. Only `x`,
    /// `y`, `color` and `size` have scales.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scales: BTreeMap<Channel, Scale>,
    /// Cartesian, flipped (X drawn vertically) or polar.
    #[serde(default, skip_serializing_if = "Coord::is_cartesian")]
    pub coord: Coord,
    /// Small multiples: one plot per value of a column.
    #[serde(default, skip_serializing_if = "Facet::is_empty")]
    pub facet: Facet,
    /// Lines drawn at a fixed value of X or Y.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<Reference>,
    /// What a click or a brush on the plot selects. Declared now and checked;
    /// dashboards (A6) turn them into filters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selections: Vec<Selection>,
}

impl PlotSpec {
    /// A spec over `data` with one layer and nothing else.
    pub fn single(data: DataRef, layer: Layer) -> PlotSpec {
        PlotSpec {
            data,
            fold: None,
            layers: vec![layer],
            scales: BTreeMap::new(),
            coord: Coord::Cartesian,
            facet: Facet::default(),
            references: Vec::new(),
            selections: Vec::new(),
        }
    }
}

/// Several columns turned into two — which column a value came from, and the
/// value — before anything is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fold {
    /// The columns, at least two, all numbers.
    pub columns: Vec<String>,
    /// The name of the column holding which column a row came from.
    #[serde(default = "Fold::default_key")]
    pub key: String,
    /// The name of the column holding the value.
    #[serde(default = "Fold::default_value")]
    pub value: String,
}

impl Fold {
    fn default_key() -> String {
        "variable".to_owned()
    }

    fn default_value() -> String {
        "value".to_owned()
    }

    /// A fold of `columns` into `variable` and `value`.
    pub fn of(columns: impl IntoIterator<Item = impl Into<String>>) -> Fold {
        Fold {
            columns: columns.into_iter().map(Into::into).collect(),
            key: Fold::default_key(),
            value: Fold::default_value(),
        }
    }
}

/// A visual channel a column can be mapped to, or a facet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Horizontal position.
    X,
    /// Vertical position.
    Y,
    /// Colour.
    Color,
    /// Size of a point.
    Size,
    /// Shape of a point.
    Shape,
    /// A text label.
    Label,
    /// One row of plots per value.
    Row,
    /// One column of plots per value.
    Column,
    /// One plot per value, wrapped into rows.
    Wrap,
}

impl Channel {
    /// The channels a layer's encoding has, in order.
    pub const ENCODED: [Channel; 6] = [
        Channel::X,
        Channel::Y,
        Channel::Color,
        Channel::Size,
        Channel::Shape,
        Channel::Label,
    ];

    /// The facet channels.
    pub const FACETS: [Channel; 3] = [Channel::Row, Channel::Column, Channel::Wrap];

    /// Its name as the JSON spells it, and as a column of a layer's data is
    /// called.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::X => "x",
            Channel::Y => "y",
            Channel::Color => "color",
            Channel::Size => "size",
            Channel::Shape => "shape",
            Channel::Label => "label",
            Channel::Row => "row",
            Channel::Column => "column",
            Channel::Wrap => "wrap",
        }
    }

    /// Its name in a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            Channel::X => "X",
            Channel::Y => "Y",
            Channel::Color => "Color",
            Channel::Size => "Size",
            Channel::Shape => "Shape",
            Channel::Label => "Label",
            Channel::Row => "Facet rows",
            Channel::Column => "Facet columns",
            Channel::Wrap => "Wrap",
        }
    }

    /// Whether it places marks on an axis.
    pub fn is_positional(self) -> bool {
        matches!(self, Channel::X | Channel::Y)
    }

    /// Whether it splits the plot into small multiples.
    pub fn is_facet(self) -> bool {
        matches!(self, Channel::Row | Channel::Column | Channel::Wrap)
    }
}

/// A column mapped to a channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDef {
    /// The column's name.
    pub field: String,
    /// Bin its values into ranges first (numbers only); `{}` chooses the
    /// width by the Freedman–Diaconis rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<Bin>,
}

impl FieldDef {
    /// `field`, not binned.
    pub fn of(field: impl Into<String>) -> FieldDef {
        FieldDef {
            field: field.into(),
            bin: None,
        }
    }

    /// `field`, binned by the default rule.
    pub fn binned(field: impl Into<String>) -> FieldDef {
        FieldDef {
            field: field.into(),
            bin: Some(Bin::default()),
        }
    }
}

/// How a column is binned. With neither set, the Freedman–Diaconis rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Bin {
    /// The width of a bin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f64>,
    /// About how many bins (the width is rounded to a round number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bins: Option<u32>,
}

/// Which column each channel of a layer shows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Encoding {
    /// Horizontal position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<FieldDef>,
    /// Vertical position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<FieldDef>,
    /// Colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<FieldDef>,
    /// Size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<FieldDef>,
    /// Shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<FieldDef>,
    /// Text label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<FieldDef>,
}

impl Encoding {
    /// The column on `channel` (never a facet's: those are the spec's).
    pub fn get(&self, channel: Channel) -> Option<&FieldDef> {
        match channel {
            Channel::X => self.x.as_ref(),
            Channel::Y => self.y.as_ref(),
            Channel::Color => self.color.as_ref(),
            Channel::Size => self.size.as_ref(),
            Channel::Shape => self.shape.as_ref(),
            Channel::Label => self.label.as_ref(),
            Channel::Row | Channel::Column | Channel::Wrap => None,
        }
    }

    /// Put `field` on `channel` (facets are ignored).
    pub fn set(&mut self, channel: Channel, field: Option<FieldDef>) {
        match channel {
            Channel::X => self.x = field,
            Channel::Y => self.y = field,
            Channel::Color => self.color = field,
            Channel::Size => self.size = field,
            Channel::Shape => self.shape = field,
            Channel::Label => self.label = field,
            Channel::Row | Channel::Column | Channel::Wrap => {}
        }
    }

    /// Every channel with a column, in order.
    pub fn iter(&self) -> impl Iterator<Item = (Channel, &FieldDef)> {
        Channel::ENCODED
            .into_iter()
            .filter_map(|c| self.get(c).map(|f| (c, f)))
    }
}

/// What a layer draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    /// A point per row (or per group).
    Point,
    /// A line through the rows, in order of X.
    Line,
    /// A bar from zero.
    Bar,
    /// An area from zero.
    Area,
    /// A box plot's box and whiskers.
    Box,
    /// A band between a lower and an upper value: a confidence band.
    Band,
    /// An error bar between a lower and an upper value.
    Errorbar,
    /// A text label.
    Text,
    /// A rectangle per X and Y: a heatmap's cell.
    Rect,
}

impl Mark {
    /// Every mark, in the order the mark palette shows them.
    pub const ALL: [Mark; 9] = [
        Mark::Point,
        Mark::Line,
        Mark::Bar,
        Mark::Area,
        Mark::Box,
        Mark::Rect,
        Mark::Text,
        Mark::Band,
        Mark::Errorbar,
    ];

    /// Its name in a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            Mark::Point => "points",
            Mark::Line => "a line",
            Mark::Bar => "bars",
            Mark::Area => "an area",
            Mark::Box => "box plots",
            Mark::Band => "a band",
            Mark::Errorbar => "error bars",
            Mark::Text => "text",
            Mark::Rect => "a heatmap",
        }
    }
}

/// How a summary of a column is computed in an Aggregate stat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateFn {
    /// How many values are not missing.
    Count,
    /// The total.
    Sum,
    /// The mean.
    Mean,
    /// The median.
    Median,
    /// The smallest value.
    Min,
    /// The largest value.
    Max,
    /// The sample standard deviation.
    Sd,
}

impl AggregateFn {
    /// Its name in a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            AggregateFn::Count => "count",
            AggregateFn::Sum => "sum",
            AggregateFn::Mean => "mean",
            AggregateFn::Median => "median",
            AggregateFn::Min => "minimum",
            AggregateFn::Max => "maximum",
            AggregateFn::Sd => "standard deviation",
        }
    }
}

/// How a smoother is fitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmoothMethod {
    /// A straight line by least squares, from sums computed in SQL.
    #[default]
    Linear,
    /// Local quadratic regression, fitted in memory on a sample.
    Loess,
}

/// The transform a layer's rows go through before they are drawn. Computed on
/// the server, so the browser receives bins and summaries, not rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Stat {
    /// The rows as they are (sampled above the layer's limit).
    #[default]
    Identity,
    /// How many rows there are for each combination of the other channels'
    /// values (and bins): a bar chart of counts, a histogram, a heatmap.
    Count,
    /// A summary of one channel's column for each combination of the others'.
    Aggregate {
        /// The summary.
        function: AggregateFn,
        /// The channel whose column is summarised: Y unless said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<Channel>,
    },
    /// Quantiles of Y for each group.
    Quantiles {
        /// The probabilities, each between 0 and 1.
        probabilities: Vec<f64>,
    },
    /// A box plot's five numbers for each group: the quartiles, and whiskers
    /// to the furthest values within `coef` interquartile ranges of the box,
    /// with the values beyond them as outliers.
    Boxplot {
        /// How many interquartile ranges the whiskers reach.
        #[serde(default = "default_coef")]
        coef: f64,
    },
    /// The mean of Y for each group, with a confidence interval.
    Summary {
        /// The confidence level.
        #[serde(default = "default_level")]
        level: f64,
    },
    /// A kernel density estimate of X for each group.
    Density {
        /// The Gaussian kernel's standard deviation; by Silverman's rule of
        /// thumb (R's `bw.nrd0`) when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bandwidth: Option<f64>,
        /// A multiple of the bandwidth.
        #[serde(default = "default_adjust")]
        adjust: f64,
    },
    /// A smooth curve of Y against X for each group, with a confidence band.
    Smooth {
        /// Linear or loess.
        #[serde(default)]
        method: SmoothMethod,
        /// The share of the points each local fit of a loess uses.
        #[serde(default = "default_span")]
        span: f64,
        /// Whether to compute the confidence band.
        #[serde(default = "default_true")]
        se: bool,
        /// The confidence level of the band.
        #[serde(default = "default_level")]
        level: f64,
    },
}

fn default_coef() -> f64 {
    1.5
}

fn default_level() -> f64 {
    0.95
}

fn default_adjust() -> f64 {
    1.0
}

fn default_span() -> f64 {
    0.75
}

fn default_true() -> bool {
    true
}

impl Stat {
    /// Its name, as the JSON's `kind` spells it.
    pub fn kind(&self) -> &'static str {
        match self {
            Stat::Identity => "identity",
            Stat::Count => "count",
            Stat::Aggregate { .. } => "aggregate",
            Stat::Quantiles { .. } => "quantiles",
            Stat::Boxplot { .. } => "boxplot",
            Stat::Summary { .. } => "summary",
            Stat::Density { .. } => "density",
            Stat::Smooth { .. } => "smooth",
        }
    }

    /// Its name in a sentence.
    pub fn describe(&self) -> &'static str {
        match self {
            Stat::Identity => "the rows as they are",
            Stat::Count => "a count",
            Stat::Aggregate { .. } => "a summary",
            Stat::Quantiles { .. } => "quantiles",
            Stat::Boxplot { .. } => "a box plot",
            Stat::Summary { .. } => "a mean with a confidence interval",
            Stat::Density { .. } => "a density",
            Stat::Smooth { .. } => "a smoother",
        }
    }

    /// The default box plot.
    pub fn boxplot() -> Stat {
        Stat::Boxplot { coef: 1.5 }
    }

    /// A summary with a 95% interval.
    pub fn summary() -> Stat {
        Stat::Summary { level: 0.95 }
    }

    /// A density with the default bandwidth.
    pub fn density() -> Stat {
        Stat::Density {
            bandwidth: None,
            adjust: 1.0,
        }
    }

    /// A smoother by `method` with its defaults.
    pub fn smooth(method: SmoothMethod) -> Stat {
        Stat::Smooth {
            method,
            span: 0.75,
            se: true,
            level: 0.95,
        }
    }

    /// An aggregate of Y by `function`.
    pub fn aggregate(function: AggregateFn) -> Stat {
        Stat::Aggregate {
            function,
            channel: None,
        }
    }
}

/// One layer of a plot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    /// What is drawn.
    pub mark: Mark,
    /// Which columns the channels show.
    #[serde(default)]
    pub encoding: Encoding,
    /// What the rows go through first.
    #[serde(default, skip_serializing_if = "is_identity")]
    pub stat: Stat,
    /// The most rows a layer that draws rows shows before it samples
    /// ([`DEFAULT_SAMPLE`](super::DEFAULT_SAMPLE) when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<u64>,
}

fn is_identity(stat: &Stat) -> bool {
    *stat == Stat::Identity
}

impl Layer {
    /// A layer of `mark` with `stat`, and no columns yet.
    pub fn new(mark: Mark, stat: Stat) -> Layer {
        Layer {
            mark,
            encoding: Encoding::default(),
            stat,
            sample: None,
        }
    }

    /// The same layer with `field` on `channel`.
    pub fn with(mut self, channel: Channel, field: FieldDef) -> Layer {
        self.encoding.set(channel, Some(field));
        self
    }
}

/// How a scale maps values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleKind {
    /// In proportion.
    #[default]
    Linear,
    /// By logarithm: values of 0 or less are left out.
    Log,
    /// By square root: values below 0 are left out.
    Sqrt,
}

/// A channel's scale.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Scale {
    /// How values map.
    #[serde(default)]
    pub kind: ScaleKind,
    /// Whether a continuous position scale includes zero (the renderer's
    /// default when absent: yes for bars and areas).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zero: Option<bool>,
    /// A fixed domain instead of the data's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<Vec<Json>>,
    /// A named colour scheme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// Run the scale the other way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reverse: bool,
}

/// The coordinate system.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coord {
    /// X across, Y up.
    #[default]
    Cartesian,
    /// X up, Y across: horizontal bars and box plots.
    Flipped,
    /// X around, Y outwards.
    Polar,
}

impl Coord {
    fn is_cartesian(&self) -> bool {
        *self == Coord::Cartesian
    }
}

/// Small multiples.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Facet {
    /// One row of plots per value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<FieldDef>,
    /// One column of plots per value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<FieldDef>,
    /// One plot per value, wrapped. Not with `row` or `column`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap: Option<FieldDef>,
    /// How many plots a wrapped row holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<u32>,
}

impl Facet {
    /// Whether there are no facets.
    pub fn is_empty(&self) -> bool {
        self.row.is_none() && self.column.is_none() && self.wrap.is_none()
    }

    /// The column on facet channel `channel`.
    pub fn get(&self, channel: Channel) -> Option<&FieldDef> {
        match channel {
            Channel::Row => self.row.as_ref(),
            Channel::Column => self.column.as_ref(),
            Channel::Wrap => self.wrap.as_ref(),
            _ => None,
        }
    }

    /// Every facet channel with a column.
    pub fn iter(&self) -> impl Iterator<Item = (Channel, &FieldDef)> {
        Channel::FACETS
            .into_iter()
            .filter_map(|c| self.get(c).map(|f| (c, f)))
    }
}

/// A line at a fixed value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    /// X (a vertical line) or Y (a horizontal one).
    pub channel: Channel,
    /// Where: a number, or a category or date as the axis has it.
    pub value: Json,
    /// What the line is labelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// What a selection picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionKind {
    /// The values under a click.
    Point,
    /// A range brushed along an axis.
    Interval,
}

/// A selection a plot offers (used from A6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// Its name, unique in the spec.
    pub name: String,
    /// A click or a brush.
    pub kind: SelectionKind,
    /// The channels whose columns it filters on.
    pub channels: Vec<Channel>,
}

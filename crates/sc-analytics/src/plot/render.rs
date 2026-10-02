//! Drawing a spec (analytics TODO A2.3–A2.6): each layer's stat compiled to
//! SQL over the dataset's compiled query, run, and finished in Rust; the scale
//! domains resolved over every layer; and the sentence saying why, when a spec
//! cannot be drawn.
//!
//! **Execution.** Datasets are not materialised and tables can be large, so
//! the browser never receives rows it would only bin. Every layer is one or a
//! few queries of the same shape:
//!
//! ```text
//! data    (the dataset's last stage, unordered)                  AS _fd_d
//!         — or one SELECT per folded column, UNION ALL'd
//! points  SELECT <group keys> AS _g0…, <inputs> AS _v0… FROM data
//!         WHERE <values a log scale cannot show are left out>     AS _fd_p
//! stat    SELECT _g0…, count(*) … FROM points GROUP BY _g0…
//! ```
//!
//! Group keys are the channels that split the rows — X for a bar chart,
//! Color, the facets — and a binned channel's key is its bin number,
//! `floor((x − origin) / width)`. Percentiles (box plots, medians, the
//! interquartile range a bin width or a bandwidth needs) are taken in SQL with
//! `row_number()` over each group, on both databases alike; neither has a
//! percentile aggregate the other shares.
//!
//! What SQL cannot do is done in memory on what it returns: the kernel density
//! from fine bins (or from the values, when there are few), the loess on a
//! sample, the confidence intervals from counts, means and deviations.
//!
//! **Data comes back by channel.** A layer's data is a small table whose
//! columns are named by the channel they are drawn on — `x`, `x_end` for a
//! bin's upper edge, `y`, `y_lower` and `y_upper` for a band, `y_q1`,
//! `y_median` and `y_q3` for a box, `color`, `wrap` — so the renderer needs no
//! knowledge of the stat to place a value.

use std::collections::{BTreeMap, BTreeSet};

use sc_catalog::Catalog;
use sc_dataset::{ColType, Options, Schema, Stage, StageShape, compile, scramble, value_json};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    BinOp, CaseArm, Expr, Nulls, OrderBy, OrderDir, Projection, Select, Source, Statement, UnOp,
    Value,
};
use serde::Serialize;
use serde_json::{Value as Json, json};

use super::math::{
    self, BinParams, ColumnStats, LinearSums, bandwidth_nrd0, bin_params, density_grid, kde,
    mean_interval, quantile,
};
use super::spec::{
    AggregateFn, Cell, Channel, Coord, DataRef, Layer, Mark, PlotSpec, ScaleKind, SmoothMethod,
    Stat, TableSpec,
};
use super::validate::{Dim, LayerPlan, folded_shape, plan, validate, validate_table};

/// The rows a layer that draws rows shows before it samples.
pub const DEFAULT_SAMPLE: u64 = 10_000;
/// The most a layer may ask to show.
pub const MAX_SAMPLE: u64 = 100_000;
/// The most rows a summarising layer returns; more are cut off, and said so.
pub const MAX_GROUP_ROWS: usize = 5_000;
/// The most small multiples a facet makes.
pub const MAX_FACETS: usize = 48;
/// The most curves (densities, smoothers) a layer draws.
pub const MAX_CURVES: usize = 50;
/// The most boxes a box plot draws.
pub const MAX_BOXES: usize = 500;
/// The most outliers a box plot returns.
pub const MAX_OUTLIERS: usize = 2_000;
/// At most this many values, a density is computed from the values
/// themselves; above it, from fine bins.
pub const EXACT_DENSITY: u64 = 20_000;
/// How many fine bins a density of many values is computed from.
const DENSITY_BINS: f64 = 2_048.0;
/// The most points a loess is fitted to; more are sampled.
pub const LOESS_SAMPLE: u64 = 1_000;
/// The seed of every sample, so a plot draws the same points each time.
pub(crate) const SEED: i64 = 20_240_601;

pub(crate) const DATA: &str = "_fd_d";
pub(crate) const POINTS: &str = "_fd_p";
pub(crate) const INNER: &str = "_fd_q";

/// What `render_plot` answers: the data to draw, or why there is none.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Rendered {
    /// The plot's data.
    Plot(PlotData),
    /// The spec cannot be drawn.
    Refused {
        /// The first reason, as a sentence.
        error: String,
        /// Every reason.
        problems: Vec<String>,
    },
}

impl Rendered {
    fn refuse(error: impl Into<String>) -> Rendered {
        let error = error.into();
        Rendered::Refused {
            problems: vec![error.clone()],
            error,
        }
    }
}

/// A drawn plot's data.
#[derive(Debug, Clone, Serialize)]
pub struct PlotData {
    /// One per layer of the spec, in order.
    pub layers: Vec<LayerData>,
    /// The values each channel spans over every layer (and reference line).
    pub domains: BTreeMap<String, Domain>,
    /// The values of each facet channel, in order: one small multiple each.
    pub facets: BTreeMap<String, Vec<Json>>,
    /// Where each binned column's bins start and how wide they are.
    pub bins: BTreeMap<String, BinParams>,
    /// What the reader should know: rows a log scale left out, groups too
    /// small for a density.
    pub warnings: Vec<String>,
}

/// One layer's data.
#[derive(Debug, Clone, Serialize)]
pub struct LayerData {
    /// What to draw.
    pub mark: Mark,
    /// The stat it went through.
    pub stat: &'static str,
    /// The columns of `rows`, named by channel (see the module docs).
    pub columns: Vec<String>,
    /// The rows to draw.
    pub rows: Vec<Vec<Json>>,
    /// A box plot's outliers: the group columns and `y`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outliers: Option<Table>,
    /// Whether the rows are a sample of `total`.
    pub sampled: bool,
    /// How many rows of the dataset the layer is drawn from.
    pub total: u64,
    /// Whether there were more groups than are returned.
    pub truncated: bool,
    /// What the stat chose: a density's bandwidths, a smoother's method.
    #[serde(skip_serializing_if = "Json::is_null")]
    pub info: Json,
}

/// A small table.
#[derive(Debug, Clone, Serialize)]
pub struct Table {
    /// Its columns.
    pub columns: Vec<String>,
    /// Its rows.
    pub rows: Vec<Vec<Json>>,
}

/// The values one channel spans.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Domain {
    /// `continuous` when every value is a number, `discrete` otherwise.
    pub kind: &'static str,
    /// The smallest value (numbers, and text such as dates, which sort as
    /// they read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<Json>,
    /// The largest value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<Json>,
    /// Every value in order, when there are at most [`DOMAIN_VALUES`]; a
    /// discrete domain always lists them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<Json>>,
}

/// The most values a continuous domain lists.
pub const DOMAIN_VALUES: usize = 100;

/// Why a render stopped: a sentence for the person, or a failure.
pub(crate) enum Halt {
    Refuse(String),
    Fail(Error),
}

impl From<Error> for Halt {
    fn from(e: Error) -> Halt {
        Halt::Fail(e)
    }
}

pub(crate) type Step<T> = std::result::Result<T, Halt>;

/// Draw `spec`: every layer's data and the domains, or the sentence saying
/// why it cannot be drawn. Reads as the admin (A9 is where a restricted
/// user's reads go through their permissions).
pub async fn render_plot(catalog: &Catalog, spec: &PlotSpec) -> Result<Rendered> {
    let stage = match last_stage(catalog, &spec.data).await? {
        Ok(stage) => stage,
        Err(sentence) => return Ok(Rendered::refuse(sentence)),
    };
    let problems = validate(spec, &stage.shape());
    if let Some(first) = problems.first() {
        return Ok(Rendered::Refused {
            error: first.clone(),
            problems,
        });
    }
    let shape = folded_shape(&stage.shape(), spec.fold.as_ref())
        .map_err(|p| Error::invalid(p.join("; ")))?;
    let mut renderer = Renderer::new(catalog, spec, &stage, shape);
    match renderer.render().await {
        Ok(data) => Ok(Rendered::Plot(data)),
        Err(Halt::Refuse(sentence)) => Ok(Rendered::refuse(sentence)),
        Err(Halt::Fail(e)) => Err(e),
    }
}

/// The last stage of the dataset `data` names, or the sentence saying why it
/// does not read.
pub(crate) async fn last_stage(
    catalog: &Catalog,
    data: &DataRef,
) -> Result<std::result::Result<Stage, String>> {
    let DataRef::Dataset { dataset } = data;
    let Some(def) = sc_dataset::load_dataset(catalog, *dataset).await? else {
        return Ok(Err(
            "the dataset this plot reads is gone; pick another".to_owned()
        ));
    };
    let schema = Schema::of_catalog(catalog)?;
    let library = sc_dataset::load_library(catalog).await?;
    let compiled = compile(&schema, &library, &def, Options::default());
    Ok(match compiled.last() {
        Ok(stage) => Ok(stage.clone()),
        Err(e) => Err(format!("the dataset `{}` does not read: {e}", def.name)),
    })
}

/// The most rows a summary table's body has; more are cut off, and said so.
pub const MAX_TABLE_ROWS: usize = MAX_GROUP_ROWS;

/// What `render_table` answers: the table, or why there is none.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RenderedTable {
    /// The table's data.
    Table(Box<TableData>),
    /// The spec cannot be made into a table.
    Refused {
        /// The first reason, as a sentence.
        error: String,
        /// Every reason.
        problems: Vec<String>,
    },
}

/// A summary table's data. Each part is a small table whose columns are the
/// dimensions' values — `r0`, `r1`, … for the rows' and `c0`, … for the
/// columns', a binned one followed by its upper edge (`r0_end`) — then `n`,
/// the number of rows, and `v0`, `v1`, … one per cell.
#[derive(Debug, Clone, Serialize)]
pub struct TableData {
    /// The row dimensions' columns, outermost first.
    pub rows: Vec<String>,
    /// The column dimensions' columns, outermost first.
    pub columns: Vec<String>,
    /// What each cell shows, in words ("mean of price").
    pub cells: Vec<String>,
    /// One row per combination of the row and column dimensions' values.
    pub body: Table,
    /// One row per combination of the row dimensions' values, over every
    /// column: the Total column. With row and column dimensions and totals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_totals: Option<Table>,
    /// One row per combination of the column dimensions' values: the Total
    /// row. With row and column dimensions and totals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column_totals: Option<Table>,
    /// The whole table's cells: the corner. With any dimension and totals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grand_total: Option<Table>,
    /// Where each binned column's bins start and how wide they are.
    pub bins: BTreeMap<String, BinParams>,
    /// How many rows of the dataset the table summarises.
    pub total: u64,
    /// Whether the body had more rows than are returned.
    pub truncated: bool,
}

/// Make the summary table `spec` describes: its body and totals, or the
/// sentence saying why it cannot. Reads as the admin, as `render_plot` does.
pub async fn render_table(catalog: &Catalog, spec: &TableSpec) -> Result<RenderedTable> {
    let refuse = |error: String| RenderedTable::Refused {
        problems: vec![error.clone()],
        error,
    };
    let stage = match last_stage(catalog, &spec.data).await? {
        Ok(stage) => stage,
        Err(sentence) => return Ok(refuse(sentence)),
    };
    let problems = validate_table(spec, &stage.shape());
    if let Some(first) = problems.first() {
        return Ok(RenderedTable::Refused {
            error: first.clone(),
            problems,
        });
    }
    let shape = folded_shape(&stage.shape(), spec.fold.as_ref())
        .map_err(|p| Error::invalid(p.join("; ")))?;
    // The plot machinery over a spec with no layers: the fold is its data.
    let mut carrier = PlotSpec::single(spec.data.clone(), Layer::new(Mark::Text, Stat::Count));
    carrier.layers.clear();
    carrier.fold = spec.fold.clone();
    let mut renderer = Renderer::new(catalog, &carrier, &stage, shape);
    match renderer.table(spec).await {
        Ok(data) => Ok(RenderedTable::Table(Box::new(data))),
        Err(Halt::Refuse(sentence)) => Ok(refuse(sentence)),
        Err(Halt::Fail(e)) => Err(e),
    }
}

/// A group key as SQL: the column, or its bin number.
#[derive(Clone)]
pub(crate) struct Key {
    pub(crate) channel: Channel,
    pub(crate) expr: Expr,
    pub(crate) ty: ColType,
    pub(crate) bin: Option<BinParams>,
}

pub(crate) struct Renderer<'a> {
    catalog: &'a Catalog,
    spec: &'a PlotSpec,
    stage: &'a Stage,
    /// The columns the layers read (after the fold).
    shape: StageShape,
    /// The bins of each binned column, worked out once per render.
    bins: BTreeMap<String, BinParams>,
    warnings: Vec<String>,
    warned: BTreeSet<(String, &'static str)>,
}

impl<'a> Renderer<'a> {
    pub(crate) fn new(
        catalog: &'a Catalog,
        spec: &'a PlotSpec,
        stage: &'a Stage,
        shape: StageShape,
    ) -> Renderer<'a> {
        Renderer {
            catalog,
            spec,
            stage,
            shape,
            bins: BTreeMap::new(),
            warnings: Vec::new(),
            warned: BTreeSet::new(),
        }
    }

    async fn render(&mut self) -> Step<PlotData> {
        let mut layers = Vec::with_capacity(self.spec.layers.len());
        for layer in &self.spec.layers {
            let plan = plan(layer, &self.spec.facet, &self.shape)
                .map_err(|p| Halt::Refuse(p.join("; ")))?;
            layers.push(self.layer(layer, &plan).await?);
        }
        let facets = self.facets(&layers)?;
        let domains = self.domains(&layers);
        Ok(PlotData {
            layers,
            domains,
            facets,
            bins: std::mem::take(&mut self.bins),
            warnings: std::mem::take(&mut self.warnings),
        })
    }

    async fn layer(&mut self, layer: &Layer, plan: &LayerPlan) -> Step<LayerData> {
        if self.spec.coord == Coord::Parallel {
            let mut data = self.parallel(layer, plan).await?;
            data.mark = layer.mark;
            data.stat = layer.stat.kind();
            return Ok(data);
        }
        let filters = self.scale_filters(layer, plan).await?;
        let mut keys = Vec::with_capacity(plan.dims.len());
        for dim in &plan.dims {
            keys.push(self.key(dim).await?);
        }
        let mut data = match &layer.stat {
            Stat::Identity => self.identity(layer, &keys, filters).await?,
            Stat::Count => self.count(plan, &keys, filters).await?,
            Stat::Aggregate { function, .. } => {
                self.aggregate(plan, *function, &keys, filters).await?
            }
            Stat::Quantiles { probabilities } => {
                self.quantiles(plan, probabilities, &keys, filters).await?
            }
            Stat::Boxplot { coef } => self.boxplot(plan, *coef, &keys, filters).await?,
            Stat::Summary { level } => self.summary(plan, *level, &keys, filters).await?,
            Stat::Density { bandwidth, adjust } => {
                self.density(plan, *bandwidth, *adjust, &keys, filters)
                    .await?
            }
            Stat::Smooth {
                method,
                span,
                se,
                level,
            } => {
                self.smooth(plan, *method, *span, *se, *level, &keys, filters)
                    .await?
            }
            Stat::Correlation { x, y } => self.correlation(plan, x, y, &keys, filters).await?,
        };
        data.mark = layer.mark;
        data.stat = layer.stat.kind();
        Ok(data)
    }

    // --- the source, the points, the keys ---------------------------------

    /// The rows every layer reads: the dataset's last stage, folded.
    fn data(&self) -> Step<Source> {
        let rows = self.stage.unordered_query().map_err(Halt::Refuse)?;
        let Some(fold) = &self.spec.fold else {
            return Ok(Source::subquery(rows, DATA));
        };
        let kept: Vec<String> = self
            .stage
            .shape()
            .columns
            .iter()
            .filter(|c| !fold.columns.contains(&c.name))
            .map(|c| c.name.clone())
            .collect();
        let kept_columns = || -> Vec<Projection> {
            kept.iter()
                .map(|k| Projection::expr_as(Expr::qcol("_fd_s", k.clone()), k.clone()))
                .collect()
        };
        let name = |c: &str| cast(Expr::lit(Value::Text(c.to_owned())), "text");
        let value = |c: &str| cast(Expr::qcol("_fd_s", c.to_owned()), "double precision");
        let parts = match fold.pairs {
            None => fold
                .columns
                .iter()
                .map(|folded| {
                    let mut columns = kept_columns();
                    columns.push(Projection::expr_as(name(folded), fold.key.trim()));
                    columns.push(Projection::expr_as(value(folded), fold.value.trim()));
                    Select::from(Source::subquery(rows.clone(), "_fd_s")).columns(columns)
                })
                .collect(),
            // One SELECT per pair, the first column of the pair on `_x`.
            Some(pairs) => {
                let names = fold.pair_names();
                let mut parts = Vec::new();
                for (i, a) in fold.columns.iter().enumerate() {
                    for (j, b) in fold.columns.iter().enumerate() {
                        if i == j && !pairs.diagonal {
                            continue;
                        }
                        let mut columns = kept_columns();
                        columns.push(Projection::expr_as(name(a), names[0].clone()));
                        columns.push(Projection::expr_as(value(a), names[1].clone()));
                        columns.push(Projection::expr_as(name(b), names[2].clone()));
                        columns.push(Projection::expr_as(value(b), names[3].clone()));
                        parts.push(
                            Select::from(Source::subquery(rows.clone(), "_fd_s")).columns(columns),
                        );
                    }
                }
                parts
            }
        };
        Ok(Source::union_all(parts, DATA))
    }

    pub(crate) fn field(&self, name: &str) -> Expr {
        Expr::qcol(DATA, name)
    }

    /// The points a stat reads: the group keys as `_g0…`, the inputs as
    /// `_v0…`, and only rows the scales can show.
    pub(crate) fn points(&self, keys: &[Key], inputs: Vec<Expr>, filters: &[Expr]) -> Step<Select> {
        let mut columns: Vec<Projection> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| Projection::expr_as(k.expr.clone(), g(i)))
            .collect();
        columns.extend(
            inputs
                .into_iter()
                .enumerate()
                .map(|(i, e)| Projection::expr_as(e, v(i))),
        );
        let mut select = Select::from(self.data()?).columns(columns);
        select.filter = filters.iter().cloned().reduce(Expr::and);
        Ok(select)
    }

    /// The SQL a group key is: the column, or the number of its bin.
    pub(crate) async fn key(&mut self, dim: &Dim) -> Step<Key> {
        let column = self.field(&dim.field);
        let bin = match &dim.bin {
            None => None,
            Some(rule) => Some(self.bin_of(&dim.field, dim.ty, rule).await?),
        };
        let expr = match bin {
            None => column,
            Some(b) => Expr::Func {
                name: "floor".into(),
                args: vec![Expr::binary(
                    BinOp::Div,
                    Expr::binary(BinOp::Sub, column, num(b.origin)),
                    num(b.width),
                )],
            },
        };
        Ok(Key {
            channel: dim.channel,
            expr,
            ty: dim.ty,
            bin,
        })
    }

    /// The bins of `field`: the same for every layer and facet of the plot,
    /// so their bars line up.
    async fn bin_of(
        &mut self,
        field: &str,
        ty: ColType,
        rule: &super::spec::Bin,
    ) -> Step<BinParams> {
        if let Some(b) = self.bins.get(field) {
            return Ok(*b);
        }
        let points = Select::from(self.data()?)
            .columns(vec![Projection::expr_as(
                cast(self.field(field), "double precision"),
                v(0),
            )])
            .filter(Expr::unary(UnOp::IsNotNull, self.field(field)));
        let stats = self.percentiles(points, 0, &[0.25, 0.75], 1).await?;
        let Some(s) = stats.into_iter().next() else {
            // Nothing to bin: any bins will do.
            let b = BinParams {
                origin: 0.0,
                width: rule.width.unwrap_or(1.0),
            };
            self.bins.insert(field.to_owned(), b);
            return Ok(b);
        };
        let b = bin_params(
            &ColumnStats {
                n: s.n,
                min: s.min,
                max: s.max,
                q1: s.q[0],
                q3: s.q[1],
            },
            rule.width,
            rule.bins,
            ty == ColType::Int,
        );
        self.bins.insert(field.to_owned(), b);
        Ok(b)
    }

    /// The conditions leaving out what a log or square-root scale cannot show,
    /// and a warning for each column that loses rows to one.
    async fn scale_filters(&mut self, layer: &Layer, plan: &LayerPlan) -> Step<Vec<Expr>> {
        let mut filters = Vec::new();
        for (channel, scale) in &self.spec.scales {
            if scale.kind == ScaleKind::Linear || plan.out == Some(*channel) {
                continue;
            }
            if matches!(layer.stat, Stat::Density { .. }) && *channel == Channel::Y {
                continue;
            }
            let Some(f) = layer.encoding.get(*channel) else {
                continue;
            };
            let column = self.field(&f.field);
            let (op, bad, words) = match scale.kind {
                ScaleKind::Log => (BinOp::Gt, BinOp::Le, "of 0 or less"),
                _ => (BinOp::Ge, BinOp::Lt, "below 0"),
            };
            filters.push(Expr::binary(op, column.clone(), num(0.0)));
            let kind = if scale.kind == ScaleKind::Log {
                "log"
            } else {
                "square-root"
            };
            if self.warned.insert((f.field.clone(), kind)) {
                let mut count = Select::from(self.data()?)
                    .columns(vec![Projection::expr_as(agg("count", vec![]), "n")]);
                count.filter = Some(Expr::binary(bad, column, num(0.0)));
                let left_out = self.scalar_count(count).await?;
                if left_out > 0 {
                    self.warnings.push(format!(
                        "{left_out} {} with `{}` {words} {} left out: a {kind} scale on {} cannot show {}",
                        rows_word(left_out),
                        f.field,
                        if left_out == 1 { "is" } else { "are" },
                        channel.describe(),
                        if left_out == 1 { "it" } else { "them" }
                    ));
                }
            }
        }
        Ok(filters)
    }

    // --- the stats ----------------------------------------------------------

    /// The rows themselves, sampled above the layer's limit.
    async fn identity(
        &mut self,
        layer: &Layer,
        keys: &[Key],
        filters: Vec<Expr>,
    ) -> Step<LayerData> {
        let points = self.points(keys, Vec::new(), &filters)?;
        let total = self.count_rows(points.clone()).await?;
        let limit = layer.sample.unwrap_or(DEFAULT_SAMPLE).clamp(1, MAX_SAMPLE);
        let sampled = total > limit;
        let select = if sampled {
            sample(points, keys.len(), limit)
        } else {
            let mut s = Select::from(Source::subquery(points, POINTS)).columns(
                (0..keys.len())
                    .map(|i| Projection::expr_as(Expr::qcol(POINTS, g(i)), g(i)))
                    .collect(),
            );
            s.order = group_order(POINTS, keys.len());
            s
        };
        let rows = self.run(select).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let values = row.into_values();
            out.push(key_values(keys, &values));
        }
        // A line joins its points in order of X.
        if matches!(layer.mark, Mark::Line | Mark::Area)
            && let Some(x) = keys.iter().position(|k| k.channel == Channel::X)
        {
            let x = key_column_index(keys, x);
            out.sort_by(|a, b| compare_json(&a[x], &b[x]));
        }
        Ok(LayerData {
            columns: key_columns(keys),
            rows: out,
            sampled,
            total,
            ..LayerData::empty()
        })
    }

    async fn count(
        &mut self,
        plan: &LayerPlan,
        keys: &[Key],
        filters: Vec<Expr>,
    ) -> Step<LayerData> {
        let points = self.points(keys, Vec::new(), &filters)?;
        let total = self.count_rows(points.clone()).await?;
        let mut columns = group_projections(keys.len());
        columns.push(Projection::expr_as(agg("count", vec![]), "_n"));
        let select = grouped(points, columns, keys.len(), MAX_GROUP_ROWS + 1);
        let out = plan.out.unwrap_or(Channel::Y);
        self.grouped_layer(keys, select, total, &[out.as_str()], |values| {
            vec![value_json(&values[0])]
        })
        .await
    }

    async fn aggregate(
        &mut self,
        plan: &LayerPlan,
        function: AggregateFn,
        keys: &[Key],
        filters: Vec<Expr>,
    ) -> Step<LayerData> {
        let out = plan.out.unwrap_or(Channel::Y);
        let input = plan
            .inputs
            .first()
            .map(|i| i.field.clone())
            .unwrap_or_default();
        let numeric = matches!(
            function,
            AggregateFn::Sum | AggregateFn::Mean | AggregateFn::Median | AggregateFn::Sd
        );
        let value = if numeric {
            cast(self.field(&input), "double precision")
        } else {
            self.field(&input)
        };
        let points = self.points(keys, vec![value], &filters)?;
        let total = self.count_rows(points.clone()).await?;
        if function == AggregateFn::Median {
            let groups = self
                .percentiles(points, keys.len(), &[0.5], MAX_GROUP_ROWS + 1)
                .await?;
            let truncated = groups.len() > MAX_GROUP_ROWS;
            let rows = groups
                .iter()
                .take(MAX_GROUP_ROWS)
                .map(|q| {
                    let mut row = key_values(keys, &q.keys);
                    row.push(json_num(q.q[0]));
                    row
                })
                .collect();
            let mut columns = key_columns(keys);
            columns.push(out.as_str().to_owned());
            return Ok(LayerData {
                columns,
                rows,
                total,
                truncated,
                ..LayerData::empty()
            });
        }
        let func = match function {
            AggregateFn::Count => "count",
            AggregateFn::Sum => "sum",
            AggregateFn::Mean => "avg",
            AggregateFn::Min => "min",
            AggregateFn::Max => "max",
            AggregateFn::Sd => "stddev_samp",
            AggregateFn::Median => unreachable!("handled above"),
        };
        let mut columns = group_projections(keys.len());
        columns.push(Projection::expr_as(
            agg(func, vec![Expr::qcol(POINTS, v(0))]),
            "_a",
        ));
        let select = grouped(points, columns, keys.len(), MAX_GROUP_ROWS + 1);
        let input_ty = self.shape.column(&input).map_or(ColType::Unknown, |c| c.ty);
        self.grouped_layer(keys, select, total, &[out.as_str()], move |values| {
            let v = &values[0];
            vec![match (function, input_ty) {
                (AggregateFn::Min | AggregateFn::Max, ColType::Bool) => match v {
                    Value::Int(n) => json!(*n != 0),
                    other => value_json(other),
                },
                _ => value_json(v),
            }]
        })
        .await
    }

    async fn quantiles(
        &mut self,
        plan: &LayerPlan,
        probabilities: &[f64],
        keys: &[Key],
        filters: Vec<Expr>,
    ) -> Step<LayerData> {
        let input = &plan.inputs[0].field;
        let points = self.points(
            keys,
            vec![cast(self.field(input), "double precision")],
            &filters,
        )?;
        let total = self.count_rows(points.clone()).await?;
        let groups = self
            .percentiles(points, keys.len(), probabilities, MAX_GROUP_ROWS + 1)
            .await?;
        let mut columns = key_columns(keys);
        columns.push("n".to_owned());
        columns.extend(probabilities.iter().map(|p| format!("y_p{}", percent(*p))));
        Ok(LayerData {
            columns,
            truncated: groups.len() > MAX_GROUP_ROWS,
            rows: groups
                .iter()
                .take(MAX_GROUP_ROWS)
                .map(|q| {
                    let mut row = key_values(keys, &q.keys);
                    row.push(json!(q.n));
                    row.extend(q.q.iter().map(|x| json_num(*x)));
                    row
                })
                .collect(),
            total,
            ..LayerData::empty()
        })
    }

    async fn boxplot(
        &mut self,
        plan: &LayerPlan,
        coef: f64,
        keys: &[Key],
        filters: Vec<Expr>,
    ) -> Step<LayerData> {
        let input = &plan.inputs[0].field;
        let points = self.points(
            keys,
            vec![cast(self.field(input), "double precision")],
            &filters,
        )?;
        let groups = self
            .percentiles(
                points.clone(),
                keys.len(),
                &[0.25, 0.5, 0.75],
                MAX_BOXES + 1,
            )
            .await?;
        if groups.len() > MAX_BOXES {
            return Err(Halt::Refuse(format!(
                "the box plot would have more than {MAX_BOXES} boxes, which is more than can be read; \
                 put fewer values on X or Color, or bin them"
            )));
        }
        let total: u64 = groups.iter().map(|q| q.n).sum();
        // The fences, and which groups have values beyond them.
        let fences: Vec<(f64, f64)> = groups
            .iter()
            .map(|q| {
                let iqr = q.q[2] - q.q[0];
                (q.q[0] - coef * iqr, q.q[2] + coef * iqr)
            })
            .collect();
        let beyond: Vec<usize> = (0..groups.len())
            .filter(|&i| groups[i].min < fences[i].0 || groups[i].max > fences[i].1)
            .collect();
        let mut whiskers: Vec<(f64, f64)> = groups.iter().map(|q| (q.min, q.max)).collect();
        let mut outliers = Table {
            columns: {
                let mut c = key_columns(keys);
                c.push("y".to_owned());
                c
            },
            rows: Vec::new(),
        };
        let mut truncated = false;
        if !beyond.is_empty() {
            // The fence of each group with outliers, as a CASE over its keys
            // (NULL for the others).
            let value = Expr::qcol(POINTS, v(0));
            let fence = |which: fn(&(f64, f64)) -> f64| Expr::Case {
                operand: None,
                arms: beyond
                    .iter()
                    .map(|&i| CaseArm {
                        when: group_match(keys.len(), &groups[i].keys),
                        then: num(which(&fences[i])),
                    })
                    .collect(),
                else_result: None,
            };
            let (lo, hi) = (fence(|f| f.0), fence(|f| f.1));
            let mut columns = group_projections(keys.len());
            columns.push(Projection::expr_as(
                agg(
                    "min",
                    vec![case_when(
                        Expr::binary(BinOp::Ge, value.clone(), lo.clone()),
                        value.clone(),
                    )],
                ),
                "_lo",
            ));
            columns.push(Projection::expr_as(
                agg(
                    "max",
                    vec![case_when(
                        Expr::binary(BinOp::Le, value.clone(), hi.clone()),
                        value.clone(),
                    )],
                ),
                "_hi",
            ));
            let mut select =
                Select::from(Source::subquery(points.clone(), POINTS)).columns(columns);
            select.filter = Some(Expr::unary(UnOp::IsNotNull, lo.clone()));
            select.group = group_exprs(POINTS, keys.len());
            for row in self.run(select).await? {
                let values = row.into_values();
                let (k, rest) = values.split_at(keys.len());
                if let Some(i) = groups.iter().position(|q| same_keys(&q.keys, k)) {
                    whiskers[i] = (
                        f64_of(&rest[0]).unwrap_or(groups[i].min),
                        f64_of(&rest[1]).unwrap_or(groups[i].max),
                    );
                }
            }
            let mut columns = group_projections(keys.len());
            columns.push(Projection::expr_as(value.clone(), "_y"));
            let mut select = Select::from(Source::subquery(points, POINTS)).columns(columns);
            select.filter = Some(Expr::binary(
                BinOp::Or,
                Expr::binary(BinOp::Lt, value.clone(), lo),
                Expr::binary(BinOp::Gt, value.clone(), hi),
            ));
            select.order = group_order(POINTS, keys.len());
            select.order.push(OrderBy::asc(value));
            select.limit = Some(MAX_OUTLIERS as u64 + 1);
            let rows = self.run(select).await?;
            truncated = rows.len() > MAX_OUTLIERS;
            for row in rows.into_iter().take(MAX_OUTLIERS) {
                let values = row.into_values();
                let (k, rest) = values.split_at(keys.len());
                let mut out = key_values(keys, k);
                out.push(value_json(&rest[0]));
                outliers.rows.push(out);
            }
        }
        let mut columns = key_columns(keys);
        columns.extend(
            ["n", "y_lower", "y_q1", "y_median", "y_q3", "y_upper"]
                .into_iter()
                .map(str::to_owned),
        );
        let rows = groups
            .iter()
            .zip(&whiskers)
            .map(|(q, (lo, hi))| {
                let mut row = key_values(keys, &q.keys);
                row.push(json!(q.n));
                row.extend([*lo, q.q[0], q.q[1], q.q[2], *hi].map(json_num));
                row
            })
            .collect();
        Ok(LayerData {
            columns,
            rows,
            outliers: Some(outliers),
            total,
            truncated,
            ..LayerData::empty()
        })
    }

    async fn summary(
        &mut self,
        plan: &LayerPlan,
        level: f64,
        keys: &[Key],
        filters: Vec<Expr>,
    ) -> Step<LayerData> {
        let input = &plan.inputs[0].field;
        let points = self.points(
            keys,
            vec![cast(self.field(input), "double precision")],
            &filters,
        )?;
        let total = self.count_rows(points.clone()).await?;
        let value = Expr::qcol(POINTS, v(0));
        let mut columns = group_projections(keys.len());
        columns.push(Projection::expr_as(agg("count", vec![value.clone()]), "_n"));
        columns.push(Projection::expr_as(agg("avg", vec![value.clone()]), "_m"));
        columns.push(Projection::expr_as(agg("stddev_samp", vec![value]), "_s"));
        let select = grouped(points, columns, keys.len(), MAX_GROUP_ROWS + 1);
        self.grouped_layer(
            keys,
            select,
            total,
            &["n", "y", "y_lower", "y_upper"],
            move |values| {
                let n = f64_of(&values[0]).unwrap_or(0.0) as u64;
                let mean = f64_of(&values[1]);
                let interval = mean.and_then(|m| mean_interval(n, m, f64_of(&values[2]), level));
                vec![
                    json!(n),
                    mean.map_or(Json::Null, json_num),
                    interval.map_or(Json::Null, |(lo, _)| json_num(lo)),
                    interval.map_or(Json::Null, |(_, hi)| json_num(hi)),
                ]
            },
        )
        .await
    }

    async fn density(
        &mut self,
        plan: &LayerPlan,
        bandwidth: Option<f64>,
        adjust: f64,
        keys: &[Key],
        filters: Vec<Expr>,
    ) -> Step<LayerData> {
        let input = &plan.inputs[0].field;
        let points = self.points(
            keys,
            vec![cast(self.field(input), "double precision")],
            &filters,
        )?;
        let groups = self
            .percentiles(points.clone(), keys.len(), &[0.25, 0.75], MAX_CURVES + 1)
            .await?;
        if groups.len() > MAX_CURVES {
            return Err(Halt::Refuse(too_many_curves("densities")));
        }
        let total: u64 = groups.iter().map(|q| q.n).sum();
        // Each group's weighted points: the values themselves when there are
        // few, else the counts of fine bins at their centres.
        let mut weighted: Vec<Vec<(f64, f64)>> = vec![Vec::new(); groups.len()];
        let value = Expr::qcol(POINTS, v(0));
        let lo = groups.iter().map(|q| q.min).fold(f64::INFINITY, f64::min);
        let hi = groups
            .iter()
            .map(|q| q.max)
            .fold(f64::NEG_INFINITY, f64::max);
        let fine = (hi - lo) / DENSITY_BINS;
        if total <= EXACT_DENSITY || !math::positive(fine) {
            let mut columns = group_projections(keys.len());
            columns.push(Projection::expr_as(value.clone(), "_x"));
            let mut select = Select::from(Source::subquery(points, POINTS)).columns(columns);
            select.filter = Some(Expr::unary(UnOp::IsNotNull, value));
            select.limit = Some(EXACT_DENSITY.max(1) + 1);
            if total > EXACT_DENSITY {
                // Every value the same: one weighted point per group.
                for (i, q) in groups.iter().enumerate() {
                    weighted[i].push((q.min, q.n as f64));
                }
            } else {
                for row in self.run(select).await? {
                    let values = row.into_values();
                    let (k, rest) = values.split_at(keys.len());
                    if let (Some(i), Some(x)) = (
                        groups.iter().position(|q| same_keys(&q.keys, k)),
                        f64_of(&rest[0]),
                    ) {
                        weighted[i].push((x, 1.0));
                    }
                }
            }
        } else {
            let bin = Expr::Func {
                name: "floor".into(),
                args: vec![Expr::binary(
                    BinOp::Div,
                    Expr::binary(BinOp::Sub, value.clone(), num(lo)),
                    num(fine),
                )],
            };
            let mut columns = group_projections(keys.len());
            columns.push(Projection::expr_as(bin.clone(), "_b"));
            columns.push(Projection::expr_as(agg("count", vec![]), "_c"));
            let mut select = Select::from(Source::subquery(points, POINTS)).columns(columns);
            select.filter = Some(Expr::unary(UnOp::IsNotNull, value));
            select.group = group_exprs(POINTS, keys.len());
            select.group.push(bin);
            for row in self.run(select).await? {
                let values = row.into_values();
                let (k, rest) = values.split_at(keys.len());
                if let (Some(i), Some(b), Some(c)) = (
                    groups.iter().position(|q| same_keys(&q.keys, k)),
                    f64_of(&rest[0]),
                    f64_of(&rest[1]),
                ) {
                    let b = b.min(DENSITY_BINS - 1.0);
                    weighted[i].push((lo + (b + 0.5) * fine, c));
                }
            }
        }
        let mut columns = key_columns(keys);
        columns.push("x".to_owned());
        columns.push("y".to_owned());
        let mut rows = Vec::new();
        let mut bandwidths = Vec::new();
        for (q, pts) in groups.iter().zip(&weighted) {
            if q.n < 2 {
                self.warnings.push(format!(
                    "a density needs at least two values, so a group with {} is not drawn",
                    q.n
                ));
                bandwidths.push(Json::Null);
                continue;
            }
            let bw = bandwidth.unwrap_or_else(|| {
                bandwidth_nrd0(q.n, q.sd.unwrap_or(0.0), q.q[1] - q.q[0], q.min)
            }) * adjust;
            bandwidths.push(json_num(bw));
            let grid = density_grid(q.min, q.max, bw);
            let density = kde(pts, bw, &grid);
            let key = key_values(keys, &q.keys);
            for (x, y) in grid.into_iter().zip(density) {
                let mut row = key.clone();
                row.push(json_num(x));
                row.push(json_num(y));
                rows.push(row);
            }
        }
        Ok(LayerData {
            columns,
            rows,
            total,
            info: json!({ "bandwidths": bandwidths, "exact": total <= EXACT_DENSITY }),
            ..LayerData::empty()
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn smooth(
        &mut self,
        plan: &LayerPlan,
        method: SmoothMethod,
        span: f64,
        se: bool,
        level: f64,
        keys: &[Key],
        filters: Vec<Expr>,
    ) -> Step<LayerData> {
        let (fx, fy) = (&plan.inputs[0].field, &plan.inputs[1].field);
        let points = self.points(
            keys,
            vec![
                cast(self.field(fx), "double precision"),
                cast(self.field(fy), "double precision"),
            ],
            &filters,
        )?;
        let (x, y) = (Expr::qcol(POINTS, v(0)), Expr::qcol(POINTS, v(1)));
        let both =
            Expr::unary(UnOp::IsNotNull, x.clone()).and(Expr::unary(UnOp::IsNotNull, y.clone()));
        let mut columns = key_columns(keys);
        columns.extend(
            ["x", "y", "y_lower", "y_upper"]
                .into_iter()
                .map(str::to_owned),
        );
        let curve_rows = |key: &[Json], curve: Vec<math::CurvePoint>| -> Vec<Vec<Json>> {
            curve
                .into_iter()
                .map(|p| {
                    let mut row = key.to_vec();
                    row.push(json_num(p.x));
                    row.push(json_num(p.y));
                    row.push(p.band.map_or(Json::Null, |b| json_num(b.0)));
                    row.push(p.band.map_or(Json::Null, |b| json_num(b.1)));
                    row
                })
                .collect()
        };
        match method {
            SmoothMethod::Linear => {
                let rows = self.linear_sums(points, keys.len()).await?;
                if rows.len() > MAX_CURVES {
                    return Err(Halt::Refuse(too_many_curves("smoothers")));
                }
                let mut out = Vec::new();
                let mut total = 0;
                for (k, sums) in rows {
                    total += sums.n;
                    if let Some(curve) = math::linear_curve(&sums, se, level) {
                        out.extend(curve_rows(&key_values(keys, &k), curve));
                    }
                }
                Ok(LayerData {
                    columns,
                    rows: out,
                    total,
                    info: json!({ "method": "linear" }),
                    ..LayerData::empty()
                })
            }
            SmoothMethod::Loess => {
                let mut wanted = group_projections(keys.len());
                wanted.push(Projection::expr_as(x.clone(), v(0)));
                wanted.push(Projection::expr_as(y.clone(), v(1)));
                let mut filtered = Select::from(Source::subquery(points, POINTS)).columns(wanted);
                filtered.filter = Some(both);
                let total = self.count_rows(filtered.clone()).await?;
                let sampled = total > LOESS_SAMPLE;
                let select = if sampled {
                    sample(filtered, keys.len() + 2, LOESS_SAMPLE)
                } else {
                    Select::from(Source::subquery(filtered, INNER)).columns(
                        (0..keys.len())
                            .map(g)
                            .chain([v(0), v(1)])
                            .map(|c| Projection::expr_as(Expr::qcol(INNER, c.clone()), c))
                            .collect(),
                    )
                };
                let mut groups: Vec<(Vec<Value>, Vec<f64>, Vec<f64>)> = Vec::new();
                for row in self.run(select).await? {
                    let values = row.into_values();
                    let (k, rest) = values.split_at(keys.len());
                    let (Some(px), Some(py)) = (f64_of(&rest[0]), f64_of(&rest[1])) else {
                        continue;
                    };
                    match groups.iter_mut().find(|(gk, _, _)| same_keys(gk, k)) {
                        Some((_, xs, ys)) => {
                            xs.push(px);
                            ys.push(py);
                        }
                        None => groups.push((k.to_vec(), vec![px], vec![py])),
                    }
                }
                if groups.len() > MAX_CURVES {
                    return Err(Halt::Refuse(too_many_curves("smoothers")));
                }
                groups.sort_by(|a, b| compare_keys(&a.0, &b.0));
                let mut out = Vec::new();
                for (k, xs, ys) in &groups {
                    if let Some(curve) = math::loess_curve(xs, ys, span, se, level) {
                        out.extend(curve_rows(&key_values(keys, k), curve));
                    }
                }
                Ok(LayerData {
                    columns,
                    rows: out,
                    total,
                    sampled,
                    info: json!({ "method": "loess", "span": span }),
                    ..LayerData::empty()
                })
            }
        }
    }

    /// Pearson's correlation of two columns for each group, from the same
    /// sums as a linear smoother.
    async fn correlation(
        &mut self,
        plan: &LayerPlan,
        x: &str,
        y: &str,
        keys: &[Key],
        filters: Vec<Expr>,
    ) -> Step<LayerData> {
        let points = self.points(
            keys,
            vec![
                cast(self.field(x), "double precision"),
                cast(self.field(y), "double precision"),
            ],
            &filters,
        )?;
        let groups = self.linear_sums(points, keys.len()).await?;
        let truncated = groups.len() > MAX_GROUP_ROWS;
        let mut total = 0;
        let mut rows = Vec::new();
        for (k, sums) in groups.into_iter().take(MAX_GROUP_ROWS) {
            total += sums.n;
            let r = (sums.n >= 2 && math::positive(sums.sxx) && math::positive(sums.syy))
                .then(|| (sums.sxy / (sums.sxx * sums.syy).sqrt()).clamp(-1.0, 1.0));
            let mut row = key_values(keys, &k);
            row.push(r.map_or(Json::Null, json_num));
            row.push(json!(sums.n));
            rows.push(row);
        }
        let mut columns = key_columns(keys);
        columns.push(plan.out.unwrap_or(Channel::Color).as_str().to_owned());
        columns.push("n".to_owned());
        Ok(LayerData {
            columns,
            rows,
            total,
            truncated,
            info: json!({ "method": "pearson" }),
            ..LayerData::empty()
        })
    }

    /// Parallel coordinates: the rows themselves, each with the fold's
    /// columns side by side (`y_0`, `y_1`, … in the fold's order, named in
    /// `info.axes`) rather than folded, so that a row is one line.
    async fn parallel(&mut self, layer: &Layer, plan: &LayerPlan) -> Step<LayerData> {
        let Some(fold) = &self.spec.fold else {
            return Err(Halt::Refuse(
                "parallel coordinates need several columns compared as one variable".to_owned(),
            ));
        };
        let mut keys = Vec::new();
        for dim in plan.dims.iter().filter(|d| !d.channel.is_positional()) {
            keys.push(self.key(dim).await?);
        }
        let mut columns: Vec<Projection> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| Projection::expr_as(k.expr.clone(), g(i)))
            .collect();
        columns.extend(
            fold.columns
                .iter()
                .enumerate()
                .map(|(i, c)| Projection::expr_as(cast(self.field(c), "double precision"), v(i))),
        );
        let rows = self.stage.unordered_query().map_err(Halt::Refuse)?;
        let points = Select::from(Source::subquery(rows, DATA)).columns(columns);
        let total = self.count_rows(points.clone()).await?;
        let limit = layer.sample.unwrap_or(DEFAULT_SAMPLE).clamp(1, MAX_SAMPLE);
        let sampled = total > limit;
        let width = keys.len() + fold.columns.len();
        let select = if sampled {
            sample(points, width, limit)
        } else {
            points
        };
        let mut out = Vec::new();
        for row in self.run(select).await? {
            let values = row.into_values();
            let (k, rest) = values.split_at(keys.len());
            let mut line = key_values(&keys, k);
            line.extend(rest.iter().map(value_json));
            out.push(line);
        }
        let mut names = key_columns(&keys);
        names.extend((0..fold.columns.len()).map(|i| format!("y_{i}")));
        Ok(LayerData {
            columns: names,
            rows: out,
            sampled,
            total,
            info: json!({ "axes": fold.columns }),
            ..LayerData::empty()
        })
    }

    // --- summary tables -----------------------------------------------------

    async fn table(&mut self, spec: &TableSpec) -> Step<TableData> {
        let cells: Vec<Cell> = if spec.cells.is_empty() {
            vec![Cell::count()]
        } else {
            spec.cells.clone()
        };
        let mut row_keys = Vec::new();
        for f in &spec.rows {
            row_keys.push(self.key(&self.table_dim(f)).await?);
        }
        let mut column_keys = Vec::new();
        for f in &spec.columns {
            column_keys.push(self.key(&self.table_dim(f)).await?);
        }
        let all: Vec<Key> = row_keys.iter().chain(&column_keys).cloned().collect();
        let names = |keys: &[Key], prefix: &str, from: usize| -> Vec<String> {
            let mut out = Vec::new();
            for (i, k) in keys.iter().enumerate() {
                out.push(format!("{prefix}{}", i + from));
                if k.bin.is_some() {
                    out.push(format!("{prefix}{}_end", i + from));
                }
            }
            out
        };
        let row_names = names(&row_keys, "r", 0);
        let column_names = names(&column_keys, "c", 0);
        let (body, total) = self
            .table_part(&all, &[&row_names[..], &column_names[..]].concat(), &cells)
            .await?;
        let truncated = body.rows.len() > MAX_TABLE_ROWS;
        let mut body = body;
        body.rows.truncate(MAX_TABLE_ROWS);
        let both = !row_keys.is_empty() && !column_keys.is_empty();
        let any = !row_keys.is_empty() || !column_keys.is_empty();
        let row_totals = if spec.totals && both {
            Some(self.table_part(&row_keys, &row_names, &cells).await?.0)
        } else {
            None
        };
        let column_totals = if spec.totals && both {
            Some(
                self.table_part(&column_keys, &column_names, &cells)
                    .await?
                    .0,
            )
        } else {
            None
        };
        let grand_total = if spec.totals && any {
            Some(self.table_part(&[], &[], &cells).await?.0)
        } else {
            None
        };
        let truncate = |t: Option<Table>| {
            t.map(|mut t| {
                t.rows.truncate(MAX_TABLE_ROWS);
                t
            })
        };
        Ok(TableData {
            rows: spec.rows.iter().map(|f| f.field.clone()).collect(),
            columns: spec.columns.iter().map(|f| f.field.clone()).collect(),
            cells: cells.iter().map(Cell::describe).collect(),
            body,
            row_totals: truncate(row_totals),
            column_totals: truncate(column_totals),
            grand_total,
            bins: std::mem::take(&mut self.bins),
            total,
            truncated,
        })
    }

    /// A dimension of a table, as a plot's group key is made.
    fn table_dim(&self, f: &super::spec::FieldDef) -> Dim {
        Dim {
            channel: Channel::X,
            field: f.field.clone(),
            ty: self
                .shape
                .column(&f.field)
                .map_or(ColType::Unknown, |c| c.ty),
            bin: f.bin,
        }
    }

    /// The cells grouped by `keys` (named `names`): at most
    /// [`MAX_TABLE_ROWS`] + 1 rows in key order, and how many dataset rows
    /// they summarise.
    async fn table_part(
        &mut self,
        keys: &[Key],
        names: &[String],
        cells: &[Cell],
    ) -> Step<(Table, u64)> {
        let numeric = |f: AggregateFn| {
            matches!(
                f,
                AggregateFn::Sum | AggregateFn::Mean | AggregateFn::Median | AggregateFn::Sd
            )
        };
        let inputs: Vec<Expr> = cells
            .iter()
            .map(|c| match &c.field {
                None => int(1),
                Some(f) if numeric(c.function) => cast(self.field(f), "double precision"),
                Some(f) => self.field(f),
            })
            .collect();
        let points = self.points(keys, inputs, &[])?;
        let mut columns = group_projections(keys.len());
        columns.push(Projection::expr_as(agg("count", vec![]), "_n"));
        for (i, c) in cells.iter().enumerate() {
            let value = Expr::qcol(POINTS, v(i));
            let func = match c.function {
                AggregateFn::Count => "count",
                AggregateFn::Sum => "sum",
                AggregateFn::Mean => "avg",
                AggregateFn::Min => "min",
                AggregateFn::Max => "max",
                AggregateFn::Sd => "stddev_samp",
                // Filled in below from the percentiles.
                AggregateFn::Median => "count",
            };
            columns.push(Projection::expr_as(
                agg(func, vec![value]),
                format!("_c{i}"),
            ));
        }
        let select = grouped(points.clone(), columns, keys.len(), MAX_TABLE_ROWS + 1);
        let rows = self.run(select).await?;
        let mut out: Vec<(Vec<Value>, Vec<Json>)> = Vec::with_capacity(rows.len());
        let mut total = 0;
        for row in rows {
            let values = row.into_values();
            let (k, rest) = values.split_at(keys.len());
            let n = f64_of(&rest[0]).unwrap_or(0.0) as u64;
            total += n;
            let mut line = vec![json!(n)];
            for (c, value) in cells.iter().zip(&rest[1..]) {
                line.push(match (c.function, &c.field) {
                    (AggregateFn::Min | AggregateFn::Max, Some(f))
                        if self.shape.column(f).is_some_and(|c| c.ty == ColType::Bool) =>
                    {
                        match value {
                            Value::Int(b) => json!(*b != 0),
                            other => value_json(other),
                        }
                    }
                    _ => value_json(value),
                });
            }
            out.push((k.to_vec(), line));
        }
        // Medians, one percentile query per cell, matched to the groups.
        for (i, c) in cells.iter().enumerate() {
            if c.function != AggregateFn::Median {
                continue;
            }
            for line in &mut out {
                line.1[i + 1] = Json::Null;
            }
            let Some(f) = &c.field else { continue };
            let points = self.points(keys, vec![cast(self.field(f), "double precision")], &[])?;
            for q in self
                .percentiles(points, keys.len(), &[0.5], MAX_TABLE_ROWS + 1)
                .await?
            {
                if let Some(line) = out.iter_mut().find(|(k, _)| same_keys(k, &q.keys)) {
                    line.1[i + 1] = json_num(q.q[0]);
                }
            }
        }
        let mut columns = names.to_vec();
        columns.push("n".to_owned());
        columns.extend((0..cells.len()).map(v_name));
        let rows = out
            .into_iter()
            .map(|(k, line)| {
                let mut row = key_values(keys, &k);
                row.extend(line);
                row
            })
            .collect();
        Ok((Table { columns, rows }, total))
    }

    // --- shared queries ------------------------------------------------------

    /// For each group of `points` (its first `groups` columns the keys, `_v0`
    /// and `_v1` the two values, rows missing either left out): the sums a
    /// least-squares line and a correlation need, about the group's means —
    /// the means from a window over the group, so it is one query. At most
    /// [`MAX_GROUP_ROWS`] + 1 groups, in key order.
    pub(crate) async fn linear_sums(
        &self,
        points: Select,
        groups: usize,
    ) -> Step<Vec<(Vec<Value>, LinearSums)>> {
        let (x, y) = (Expr::qcol(POINTS, v(0)), Expr::qcol(POINTS, v(1)));
        let both =
            Expr::unary(UnOp::IsNotNull, x.clone()).and(Expr::unary(UnOp::IsNotNull, y.clone()));
        let partition = group_exprs(POINTS, groups);
        let mut inner_columns = group_projections(groups);
        inner_columns.push(Projection::expr_as(x.clone(), v(0)));
        inner_columns.push(Projection::expr_as(y.clone(), v(1)));
        inner_columns.push(Projection::expr_as(
            win("avg", vec![x.clone()], partition.clone()),
            "_mx",
        ));
        inner_columns.push(Projection::expr_as(win("avg", vec![y], partition), "_my"));
        let mut inner = Select::from(Source::subquery(points, POINTS)).columns(inner_columns);
        inner.filter = Some(both);
        let q = |c: &str| Expr::qcol(INNER, c);
        let dx = Expr::binary(BinOp::Sub, q(&v(0)), q("_mx"));
        let dy = Expr::binary(BinOp::Sub, q(&v(1)), q("_my"));
        let mut outer_columns: Vec<Projection> = (0..groups)
            .map(|i| Projection::expr_as(q(&g(i)), g(i)))
            .collect();
        outer_columns.extend([
            Projection::expr_as(agg("count", vec![]), "_n"),
            Projection::expr_as(agg("max", vec![q("_mx")]), "_mx"),
            Projection::expr_as(agg("max", vec![q("_my")]), "_my"),
            Projection::expr_as(
                agg(
                    "sum",
                    vec![Expr::binary(BinOp::Mul, dx.clone(), dx.clone())],
                ),
                "_sxx",
            ),
            Projection::expr_as(
                agg("sum", vec![Expr::binary(BinOp::Mul, dx, dy.clone())]),
                "_sxy",
            ),
            Projection::expr_as(
                agg("sum", vec![Expr::binary(BinOp::Mul, dy.clone(), dy)]),
                "_syy",
            ),
            Projection::expr_as(agg("min", vec![q(&v(0))]), "_x0"),
            Projection::expr_as(agg("max", vec![q(&v(0))]), "_x1"),
        ]);
        let mut outer = Select::from(Source::subquery(inner, INNER)).columns(outer_columns);
        outer.group = group_exprs(INNER, groups);
        outer.order = group_order(INNER, groups);
        outer.limit = Some(MAX_GROUP_ROWS as u64 + 1);
        let rows = self.run(outer).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let values = row.into_values();
                let (k, s) = values.split_at(groups);
                let f = |i: usize| f64_of(&s[i]).unwrap_or(0.0);
                (
                    k.to_vec(),
                    LinearSums {
                        n: f(0) as u64,
                        mean_x: f(1),
                        mean_y: f(2),
                        sxx: f(3),
                        sxy: f(4),
                        syy: f(5),
                        min_x: f(6),
                        max_x: f(7),
                    },
                )
            })
            .collect())
    }

    /// For each group of `points` (its first `groups` columns are the keys,
    /// `_v0` the value): how many values, the smallest, largest, mean and
    /// standard deviation, and the quantiles at `probabilities` — at most
    /// `limit` groups, in key order.
    async fn percentiles(
        &self,
        points: Select,
        groups: usize,
        probabilities: &[f64],
        limit: usize,
    ) -> Step<Vec<GroupStats>> {
        let value = Expr::qcol(POINTS, v(0));
        let partition = group_exprs(POINTS, groups);
        let mut inner_columns = group_projections(groups);
        inner_columns.push(Projection::expr_as(value.clone(), v(0)));
        inner_columns.push(Projection::expr_as(
            Expr::row_number(partition.clone(), vec![OrderBy::asc(value.clone())]),
            "_rn",
        ));
        inner_columns.push(Projection::expr_as(
            win("count", vec![value.clone()], partition),
            "_cnt",
        ));
        let mut inner = Select::from(Source::subquery(points, POINTS)).columns(inner_columns);
        inner.filter = Some(Expr::unary(UnOp::IsNotNull, value));
        let q = |c: &str| Expr::qcol(INNER, c);
        let x = q(&v(0));
        let mut columns: Vec<Projection> = (0..groups)
            .map(|i| Projection::expr_as(q(&g(i)), g(i)))
            .collect();
        columns.extend([
            Projection::expr_as(agg("max", vec![q("_cnt")]), "_n"),
            Projection::expr_as(agg("min", vec![x.clone()]), "_min"),
            Projection::expr_as(agg("max", vec![x.clone()]), "_max"),
            Projection::expr_as(agg("avg", vec![x.clone()]), "_mean"),
            Projection::expr_as(agg("stddev_samp", vec![x.clone()]), "_sd"),
        ]);
        for (i, p) in probabilities.iter().enumerate() {
            // The 1-based positions either side of the p quantile.
            let lo = Expr::Func {
                name: "floor".into(),
                args: vec![Expr::binary(
                    BinOp::Mul,
                    Expr::binary(BinOp::Sub, q("_cnt"), int(1)),
                    num(*p),
                )],
            };
            for (j, offset) in [1, 2].into_iter().enumerate() {
                let at = Expr::binary(
                    BinOp::Eq,
                    q("_rn"),
                    Expr::binary(BinOp::Add, lo.clone(), int(offset)),
                );
                columns.push(Projection::expr_as(
                    agg("max", vec![case_when(at, x.clone())]),
                    format!("_q{i}_{j}"),
                ));
            }
        }
        let mut outer = Select::from(Source::subquery(inner, INNER)).columns(columns);
        outer.group = group_exprs(INNER, groups);
        outer.order = group_order(INNER, groups);
        outer.limit = Some(limit as u64);
        let rows = self.run(outer).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let values = row.into_values();
            let (k, s) = values.split_at(groups);
            let n = f64_of(&s[0]).unwrap_or(0.0) as u64;
            if n == 0 {
                continue;
            }
            let qs = probabilities
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let at_lo = f64_of(&s[5 + 2 * i]).unwrap_or(f64::NAN);
                    quantile(n, *p, at_lo, f64_of(&s[6 + 2 * i]))
                })
                .collect();
            out.push(GroupStats {
                keys: k.to_vec(),
                n,
                min: f64_of(&s[1]).unwrap_or(f64::NAN),
                max: f64_of(&s[2]).unwrap_or(f64::NAN),
                sd: f64_of(&s[4]),
                q: qs,
            });
        }
        Ok(out)
    }

    /// Run a grouped select whose rows are the keys then the stat's values,
    /// and make a layer of it: `finish` turns the values into the columns
    /// named `names`.
    async fn grouped_layer(
        &self,
        keys: &[Key],
        select: Select,
        total: u64,
        names: &[&str],
        finish: impl Fn(&[Value]) -> Vec<Json>,
    ) -> Step<LayerData> {
        let rows = self.run(select).await?;
        let truncated = rows.len() > MAX_GROUP_ROWS;
        let rows = rows
            .into_iter()
            .take(MAX_GROUP_ROWS)
            .map(|row| {
                let values = row.into_values();
                let (k, rest) = values.split_at(keys.len());
                let mut out = key_values(keys, k);
                out.extend(finish(rest));
                out
            })
            .collect();
        let mut columns = key_columns(keys);
        columns.extend(names.iter().map(|n| (*n).to_owned()));
        Ok(LayerData {
            columns,
            rows,
            total,
            truncated,
            ..LayerData::empty()
        })
    }

    pub(crate) async fn count_rows(&self, points: Select) -> Step<u64> {
        let count = Select::from(Source::subquery(points, POINTS))
            .columns(vec![Projection::expr_as(agg("count", vec![]), "n")]);
        self.scalar_count(count).await
    }

    async fn scalar_count(&self, select: Select) -> Step<u64> {
        let rows = self.run(select).await?;
        Ok(rows
            .first()
            .and_then(|r| r.get_index(0))
            .and_then(f64_of)
            .map_or(0, |n| n as u64))
    }

    pub(crate) async fn run(&self, select: Select) -> Step<Vec<Row>> {
        Ok(self
            .catalog
            .primary()
            .query(&Statement::from(select))
            .await?
            .try_collect()
            .await?)
    }

    // --- facets and domains --------------------------------------------------

    /// The values of each facet channel over every layer, refused when there
    /// are too many to draw.
    fn facets(&self, layers: &[LayerData]) -> Step<BTreeMap<String, Vec<Json>>> {
        let mut out = BTreeMap::new();
        for (channel, f) in self.spec.facet.iter() {
            let mut values: Vec<Json> = Vec::new();
            for layer in layers {
                if let Some(i) = layer.columns.iter().position(|c| c == channel.as_str()) {
                    for row in &layer.rows {
                        if !values.contains(&row[i]) {
                            values.push(row[i].clone());
                        }
                    }
                }
            }
            if values.len() > MAX_FACETS {
                return Err(Halt::Refuse(format!(
                    "{} by `{}` makes {} plots, and at most {MAX_FACETS} are drawn; {}",
                    channel.describe(),
                    f.field,
                    values.len(),
                    if f.bin.is_some() {
                        "make its bins wider"
                    } else {
                        "bin it, or filter the dataset"
                    }
                )));
            }
            values.sort_by(compare_json);
            out.insert(channel.as_str().to_owned(), values);
        }
        Ok(out)
    }

    /// The values each channel spans, over every layer's columns named by it
    /// (`y`, `y_lower`, `y_q3`, …), the outliers, and the reference lines.
    fn domains(&self, layers: &[LayerData]) -> BTreeMap<String, Domain> {
        let mut values: BTreeMap<String, Vec<Json>> = BTreeMap::new();
        let mut take = |name: &str, columns: &[String], rows: &[Vec<Json>]| {
            for (i, c) in columns.iter().enumerate() {
                let channel = c.split('_').next().unwrap_or(c);
                if channel != name {
                    continue;
                }
                let entry = values.entry(name.to_owned()).or_default();
                entry.extend(rows.iter().map(|r| r[i].clone()).filter(|v| !v.is_null()));
            }
        };
        for channel in Channel::ENCODED {
            for layer in layers {
                take(channel.as_str(), &layer.columns, &layer.rows);
                if let Some(o) = &layer.outliers {
                    take(channel.as_str(), &o.columns, &o.rows);
                }
            }
        }
        for r in &self.spec.references {
            values
                .entry(r.channel.as_str().to_owned())
                .or_default()
                .push(r.value.clone());
        }
        values
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(channel, mut v)| {
                let numeric = v.iter().all(Json::is_number);
                v.sort_by(compare_json);
                v.dedup();
                let (min, max) = (v.first().cloned(), v.last().cloned());
                let domain = if numeric {
                    Domain {
                        kind: "continuous",
                        min,
                        max,
                        values: (v.len() <= DOMAIN_VALUES).then_some(v),
                    }
                } else {
                    Domain {
                        kind: "discrete",
                        min,
                        max,
                        values: Some(v),
                    }
                };
                (channel, domain)
            })
            .collect()
    }
}

/// What the percentile query says about one group.
struct GroupStats {
    keys: Vec<Value>,
    n: u64,
    min: f64,
    max: f64,
    sd: Option<f64>,
    q: Vec<f64>,
}

impl LayerData {
    fn empty() -> LayerData {
        LayerData {
            mark: Mark::Point,
            stat: "identity",
            columns: Vec::new(),
            rows: Vec::new(),
            outliers: None,
            sampled: false,
            total: 0,
            truncated: false,
            info: Json::Null,
        }
    }
}

// --- helpers ---------------------------------------------------------------

pub(crate) fn g(i: usize) -> String {
    format!("_g{i}")
}

pub(crate) fn v(i: usize) -> String {
    format!("_v{i}")
}

/// A table cell's column.
fn v_name(i: usize) -> String {
    format!("v{i}")
}

/// A typed number, so Postgres knows what the placeholder is.
pub(crate) fn num(x: f64) -> Expr {
    cast(Expr::lit(Value::Float(x)), "double precision")
}

fn int(n: i64) -> Expr {
    cast(Expr::lit(Value::Int(n)), "bigint")
}

pub(crate) fn cast(expr: Expr, type_name: &str) -> Expr {
    Expr::Cast {
        expr: Box::new(expr),
        type_name: type_name.to_owned(),
    }
}

pub(crate) fn agg(func: &str, args: Vec<Expr>) -> Expr {
    Expr::Agg {
        func: func.to_owned(),
        distinct: false,
        args,
    }
}

fn win(func: &str, args: Vec<Expr>, partition: Vec<Expr>) -> Expr {
    Expr::Window {
        func: func.to_owned(),
        args,
        partition,
        order: Vec::new(),
    }
}

fn case_when(cond: Expr, value: Expr) -> Expr {
    Expr::Case {
        operand: None,
        arms: vec![CaseArm {
            when: cond,
            then: value,
        }],
        else_result: None,
    }
}

pub(crate) fn group_exprs(alias: &str, n: usize) -> Vec<Expr> {
    (0..n).map(|i| Expr::qcol(alias, g(i))).collect()
}

pub(crate) fn group_projections(n: usize) -> Vec<Projection> {
    (0..n)
        .map(|i| Projection::expr_as(Expr::qcol(POINTS, g(i)), g(i)))
        .collect()
}

/// Order by the keys, missing values last (both databases agree then).
pub(crate) fn group_order(alias: &str, n: usize) -> Vec<OrderBy> {
    group_exprs(alias, n)
        .into_iter()
        .map(|e| OrderBy {
            expr: e,
            dir: OrderDir::Asc,
            nulls: Some(Nulls::Last),
        })
        .collect()
}

/// `SELECT columns FROM (points) GROUP BY keys ORDER BY keys LIMIT limit`.
pub(crate) fn grouped(
    points: Select,
    columns: Vec<Projection>,
    keys: usize,
    limit: usize,
) -> Select {
    let mut select = Select::from(Source::subquery(points, POINTS)).columns(columns);
    select.group = group_exprs(POINTS, keys);
    select.order = group_order(POINTS, keys);
    select.limit = Some(limit as u64);
    select
}

/// A random sample of `limit` of `points`' rows (its first `width` columns),
/// the same each time: the rows are numbered in order of every column, the
/// numbers scrambled from a fixed seed (as a dataset's Limit samples), and the
/// smallest kept. Rows that tie on every column are interchangeable, so the
/// sample is the same whichever of them is numbered first.
fn sample(points: Select, width: usize, limit: u64) -> Select {
    let names: Vec<String> = points
        .columns
        .iter()
        .take(width)
        .enumerate()
        .map(|(i, p)| match p {
            Projection::Expr { alias: Some(a), .. } => a.clone(),
            _ => format!("_c{i}"),
        })
        .collect();
    let order: Vec<OrderBy> = names
        .iter()
        .map(|n| OrderBy {
            expr: Expr::qcol(POINTS, n.clone()),
            dir: OrderDir::Asc,
            nulls: Some(Nulls::Last),
        })
        .collect();
    let mut numbered: Vec<Projection> = names
        .iter()
        .map(|n| Projection::expr_as(Expr::qcol(POINTS, n.clone()), n.clone()))
        .collect();
    numbered.push(Projection::expr_as(
        Expr::row_number(Vec::new(), order),
        "_rn",
    ));
    let inner = Select::from(Source::subquery(points, POINTS)).columns(numbered);
    let mut outer = Select::from(Source::subquery(inner, INNER)).columns(
        names
            .iter()
            .map(|n| Projection::expr_as(Expr::qcol(INNER, n.clone()), n.clone()))
            .collect(),
    );
    outer.order = vec![OrderBy::asc(scramble(Expr::qcol(INNER, "_rn"), SEED))];
    outer.limit = Some(limit);
    outer
}

/// The condition picking one group of points by its keys.
fn group_match(n: usize, keys: &[Value]) -> Expr {
    (0..n)
        .map(|i| {
            let column = Expr::qcol(POINTS, g(i));
            match &keys[i] {
                Value::Null => Expr::unary(UnOp::IsNull, column),
                value => Expr::binary(BinOp::IsNotDistinct, column, Expr::lit(value.clone())),
            }
        })
        .reduce(Expr::and)
        .unwrap_or_else(|| Expr::binary(BinOp::Eq, int(1), int(1)))
}

/// The names of the columns the keys become: a binned key is two, its bin's
/// lower and upper edge.
fn key_columns(keys: &[Key]) -> Vec<String> {
    let mut out = Vec::new();
    for k in keys {
        out.push(k.channel.as_str().to_owned());
        if k.bin.is_some() {
            out.push(format!("{}_end", k.channel.as_str()));
        }
    }
    out
}

/// Where key `i`'s first column is among [`key_columns`].
fn key_column_index(keys: &[Key], i: usize) -> usize {
    keys[..i]
        .iter()
        .map(|k| if k.bin.is_some() { 2 } else { 1 })
        .sum()
}

/// The key values of a row as JSON: a bin number as its edges, a boolean
/// SQLite returned as an integer as a boolean.
pub(crate) fn key_values(keys: &[Key], values: &[Value]) -> Vec<Json> {
    let mut out = Vec::new();
    for (k, value) in keys.iter().zip(values) {
        match k.bin {
            Some(b) => match f64_of(value) {
                Some(i) => {
                    let lo = b.origin + i * b.width;
                    out.push(json_num(tidy(lo)));
                    out.push(json_num(tidy(lo + b.width)));
                }
                None => {
                    out.push(Json::Null);
                    out.push(Json::Null);
                }
            },
            None => out.push(match (k.ty, value) {
                (ColType::Bool, Value::Int(n)) => json!(*n != 0),
                _ => value_json(value),
            }),
        }
    }
    out
}

/// A bin edge without the binary noise of `origin + i·width` (0.30000000000000004).
fn tidy(x: f64) -> f64 {
    let scaled = (x * 1e9).round() / 1e9;
    if (scaled - x).abs() < 1e-9 * (1.0 + x.abs()) {
        scaled
    } else {
        x
    }
}

pub(crate) fn same_keys(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x == y || matches!((f64_of(x), f64_of(y)), (Some(p), Some(q)) if p == q))
}

fn compare_keys(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = compare_json(&value_json(x), &value_json(y));
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}

/// Numbers by value, text by its characters, missing values last.
pub(crate) fn compare_json(a: &Json, b: &Json) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Json::Null, Json::Null) => Ordering::Equal,
        (Json::Null, _) => Ordering::Greater,
        (_, Json::Null) => Ordering::Less,
        (Json::Number(x), Json::Number(y)) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(Ordering::Equal),
        (Json::Bool(x), Json::Bool(y)) => x.cmp(y),
        (Json::String(x), Json::String(y)) => x.cmp(y),
        (Json::Number(_), _) => Ordering::Less,
        (_, Json::Number(_)) => Ordering::Greater,
        _ => a.to_string().cmp(&b.to_string()),
    }
}

pub(crate) fn f64_of(value: &Value) -> Option<f64> {
    match value {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::Decimal(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

pub(crate) fn json_num(x: f64) -> Json {
    serde_json::Number::from_f64(x).map_or(Json::Null, Json::Number)
}

/// A probability as a percentage for a column name: 0.25 → `25`, 0.975 →
/// `97.5`.
fn percent(p: f64) -> String {
    let s = format!("{:.4}", p * 100.0);
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn rows_word(n: u64) -> &'static str {
    if n == 1 { "row" } else { "rows" }
}

fn too_many_curves(what: &str) -> String {
    format!(
        "the layer would draw more than {MAX_CURVES} {what}, one per group; \
         put fewer values on Color, or bin them"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probabilities_name_their_columns() {
        assert_eq!(percent(0.25), "25");
        assert_eq!(percent(0.975), "97.5");
        assert_eq!(percent(0.5), "50");
    }

    #[test]
    fn bin_edges_are_tidy() {
        assert_eq!(tidy(0.1 + 0.2), 0.3);
        assert_eq!(tidy(1990.0), 1990.0);
    }
}

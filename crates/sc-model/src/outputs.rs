//! What a fit shows: its **outputs** (analytics TODO A3.1–A3.2).
//!
//! A fit has tables — its parameter blocks, its metrics — and plots. A plot is
//! a **plot spec** (the Analytics UI's grammar, `sc_analytics::plot`) over
//! **fit output data**: a small frame the fit produced and stored beside the
//! instance, such as each row's fitted value and residual, or a posterior's
//! draws. The spec is JSON here, because this crate sits below the one that
//! knows what a spec means; the server fills in its `data` (`{ "kind":
//! "fit_output", "instance": …, "name": … }`) and draws it with `render_plot`,
//! which reads the frame from the instance rather than through SQL.
//!
//! **Who declares what.** A provider declares its outputs
//! ([`ModelProvider::outputs`](crate::ModelProvider::outputs)), and the default
//! is [`standard_outputs`]: the parameter tables, the metrics, and the plots
//! that suit the fit's [`Outcome`] — residuals for a regression, a confusion
//! heatmap for a classification, a scatter plot coloured by cluster, trace,
//! rank and density plots for a posterior. A module's provider declares them
//! in its manifest (`outputs: [...]`) in the same JSON. The **data** is mostly
//! the host's — [`ROWS_OUTPUT`] and [`DRAWS_OUTPUT`] are made here from what a
//! fit already has in hand (the predictions it scored, the draws it sampled) —
//! and a provider may add frames of its own in
//! [`FitResult::outputs`](crate::FitResult::outputs).
//!
//! **Bounded.** A frame is stored whole as JSON, so it is capped:
//! [`MAX_OUTPUT_ROWS`] rows (more are thinned by a stride, and the frame says
//! how many there were), and a posterior's plots show at most
//! [`MAX_OUTPUT_PARAMETERS`] parameters of at most [`MAX_DRAWS_PER_CHAIN`]
//! draws a chain.
//!
//! **Plots can be optional**: not shown until somebody picks them from the
//! model editor's "More plots".

use std::collections::{BTreeMap, BTreeSet};

use sc_catalog::{Catalog, DataField, Table};
use sc_db::Transaction;
use sc_error::{Error, Result};
use sc_query::{Delete, Expr, InSet, Insert, Projection, Select, Source, Statement, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use statrs::distribution::{ContinuousCDF, Normal};
use uuid::Uuid;

use crate::encode::{Encoded, Encoding};
use crate::frame::{Column, ColumnType, Frame};
use crate::instance::InstanceId;
use crate::interface::Interface;
use crate::model::ModelId;
use crate::posterior::DrawSeries;
use crate::provider::{Outcome, ParameterBlock, Prediction};
use crate::split::Part;
use crate::store::{bad_column, rows};

/// The instance attribute holding a fit's declared outputs, in order.
pub const ATTR_OUTPUTS: &str = "outputs";

/// The output data a predicting fit stores: one row per scored row of every
/// split, with the columns the model read, `split`, and what the outcome
/// makes — `actual`, `fitted`, `residual`, `standardised_residual` and
/// `theoretical_quantile` for a regression; `actual`, `predicted`,
/// `probability` and `correct` for a classification; `cluster`; or
/// `component_1`, `component_2`, … for an embedding.
pub const ROWS_OUTPUT: &str = "rows";

/// The output data a posterior stores: one row per kept draw of each plotted
/// parameter — `parameter` (`alpha[3]`), `chain`, `iteration`, `value` and
/// `rank` (its rank among every chain's draws of that parameter).
pub const DRAWS_OUTPUT: &str = "draws";

/// The most rows an output frame stores; more are thinned by a stride.
pub const MAX_OUTPUT_ROWS: usize = 20_000;

/// The most parameters a posterior's plots show — scalars first.
pub const MAX_OUTPUT_PARAMETERS: usize = 8;

/// The most draws per chain a posterior's plots show; more are thinned.
pub const MAX_DRAWS_PER_CHAIN: usize = 500;

/// The most components of an embedding the rows output keeps.
const MAX_COMPONENTS: usize = 10;

/// One output of a fit, as a provider declares it and the instance records it.
///
/// ```json
/// { "name": "residuals", "label": "Residuals against fitted values",
///   "kind": "plot", "data": "rows",
///   "spec": { "layers": [ { "mark": "point", "encoding": {
///       "x": { "field": "fitted" }, "y": { "field": "residual" } } } ] } }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputDecl {
    /// Its key: unique among the fit's outputs, and what a screen remembers it
    /// by.
    pub name: String,
    /// What it is called on the screen.
    pub label: String,
    /// Not shown until it is asked for ("More plots").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// What it is.
    #[serde(flatten)]
    pub kind: OutputKind,
}

/// What an output shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputKind {
    /// One of the fit's parameter blocks, by name.
    Parameters {
        /// The block's name (`Coefficients`).
        block: String,
    },
    /// The fit's metrics, per split.
    Metrics,
    /// An output frame, as a table.
    Table {
        /// The output data's name.
        data: String,
    },
    /// A plot spec drawn over an output frame. Its `data` is the server's to
    /// fill in; anything there is replaced.
    Plot {
        /// The output data's name.
        data: String,
        /// The plot spec, without its data.
        spec: Json,
    },
}

impl OutputDecl {
    /// A table of the parameter block `block`, labelled with its name.
    pub fn parameters(name: impl Into<String>, block: impl Into<String>) -> OutputDecl {
        let block = block.into();
        OutputDecl {
            name: name.into(),
            label: block.clone(),
            optional: false,
            kind: OutputKind::Parameters { block },
        }
    }

    /// The metrics table.
    pub fn metrics() -> OutputDecl {
        OutputDecl {
            name: "metrics".to_owned(),
            label: "Metrics".to_owned(),
            optional: false,
            kind: OutputKind::Metrics,
        }
    }

    /// A plot of `spec` over the output data `data`.
    pub fn plot(
        name: impl Into<String>,
        label: impl Into<String>,
        data: impl Into<String>,
        spec: Json,
    ) -> OutputDecl {
        OutputDecl {
            name: name.into(),
            label: label.into(),
            optional: false,
            kind: OutputKind::Plot {
                data: data.into(),
                spec,
            },
        }
    }

    /// The same output, marked optional.
    #[must_use]
    pub fn optional(mut self) -> OutputDecl {
        self.optional = true;
        self
    }

    /// The output data this output reads, if it reads any.
    pub fn data(&self) -> Option<&str> {
        match &self.kind {
            OutputKind::Table { data } | OutputKind::Plot { data, .. } => Some(data),
            OutputKind::Parameters { .. } | OutputKind::Metrics => None,
        }
    }
}

/// One output frame, as stored: the frame, and how many rows it stood for
/// before it was thinned.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputData {
    /// The rows kept.
    pub frame: Frame,
    /// How many rows there were.
    pub total: usize,
}

impl OutputData {
    /// A frame that was not thinned.
    pub fn whole(frame: Frame) -> OutputData {
        OutputData {
            total: frame.rows,
            frame,
        }
    }

    /// Whether the frame is a thinned sample of `total` rows.
    pub fn sampled(&self) -> bool {
        self.total > self.frame.rows
    }

    /// As stored: the frame's JSON with `total` beside its `rows`.
    pub fn to_json(&self) -> Json {
        let mut json = self.frame.to_json();
        if let Json::Object(map) = &mut json {
            map.insert("total".to_owned(), Json::from(self.total));
        }
        json
    }

    /// Back from what [`to_json`](OutputData::to_json) wrote. A frame with no
    /// `total` (a module's) stood for its own rows.
    pub fn from_json(json: &Json) -> Result<OutputData> {
        let frame = Frame::from_json(json)?;
        let total = json
            .get("total")
            .and_then(Json::as_u64)
            .map_or(frame.rows, |n| n as usize)
            .max(frame.rows);
        Ok(OutputData { frame, total })
    }
}

impl Serialize for OutputData {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.to_json().serialize(s)
    }
}

impl<'de> Deserialize<'de> for OutputData {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<OutputData, D::Error> {
        let json = Json::deserialize(d)?;
        OutputData::from_json(&json).map_err(serde::de::Error::custom)
    }
}

/// What a provider decides its outputs from.
#[derive(Debug, Clone, Copy)]
pub struct OutputContext<'a> {
    /// What the fit produced.
    pub outcome: &'a Outcome,
    /// The model's configuration.
    pub configuration: &'a Attrs,
    /// The dataset columns the fit read as features, with their types.
    pub features: &'a [(String, ColumnType)],
    /// The fit's parameter blocks.
    pub parameters: &'a [ParameterBlock],
    /// The output frames the fit stored, by name.
    pub data: &'a BTreeMap<String, OutputData>,
}

impl OutputContext<'_> {
    /// Whether the fit stored the output data `name`.
    pub fn has(&self, name: &str) -> bool {
        self.data.contains_key(name)
    }

    /// The columns of the output data `name`, when there is one.
    pub fn columns(&self, name: &str) -> Vec<(String, ColumnType)> {
        self.data.get(name).map_or_else(Vec::new, |d| {
            d.frame
                .columns
                .iter()
                .map(|(n, c)| (n.clone(), c.kind()))
                .collect()
        })
    }
}

/// The outputs every fit of this outcome has, unless its provider says
/// otherwise: a table per parameter block that is a table, the metrics
/// (when there are any to show), and the plots of [`outcome_plots`].
pub fn standard_outputs(ctx: &OutputContext<'_>) -> Vec<OutputDecl> {
    let mut out = parameter_tables(ctx.parameters);
    if !matches!(ctx.outcome, Outcome::Test | Outcome::Posterior { .. }) {
        out.push(OutputDecl::metrics());
    }
    out.extend(outcome_plots(ctx));
    out
}

/// A table output for each parameter block that is a table, named by the
/// block (`coefficients`), and one for the scalars together when there are
/// any — what an admin read off the instance screen.
pub fn parameter_tables(parameters: &[ParameterBlock]) -> Vec<OutputDecl> {
    let mut out = Vec::new();
    let mut names = BTreeSet::new();
    for block in parameters {
        if let ParameterBlock::Table { name, .. } | ParameterBlock::Text { name, .. } = block {
            let key = unique(&mut names, &slug(name));
            out.push(OutputDecl::parameters(key, name.clone()));
        }
    }
    if parameters
        .iter()
        .any(|b| matches!(b, ParameterBlock::Scalar { .. }))
    {
        let key = unique(&mut names, "statistics");
        out.push(OutputDecl {
            name: key,
            label: "Statistics".to_owned(),
            optional: false,
            kind: OutputKind::Parameters {
                block: SCALARS.to_owned(),
            },
        });
    }
    out
}

/// The block name meaning "every scalar parameter, as one table".
pub const SCALARS: &str = "*";

/// The plots that suit `ctx`'s outcome, over the host's output data.
pub fn outcome_plots(ctx: &OutputContext<'_>) -> Vec<OutputDecl> {
    let rows = ROWS_OUTPUT;
    match ctx.outcome {
        Outcome::Regression { .. } if ctx.has(rows) => vec![
            OutputDecl::plot(
                "residuals_fitted",
                "Residuals against fitted values",
                rows,
                json!({
                    "layers": [
                        { "mark": "point", "encoding": {
                            "x": { "field": "fitted" }, "y": { "field": "residual" },
                            "color": { "field": "split" } } },
                        { "mark": "line",
                          "stat": { "kind": "smooth", "method": "loess", "se": false },
                          "encoding": {
                            "x": { "field": "fitted" }, "y": { "field": "residual" } } }
                    ],
                    "references": [ { "channel": "y", "value": 0 } ]
                }),
            ),
            OutputDecl::plot(
                "actual_predicted",
                "Actual against predicted",
                rows,
                json!({
                    "layers": [
                        { "mark": "point", "encoding": {
                            "x": { "field": "fitted" }, "y": { "field": "actual" },
                            "color": { "field": "split" } } },
                        { "mark": "line", "encoding": {
                            "x": { "field": "fitted" }, "y": { "field": "fitted" } } }
                    ]
                }),
            ),
            OutputDecl::plot(
                "qq",
                "Normal Q-Q plot of the residuals",
                rows,
                json!({
                    "layers": [
                        { "mark": "point", "encoding": {
                            "x": { "field": "theoretical_quantile" },
                            "y": { "field": "standardised_residual" } } },
                        { "mark": "line", "encoding": {
                            "x": { "field": "theoretical_quantile" },
                            "y": { "field": "theoretical_quantile" } } }
                    ]
                }),
            )
            .optional(),
            OutputDecl::plot(
                "residual_histogram",
                "Histogram of the residuals",
                rows,
                json!({
                    "layers": [ { "mark": "bar", "stat": { "kind": "count" },
                                  "encoding": { "x": { "field": "residual", "bin": {} } } } ]
                }),
            )
            .optional(),
        ],
        Outcome::Classification { .. } if ctx.has(rows) => {
            let mut plots = vec![OutputDecl::plot(
                "confusion",
                "Actual against predicted classes",
                rows,
                json!({
                    "layers": [ { "mark": "rect", "stat": { "kind": "count" }, "encoding": {
                        "x": { "field": "predicted" }, "y": { "field": "actual" } } } ]
                }),
            )];
            if ctx
                .columns(rows)
                .iter()
                .any(|(n, t)| n == "probability" && *t == ColumnType::Float)
            {
                plots.push(OutputDecl::plot(
                    "calibration",
                    "How often the predicted class is right, by its probability",
                    rows,
                    json!({
                        "layers": [ { "mark": "line",
                            "stat": { "kind": "aggregate", "function": "mean" },
                            "encoding": {
                                "x": { "field": "probability", "bin": { "bins": 10 } },
                                "y": { "field": "correct" } } } ]
                    }),
                ));
                plots.push(
                    OutputDecl::plot(
                        "probability",
                        "Probability of the predicted class, by actual class",
                        rows,
                        json!({
                            "layers": [ { "mark": "box", "stat": { "kind": "boxplot" },
                                "encoding": {
                                    "x": { "field": "actual" },
                                    "y": { "field": "probability" } } } ]
                        }),
                    )
                    .optional(),
                );
            }
            plots
        }
        Outcome::Cluster if ctx.has(rows) => {
            let mut plots = vec![OutputDecl::plot(
                "cluster_sizes",
                "Cluster sizes",
                rows,
                json!({
                    "layers": [ { "mark": "bar", "stat": { "kind": "count" },
                                  "encoding": { "x": { "field": "cluster" } } } ]
                }),
            )];
            let numeric: Vec<&str> = ctx
                .features
                .iter()
                .filter(|(_, t)| matches!(t, ColumnType::Float | ColumnType::Int))
                .map(|(n, _)| n.as_str())
                .collect();
            if let [x, y, ..] = numeric.as_slice() {
                plots.push(OutputDecl::plot(
                    "clusters",
                    format!("Clusters by {x} and {y}"),
                    rows,
                    json!({
                        "layers": [ { "mark": "point", "encoding": {
                            "x": { "field": x }, "y": { "field": y },
                            "color": { "field": "cluster" } } } ]
                    }),
                ));
            }
            plots
        }
        Outcome::Embedding { dimensions } if *dimensions >= 2 && ctx.has(rows) => {
            vec![OutputDecl::plot(
                "components",
                "The first two components",
                rows,
                json!({
                    "layers": [ { "mark": "point", "encoding": {
                        "x": { "field": "component_1" }, "y": { "field": "component_2" } } } ]
                }),
            )]
        }
        Outcome::Posterior { .. } if ctx.has(DRAWS_OUTPUT) => posterior_plots(),
        _ => Vec::new(),
    }
}

/// Trace, rank and density plots of the draws, one small multiple per
/// parameter.
pub fn posterior_plots() -> Vec<OutputDecl> {
    let draws = DRAWS_OUTPUT;
    vec![
        OutputDecl::plot(
            "trace",
            "Trace plots",
            draws,
            json!({
                "layers": [ { "mark": "line", "encoding": {
                    "x": { "field": "iteration" }, "y": { "field": "value" },
                    "color": { "field": "chain" } } } ],
                "facet": { "wrap": { "field": "parameter" }, "scales": "free" }
            }),
        ),
        OutputDecl::plot(
            "rank",
            "Rank plots",
            draws,
            json!({
                "layers": [ { "mark": "bar", "stat": { "kind": "count" }, "encoding": {
                    "x": { "field": "rank", "bin": { "bins": 20 } } } } ],
                "facet": { "row": { "field": "chain" }, "column": { "field": "parameter" },
                           "scales": "free" }
            }),
        )
        .optional(),
        OutputDecl::plot(
            "density",
            "Posterior densities",
            draws,
            json!({
                "layers": [ { "mark": "line", "stat": { "kind": "density" }, "encoding": {
                    "x": { "field": "value" }, "color": { "field": "chain" } } } ],
                "facet": { "wrap": { "field": "parameter" }, "scales": "free" }
            }),
        )
        .optional(),
    ]
}

/// `outputs` with duplicate names dropped (the first is kept), and every name
/// trimmed — what an instance records.
pub fn tidy_outputs(outputs: Vec<OutputDecl>) -> Vec<OutputDecl> {
    let mut seen = BTreeSet::new();
    outputs
        .into_iter()
        .filter_map(|mut o| {
            o.name = o.name.trim().to_owned();
            (!o.name.is_empty() && seen.insert(o.name.clone())).then_some(o)
        })
        .collect()
}

/// The outputs an instance recorded, or none.
pub fn instance_outputs(instance: &crate::ModelInstance) -> Result<Vec<OutputDecl>> {
    match instance.attributes.get(ATTR_OUTPUTS) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(json) => serde_json::from_value(json.clone()).map_err(|e| {
            Error::msg(format!(
                "model instance {}: its outputs could not be read: {e}",
                instance.id
            ))
        }),
    }
}

// --- the store ----------------------------------------------------------------

/// The table a fit's output frames are stored in, one row per frame.
pub const OUTPUTS_TABLE: &str = "_fd_model_outputs";

const COL_ID: &str = "id";
const COL_INSTANCE: &str = "instance";
const COL_NAME: &str = "name";
const COL_DATA: &str = "data";

/// The fields of `_fd_model_outputs`: an id, the instance (not a foreign key,
/// for the reason `_fd_model_draws.instance` is not), the frame's name, and
/// the frame as [`OutputData::to_json`] writes it.
fn output_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField::plain(COL_INSTANCE, TypeRef::Basic(BasicType::Uuid)).required(),
        DataField::plain(COL_NAME, TypeRef::Basic(BasicType::Text)).required(),
        DataField::plain(COL_DATA, TypeRef::Basic(BasicType::Json)).required(),
    ]
}

/// Ensure `_fd_model_outputs` exists. Idempotent; bootstrapped with the
/// instances table, since every fitted instance may have outputs.
pub async fn bootstrap_model_outputs(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(OUTPUTS_TABLE, &output_fields())
        .await
}

/// Write `data` for `instance` on `tx`, replacing what it had — half of "the
/// instance is fitted", the instance row being the other.
pub(crate) async fn write_outputs(
    tx: &mut dyn Transaction,
    instance: InstanceId,
    data: &BTreeMap<String, OutputData>,
) -> Result<()> {
    delete_outputs_on(tx, Expr::col(COL_INSTANCE).eq(Expr::lit(instance.0))).await?;
    for (name, frame) in data {
        let insert = Insert::row(
            OUTPUTS_TABLE,
            vec![
                COL_ID.to_owned(),
                COL_INSTANCE.to_owned(),
                COL_NAME.to_owned(),
                COL_DATA.to_owned(),
            ],
            vec![
                Expr::lit(Uuid::new_v4()),
                Expr::lit(instance.0),
                Expr::lit(name.clone()),
                Expr::lit(Value::Json(frame.to_json())),
            ],
        );
        tx.query(&Statement::from(insert))
            .await?
            .try_collect()
            .await?;
    }
    Ok(())
}

/// Delete the output frames of one instance on `tx`.
pub(crate) async fn delete_instance_outputs(
    tx: &mut dyn Transaction,
    instance: InstanceId,
) -> Result<()> {
    delete_outputs_on(tx, Expr::col(COL_INSTANCE).eq(Expr::lit(instance.0))).await
}

/// Delete the output frames of every instance of `model` on `tx`, before the
/// instances themselves.
pub(crate) async fn delete_model_outputs(tx: &mut dyn Transaction, model: ModelId) -> Result<()> {
    let mut ids = Select::from(Source::table(crate::INSTANCES_TABLE))
        .filter(Expr::col(crate::instance_store::COL_MODEL).eq(Expr::lit(model.0)));
    ids.columns = vec![Projection::expr(Expr::col(crate::instance_store::COL_ID))];
    delete_outputs_on(
        tx,
        Expr::In {
            e: Box::new(Expr::col(COL_INSTANCE)),
            set: InSet::Subquery(Box::new(ids)),
        },
    )
    .await
}

async fn delete_outputs_on(tx: &mut dyn Transaction, filter: Expr) -> Result<()> {
    let delete = Delete::from(OUTPUTS_TABLE).filter(filter);
    tx.query(&Statement::from(delete))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// The output frame `name` of `instance`, if it stored one — what a plot over
/// fit output data reads (`render_plot`'s `fit_output`).
pub async fn load_output_data(
    catalog: &Catalog,
    instance: InstanceId,
    name: &str,
) -> Result<Option<OutputData>> {
    if catalog.get(OUTPUTS_TABLE)?.is_none() {
        return Ok(None);
    }
    let select = Select::from(Source::table(OUTPUTS_TABLE)).filter(
        Expr::col(COL_INSTANCE)
            .eq(Expr::lit(instance.0))
            .and(Expr::col(COL_NAME).eq(Expr::lit(name))),
    );
    let Some(row) = rows(catalog, select).await?.into_iter().next() else {
        return Ok(None);
    };
    let json = match row.get(COL_DATA) {
        Some(Value::Json(json)) => json.clone(),
        Some(Value::Text(text)) => serde_json::from_str(text)
            .map_err(|e| Error::invalid(format!("{OUTPUTS_TABLE} row `{name}`: not JSON: {e}")))?,
        other => return Err(bad_column(COL_DATA, "a JSON frame", other)),
    };
    OutputData::from_json(&json)
        .map(Some)
        .map_err(|e| Error::invalid(format!("{OUTPUTS_TABLE} row `{name}`: {e}")))
}

// --- the host's output data ---------------------------------------------------

/// One scored part of a fit: which split, its rows before encoding, what the
/// encoding kept, and the predictions of those rows.
pub(crate) struct ScoredPart<'a> {
    pub part: Part,
    pub frame: &'a Frame,
    pub encoded: &'a Encoded,
    pub predictions: &'a [Prediction],
}

/// The [`ROWS_OUTPUT`] frame of a predicting fit: the columns the model read
/// (as the dataset had them), `split`, and the outcome's own columns.
pub(crate) fn rows_output(
    outcome: &Outcome,
    encoding: &Encoding,
    parts: &[ScoredPart<'_>],
) -> Result<OutputData> {
    let total: usize = parts.iter().map(|p| p.encoded.rows.len()).sum();
    let stride = total.div_ceil(MAX_OUTPUT_ROWS).max(1);
    // Which rows of which part are kept: every `stride`-th of them all, as
    // (part, position among the part's encoded rows).
    let mut kept: Vec<(usize, usize)> = Vec::new();
    let mut by_part: Vec<Vec<usize>> = vec![Vec::new(); parts.len()];
    let mut n = 0usize;
    for (p, part) in parts.iter().enumerate() {
        for i in 0..part.encoded.rows.len() {
            if n % stride == 0 {
                kept.push((p, i));
                by_part[p].push(part.encoded.rows[i]);
            }
            n += 1;
        }
    }

    let label = outcome.label();
    let made = made_columns(outcome);
    let mut columns: Vec<(String, Column)> = Vec::new();
    let mut seen = BTreeSet::new();
    for enc in &encoding.columns {
        let name = enc.column();
        if Some(name) == label || made.contains(&name) || !seen.insert(name.to_owned()) {
            continue;
        }
        let mut pieces = Vec::with_capacity(parts.len());
        for (part, rows) in parts.iter().zip(&by_part) {
            let Some(column) = part.frame.column(name) else {
                return Err(Error::msg(format!(
                    "fit outputs: the feature `{name}` is missing from a split"
                )));
            };
            pieces.push(column.take(rows));
        }
        columns.push((name.to_owned(), Column::concat(pieces)));
    }

    let split: Vec<Option<String>> = kept
        .iter()
        .map(|(p, _)| Some(part_name(parts[*p].part).to_owned()))
        .collect();
    columns.push(("split".to_owned(), Column::Str(split)));

    let target = |p: usize, i: usize| -> Option<f64> {
        parts[p]
            .encoded
            .target
            .as_ref()
            .and_then(|t| t.get(i))
            .copied()
    };
    let prediction = |p: usize, i: usize| parts[p].predictions.get(i);
    match outcome {
        Outcome::Regression { .. } => {
            let actual: Vec<Option<f64>> = kept.iter().map(|(p, i)| target(*p, *i)).collect();
            let fitted: Vec<Option<f64>> = kept
                .iter()
                .map(|(p, i)| match prediction(*p, *i) {
                    Some(Prediction::Number { value }) => Some(*value),
                    _ => None,
                })
                .collect();
            let residual: Vec<Option<f64>> = actual
                .iter()
                .zip(&fitted)
                .map(|(a, f)| Some((*a)? - (*f)?))
                .collect();
            let (standardised, theoretical) = qq(&residual);
            columns.push(("actual".to_owned(), Column::Float(actual)));
            columns.push(("fitted".to_owned(), Column::Float(fitted)));
            columns.push(("residual".to_owned(), Column::Float(residual)));
            columns.push((
                "standardised_residual".to_owned(),
                Column::Float(standardised),
            ));
            columns.push((
                "theoretical_quantile".to_owned(),
                Column::Float(theoretical),
            ));
        }
        Outcome::Classification { .. } => {
            let class = |index: f64| -> Option<String> {
                let target = encoding.target.as_ref()?;
                target.class(index as usize).ok().map(str::to_owned)
            };
            let actual: Vec<Option<String>> = kept
                .iter()
                .map(|(p, i)| target(*p, *i).and_then(class))
                .collect();
            let mut probability = Vec::with_capacity(kept.len());
            let predicted: Vec<Option<String>> = kept
                .iter()
                .map(|(p, i)| match prediction(*p, *i) {
                    Some(Prediction::ClassIndex {
                        index,
                        probability: pr,
                    }) => {
                        probability.push(*pr);
                        class(*index as f64)
                    }
                    Some(Prediction::Class {
                        class,
                        probability: pr,
                    }) => {
                        probability.push(*pr);
                        Some(class.clone())
                    }
                    _ => {
                        probability.push(None);
                        None
                    }
                })
                .collect();
            let correct: Vec<Option<f64>> = actual
                .iter()
                .zip(&predicted)
                .map(|(a, p)| Some(if a.as_ref()? == p.as_ref()? { 1.0 } else { 0.0 }))
                .collect();
            columns.push(("actual".to_owned(), Column::Str(actual)));
            columns.push(("predicted".to_owned(), Column::Str(predicted)));
            if probability.iter().any(Option::is_some) {
                columns.push(("probability".to_owned(), Column::Float(probability)));
            }
            columns.push(("correct".to_owned(), Column::Float(correct)));
        }
        Outcome::Cluster => {
            let clusters: Vec<Option<usize>> = kept
                .iter()
                .map(|(p, i)| match prediction(*p, *i) {
                    Some(Prediction::Cluster { cluster }) => Some(*cluster),
                    _ => None,
                })
                .collect();
            // Text, so that a colour is a category; zero-padded, so that the
            // clusters sort as numbers do.
            let width = clusters
                .iter()
                .flatten()
                .max()
                .map_or(1, |m| m.to_string().len());
            columns.push((
                "cluster".to_owned(),
                Column::Str(
                    clusters
                        .into_iter()
                        .map(|c| c.map(|c| format!("{c:0width$}")))
                        .collect(),
                ),
            ));
        }
        Outcome::Embedding { dimensions } => {
            for d in 0..(*dimensions).min(MAX_COMPONENTS) {
                let values: Vec<Option<f64>> = kept
                    .iter()
                    .map(|(p, i)| match prediction(*p, *i) {
                        Some(Prediction::Vector { values }) => values.get(d).copied(),
                        _ => None,
                    })
                    .collect();
                columns.push((format!("component_{}", d + 1), Column::Float(values)));
            }
        }
        Outcome::Test | Outcome::Posterior { .. } => {}
    }
    Ok(OutputData {
        frame: Frame::new(columns, Vec::new())?,
        total,
    })
}

/// The columns [`rows_output`] makes for `outcome`, which a dataset column of
/// the same name gives way to.
fn made_columns(outcome: &Outcome) -> Vec<&'static str> {
    let mut made = vec!["split"];
    made.extend(match outcome {
        Outcome::Regression { .. } => &[
            "actual",
            "fitted",
            "residual",
            "standardised_residual",
            "theoretical_quantile",
        ][..],
        Outcome::Classification { .. } => &["actual", "predicted", "probability", "correct"][..],
        Outcome::Cluster => &["cluster"][..],
        Outcome::Embedding { .. } => &[
            "component_1",
            "component_2",
            "component_3",
            "component_4",
            "component_5",
            "component_6",
            "component_7",
            "component_8",
            "component_9",
            "component_10",
        ][..],
        Outcome::Test | Outcome::Posterior { .. } => &[][..],
    });
    made
}

fn part_name(part: Part) -> &'static str {
    match part {
        Part::Train => "train",
        Part::Validation => "validation",
        Part::Test => "test",
    }
}

/// The residuals standardised by their standard deviation, and each one's
/// normal quantile at its rank — R's `qqnorm`, with `ppoints`'
/// `(i − a) / (n + 1 − 2a)`, `a` being 3/8 up to ten values and 1/2 above.
fn qq(residuals: &[Option<f64>]) -> (Vec<Option<f64>>, Vec<Option<f64>>) {
    let present: Vec<f64> = residuals.iter().flatten().copied().collect();
    let n = present.len();
    if n < 2 {
        return (vec![None; residuals.len()], vec![None; residuals.len()]);
    }
    let mean = present.iter().sum::<f64>() / n as f64;
    let sd = (present.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt();
    let standardised: Vec<Option<f64>> = residuals
        .iter()
        .map(|r| r.map(|r| if sd > 0.0 { r / sd } else { 0.0 }))
        .collect();
    let mut order: Vec<usize> = (0..residuals.len())
        .filter(|i| standardised[*i].is_some())
        .collect();
    order.sort_by(|a, b| {
        standardised[*a]
            .unwrap_or_default()
            .total_cmp(&standardised[*b].unwrap_or_default())
    });
    let a = if n <= 10 { 0.375 } else { 0.5 };
    let normal = Normal::standard();
    let mut theoretical = vec![None; residuals.len()];
    for (rank, i) in order.into_iter().enumerate() {
        let p = (rank as f64 + 1.0 - a) / (n as f64 + 1.0 - 2.0 * a);
        theoretical[i] = Some(normal.inverse_cdf(p));
    }
    (standardised, theoretical)
}

/// The [`DRAWS_OUTPUT`] frame of a posterior: the post-warmup draws of at most
/// [`MAX_OUTPUT_PARAMETERS`] parameters (the program's `parameters` block when
/// its interface is known, scalars first; otherwise every variable that is not
/// the sampler's `…__`), each chain thinned to at most
/// [`MAX_DRAWS_PER_CHAIN`], with each draw's rank among them all.
///
/// `None` when there is nothing to plot.
pub(crate) fn draws_output(
    draws: &[DrawSeries],
    interface: Option<&Interface>,
) -> Result<Option<OutputData>> {
    let declared: Option<BTreeSet<&str>> =
        interface.map(|i| i.parameters.iter().map(|d| d.name.as_str()).collect());
    let wanted = |s: &DrawSeries| {
        !s.warmup
            && !s.variable.ends_with("__")
            && declared
                .as_ref()
                .is_none_or(|d| d.contains(s.variable.as_str()))
    };
    // The elements, in the order they were drawn, scalars first.
    let mut elements: Vec<(&str, &[usize])> = Vec::new();
    for s in draws.iter().filter(|s| wanted(s)) {
        let key = (s.variable.as_str(), s.element.as_slice());
        if !elements.contains(&key) {
            elements.push(key);
        }
    }
    elements.sort_by_key(|(_, e)| !e.is_empty());
    elements.truncate(MAX_OUTPUT_PARAMETERS);
    if elements.is_empty() {
        return Ok(None);
    }

    let mut parameter = Vec::new();
    let mut chain = Vec::new();
    let mut iteration = Vec::new();
    let mut value = Vec::new();
    let mut rank = Vec::new();
    let mut total = 0usize;
    for (variable, element) in &elements {
        let series: Vec<&DrawSeries> = draws
            .iter()
            .filter(|s| wanted(s) && s.variable == *variable && s.element == *element)
            .collect();
        let start = value.len();
        let mut values_here: Vec<f64> = Vec::new();
        for s in &series {
            total += s.draws.len();
            let stride = s.draws.len().div_ceil(MAX_DRAWS_PER_CHAIN).max(1);
            for (i, v) in s.draws.iter().enumerate().step_by(stride) {
                parameter.push(Some(s.label()));
                chain.push(Some(s.chain.to_string()));
                iteration.push(Some(i as i64 + 1));
                value.push(v.is_finite().then_some(*v));
                values_here.push(*v);
            }
        }
        // The rank of each draw among this parameter's kept draws, 1-based,
        // ties broken by order — what a rank plot bins.
        let mut order: Vec<usize> = (0..values_here.len()).collect();
        order.sort_by(|a, b| values_here[*a].total_cmp(&values_here[*b]));
        let mut ranks = vec![0.0; values_here.len()];
        for (r, i) in order.into_iter().enumerate() {
            ranks[i] = r as f64 + 1.0;
        }
        rank.extend(ranks.into_iter().map(Some));
        debug_assert_eq!(rank.len(), value.len(), "ranks out of step at {start}");
    }
    let frame = Frame::new(
        vec![
            ("parameter".to_owned(), Column::Str(parameter)),
            ("chain".to_owned(), Column::Str(chain)),
            ("iteration".to_owned(), Column::Int(iteration)),
            ("value".to_owned(), Column::Float(value)),
            ("rank".to_owned(), Column::Float(rank)),
        ],
        Vec::new(),
    )?;
    Ok(Some(OutputData { frame, total }))
}

/// A parameter block's name as an output's key: lower case, words joined by
/// `_`.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let out = out.trim_end_matches('_').to_owned();
    if out.is_empty() {
        "table".to_owned()
    } else {
        out
    }
}

fn unique(seen: &mut BTreeSet<String>, base: &str) -> String {
    let mut name = base.to_owned();
    let mut n = 2;
    while !seen.insert(name.clone()) {
        name = format!("{base}_{n}");
        n += 1;
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ParameterRow;

    fn ctx<'a>(
        outcome: &'a Outcome,
        features: &'a [(String, ColumnType)],
        parameters: &'a [ParameterBlock],
        data: &'a BTreeMap<String, OutputData>,
        configuration: &'a Attrs,
    ) -> OutputContext<'a> {
        OutputContext {
            outcome,
            configuration,
            features,
            parameters,
            data,
        }
    }

    fn rows_with(columns: &[(&str, Column)]) -> BTreeMap<String, OutputData> {
        let frame = Frame::new(
            columns
                .iter()
                .map(|(n, c)| ((*n).to_owned(), c.clone()))
                .collect(),
            Vec::new(),
        )
        .unwrap();
        BTreeMap::from([(ROWS_OUTPUT.to_owned(), OutputData::whole(frame))])
    }

    #[test]
    fn a_declaration_round_trips_as_the_json_a_module_writes() {
        let json = json!({
            "name": "residuals", "label": "Residuals", "optional": true,
            "kind": "plot", "data": "rows",
            "spec": { "layers": [ { "mark": "point" } ] }
        });
        let decl: OutputDecl = serde_json::from_value(json.clone()).unwrap();
        assert!(decl.optional);
        assert_eq!(decl.data(), Some("rows"));
        assert_eq!(serde_json::to_value(&decl).unwrap(), json);

        let table: OutputDecl = serde_json::from_value(
            json!({ "name": "c", "label": "Coefficients", "kind": "parameters", "block": "Coefficients" }),
        )
        .unwrap();
        assert!(!table.optional);
        assert_eq!(
            table.kind,
            OutputKind::Parameters {
                block: "Coefficients".to_owned()
            }
        );
    }

    #[test]
    fn output_data_round_trips_with_its_total() {
        let frame = Frame::new(
            vec![("x".to_owned(), Column::Float(vec![Some(1.0), None]))],
            Vec::new(),
        )
        .unwrap();
        let data = OutputData { frame, total: 7 };
        assert!(data.sampled());
        let back: OutputData =
            serde_json::from_value(serde_json::to_value(&data).unwrap()).unwrap();
        assert_eq!(back, data);
        // A module's frame has no total: it stood for its own rows.
        let bare = OutputData::from_json(&data.frame.to_json()).unwrap();
        assert_eq!(bare.total, 2);
        assert!(!bare.sampled());
    }

    #[test]
    fn a_regression_shows_its_tables_and_two_plots_with_two_more_on_request() {
        let outcome = Outcome::Regression {
            label: "price".to_owned(),
        };
        let parameters = vec![
            ParameterBlock::table(
                "Coefficients",
                ["term", "estimate"],
                vec![ParameterRow::new([json!("area"), json!(2.0)])],
            )
            .unwrap(),
            ParameterBlock::scalar("R²", 0.5),
        ];
        let data = rows_with(&[("fitted", Column::Float(vec![Some(1.0)]))]);
        let config = Attrs::new();
        let outputs = standard_outputs(&ctx(&outcome, &[], &parameters, &data, &config));
        let names: Vec<(&str, bool)> = outputs
            .iter()
            .map(|o| (o.name.as_str(), o.optional))
            .collect();
        assert_eq!(
            names,
            [
                ("coefficients", false),
                ("statistics", false),
                ("metrics", false),
                ("residuals_fitted", false),
                ("actual_predicted", false),
                ("qq", true),
                ("residual_histogram", true),
            ]
        );
        // Without the rows, there is nothing to plot.
        let none = BTreeMap::new();
        let outputs = standard_outputs(&ctx(&outcome, &[], &parameters, &none, &config));
        assert!(outputs.iter().all(|o| o.data().is_none()));
    }

    #[test]
    fn a_clustering_plots_its_first_two_numeric_features() {
        let features = vec![
            ("region".to_owned(), ColumnType::Str),
            ("area".to_owned(), ColumnType::Float),
            ("price".to_owned(), ColumnType::Int),
        ];
        let data = rows_with(&[("cluster", Column::Str(vec![Some("0".to_owned())]))]);
        let config = Attrs::new();
        let outputs = outcome_plots(&ctx(&Outcome::Cluster, &features, &[], &data, &config));
        assert_eq!(outputs.len(), 2);
        let OutputKind::Plot { spec, .. } = &outputs[1].kind else {
            panic!("a plot")
        };
        assert_eq!(spec["layers"][0]["encoding"]["x"]["field"], "area");
        assert_eq!(spec["layers"][0]["encoding"]["y"]["field"], "price");
        assert_eq!(spec["layers"][0]["encoding"]["color"]["field"], "cluster");
    }

    #[test]
    fn duplicate_output_names_keep_the_first() {
        let tidy = tidy_outputs(vec![
            OutputDecl::metrics(),
            OutputDecl::parameters(" metrics ", "Coefficients"),
            OutputDecl::parameters("", "Coefficients"),
        ]);
        assert_eq!(tidy.len(), 1);
        assert_eq!(tidy[0].kind, OutputKind::Metrics);
    }

    #[test]
    fn the_qq_quantiles_are_rs_ppoints() {
        let (standardised, theoretical) = qq(&[Some(3.0), Some(-1.0), None, Some(1.0)]);
        // n = 3: (i − 3/8) / (3 + 1/4).
        let normal = Normal::standard();
        let p = |i: f64| normal.inverse_cdf((i - 0.375) / 3.25);
        assert_eq!(theoretical[2], None);
        assert!((theoretical[1].unwrap() - p(1.0)).abs() < 1e-12);
        assert!((theoretical[3].unwrap() - p(2.0)).abs() < 1e-12);
        assert!((theoretical[0].unwrap() - p(3.0)).abs() < 1e-12);
        // sd of 3, −1, 1 is 2.
        assert_eq!(standardised[0], Some(1.5));
    }

    #[test]
    fn a_posteriors_draws_are_thinned_ranked_and_scalars_come_first() {
        let long: Vec<f64> = (0..1200).map(f64::from).collect();
        let draws = vec![
            DrawSeries::new("alpha", vec![1], 1, vec![3.0, 1.0, 2.0]),
            DrawSeries::new("sigma", vec![], 1, long.clone()),
            DrawSeries::new("sigma", vec![], 2, long),
            DrawSeries::new("lp__", vec![], 1, vec![0.0; 3]),
            DrawSeries::new("alpha", vec![1], 1, vec![9.0]).warmup(),
        ];
        let out = draws_output(&draws, None).unwrap().unwrap();
        assert_eq!(out.total, 2403);
        let Column::Str(parameter) = out.frame.column("parameter").unwrap() else {
            panic!("text")
        };
        // sigma first (a scalar), 400 a chain after thinning by 3; then alpha[1].
        assert_eq!(parameter[0].as_deref(), Some("sigma"));
        assert_eq!(parameter.len(), 800 + 3);
        assert_eq!(parameter[800].as_deref(), Some("alpha[1]"));
        let Column::Float(rank) = out.frame.column("rank").unwrap() else {
            panic!("float")
        };
        assert_eq!(&rank[800..], &[Some(3.0), Some(1.0), Some(2.0)]);
        // Nothing but sampler variables: nothing to plot.
        assert!(
            draws_output(&[DrawSeries::new("lp__", vec![], 1, vec![1.0])], None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_block_name_becomes_a_key() {
        assert_eq!(slug("Cluster centres"), "cluster_centres");
        assert_eq!(
            slug("Analysis of variance (one-way)"),
            "analysis_of_variance_one_way"
        );
        assert_eq!(slug("—"), "table");
    }
}

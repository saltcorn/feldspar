//! Reading a fitted posterior back out (Stan TODO §16): its draws by key or
//! label, a summary of any variable on demand, the write-back's rows, and the
//! draws as CmdStan-shaped CSV.
//!
//! **Positions never leave the host.** A request names elements by the keys
//! and labels of this instance's own coordinates (`{ "counties": ["27001"] }`),
//! or by position for a numbered axis; an answer carries the keys and labels
//! beside every position it mentions. The axes are the ones the fit recorded
//! ([`ATTR_AXES`](crate::ATTR_AXES)), so editing the model afterwards changes
//! the model and never how an existing instance reads.
//!
//! Everything here is a function of the instance row and `_fd_model_draws`,
//! so the admin API and a code body's `models` global (its handle's
//! `m.writePosterior` among them) all answer the same question the same way.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;

use crate::bind::{Axis, Coordinates, DimensionKind, RecordedAxes, element_label, named_axes};
use crate::diagnose::{MCMC_COLUMNS, MODE_COLUMNS};
use crate::draws::{DrawsQuery, DrawsReader};
use crate::fit::{ATTR_AXES, ATTR_COORDINATES};
use crate::instance::{FitStatus, InstanceId, ModelInstance};
use crate::metrics::{Metrics, SplitMetrics};
use crate::posterior::{DrawSeries, PosteriorMethod};
use crate::provider::ParameterBlock;
use crate::summary::ElementSummary;

/// The default ceiling on the numbers one draws response carries
/// (`--stan-max-draws-response`): 2 million, about 25 MB of JSON.
pub const DEFAULT_MAX_DRAWS_RESPONSE: u64 = 2_000_000;

/// Which elements of a variable a request is about.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Selection {
    /// Every element.
    #[default]
    All,
    /// These 1-based index arrays.
    Positions(Vec<Vec<usize>>),
    /// Per axis (by its heading, its dimension's name, or its 1-based
    /// ordinal), the keys or labels wanted; an axis not named is taken whole.
    ByAxis(BTreeMap<String, Vec<Json>>),
}

impl Selection {
    /// A selection as a request writes it: absent or `null` for every element;
    /// a list of index arrays (`[[1], [3]]`, or `[1, 3]` for a one-axis
    /// variable); or an object from an axis to the keys or labels wanted
    /// (`{ "counties": ["27001", "Aitkin"] }`).
    pub fn from_json(json: Option<&Json>) -> Result<Selection> {
        let wrong = || {
            Error::invalid(
                "`elements` is a list of index arrays (`[[1], [3]]`) or an object from an axis \
                 to the keys or labels wanted (`{ \"counties\": [\"27001\"] }`)",
            )
        };
        match json {
            None | Some(Json::Null) => Ok(Selection::All),
            Some(Json::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Json::Array(index) => index
                        .iter()
                        .map(|i| {
                            i.as_u64()
                                .and_then(|i| usize::try_from(i).ok())
                                .ok_or_else(wrong)
                        })
                        .collect(),
                    other => other
                        .as_u64()
                        .and_then(|i| usize::try_from(i).ok())
                        .map(|i| vec![i])
                        .ok_or_else(wrong),
                })
                .collect::<Result<Vec<_>>>()
                .map(Selection::Positions),
            Some(Json::Object(map)) => Ok(Selection::ByAxis(
                map.iter()
                    .map(|(axis, wanted)| {
                        let wanted = match wanted {
                            Json::Array(values) => values.clone(),
                            one => vec![one.clone()],
                        };
                        (axis.clone(), wanted)
                    })
                    .collect(),
            )),
            Some(_) => Err(wrong()),
        }
    }
}

/// The text a key is compared by: a string as itself, anything else as JSON.
fn key_text(key: &Json) -> String {
    match key {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// One output variable of a fitted instance, as its recorded axes and this
/// instance's coordinates label it.
#[derive(Debug, Clone, PartialEq)]
pub struct PosteriorView {
    /// The variable.
    pub variable: String,
    /// Positions per axis.
    pub dims: Vec<usize>,
    /// Each axis's heading, dimension, labels and keys.
    pub axes: Vec<Axis>,
    /// How the draws were made.
    pub method: PosteriorMethod,
}

/// The output variables `instance` recorded, sampler variables included, in
/// name order.
pub fn posterior_variables(instance: &ModelInstance) -> Vec<String> {
    recorded(instance)
        .map(|r| r.into_keys().collect())
        .unwrap_or_default()
}

/// The instance's recorded axes, when it has any.
fn recorded(instance: &ModelInstance) -> Option<BTreeMap<String, RecordedAxes>> {
    serde_json::from_value(instance.attributes.get(ATTR_AXES)?.clone()).ok()
}

/// The instance's coordinates (empty when nothing was bound).
pub fn instance_coordinates(instance: &ModelInstance) -> Result<Coordinates> {
    match instance.attributes.get(ATTR_COORDINATES) {
        None | Some(Json::Null) => Ok(Coordinates::default()),
        Some(json) => serde_json::from_value(json.clone()).map_err(|e| {
            Error::msg(format!(
                "instance {}: its coordinates are unreadable: {e}",
                instance.id
            ))
        }),
    }
}

/// How `instance`'s draws were made, from its metrics.
pub fn posterior_method(instance: &ModelInstance) -> PosteriorMethod {
    let metrics = SplitMetrics::from_json(&instance.metrics).ok();
    match metrics.as_ref().and_then(|m| m.train.as_ref()) {
        Some(Metrics::PosteriorMode(_)) => PosteriorMethod::Mode,
        Some(Metrics::PosteriorApproximation(_)) => PosteriorMethod::Approximation,
        _ => PosteriorMethod::Mcmc,
    }
}

/// Chains and post-warmup draws per chain, from `instance`'s metrics.
fn draws_shape(instance: &ModelInstance) -> Option<(usize, usize)> {
    let metrics = SplitMetrics::from_json(&instance.metrics).ok()?;
    match metrics.train? {
        Metrics::Posterior(m) => Some((m.chains, m.draws_per_chain)),
        Metrics::PosteriorApproximation(m) => Some((1, m.draws)),
        Metrics::PosteriorMode(_) => Some((1, 1)),
        _ => None,
    }
}

impl PosteriorView {
    /// `variable` of `instance`, refused by name when the instance is not a
    /// fitted posterior or drew no such variable.
    pub fn of(instance: &ModelInstance, variable: &str) -> Result<PosteriorView> {
        if instance.status != FitStatus::Fitted {
            return Err(Error::invalid(format!(
                "instance {} is `{}`, so it has no posterior to read",
                instance.id, instance.status
            )));
        }
        let Some(all) = recorded(instance) else {
            return Err(Error::invalid(format!(
                "instance {} is not a posterior: it has no draws to read",
                instance.id
            )));
        };
        let Some(recorded) = all.get(variable) else {
            let program: Vec<String> = all
                .keys()
                .filter(|v| !v.ends_with("__"))
                .map(|v| format!("`{v}`"))
                .collect();
            return Err(Error::not_found(format!(
                "instance {} has no variable `{variable}`{}",
                instance.id,
                if program.is_empty() {
                    String::new()
                } else {
                    format!(" (its variables are {})", program.join(", "))
                }
            )));
        };
        let coordinates = instance_coordinates(instance)?;
        Ok(PosteriorView {
            variable: variable.to_owned(),
            dims: recorded.dims.clone(),
            axes: named_axes(&coordinates, recorded),
            method: posterior_method(instance),
        })
    }

    /// How many elements: the product of the dims (1 for a scalar).
    pub fn size(&self) -> usize {
        self.dims.iter().product()
    }

    /// Every element, outer axis slowest — the order the summary tables and
    /// every answer here list them in.
    pub fn elements(&self) -> Vec<Vec<usize>> {
        let mut out = vec![Vec::new()];
        for &n in &self.dims {
            out = out
                .into_iter()
                .flat_map(|prefix| {
                    (1..=n).map(move |i| {
                        let mut e = prefix.clone();
                        e.push(i);
                        e
                    })
                })
                .collect();
        }
        out
    }

    /// The elements `selection` names, in the order [`elements`](Self::elements)
    /// lists them (for a selection by axis) or as given (by position) — each
    /// refused by name when it is not one of this variable's.
    pub fn select(&self, selection: &Selection) -> Result<Vec<Vec<usize>>> {
        match selection {
            Selection::All => Ok(self.elements()),
            Selection::Positions(positions) => {
                for element in positions {
                    let fits = element.len() == self.dims.len()
                        && element
                            .iter()
                            .zip(&self.dims)
                            .all(|(i, n)| (1..=*n).contains(i));
                    if !fits {
                        return Err(Error::invalid(format!(
                            "`{}` has no element [{}]: its shape is [{}]",
                            self.variable,
                            element
                                .iter()
                                .map(usize::to_string)
                                .collect::<Vec<_>>()
                                .join(", "),
                            self.dims
                                .iter()
                                .map(usize::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )));
                    }
                }
                Ok(positions.clone())
            }
            Selection::ByAxis(wanted) => {
                let mut per_axis: Vec<Option<BTreeSet<usize>>> = vec![None; self.axes.len()];
                for (name, values) in wanted {
                    // By heading, by dimension, or by its 1-based ordinal —
                    // `{"1": [...]}` is the first axis whatever it is called,
                    // which is what a code body's `{ keys: [...] }` sends.
                    let k = self
                        .axes
                        .iter()
                        .position(|a| &a.name == name || a.dimension.as_deref() == Some(name))
                        .or_else(|| {
                            name.parse::<usize>()
                                .ok()
                                .filter(|k| (1..=self.axes.len()).contains(k))
                                .map(|k| k - 1)
                        })
                        .ok_or_else(|| {
                            Error::invalid(format!(
                                "`{}` has no axis `{name}` (its axes are {})",
                                self.variable,
                                if self.axes.is_empty() {
                                    "none: it is a scalar".to_owned()
                                } else {
                                    self.axes
                                        .iter()
                                        .map(|a| format!("`{}`", a.name))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                }
                            ))
                        })?;
                    let mut chosen = BTreeSet::new();
                    for value in values {
                        chosen.insert(self.position_of(k, value)?);
                    }
                    per_axis[k] = Some(chosen);
                }
                Ok(self
                    .elements()
                    .into_iter()
                    .filter(|e| {
                        e.iter()
                            .zip(&per_axis)
                            .all(|(i, chosen)| chosen.as_ref().is_none_or(|c| c.contains(i)))
                    })
                    .collect())
            }
        }
    }

    /// The 1-based position on axis `k` of `value` — a key first, then a
    /// label, then (for a numbered axis) a position.
    fn position_of(&self, k: usize, value: &Json) -> Result<usize> {
        let axis = &self.axes[k];
        let text = key_text(value);
        if let Some(i) = axis.keys.iter().position(|key| key_text(key) == text) {
            return Ok(i + 1);
        }
        if let Some(i) = axis.labels.iter().position(|label| *label == text) {
            return Ok(i + 1);
        }
        if axis.labels.is_empty() {
            if let Some(i) = value
                .as_u64()
                .or_else(|| text.parse().ok())
                .and_then(|i| usize::try_from(i).ok())
                .filter(|i| (1..=self.dims[k]).contains(i))
            {
                return Ok(i);
            }
        }
        Err(Error::invalid(format!(
            "`{text}` is not {} of `{}`{}",
            if axis.labels.is_empty() {
                format!("a position (1 to {}) on axis `{}`", self.dims[k], axis.name)
            } else {
                format!("a key or a label of `{}`", axis.name)
            },
            self.variable,
            if axis.dimension.is_some() {
                " in this fit"
            } else {
                ""
            }
        )))
    }

    /// `alpha[Aitkin]`.
    pub fn name(&self, element: &[usize]) -> String {
        element_label(&self.variable, element, &self.axes)
    }

    /// Per axis, the element's label (or its position on a numbered axis).
    pub fn cells(&self, element: &[usize]) -> Vec<Json> {
        element
            .iter()
            .enumerate()
            .map(|(k, i)| {
                self.axes
                    .get(k)
                    .map_or_else(|| Json::from(*i), |a| a.cell(*i))
            })
            .collect()
    }

    /// Per axis, the element's key: the dimension's key, a design's column
    /// name, or the position on a numbered axis.
    pub fn keys(&self, element: &[usize]) -> Vec<Json> {
        element
            .iter()
            .enumerate()
            .map(|(k, i)| match self.axes.get(k) {
                Some(a) if !a.keys.is_empty() => a.keys[i - 1].clone(),
                Some(a) if !a.labels.is_empty() => Json::from(a.labels[i - 1].clone()),
                _ => Json::from(*i),
            })
            .collect()
    }

    /// Per axis, every position's label (or number) — the `labels` of an
    /// answer.
    pub fn axis_labels(&self) -> Vec<Vec<Json>> {
        self.dims
            .iter()
            .enumerate()
            .map(|(k, &n)| (1..=n).map(|i| self.axes[k].cell(i)).collect())
            .collect()
    }

    /// Per axis, every position's key — the `keys` of an answer.
    pub fn axis_keys(&self) -> Vec<Vec<Json>> {
        self.dims
            .iter()
            .enumerate()
            .map(|(k, &n)| {
                (1..=n)
                    .map(|i| {
                        let mut e = vec![1; self.dims.len()];
                        e[k] = i;
                        self.keys(&e)[k].clone()
                    })
                    .collect()
            })
            .collect()
    }
}

/// What a draws request asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawsRequest {
    /// The variable.
    pub variable: String,
    /// Which of its elements.
    pub selection: Selection,
    /// Only these chains; `None` for all of them.
    pub chains: Option<Vec<u32>>,
    /// Whether the warmup draws come back too, as series of their own.
    pub warmup: bool,
    /// Keep every `thin`-th draw (1 for all of them).
    pub thin: usize,
}

impl DrawsRequest {
    /// Every post-warmup draw of every element of `variable`.
    pub fn variable(variable: impl Into<String>) -> DrawsRequest {
        DrawsRequest {
            variable: variable.into(),
            selection: Selection::All,
            chains: None,
            warmup: false,
            thin: 1,
        }
    }
}

/// One chain's draws in an answer: one array per selected element, in the
/// answer's element order.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ChainDraws {
    /// The chain, from 1.
    pub chain: u32,
    /// Whether these are its warmup iterations.
    pub warmup: bool,
    /// Per element, the draws in iteration order. NaN is `null`.
    pub draws: Vec<Vec<f64>>,
}

/// A variable's draws, labelled — what `getModelDraws` and a handle's `m.draws`
/// answer.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct VariableDraws {
    /// The variable.
    pub variable: String,
    /// Positions per axis — the whole variable's, whatever was selected.
    pub dims: Vec<usize>,
    /// Each axis's heading: its dimension's name, or `index`.
    pub axes: Vec<String>,
    /// Per axis, every position's label.
    pub labels: Vec<Vec<Json>>,
    /// Per axis, every position's key.
    pub keys: Vec<Vec<Json>>,
    /// The selected elements, as 1-based index arrays, in the order each
    /// chain's `draws` lists them.
    pub elements: Vec<Vec<usize>>,
    /// The same elements by name — `alpha[Aitkin]`.
    pub names: Vec<String>,
    /// Every `thin`-th draw was kept.
    pub thin: usize,
    /// The chains.
    pub chains: Vec<ChainDraws>,
}

/// The "too big a response" refusal, with the arithmetic.
fn too_many(numbers: u64, max: u64, elements: usize, chains: usize, draws: usize) -> Error {
    Error::invalid(format!(
        "that is {numbers} numbers ({elements} element{} × {chains} chain{} × {draws} draws), \
         more than the {max} one response may carry (`--stan-max-draws-response`): select fewer \
         elements or chains, or keep every n-th draw (`thin`)",
        if elements == 1 { "" } else { "s" },
        if chains == 1 { "" } else { "s" },
    ))
}

/// Read the draws `request` asks for, labelled, refusing an answer of more
/// than `max_numbers` numbers — before reading where the instance's metrics
/// say how many there are, and after in any case.
pub async fn read_draws(
    catalog: &Catalog,
    instance: &ModelInstance,
    request: &DrawsRequest,
    max_numbers: u64,
) -> Result<VariableDraws> {
    let view = PosteriorView::of(instance, &request.variable)?;
    let elements = view.select(&request.selection)?;
    let thin = request.thin.max(1);
    if let Some((chains, per_chain)) = draws_shape(instance) {
        let chains = request.chains.as_ref().map_or(chains, Vec::len);
        let draws = per_chain.div_ceil(thin);
        let numbers = (elements.len() * chains * draws) as u64;
        if numbers > max_numbers {
            return Err(too_many(
                numbers,
                max_numbers,
                elements.len(),
                chains,
                draws,
            ));
        }
    }

    let series = read_series(catalog, instance.id, &view, &elements, request).await?;
    let at: HashMap<&[usize], usize> = elements
        .iter()
        .enumerate()
        .map(|(i, e)| (e.as_slice(), i))
        .collect();
    let mut chains: BTreeMap<(bool, u32), Vec<Vec<f64>>> = BTreeMap::new();
    for s in &series {
        let Some(&i) = at.get(s.element.as_slice()) else {
            continue;
        };
        let draws = chains
            .entry((s.warmup, s.chain))
            .or_insert_with(|| vec![Vec::new(); elements.len()]);
        draws[i] = s.draws.iter().step_by(thin).copied().collect();
    }
    let numbers: u64 = chains.values().flatten().map(|d| d.len() as u64).sum();
    if numbers > max_numbers {
        let most = chains.values().flatten().map(Vec::len).max().unwrap_or(0);
        return Err(too_many(
            numbers,
            max_numbers,
            elements.len(),
            chains.len(),
            most,
        ));
    }
    // Warmup first, as CmdStan writes it.
    let mut out: Vec<ChainDraws> = chains
        .into_iter()
        .map(|((warmup, chain), draws)| ChainDraws {
            chain,
            warmup,
            draws,
        })
        .collect();
    out.sort_by_key(|c| (c.chain, !c.warmup));
    Ok(VariableDraws {
        variable: view.variable.clone(),
        dims: view.dims.clone(),
        axes: view.axes.iter().map(|a| a.name.clone()).collect(),
        labels: view.axis_labels(),
        keys: view.axis_keys(),
        names: elements.iter().map(|e| view.name(e)).collect(),
        elements,
        thin,
        chains: out,
    })
}

/// The stored series of `elements` of the view's variable, refused by name
/// when the instance kept none of them.
async fn read_series(
    catalog: &Catalog,
    instance: InstanceId,
    view: &PosteriorView,
    elements: &[Vec<usize>],
    request: &DrawsRequest,
) -> Result<Vec<DrawSeries>> {
    if elements.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = DrawsQuery::variable(view.variable.clone());
    if elements.len() < view.size() {
        query = query.elements(elements.to_vec());
    }
    if let Some(chains) = &request.chains {
        query = query.chains(chains.clone());
    }
    if request.warmup {
        query = query.with_warmup();
    }
    let series = DrawsReader::new(catalog, instance).read(&query).await?;
    if series.is_empty() && request.chains.as_ref().is_none_or(|c| !c.is_empty()) {
        return Err(not_kept(&view.variable));
    }
    Ok(series)
}

fn not_kept(variable: &str) -> Error {
    Error::invalid(format!(
        "the draws of `{variable}` were not kept (the model keeps only the summary, \
         `keep_draws: false`, or leaves `{variable}` out, `exclude_variables`); its summary is on \
         the instance"
    ))
}

/// A variable's summary, per selected element — what `getPosteriorSummary`
/// and a handle's `m.summary` answer, and what the write-back writes.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct VariableSummary {
    /// The variable.
    pub variable: String,
    /// `draws` when it was computed now from the stored draws; `stored` when
    /// the draws were not kept and this is the table the fit stored.
    pub source: &'static str,
    /// The headings: one per axis, then the statistics.
    pub columns: Vec<String>,
    /// The selected elements, as 1-based index arrays.
    pub elements: Vec<Vec<usize>>,
    /// The same elements by name.
    pub names: Vec<String>,
    /// Per element, its keys (one per axis).
    pub keys: Vec<Vec<Json>>,
    /// Per element, the cells under `columns`: its labels, then its
    /// statistics. An undefined statistic is `null`.
    pub rows: Vec<Vec<Json>>,
}

impl VariableSummary {
    /// Statistic `name` of row `row`, when it is a column and a number.
    pub fn statistic(&self, row: usize, name: &str) -> Option<f64> {
        let k = self.columns.iter().position(|c| c == name)?;
        self.rows.get(row)?.get(k)?.as_f64()
    }

    /// The statistics this summary has, after the label columns.
    pub fn statistics(&self) -> &[String] {
        let labels = self.elements.first().map_or(0, Vec::len);
        &self.columns[labels.min(self.columns.len())..]
    }
}

/// The statistic names a summary of draws made by `method` has.
pub fn statistic_names(method: PosteriorMethod) -> &'static [&'static str] {
    match method {
        PosteriorMethod::Mode => &MODE_COLUMNS,
        _ => &MCMC_COLUMNS,
    }
}

/// Summarise the `selection` of `variable` from its stored draws — or, when
/// they were not kept, answer the table the fit stored for it.
pub async fn summarise_variable(
    catalog: &Catalog,
    instance: &ModelInstance,
    variable: &str,
    selection: &Selection,
) -> Result<VariableSummary> {
    let view = PosteriorView::of(instance, variable)?;
    let elements = view.select(selection)?;
    let columns: Vec<String> = view
        .axes
        .iter()
        .map(|a| a.name.clone())
        .chain(statistic_names(view.method).iter().map(|c| (*c).to_owned()))
        .collect();
    let request = DrawsRequest {
        variable: variable.to_owned(),
        selection: selection.clone(),
        chains: None,
        warmup: false,
        thin: 1,
    };
    let series = match read_series(catalog, instance.id, &view, &elements, &request).await {
        Ok(series) => series,
        Err(_) => return stored_summary(instance, &view, &elements, columns),
    };
    let mut by_element: HashMap<&[usize], Vec<(u32, &[f64])>> = HashMap::new();
    for s in &series {
        by_element
            .entry(s.element.as_slice())
            .or_default()
            .push((s.chain, s.draws.as_slice()));
    }
    let mut rows = Vec::with_capacity(elements.len());
    for element in &elements {
        let mut chains = by_element.remove(element.as_slice()).unwrap_or_default();
        chains.sort_by_key(|(c, _)| *c);
        let chains: Vec<&[f64]> = chains.into_iter().map(|(_, d)| d).collect();
        let stats: Vec<Json> = match view.method {
            PosteriorMethod::Mode => vec![number(chains.first().and_then(|c| c.first()).copied())],
            method => {
                let mut s = ElementSummary::of(&chains);
                if method == PosteriorMethod::Approximation {
                    s = s.without_rhat();
                }
                [
                    s.mean,
                    s.sd,
                    s.mcse_mean,
                    s.q5,
                    s.q50,
                    s.q95,
                    s.rhat,
                    s.ess_bulk,
                    s.ess_tail,
                ]
                .into_iter()
                .map(|x| number(Some(x)))
                .collect()
            }
        };
        rows.push(view.cells(element).into_iter().chain(stats).collect());
    }
    Ok(VariableSummary {
        variable: view.variable.clone(),
        source: "draws",
        names: elements.iter().map(|e| view.name(e)).collect(),
        keys: elements.iter().map(|e| view.keys(e)).collect(),
        elements,
        columns,
        rows,
    })
}

/// A number as JSON, NaN and the infinities as `null`.
fn number(x: Option<f64>) -> Json {
    x.and_then(serde_json::Number::from_f64)
        .map_or(Json::Null, Json::Number)
}

/// The fit's stored summary table of the view's variable, restricted to
/// `elements` — the answer when the draws were not kept.
fn stored_summary(
    instance: &ModelInstance,
    view: &PosteriorView,
    elements: &[Vec<usize>],
    columns: Vec<String>,
) -> Result<VariableSummary> {
    let table = instance.parameters.iter().find_map(|block| match block {
        ParameterBlock::Table { name, rows, .. } if *name == view.variable => Some(rows),
        _ => None,
    });
    let all = view.elements();
    let Some(rows) = table.filter(|rows| rows.len() == all.len()) else {
        return Err(Error::invalid(format!(
            "the draws of `{}` were not kept, and it has no stored summary: it has {} elements, \
             more than a fit summarises (`--stan-summary-max-elements`); keep its draws to \
             summarise it on demand",
            view.variable,
            view.size()
        )));
    };
    let at: HashMap<&[usize], usize> = all
        .iter()
        .enumerate()
        .map(|(i, e)| (e.as_slice(), i))
        .collect();
    let rows = elements
        .iter()
        .map(|e| rows[at[e.as_slice()]].cells.clone())
        .collect();
    Ok(VariableSummary {
        variable: view.variable.clone(),
        source: "stored",
        names: elements.iter().map(|e| view.name(e)).collect(),
        keys: elements.iter().map(|e| view.keys(e)).collect(),
        elements: elements.to_vec(),
        columns,
        rows,
    })
}

/// How a write-back writes (§16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Into the rows of the table a one-axis variable's rows dimension is
    /// over, matched by key. The default, because it is the write-back a
    /// hierarchical model's per-group parameter is for.
    #[default]
    Update,
    /// One new row per element into any table.
    Insert,
}

/// Which part of an element's coordinate an insert writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatePart {
    /// The dimension's key: a row's primary key, a value, a step's instant.
    #[default]
    Key,
    /// Its label.
    Label,
    /// Its 1-based position.
    Position,
}

/// One coordinate an insert writes: an axis, a field, and which part.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinateWrite {
    /// The axis, by its heading or its dimension's name.
    pub axis: String,
    /// The target field.
    pub field: String,
    /// What is written into it.
    #[serde(default)]
    pub value: CoordinatePart,
}

/// A write-back, as `writePosterior` and a code body's `m.writePosterior` take it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PosteriorWrite {
    /// The variable.
    pub variable: String,
    /// Update (the default) or insert.
    #[serde(default)]
    pub mode: WriteMode,
    /// Statistic → the field it is written into: `{ "mean": "alpha_mean" }`.
    pub statistics: BTreeMap<String, String>,
    /// The table an insert writes into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    /// The coordinates an insert writes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coordinates: Vec<CoordinateWrite>,
    /// The field an insert writes the instance's id into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_field: Option<String>,
    /// Only these elements (by key or label, as a draws request selects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elements: Option<Json>,
}

impl PosteriorWrite {
    /// A write-back from its JSON, checked for what needs no instance: a
    /// statistic to write, a table for an insert and none for an update.
    pub fn from_json(json: &Json) -> Result<PosteriorWrite> {
        let write: PosteriorWrite = serde_json::from_value(json.clone())
            .map_err(|e| Error::invalid(format!("the write-back: {e}")))?;
        write.check()?;
        Ok(write)
    }

    /// The checks [`from_json`](Self::from_json) makes.
    pub fn check(&self) -> Result<()> {
        if self.variable.trim().is_empty() {
            return Err(Error::invalid("the write-back names no variable"));
        }
        if self.statistics.is_empty() {
            return Err(Error::invalid(
                "the write-back writes no statistic: map at least one (`mean`, `sd`, `q5`, \
                 `q50`, `q95`, …) to a field",
            ));
        }
        let known: BTreeSet<&str> = MCMC_COLUMNS.iter().chain(&MODE_COLUMNS).copied().collect();
        for stat in self.statistics.keys() {
            if !known.contains(stat.as_str()) {
                return Err(Error::invalid(format!(
                    "`{stat}` is not a statistic of a summary (they are {})",
                    known
                        .iter()
                        .map(|s| format!("`{s}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        match self.mode {
            WriteMode::Update => {
                if self.table.is_some() || !self.coordinates.is_empty() {
                    return Err(Error::invalid(
                        "an update writes into the table the variable's dimension is over, \
                         matched by key: it takes no `table` and no `coordinates`",
                    ));
                }
                if self.instance_field.is_some() {
                    return Err(Error::invalid(
                        "`instance_field` is for an insert, which makes new rows",
                    ));
                }
            }
            WriteMode::Insert => {
                if self.table.as_deref().is_none_or(|t| t.trim().is_empty()) {
                    return Err(Error::invalid(
                        "an insert needs the `table` its rows go into",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Every target field, with the statistic it holds (`None` for a
    /// coordinate or the instance id).
    pub fn fields(&self) -> Vec<(&str, Option<&str>)> {
        self.statistics
            .iter()
            .map(|(stat, field)| (field.as_str(), Some(stat.as_str())))
            .chain(self.coordinates.iter().map(|c| (c.field.as_str(), None)))
            .chain(self.instance_field.iter().map(|f| (f.as_str(), None)))
            .collect()
    }
}

/// What a write-back will write, before anything is written.
#[derive(Debug, Clone, PartialEq)]
pub struct WritePlan {
    /// Update or insert.
    pub mode: WriteMode,
    /// For an update, the dataset whose rows are written — the dimension's.
    pub dataset: Option<String>,
    /// For an insert, the table.
    pub table: Option<String>,
    /// The rows: for an update each with the key of the row it updates.
    pub rows: Vec<PlannedRow>,
}

/// One row of a write-back.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedRow {
    /// The primary key of the row an update writes, as text.
    pub key: Option<String>,
    /// The values.
    pub values: Attrs,
}

/// Plan `write` of `summary` (a summary of `view`) for `instance`.
pub fn plan_write(
    view: &PosteriorView,
    summary: &VariableSummary,
    write: &PosteriorWrite,
    coordinates: &Coordinates,
    instance: InstanceId,
) -> Result<WritePlan> {
    for stat in write.statistics.keys() {
        if !summary.statistics().iter().any(|s| s == stat) {
            return Err(Error::invalid(format!(
                "`{}` has no `{stat}`: its summary has {}",
                view.variable,
                summary
                    .statistics()
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let stats = |row: usize| -> Attrs {
        write
            .statistics
            .iter()
            .map(|(stat, field)| (field.clone(), number(summary.statistic(row, stat))))
            .collect()
    };
    match write.mode {
        WriteMode::Update => {
            let [axis] = view.axes.as_slice() else {
                return Err(Error::invalid(format!(
                    "an update writes one row per element of a one-axis variable, and `{}` has \
                     {}; write it in insert mode, one row per {}",
                    view.variable,
                    match view.axes.len() {
                        0 => "none (it is a scalar)".to_owned(),
                        n => format!("{n} axes"),
                    },
                    if view.axes.is_empty() {
                        "value"
                    } else {
                        "cell"
                    }
                )));
            };
            let dimension = axis
                .dimension
                .as_deref()
                .and_then(|d| coordinates.dimension(d))
                .filter(|d| d.kind == DimensionKind::Rows)
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "an update writes into the rows of the table `{}`'s axis is about, and \
                         its axis is {}: write it in insert mode",
                        view.variable,
                        match &axis.dimension {
                            Some(d) => format!("`{d}`, which is not a dataset's rows"),
                            None => "numbered".to_owned(),
                        }
                    ))
                })?;
            let rows = summary
                .elements
                .iter()
                .enumerate()
                .map(|(row, element)| PlannedRow {
                    key: Some(key_text(&dimension.keys[element[0] - 1])),
                    values: stats(row),
                })
                .collect();
            Ok(WritePlan {
                mode: WriteMode::Update,
                dataset: Some(dimension.dataset.clone()),
                table: None,
                rows,
            })
        }
        WriteMode::Insert => {
            let mut axes = Vec::with_capacity(write.coordinates.len());
            for c in &write.coordinates {
                let k = view
                    .axes
                    .iter()
                    .position(|a| a.name == c.axis || a.dimension.as_deref() == Some(&c.axis))
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "`{}` has no axis `{}` to write into `{}`",
                            view.variable, c.axis, c.field
                        ))
                    })?;
                axes.push((k, c));
            }
            let rows = summary
                .elements
                .iter()
                .enumerate()
                .map(|(row, element)| {
                    let mut values = stats(row);
                    for (k, c) in &axes {
                        let i = element[*k];
                        let value = match c.value {
                            CoordinatePart::Key => view.keys(element)[*k].clone(),
                            CoordinatePart::Label => Json::from(view.axes[*k].text(i)),
                            CoordinatePart::Position => Json::from(i),
                        };
                        values.insert(c.field.clone(), value);
                    }
                    if let Some(field) = &write.instance_field {
                        values.insert(field.clone(), Json::from(instance.to_string()));
                    }
                    PlannedRow { key: None, values }
                })
                .collect();
            Ok(WritePlan {
                mode: WriteMode::Insert,
                dataset: None,
                table: write.table.clone(),
                rows,
            })
        }
    }
}

/// CmdStan's own sampler columns, in the order it writes them.
const SAMPLER_ORDER: [&str; 7] = [
    "lp__",
    "accept_stat__",
    "stepsize__",
    "treedepth__",
    "n_leapfrog__",
    "divergent__",
    "energy__",
];

/// The stored draws of `instance` as one CSV per chain, in CmdStan's layout:
/// a header of `alpha.1`, `Sigma.2.1` (each variable's elements column-major,
/// as CmdStan writes them), then one line per iteration, warmup first when it
/// was kept. `(file name, bytes)`, `chain-1.csv` first.
pub async fn draws_csv(
    catalog: &Catalog,
    instance: &ModelInstance,
) -> Result<Vec<(String, Vec<u8>)>> {
    let variables = posterior_variables(instance);
    let mut order: Vec<&str> = SAMPLER_ORDER
        .iter()
        .copied()
        .filter(|v| variables.iter().any(|x| x == v))
        .collect();
    order.extend(
        variables
            .iter()
            .map(String::as_str)
            .filter(|v| v.ends_with("__") && !SAMPLER_ORDER.contains(v)),
    );
    // The program's variables in the order the fit's tables list them, which
    // is the program's, then any it did not summarise.
    for block in &instance.parameters {
        if let ParameterBlock::Table { name, .. } = block {
            if let Some(v) = variables.iter().find(|v| *v == name) {
                if !order.contains(&v.as_str()) {
                    order.push(v);
                }
            }
        }
    }
    for v in &variables {
        if !order.contains(&v.as_str()) {
            order.push(v);
        }
    }

    // (chain) → columns, each (header, warmup draws, draws).
    type Column = (String, Vec<f64>, Vec<f64>);
    let mut chains: BTreeMap<u32, Vec<Column>> = BTreeMap::new();
    let reader = DrawsReader::new(catalog, instance.id);
    for variable in order {
        let mut series = reader
            .read(&DrawsQuery::variable(variable).with_warmup())
            .await?;
        // Column-major: the first index varies fastest.
        series.sort_by(|a, b| {
            let rev = |e: &[usize]| e.iter().rev().copied().collect::<Vec<_>>();
            (rev(&a.element), a.chain).cmp(&(rev(&b.element), b.chain))
        });
        for s in series {
            let header = if s.element.is_empty() {
                s.variable.clone()
            } else {
                format!(
                    "{}.{}",
                    s.variable,
                    s.element
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(".")
                )
            };
            let columns = chains.entry(s.chain).or_default();
            let column = match columns.iter_mut().find(|c| c.0 == header) {
                Some(column) => column,
                None => {
                    columns.push((header, Vec::new(), Vec::new()));
                    columns
                        .last_mut()
                        .ok_or_else(|| Error::msg("unreachable"))?
                }
            };
            if s.warmup {
                column.1 = s.draws;
            } else {
                column.2 = s.draws;
            }
        }
    }

    let mut files = Vec::with_capacity(chains.len());
    for (chain, columns) in chains {
        let mut out = String::new();
        out.push_str(
            &columns
                .iter()
                .map(|c| c.0.as_str())
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
        for part in [1usize, 2] {
            let rows = columns
                .iter()
                .map(|c| if part == 1 { c.1.len() } else { c.2.len() })
                .max()
                .unwrap_or(0);
            for i in 0..rows {
                let line: Vec<String> = columns
                    .iter()
                    .map(|c| {
                        let values = if part == 1 { &c.1 } else { &c.2 };
                        values.get(i).map_or_else(String::new, |x| csv_number(*x))
                    })
                    .collect();
                out.push_str(&line.join(","));
                out.push('\n');
            }
        }
        files.push((format!("chain-{chain}.csv"), out.into_bytes()));
    }
    Ok(files)
}

/// A draw as CmdStan writes it: the shortest text that reads back as the same
/// number, and `nan`, `inf`, `-inf`.
fn csv_number(x: f64) -> String {
    if x.is_nan() {
        "nan".to_owned()
    } else if x.is_infinite() {
        if x > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else {
        x.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bind::DimensionCoordinates;
    use serde_json::json;

    fn coordinates() -> Coordinates {
        Coordinates {
            dimensions: vec![
                DimensionCoordinates {
                    name: "counties".into(),
                    kind: DimensionKind::Rows,
                    dataset: "counties".into(),
                    column: None,
                    keys: vec![json!("27001"), json!("27003"), json!("27005")],
                    labels: vec!["Aitkin".into(), "Anoka".into(), "Becker".into()],
                },
                DimensionCoordinates {
                    name: "region".into(),
                    kind: DimensionKind::Values,
                    dataset: "main".into(),
                    column: Some("region".into()),
                    keys: vec![json!("north"), json!("south")],
                    labels: vec!["north".into(), "south".into()],
                },
            ],
            designs: BTreeMap::new(),
        }
    }

    fn view(dims: Vec<usize>, dimensions: Vec<Option<&str>>) -> PosteriorView {
        let recorded = RecordedAxes {
            dims: dims.clone(),
            dimensions: dimensions
                .into_iter()
                .map(|d| d.map(str::to_owned))
                .collect(),
        };
        PosteriorView {
            variable: "alpha".into(),
            dims,
            axes: named_axes(&coordinates(), &recorded),
            method: PosteriorMethod::Mcmc,
        }
    }

    #[test]
    fn elements_are_selected_by_key_by_label_or_by_position() {
        let v = view(vec![3], vec![Some("counties")]);
        assert_eq!(v.elements(), vec![vec![1], vec![2], vec![3]]);
        let by = |j: Json| v.select(&Selection::from_json(Some(&j)).unwrap());
        // A key, a label, and a key given as a number all find their county.
        assert_eq!(
            by(json!({ "counties": ["27005", "Aitkin"] })).unwrap(),
            vec![vec![1], vec![3]]
        );
        assert_eq!(by(json!({ "counties": 27003 })).unwrap(), vec![vec![2]]);
        // The first axis by its ordinal, whatever it is called.
        assert_eq!(by(json!({ "1": ["Becker"] })).unwrap(), vec![vec![3]]);
        assert_eq!(by(json!([2, 3])).unwrap(), vec![vec![2], vec![3]]);
        let err = by(json!({ "counties": ["Hennepin"] })).unwrap_err();
        assert!(
            err.to_string().contains(
                "`Hennepin` is not a key or a label of `counties` of `alpha` in this fit"
            ),
            "{err}"
        );
        let err = by(json!({ "schools": [1] })).unwrap_err();
        assert!(
            err.to_string()
                .contains("`alpha` has no axis `schools` (its axes are `counties`)"),
            "{err}"
        );
        let err = by(json!([[4]])).unwrap_err();
        assert!(
            err.to_string().contains("`alpha` has no element [4]"),
            "{err}"
        );
        assert_eq!(v.name(&[1]), "alpha[Aitkin]");
        assert_eq!(v.keys(&[2]), vec![json!("27003")]);
    }

    #[test]
    fn a_two_axis_selection_is_the_product_of_its_axes() {
        let v = view(vec![3, 2], vec![Some("counties"), None]);
        let chosen = v
            .select(&Selection::from_json(Some(&json!({ "counties": ["Anoka"] }))).unwrap())
            .unwrap();
        assert_eq!(chosen, vec![vec![2, 1], vec![2, 2]]);
        let chosen = v
            .select(
                &Selection::from_json(Some(&json!({ "counties": ["Anoka"], "index": [2] })))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(chosen, vec![vec![2, 2]]);
        assert_eq!(v.axis_keys()[1], vec![json!(1), json!(2)]);
    }

    fn summary(v: &PosteriorView) -> VariableSummary {
        let elements = v.elements();
        VariableSummary {
            variable: v.variable.clone(),
            source: "draws",
            columns: v
                .axes
                .iter()
                .map(|a| a.name.clone())
                .chain(MCMC_COLUMNS.iter().map(|c| (*c).to_owned()))
                .collect(),
            names: elements.iter().map(|e| v.name(e)).collect(),
            keys: elements.iter().map(|e| v.keys(e)).collect(),
            rows: elements
                .iter()
                .map(|e| {
                    v.cells(e)
                        .into_iter()
                        .chain((0..9).map(|k| json!(e[0] as f64 + k as f64 / 10.0)))
                        .collect()
                })
                .collect(),
            elements,
        }
    }

    #[test]
    fn an_update_writes_the_rows_of_the_dimensions_table_by_key() {
        let v = view(vec![3], vec![Some("counties")]);
        let write = PosteriorWrite::from_json(&json!({
            "variable": "alpha", "mode": "update",
            "statistics": { "mean": "alpha_mean", "sd": "alpha_sd" },
        }))
        .unwrap();
        let plan = plan_write(&v, &summary(&v), &write, &coordinates(), InstanceId::new()).unwrap();
        assert_eq!(plan.dataset.as_deref(), Some("counties"));
        assert_eq!(plan.rows.len(), 3);
        assert_eq!(plan.rows[2].key.as_deref(), Some("27005"));
        assert_eq!(plan.rows[2].values["alpha_mean"], json!(3.0));
        assert_eq!(plan.rows[2].values["alpha_sd"], json!(3.1));

        // A values dimension is not a table's rows, and two axes are two keys.
        let by_region = view(vec![2], vec![Some("region")]);
        let err = plan_write(
            &by_region,
            &summary(&by_region),
            &write,
            &coordinates(),
            InstanceId::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a dataset's rows"), "{err}");
        let two = view(vec![3, 2], vec![Some("counties"), None]);
        let err = plan_write(
            &two,
            &summary(&two),
            &write,
            &coordinates(),
            InstanceId::new(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("`alpha` has 2 axes; write it in insert mode"),
            "{err}"
        );
    }

    #[test]
    fn an_insert_writes_a_row_per_element_with_its_coordinates() {
        let v = view(vec![3, 2], vec![Some("counties"), None]);
        let id = InstanceId::new();
        let write = PosteriorWrite::from_json(&json!({
            "variable": "alpha", "mode": "insert", "table": "estimates",
            "statistics": { "q5": "lower", "q95": "upper" },
            "coordinates": [
                { "axis": "counties", "field": "county" },
                { "axis": "counties", "field": "county_name", "value": "label" },
                { "axis": "index", "field": "k", "value": "position" },
            ],
            "instance_field": "fit",
        }))
        .unwrap();
        let plan = plan_write(&v, &summary(&v), &write, &coordinates(), id).unwrap();
        assert_eq!(plan.table.as_deref(), Some("estimates"));
        assert_eq!(plan.rows.len(), 6);
        let row = &plan.rows[3].values;
        assert_eq!(row["county"], json!("27003"));
        assert_eq!(row["county_name"], json!("Anoka"));
        assert_eq!(row["k"], json!(2));
        assert_eq!(row["lower"], json!(2.3));
        assert_eq!(row["fit"], json!(id.to_string()));
    }

    #[test]
    fn a_write_back_is_refused_for_what_it_cannot_mean() {
        let refuse = |j: Json, says: &str| {
            let err = PosteriorWrite::from_json(&j).unwrap_err();
            assert!(err.to_string().contains(says), "{err}");
        };
        refuse(
            json!({ "variable": "alpha", "mode": "update", "statistics": {} }),
            "writes no statistic",
        );
        refuse(
            json!({ "variable": "alpha", "mode": "update", "statistics": { "median": "m" } }),
            "`median` is not a statistic",
        );
        refuse(
            json!({ "variable": "alpha", "mode": "insert", "statistics": { "mean": "m" } }),
            "needs the `table`",
        );
        refuse(
            json!({ "variable": "alpha", "mode": "update", "table": "t",
                    "statistics": { "mean": "m" } }),
            "takes no `table`",
        );
    }

    #[test]
    fn a_draw_is_written_as_cmdstan_writes_it() {
        assert_eq!(csv_number(0.1), "0.1");
        assert_eq!(csv_number(-2.5e-12), "-0.0000000000025");
        assert_eq!(csv_number(f64::NAN), "nan");
        assert_eq!(csv_number(f64::NEG_INFINITY), "-inf");
    }
}

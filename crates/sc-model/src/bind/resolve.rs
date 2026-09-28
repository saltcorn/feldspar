//! The binder itself: from datasets and a configuration to CmdStan's data file
//! (Stan TODO §§8–11).
//!
//! In three steps, each of which can refuse with a sentence naming the
//! variable, its declaration and its binding:
//!
//! 1. **Structure** ([`check_structure`]) — what can be known with no data
//!    read: every name a binding uses exists, every kind can produce its
//!    declaration's rank and element type, and the datasets have an order to be
//!    resolved in. The same function is the save-time check.
//! 2. **Resolution** — each dataset, in dependency order: its `nulls` policy,
//!    then its `unknown` policy against the dimensions it indexes into (already
//!    final, because they were resolved first), then the dimensions over it,
//!    then the stable sort a `segment_*` binding needs, and last its own rows
//!    dimension. After this every dataset's rows are the rows that are bound.
//! 3. **Evaluation and checking** — every binding becomes a typed, shaped
//!    value; every declaration is checked against it (element type, rank, every
//!    size that evaluates, every bound that evaluates); the total is checked
//!    against the data-values cap.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sc_error::{Error, Result};
use serde_json::{Map, Value as Json};

use super::dimension::{
    Coordinates, DesignCoordinates, Dimension, DimensionCoordinates, cell_text, key_text,
};
use super::spec::{Binding, DimensionSpec, Policy, Spec, Step, parse_instant};
use super::tensor::{Tensor, Values};
use super::{LABEL_COLUMN, check_bindings_declared};
use crate::encode::{apply_encoding, fit_encoding};
use crate::frame::{Column, Frame};
use crate::interface::{Declaration, Element, Interface};
use crate::provider::Outcome;

/// The default ceiling on the number of values in a bound data file
/// (`--stan-max-data-values`).
pub const DEFAULT_MAX_DATA_VALUES: u64 = 20_000_000;

/// What the binder produced: the data file, the coordinates, and the report.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundData {
    /// CmdStan's data file: one member per `data` variable (by name — CmdStan
    /// reads members in any order; the report keeps the declaration order).
    pub json: Json,
    /// Every dimension's keys and labels, and every design's columns.
    pub coordinates: Coordinates,
    /// Sizes, drops, and a line per variable — what the preview shows.
    pub report: BindReport,
}

/// What binding did, for the preview and the instance.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct BindReport {
    /// Each dataset: rows read and rows bound.
    pub datasets: Vec<DatasetReport>,
    /// Each dimension's size.
    pub dimensions: BTreeMap<String, usize>,
    /// Every drop the policies made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drops: Vec<DropReport>,
    /// Each `data` variable, in declaration order.
    pub variables: Vec<VariableReport>,
    /// The number of values in the data file.
    pub values: u64,
}

impl BindReport {
    /// How many rows of `dataset` the policies dropped.
    pub fn dropped(&self, dataset: &str) -> usize {
        self.datasets
            .iter()
            .find(|d| d.name == dataset)
            .map_or(0, |d| d.read - d.bound)
    }
}

/// One dataset's rows before and after the policies.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DatasetReport {
    /// The dataset.
    pub name: String,
    /// Rows read.
    pub read: usize,
    /// Rows bound.
    pub bound: usize,
}

/// Rows a policy dropped, and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DropReport {
    /// The dataset they left.
    pub dataset: String,
    /// The column that was null, or whose value was unknown.
    pub column: String,
    /// For an unknown value, the dimension it is not a position of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<String>,
    /// How many rows.
    pub rows: usize,
    /// The first of them, by key.
    pub first: String,
    /// The whole of it as a sentence.
    pub sentence: String,
}

/// One bound variable, for the preview.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VariableReport {
    /// The variable.
    pub name: String,
    /// Its declared type.
    pub stan_type: String,
    /// Its binding, as a sentence reads it.
    pub binding: String,
    /// The bound shape.
    pub shape: Vec<usize>,
    /// The first few values.
    pub first: Vec<Json>,
}

/// A binding's value, with what sentences need to know about it.
struct Bound {
    tensor: Tensor,
    /// The dataset whose rows its first axis runs over.
    rows_of: Option<String>,
    /// For a scalar size, what it is the size of: "the row count of `main`".
    what: Option<String>,
    /// For an `index`, the size of the dimension it indexes into.
    groups: Option<usize>,
}

/// "`y`, declared `vector[N]` and bound to `main.log_radon`" — the start of
/// every sentence about one variable.
fn about(decl: &Declaration, binding: &Binding) -> String {
    format!(
        "`{}`, declared `{}` and bound to `{}`",
        decl.name,
        decl.stan_type,
        binding.describe()
    )
}

/// A dataset's column names, for the structural check.
pub(crate) struct DatasetColumns<'a> {
    pub name: &'a str,
    pub columns: Vec<&'a str>,
}

/// Where a dimension's positions come from.
#[derive(Debug, Clone, Copy)]
struct DimensionSource<'a> {
    dataset: &'a str,
    rows: bool,
}

/// The dimension called `name`: a dataset's rows, a declared one, or a time
/// grid's `.future`.
fn dimension_source<'a>(
    spec: &'a Spec,
    datasets: &'a [DatasetColumns<'a>],
    name: &'a str,
) -> Option<DimensionSource<'a>> {
    if let Some(d) = datasets.iter().find(|d| d.name == name) {
        return Some(DimensionSource {
            dataset: d.name,
            rows: true,
        });
    }
    if let Some(dim) = spec.dimensions.get(name) {
        return Some(DimensionSource {
            dataset: dim.dataset(),
            rows: false,
        });
    }
    let grid = name.strip_suffix(".future")?;
    match spec.dimensions.get(grid)? {
        dim @ DimensionSpec::TimeGrid { .. } => Some(DimensionSource {
            dataset: dim.dataset(),
            rows: false,
        }),
        DimensionSpec::Values { .. } => None,
    }
}

/// Everything about the bindings that can be checked without reading data
/// (§10's save-time half), answering the order the datasets resolve in.
pub(crate) fn check_structure<'a>(
    interface: &Interface,
    config: &sc_types::Attrs,
    spec: &Spec,
    datasets: &'a [DatasetColumns<'a>],
) -> Result<Vec<&'a str>> {
    check_bindings_declared(interface, config)?;
    let dataset = |name: &str| datasets.iter().find(|d| d.name == name);
    let listed = || {
        datasets
            .iter()
            .map(|d| format!("`{}`", d.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let has_column = |ds: &DatasetColumns<'_>, column: &str| ds.columns.contains(&column);

    for (name, dim) in &spec.dimensions {
        let at = |msg: String| Error::invalid(format!("dimension `{name}`: {msg}"));
        if !is_identifier(name) {
            return Err(at(
                "a dimension's name must be an identifier — letters, digits and `_`".to_owned(),
            ));
        }
        if dataset(name).is_some() {
            return Err(at(format!(
                "`{name}` is already a dataset, and every dataset is a dimension of its rows"
            )));
        }
        let Some(ds) = dataset(dim.dataset()) else {
            return Err(at(format!(
                "there is no dataset `{}` (the datasets are {})",
                dim.dataset(),
                listed()
            )));
        };
        if !has_column(ds, dim.column()) {
            return Err(at(format!(
                "`{}` has no column `{}`",
                ds.name,
                dim.column()
            )));
        }
        if let DimensionSpec::TimeGrid {
            step, start, end, ..
        } = dim
        {
            Step::parse(step).map_err(|e| at(e.to_string()))?;
            for (what, text) in [("start", start), ("end", end)] {
                if let Some(text) = text {
                    if parse_instant(text).is_none() {
                        return Err(at(format!(
                            "`{what}` `{text}` is not a date (`2024-01-31`) or a timestamp"
                        )));
                    }
                }
            }
        }
    }
    for name in spec.policies.keys() {
        if dataset(name).is_none() {
            return Err(Error::invalid(format!(
                "there are policies for a dataset `{name}`, and there is no such dataset (the \
                 datasets are {})",
                listed()
            )));
        }
    }

    for decl in &interface.data {
        let Some(binding) = spec.bindings.get(&decl.name) else {
            continue;
        };
        let at = |msg: String| Error::invalid(format!("{}: {msg}", about(decl, binding)));
        if matches!(decl.element, Element::Complex | Element::Tuple) {
            return Err(at(format!(
                "a {} cannot be bound; bind its parts as separate variables",
                decl.element.name()
            )));
        }
        if let Some(name) = binding.dataset() {
            let Some(ds) = dataset(name) else {
                return Err(at(format!(
                    "there is no dataset `{name}` (the datasets are {})",
                    listed()
                )));
            };
            for column in binding.columns() {
                if !has_column(ds, column) {
                    return Err(at(format!(
                        "`{name}` has no column `{column}` (its columns are {})",
                        ds.columns
                            .iter()
                            .map(|c| format!("`{c}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }
        match binding {
            Binding::Columns { columns, .. } | Binding::Design { columns, .. }
                if columns.is_empty() =>
            {
                return Err(at("it lists no columns".to_owned()));
            }
            Binding::Size { dimension } | Binding::Index { dimension, .. } => {
                let Some(source) = dimension_source(spec, datasets, dimension) else {
                    return Err(at(format!(
                        "there is no dimension `{dimension}` (the datasets are dimensions of \
                         their rows: {}; the others are declared under `{}`)",
                        listed(),
                        super::DIMENSIONS_KEY
                    )));
                };
                if let Binding::Index {
                    match_column: Some(m),
                    ..
                } = binding
                {
                    if !source.rows {
                        return Err(at(format!(
                            "`match` compares with a column of a dataset's rows, and \
                             `{dimension}` is not a dataset"
                        )));
                    }
                    let Some(target) = dataset(source.dataset) else {
                        return Err(internal("a rows dimension without its dataset"));
                    };
                    if !has_column(target, m) {
                        return Err(at(format!(
                            "`match`: `{}` has no column `{m}`",
                            target.name
                        )));
                    }
                }
            }
            Binding::Width { of } => {
                if !matches!(spec.bindings.get(of), Some(Binding::Design { .. })) {
                    return Err(at(format!(
                        "`width` is the width of a `design`-bound variable, and `{of}` is not \
                         one"
                    )));
                }
            }
            Binding::SegmentStart { dataset, index } | Binding::SegmentSize { dataset, index } => {
                match spec.bindings.get(index) {
                    Some(Binding::Index { dataset: d, .. }) if d == dataset => {}
                    Some(Binding::Index { dataset: d, .. }) => {
                        return Err(at(format!(
                            "`{index}` indexes `{d}`, not `{dataset}`: a segment is of the \
                             dataset its index is over"
                        )));
                    }
                    _ => {
                        return Err(at(format!(
                            "a segment is over an `index`-bound variable, and `{index}` is not \
                             one"
                        )));
                    }
                }
            }
            Binding::Column {
                time: Some(time), ..
            } => {
                time.seconds().map_err(|e| at(e.to_string()))?;
                time.origin(None).map_err(|e| at(e.to_string()))?;
            }
            _ => {}
        }

        // The rank and the element type the kind can produce.
        let (rank, real) = match binding {
            Binding::Value { value } => {
                let t = Tensor::from_literal(value).map_err(|e| at(e.to_string()))?;
                (t.shape.len(), matches!(t.values, Values::Real(_)))
            }
            other => (
                other
                    .rank()
                    .ok_or_else(|| internal("a binding kind with no rank"))?,
                other.always_real(),
            ),
        };
        if rank != decl.rank() {
            return Err(at(format!(
                "it has {} but a `{}` binding produces {}",
                axes(decl.rank()),
                binding.kind(),
                axes(rank)
            )));
        }
        if real && decl.element == Element::Int {
            return Err(at(format!(
                "it is an int, and a `{}` binding produces reals",
                binding.kind()
            )));
        }
    }

    resolution_order(spec, datasets)
}

/// "no axes (a scalar)", "1 axis", "2 axes".
fn axes(rank: usize) -> String {
    match rank {
        0 => "no axes (a scalar)".to_owned(),
        1 => "1 axis".to_owned(),
        n => format!("{n} axes"),
    }
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The datasets in the order they resolve: a dataset indexed into before the
/// datasets that index it, the model's order breaking ties. A cycle is refused
/// by name, and so is a dataset indexing its own rows — its rows would depend
/// on what they are indexed by.
fn resolution_order<'a>(spec: &Spec, datasets: &'a [DatasetColumns<'a>]) -> Result<Vec<&'a str>> {
    // `edges[a]` holds each dataset `a` indexes into, with the variable.
    let mut edges: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (var, binding) in &spec.bindings {
        let Binding::Index {
            dataset, dimension, ..
        } = binding
        else {
            continue;
        };
        let Some(source) = dimension_source(spec, datasets, dimension) else {
            continue;
        };
        if source.dataset == dataset {
            if source.rows {
                return Err(Error::invalid(format!(
                    "`{var}` indexes `{dataset}` into its own rows, so its rows would depend on \
                     what indexes them: index into a related dataset over the same table \
                     instead"
                )));
            }
            continue;
        }
        edges
            .entry(dataset.as_str())
            .or_default()
            .push((source.dataset, var.as_str()));
    }
    let mut order: Vec<&str> = Vec::with_capacity(datasets.len());
    let mut remaining: Vec<&str> = datasets.iter().map(|d| d.name).collect();
    while !remaining.is_empty() {
        let ready = remaining.iter().position(|d| {
            edges
                .get(d)
                .is_none_or(|targets| targets.iter().all(|(t, _)| order.contains(t)))
        });
        match ready {
            Some(i) => order.push(remaining.remove(i)),
            None => {
                let stuck = &remaining;
                let through: Vec<String> = stuck
                    .iter()
                    .flat_map(|d| {
                        edges
                            .get(d)
                            .into_iter()
                            .flatten()
                            .filter(|(t, _)| stuck.contains(t))
                            .map(move |(t, v)| format!("`{v}` indexes `{d}` into `{t}`"))
                    })
                    .collect();
                return Err(Error::invalid(format!(
                    "the datasets {} index into each other ({}), so none of them can be \
                     resolved first",
                    remaining
                        .iter()
                        .map(|d| format!("`{d}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    through.join("; ")
                )));
            }
        }
    }
    Ok(order)
}

/// One dataset while it is resolved.
struct Working {
    frame: Frame,
    labels: Option<Vec<String>>,
    read: usize,
}

impl Working {
    /// Row `i`'s name in a sentence: its key, or its number.
    fn row_name(&self, i: usize) -> String {
        self.frame
            .keys
            .get(i)
            .map_or_else(|| format!("#{}", i + 1), |k| key_text(k).to_owned())
    }

    /// Keep only `rows`, in the order given.
    fn keep(&mut self, rows: &[usize]) -> Result<()> {
        self.frame = self.frame.take_rows(rows)?;
        if let Some(labels) = &mut self.labels {
            *labels = rows.iter().map(|i| labels[*i].clone()).collect();
        }
        Ok(())
    }
}

/// Bind `interface`'s `data` block to `datasets` (the model's own first, as
/// [`MAIN_DATASET`](crate::MAIN_DATASET), then each related one) as `config`
/// says, refusing a data file of more than `max_values` values.
///
/// A related dataset's label arrives as the [`LABEL_COLUMN`] its read added
/// (see [`binding_dataset`](super::binding_dataset)); it becomes the labels of
/// that dataset's rows and is not a column bindings can name.
pub fn bind_data(
    interface: &Interface,
    config: &sc_types::Attrs,
    datasets: &[(String, Frame)],
    max_values: u64,
) -> Result<BoundData> {
    let spec = Spec::parse(config)?;
    let mut work: BTreeMap<&str, Working> = BTreeMap::new();
    for (name, frame) in datasets {
        let mut frame = frame.clone();
        let labels = match frame.columns.iter().position(|(n, _)| n == LABEL_COLUMN) {
            Some(at) => {
                let (_, column) = frame.columns.remove(at);
                Some(
                    (0..frame.rows)
                        .map(|i| cell_text(&column, i).unwrap_or_default())
                        .collect(),
                )
            }
            None => None,
        };
        let read = frame.rows;
        work.insert(
            name.as_str(),
            Working {
                frame,
                labels,
                read,
            },
        );
    }
    let columns: Vec<DatasetColumns<'_>> = datasets
        .iter()
        .map(|(name, _)| DatasetColumns {
            name,
            columns: work[name.as_str()].frame.names(),
        })
        .collect();
    let order = check_structure(interface, config, &spec, &columns)?;
    let order: Vec<String> = order.into_iter().map(str::to_owned).collect();
    drop(columns);

    let declared = |var: &str| interface.data_variable(var);
    // The bindings in declaration order, so the first sentence is about the
    // first variable the program declares.
    let bindings: Vec<(&Declaration, &Binding)> = interface
        .data
        .iter()
        .filter_map(|d| spec.bindings.get(&d.name).map(|b| (d, b)))
        .collect();

    let mut dims: HashMap<String, Dimension> = HashMap::new();
    let mut drops: Vec<DropReport> = Vec::new();

    for name in &order {
        let name = name.as_str();
        let policies = spec.policies(name);
        let rows = work[name].frame.rows;
        let mut dropped = vec![false; rows];

        // The `nulls` policy, once per column.
        let mut checked: BTreeSet<&str> = BTreeSet::new();
        for (decl, binding) in &bindings {
            if binding.dataset() != Some(name) {
                continue;
            }
            for column_name in binding.policed_columns() {
                if !checked.insert(column_name) {
                    continue;
                }
                let w = &work[name];
                let column = column_of(&w.frame, column_name)?;
                let nulls: Vec<usize> = (0..rows).filter(|i| column.is_null_at(*i)).collect();
                let Some(&first) = nulls.first() else {
                    continue;
                };
                let first_name = w.row_name(first);
                match policies.nulls {
                    Policy::Refuse => {
                        return Err(Error::invalid(format!(
                            "{}: `{column_name}` of `{name}` is null in {} (the first is row \
                             `{first_name}`); fill {} in, filter {} out of the dataset, or set \
                             `{name}`'s `nulls` policy to `drop`",
                            about(decl, binding),
                            rows_word(nulls.len()),
                            them(nulls.len()),
                            them(nulls.len()),
                        )));
                    }
                    Policy::Drop => {
                        for i in &nulls {
                            dropped[*i] = true;
                        }
                        drops.push(DropReport {
                            dataset: name.to_owned(),
                            column: column_name.to_owned(),
                            dimension: None,
                            rows: nulls.len(),
                            first: first_name.clone(),
                            sentence: format!(
                                "dropped {} of `{name}` where `{column_name}` is null (the first \
                                 is row `{first_name}`)",
                                rows_word(nulls.len())
                            ),
                        });
                    }
                }
            }
        }

        // The `unknown` policy against the dimensions of *other* datasets,
        // which are final.
        let earlier = drops.clone();
        unknowns(
            name,
            policies.unknown,
            &bindings,
            &work,
            &dims,
            &earlier,
            &mut dropped,
            &mut drops,
            false,
        )?;
        apply_drops(working(&mut work, name)?, &dropped)?;

        // The dimensions over this dataset, and then the unknowns into them —
        // only a time grid with a given start or end can have any. Dropping
        // those cannot move a derived start or end (the rows outside a given
        // bound are the only ones dropped), so the grids stand; the values
        // dimensions are built again so they only know values that are bound.
        build_dimensions(name, &spec, &work[name].frame, &mut dims)?;
        let mut dropped = vec![false; work[name].frame.rows];
        let before = drops.len();
        unknowns(
            name,
            policies.unknown,
            &bindings,
            &work,
            &dims,
            &drops.clone(),
            &mut dropped,
            &mut drops,
            true,
        )?;
        if drops.len() > before {
            apply_drops(working(&mut work, name)?, &dropped)?;
            build_dimensions(name, &spec, &work[name].frame, &mut dims)?;
        }

        // A segment needs its dataset sorted by its index: stably, so the
        // declared order breaks ties, and before anything of it is bound.
        let segment_indexes: BTreeSet<&str> = bindings
            .iter()
            .filter_map(|(_, b)| match b {
                Binding::SegmentStart { dataset, index }
                | Binding::SegmentSize { dataset, index }
                    if dataset == name =>
                {
                    Some(index.as_str())
                }
                _ => None,
            })
            .collect();
        if segment_indexes.len() > 1 {
            return Err(Error::invalid(format!(
                "`{name}` is segmented by {}, and a dataset can be sorted by only one index",
                segment_indexes
                    .iter()
                    .map(|v| format!("`{v}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )));
        }
        if let Some(index) = segment_indexes.first() {
            let (decl, binding) = (
                declared(index).ok_or_else(|| internal("an undeclared index"))?,
                &spec.bindings[*index],
            );
            let positions = index_positions(decl, binding, &work, &dims)?;
            let mut order: Vec<usize> = (0..positions.len()).collect();
            order.sort_by_key(|i| positions[*i]);
            working(&mut work, name)?.keep(&order)?;
        }

        let w = &work[name];
        dims.insert(
            name.to_owned(),
            Dimension::rows(name, &w.frame.keys, w.labels.as_deref(), w.frame.rows),
        );
    }

    // Every binding, evaluated: the ones that depend on another binding last.
    let mut bound: HashMap<&str, Bound> = HashMap::new();
    let mut designs = BTreeMap::new();
    for pass in 0..2 {
        for (decl, binding) in &bindings {
            let dependent = matches!(
                binding,
                Binding::Width { .. } | Binding::SegmentStart { .. } | Binding::SegmentSize { .. }
            );
            if dependent != (pass == 1) {
                continue;
            }
            let value = evaluate(decl, binding, &work, &dims, &bound, &mut designs)?;
            bound.insert(decl.name.as_str(), value);
        }
    }

    // Every declaration against its value.
    let sizes: Sizes<'_> = bound
        .iter()
        .filter_map(|(name, b)| Some((*name, (b.tensor.as_int()?, b.what.clone()))))
        .collect();
    let scalars: HashMap<&str, f64> = bound
        .iter()
        .filter_map(|(name, b)| Some((*name, b.tensor.as_number()?)))
        .collect();
    let mut json = Map::new();
    let mut variables = Vec::with_capacity(bindings.len());
    let mut total: u64 = 0;
    for (decl, binding) in &bindings {
        let b = bound
            .remove(decl.name.as_str())
            .ok_or_else(|| internal("a binding that was not evaluated"))?;
        let tensor = check_declaration(decl, binding, b, &sizes, &scalars, &work)?;
        total += tensor.count() as u64;
        variables.push(VariableReport {
            name: decl.name.clone(),
            stan_type: decl.stan_type.clone(),
            binding: binding.describe(),
            shape: tensor.shape.clone(),
            first: tensor.first(5),
        });
        json.insert(decl.name.clone(), tensor.to_json());
    }
    if total > max_values {
        let mut largest: Vec<&VariableReport> = variables.iter().collect();
        largest.sort_by_key(|v| std::cmp::Reverse(v.shape.iter().product::<usize>()));
        return Err(Error::invalid(format!(
            "the bound data has {total} values, more than the {max_values} allowed \
             (`--stan-max-data-values`); the largest {} {}",
            if largest.len() == 1 { "is" } else { "are" },
            largest
                .iter()
                .take(3)
                .map(|v| format!(
                    "`{}` ({} values)",
                    v.name,
                    v.shape.iter().product::<usize>()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // The coordinates: the datasets in the model's order, then the declared
    // dimensions by name, each time grid followed by its future.
    let mut dimensions: Vec<DimensionCoordinates> = Vec::new();
    for (name, _) in datasets {
        dimensions.push(dims[name.as_str()].coords.clone());
    }
    for (name, spec_dim) in &spec.dimensions {
        if let Some(d) = dims.get(name) {
            dimensions.push(d.coords.clone());
        }
        if matches!(spec_dim, DimensionSpec::TimeGrid { .. }) {
            if let Some(d) = dims.get(&format!("{name}.future")) {
                dimensions.push(d.coords.clone());
            }
        }
    }
    let report = BindReport {
        datasets: datasets
            .iter()
            .map(|(name, _)| {
                let w = &work[name.as_str()];
                DatasetReport {
                    name: name.clone(),
                    read: w.read,
                    bound: w.frame.rows,
                }
            })
            .collect(),
        dimensions: dimensions
            .iter()
            .map(|d| (d.name.clone(), d.size()))
            .collect(),
        drops,
        variables,
        values: total,
    };
    Ok(BoundData {
        json: Json::Object(json),
        coordinates: Coordinates {
            dimensions,
            designs,
        },
        report,
    })
}

fn rows_word(n: usize) -> String {
    if n == 1 {
        "1 row".to_owned()
    } else {
        format!("{n} rows")
    }
}

fn them(n: usize) -> &'static str {
    if n == 1 { "it" } else { "them" }
}

/// A binder bug, as an error rather than a panic: every name reaching the
/// resolution has been checked by [`check_structure`].
fn internal(what: &str) -> Error {
    Error::msg(format!(
        "binding: {what} (a bug: the structural check should have caught it)"
    ))
}

/// `name`'s column `column`, which the structural check has already found.
fn column_of<'f>(frame: &'f Frame, column: &str) -> Result<&'f Column> {
    frame
        .column(column)
        .ok_or_else(|| internal(&format!("no column `{column}`")))
}

/// The dataset `name` being resolved.
fn working<'w>(work: &'w mut BTreeMap<&str, Working>, name: &str) -> Result<&'w mut Working> {
    work.get_mut(name)
        .ok_or_else(|| internal(&format!("no dataset `{name}`")))
}

/// Keep the rows not marked dropped.
fn apply_drops(w: &mut Working, dropped: &[bool]) -> Result<()> {
    if dropped.iter().any(|d| *d) {
        let keep: Vec<usize> = (0..dropped.len()).filter(|i| !dropped[*i]).collect();
        w.keep(&keep)?;
    }
    Ok(())
}

/// The declared dimensions over `dataset`, built from its rows as they now
/// stand.
fn build_dimensions(
    dataset: &str,
    spec: &Spec,
    frame: &Frame,
    dims: &mut HashMap<String, Dimension>,
) -> Result<()> {
    for (name, dim) in &spec.dimensions {
        if dim.dataset() != dataset {
            continue;
        }
        let column = column_of(frame, dim.column())?;
        match dim {
            DimensionSpec::Values { column: c, .. } => {
                dims.insert(name.clone(), Dimension::values(name, dataset, c, column));
            }
            DimensionSpec::TimeGrid {
                column: c,
                step,
                start,
                end,
                horizon,
                ..
            } => {
                let (grid, future) = Dimension::time_grid(
                    name,
                    dataset,
                    c,
                    column,
                    Step::parse(step)?,
                    start.as_deref().and_then(parse_instant),
                    end.as_deref().and_then(parse_instant),
                    *horizon,
                )?;
                dims.insert(future.coords.name.clone(), future);
                dims.insert(name.clone(), grid);
            }
        }
    }
    Ok(())
}

/// The dimension an `index` binding looks its values up in: the dimension
/// itself, or — with `match` — its dataset's rows found by another column.
fn index_dimension(
    binding: &Binding,
    work: &BTreeMap<&str, Working>,
    dims: &HashMap<String, Dimension>,
) -> Result<Option<Dimension>> {
    let Binding::Index {
        dimension,
        match_column,
        ..
    } = binding
    else {
        return Ok(None);
    };
    let Some(dim) = dims.get(dimension) else {
        return Ok(None);
    };
    match match_column {
        None => Ok(Some(dim.clone())),
        Some(m) => {
            let target = &work[dim.coords.dataset.as_str()];
            let column = column_of(&target.frame, m)?;
            dim.matching(&dim.coords.dataset, m, column).map(Some)
        }
    }
}

/// Apply the `unknown` policy to `dataset`'s `index` bindings — into other
/// datasets' dimensions (`own` false) or into the dimensions over itself
/// (`own` true).
#[allow(clippy::too_many_arguments)]
fn unknowns(
    dataset: &str,
    policy: Policy,
    bindings: &[(&Declaration, &Binding)],
    work: &BTreeMap<&str, Working>,
    dims: &HashMap<String, Dimension>,
    earlier: &[DropReport],
    dropped: &mut [bool],
    drops: &mut Vec<DropReport>,
    own: bool,
) -> Result<()> {
    let w = &work[dataset];
    for (decl, binding) in bindings {
        let Binding::Index {
            dataset: d,
            column: column_name,
            dimension,
            ..
        } = binding
        else {
            continue;
        };
        if d != dataset {
            continue;
        }
        let Some(dim) = index_dimension(binding, work, dims)? else {
            // Not built yet: a dimension over this dataset, on the other pass.
            continue;
        };
        if (dim.coords.dataset == dataset) != own {
            continue;
        }
        let column = column_of(&w.frame, column_name)?;
        let missing: Vec<usize> = (0..w.frame.rows)
            .filter(|i| {
                !dropped[*i] && !column.is_null_at(*i) && dim.position(column, *i).is_none()
            })
            .collect();
        let Some(&first) = missing.first() else {
            continue;
        };
        let value = cell_text(column, first).unwrap_or_default();
        let first_name = w.row_name(first);
        // Why the target may be missing rows: its own drops, said beside ours.
        let target = dim.coords.dataset.as_str();
        let cascade: Vec<&str> = earlier
            .iter()
            .filter(|r| r.dataset == target && target != dataset)
            .map(|r| r.sentence.as_str())
            .collect();
        let because = if cascade.is_empty() {
            String::new()
        } else {
            format!("; `{target}` itself {}", cascade.join(", and "))
        };
        match policy {
            Policy::Refuse => {
                return Err(Error::invalid(format!(
                    "{}: {} of `{dataset}` {} a `{column_name}` that is not a position of \
                     `{dimension}` (the first is `{value}`, on row `{first_name}`){because}; \
                     fix {}, filter {} out, or set `{dataset}`'s `unknown` policy to `drop`",
                    about(decl, binding),
                    rows_word(missing.len()),
                    if missing.len() == 1 { "has" } else { "have" },
                    them(missing.len()),
                    them(missing.len()),
                )));
            }
            Policy::Drop => {
                for i in &missing {
                    dropped[*i] = true;
                }
                drops.push(DropReport {
                    dataset: dataset.to_owned(),
                    column: column_name.clone(),
                    dimension: Some(dimension.clone()),
                    rows: missing.len(),
                    first: first_name.clone(),
                    sentence: format!(
                        "dropped {} of `{dataset}` whose `{column_name}` is not a position of \
                         `{dimension}` (the first is `{value}`, on row `{first_name}`){because}",
                        rows_word(missing.len())
                    ),
                });
            }
        }
    }
    Ok(())
}

/// An `index` binding's 1-based positions over its dataset's rows as they now
/// stand.
fn index_positions(
    decl: &Declaration,
    binding: &Binding,
    work: &BTreeMap<&str, Working>,
    dims: &HashMap<String, Dimension>,
) -> Result<Vec<i64>> {
    let Binding::Index {
        dataset, column, ..
    } = binding
    else {
        unreachable!("only an index has positions")
    };
    let dim = index_dimension(binding, work, dims)?
        .ok_or_else(|| internal("an index into a dimension not yet resolved"))?;
    let w = &work[dataset.as_str()];
    let values = column_of(&w.frame, column)?;
    (0..w.frame.rows)
        .map(|i| {
            dim.position(values, i).map(|p| p as i64).ok_or_else(|| {
                Error::invalid(format!(
                    "{}: row `{}` has no position in `{}`",
                    about(decl, binding),
                    w.row_name(i),
                    dim.coords.name
                ))
            })
        })
        .collect()
}

/// One column's values as numbers — ints stay ints, booleans are 0 and 1, a
/// date needs a `time` scale, and text is refused.
fn numbers(
    decl: &Declaration,
    binding: &Binding,
    dataset: &str,
    name: &str,
    column: &Column,
    time: Option<&super::spec::TimeScale>,
    rows: impl Iterator<Item = usize> + Clone,
) -> Result<Values> {
    let at = |msg: String| Error::invalid(format!("{}: {msg}", about(decl, binding)));
    let null = |i: usize| at(format!("`{name}` of `{dataset}` is null on row {}", i + 1));
    match column {
        Column::Int(v) => rows
            .map(|i| v[i].ok_or_else(|| null(i)))
            .collect::<Result<_>>()
            .map(Values::Int),
        Column::Bool(v) => rows
            .map(|i| v[i].map(i64::from).ok_or_else(|| null(i)))
            .collect::<Result<_>>()
            .map(Values::Int),
        Column::Float(v) => rows
            .map(|i| v[i].ok_or_else(|| null(i)))
            .collect::<Result<_>>()
            .map(Values::Real),
        Column::Date(v) => {
            let Some(time) = time else {
                return Err(at(format!(
                    "`{name}` of `{dataset}` is a date, and a date reaches Stan as a number only \
                     with a `time` scale — its `unit` and its `origin`"
                )));
            };
            let unit = time.seconds().map_err(|e| at(e.to_string()))?;
            let earliest = rows.clone().filter_map(|i| v[i]).min();
            let origin = time.origin(earliest).map_err(|e| at(e.to_string()))?;
            rows.map(|i| {
                v[i].map(|t| (t - origin) as f64 / unit as f64)
                    .ok_or_else(|| null(i))
            })
            .collect::<Result<_>>()
            .map(Values::Real)
        }
        Column::Str(_) => Err(at(format!(
            "`{name}` of `{dataset}` is text, and text reaches Stan only through an `index` or a \
             `design`"
        ))),
        Column::Null(_) => {
            let mut rows = rows;
            match rows.next() {
                Some(i) => Err(null(i)),
                None => Ok(Values::Real(Vec::new())),
            }
        }
    }
}

/// One binding's value.
fn evaluate(
    decl: &Declaration,
    binding: &Binding,
    work: &BTreeMap<&str, Working>,
    dims: &HashMap<String, Dimension>,
    bound: &HashMap<&str, Bound>,
    designs: &mut BTreeMap<String, DesignCoordinates>,
) -> Result<Bound> {
    let at = |msg: String| Error::invalid(format!("{}: {msg}", about(decl, binding)));
    let frame = |dataset: &str| &work[dataset].frame;
    let plain = |tensor: Tensor| Bound {
        tensor,
        rows_of: None,
        what: None,
        groups: None,
    };
    let over = |tensor: Tensor, dataset: &str| Bound {
        tensor,
        rows_of: Some(dataset.to_owned()),
        what: None,
        groups: None,
    };
    let sized = |n: usize, what: String| Bound {
        tensor: Tensor::int(n as i64),
        rows_of: None,
        what: Some(what),
        groups: None,
    };
    Ok(match binding {
        Binding::Value { value } => {
            let tensor = Tensor::from_literal(value).map_err(|e| at(e.to_string()))?;
            match tensor.as_int() {
                Some(n) => Bound {
                    what: Some(format!("the value given, {n}")),
                    ..plain(tensor)
                },
                None => plain(tensor),
            }
        }
        Binding::Count { dataset } => {
            sized(frame(dataset).rows, format!("the row count of `{dataset}`"))
        }
        Binding::Size { dimension } => sized(
            dims[dimension.as_str()].size(),
            format!("the size of `{dimension}`"),
        ),
        Binding::Column {
            dataset,
            column,
            time,
        } => {
            let f = frame(dataset);
            let values = numbers(
                decl,
                binding,
                dataset,
                column,
                column_of(f, column)?,
                time.as_ref(),
                0..f.rows,
            )?;
            over(
                Tensor {
                    shape: vec![f.rows],
                    values,
                },
                dataset,
            )
        }
        Binding::Columns { dataset, columns } => {
            let f = frame(dataset);
            let mut each = Vec::with_capacity(columns.len());
            for name in columns {
                let column = column_of(f, name)?;
                each.push(numbers(
                    decl,
                    binding,
                    dataset,
                    name,
                    column,
                    None,
                    0..f.rows,
                )?);
            }
            let real = each.iter().any(|v| matches!(v, Values::Real(_)));
            let k = columns.len();
            let values = if real {
                let mut out = vec![0.0; f.rows * k];
                for (j, v) in each.iter().enumerate() {
                    for i in 0..f.rows {
                        out[i * k + j] = v.number(i).unwrap_or(f64::NAN);
                    }
                }
                Values::Real(out)
            } else {
                let mut out = vec![0; f.rows * k];
                for (j, v) in each.iter().enumerate() {
                    if let Values::Int(v) = v {
                        for i in 0..f.rows {
                            out[i * k + j] = v[i];
                        }
                    }
                }
                Values::Int(out)
            };
            over(
                Tensor {
                    shape: vec![f.rows, k],
                    values,
                },
                dataset,
            )
        }
        Binding::Design {
            dataset,
            columns,
            standardise,
        } => {
            let f = frame(dataset);
            let sub = Frame::new(
                columns
                    .iter()
                    .map(|c| Ok((c.clone(), column_of(f, c)?.clone())))
                    .collect::<Result<_>>()?,
                Vec::new(),
            )?;
            let encoding = fit_encoding(&sub, &Outcome::Cluster, *standardise)
                .map_err(|e| at(e.to_string()))?;
            let encoded = apply_encoding(&encoding, &sub).map_err(|e| at(e.to_string()))?;
            designs.insert(
                decl.name.clone(),
                DesignCoordinates {
                    columns: encoding.feature_names(),
                    encoding: encoding.to_json()?,
                },
            );
            over(
                Tensor {
                    shape: vec![f.rows, encoding.width()],
                    values: Values::Real(encoded.features.values().to_vec()),
                },
                dataset,
            )
        }
        Binding::Width { of } => {
            let design = &bound[of.as_str()];
            sized(
                design.tensor.shape.get(1).copied().unwrap_or(0),
                format!("the width of `{of}`"),
            )
        }
        Binding::Index {
            dataset, dimension, ..
        } => Bound {
            groups: Some(dims[dimension.as_str()].size()),
            ..over(
                Tensor::ints(index_positions(decl, binding, work, dims)?),
                dataset,
            )
        },
        Binding::Present { dataset, column } | Binding::Absent { dataset, column } => {
            let f = frame(dataset);
            let c = column_of(f, column)?;
            let want_null = matches!(binding, Binding::Absent { .. });
            plain(Tensor::ints(
                (0..f.rows)
                    .filter(|i| c.is_null_at(*i) == want_null)
                    .map(|i| i as i64 + 1)
                    .collect(),
            ))
        }
        Binding::CountPresent { dataset, column } | Binding::CountAbsent { dataset, column } => {
            let f = frame(dataset);
            let c = column_of(f, column)?;
            let want_null = matches!(binding, Binding::CountAbsent { .. });
            let n = (0..f.rows)
                .filter(|i| c.is_null_at(*i) == want_null)
                .count();
            sized(
                n,
                format!(
                    "the number of rows of `{dataset}` where `{column}` is {}",
                    if want_null { "null" } else { "not null" }
                ),
            )
        }
        Binding::PresentValues { dataset, column } => {
            let f = frame(dataset);
            let c = column_of(f, column)?;
            let rows: Vec<usize> = (0..f.rows).filter(|i| !c.is_null_at(*i)).collect();
            let values = numbers(
                decl,
                binding,
                dataset,
                column,
                c,
                None,
                rows.iter().copied(),
            )?;
            plain(Tensor {
                shape: vec![rows.len()],
                values,
            })
        }
        Binding::SegmentStart { index, .. } | Binding::SegmentSize { index, .. } => {
            let idx = &bound[index.as_str()];
            let Values::Int(positions) = &idx.tensor.values else {
                unreachable!("an index is ints")
            };
            let groups = idx
                .groups
                .ok_or_else(|| internal("an index without its dimension's size"))?;
            let mut size = vec![0i64; groups];
            for p in positions {
                size[(*p - 1) as usize] += 1;
            }
            if matches!(binding, Binding::SegmentSize { .. }) {
                plain(Tensor::ints(size))
            } else {
                let mut start = Vec::with_capacity(groups);
                let mut next = 1;
                for s in &size {
                    start.push(next);
                    next += s;
                }
                plain(Tensor::ints(start))
            }
        }
    })
}

/// Each bound integer scalar, and what it is the size of.
type Sizes<'a> = HashMap<&'a str, (i64, Option<String>)>;

/// One declaration against its bound value (§10's data-time half): the
/// element type, the rank, every size expression that evaluates, and every
/// bound that evaluates. Answers the value as the data file writes it — a
/// `real` declaration's ints as reals.
fn check_declaration(
    decl: &Declaration,
    binding: &Binding,
    bound: Bound,
    sizes: &Sizes<'_>,
    scalars: &HashMap<&str, f64>,
    work: &BTreeMap<&str, Working>,
) -> Result<Tensor> {
    let at = |msg: String| Error::invalid(format!("{}: {msg}", about(decl, binding)));
    let mut tensor = bound.tensor;
    match (decl.element, &tensor.values) {
        (Element::Int, Values::Real(v)) if !v.is_empty() => {
            return Err(at(
                "it is an int, and its binding has real values; if they are whole numbers, \
                 round them in the dataset (`round(x)`)"
                    .to_owned(),
            ));
        }
        (Element::Int, Values::Real(_)) => {
            tensor = Tensor {
                shape: tensor.shape,
                values: Values::Int(Vec::new()),
            };
        }
        (Element::Real, Values::Int(_)) => tensor = tensor.into_real(),
        _ => {}
    }
    if tensor.shape.len() != decl.rank() {
        return Err(at(format!(
            "it has {} but its binding has {}",
            axes(decl.rank()),
            axes(tensor.shape.len())
        )));
    }

    let lookup = |name: &str| sizes.get(name).map(|(n, _)| *n);
    for (axis, size) in decl.dims.iter().enumerate() {
        let Some(expected) = size.eval(&lookup) else {
            continue;
        };
        let actual = tensor.shape[axis];
        if expected == actual as i64 {
            continue;
        }
        let has = if tensor.shape.len() == 1 {
            format!("has {actual} values")
        } else {
            format!(
                "is {} (axis {} is {actual})",
                tensor
                    .shape
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(" × "),
                axis + 1
            )
        };
        return Err(Error::invalid(format!(
            "`{}` is declared `{}` with {}, but its binding `{}` {has}",
            decl.name,
            decl.stan_type,
            explain(size, expected, sizes),
            binding.describe()
        )));
    }

    let scalar = |name: &str| scalars.get(name).copied();
    let lower = decl
        .lower
        .as_deref()
        .and_then(|t| Some((t, eval_bound(t, &scalar)?)));
    let upper = decl
        .upper
        .as_deref()
        .and_then(|t| Some((t, eval_bound(t, &scalar)?)));
    if lower.is_none() && upper.is_none() {
        return Ok(tensor);
    }
    let inner: usize = tensor.shape.iter().skip(1).product();
    for i in 0..tensor.count() {
        let Some(v) = tensor.values.number(i) else {
            continue;
        };
        let broken = match (lower, upper) {
            (Some((text, l)), _) if v < l => Some(("below its lower", text, l)),
            (_, Some((text, u))) if v > u => Some(("above its upper", text, u)),
            _ => None,
        };
        let Some((side, text, limit)) = broken else {
            continue;
        };
        let element = if tensor.shape.is_empty() {
            "its value".to_owned()
        } else {
            format!("element {}", element_index(&tensor.shape, i))
        };
        let row = match (&bound.rows_of, tensor.shape.is_empty()) {
            (Some(dataset), false) => {
                format!(
                    " (row `{}` of `{dataset}`)",
                    work[dataset.as_str()].row_name(i / inner.max(1))
                )
            }
            _ => String::new(),
        };
        let limit = if text.trim().parse::<f64>().ok() == Some(limit) {
            format!("`{}`", text.trim())
        } else {
            format!("`{}` = {}", text.trim(), number_text(limit))
        };
        return Err(at(format!(
            "{element} is {}{row}, {side} bound {limit}",
            tensor.values.json(i)
        )));
    }
    Ok(tensor)
}

/// `[3]`, `[2, 1]` — the 1-based index of the `i`th scalar of `shape`.
fn element_index(shape: &[usize], mut i: usize) -> String {
    let mut index = vec![0; shape.len()];
    for axis in (0..shape.len()).rev() {
        let n = shape[axis].max(1);
        index[axis] = i % n + 1;
        i /= n;
    }
    format!(
        "[{}]",
        index
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn number_text(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        x.to_string()
    }
}

/// "`N` = 919 (the row count of `main`)", "`N + 1` = 920 (`N` = 919, the row
/// count of `main`)".
fn explain(size: &crate::interface::SizeExpr, value: i64, sizes: &Sizes<'_>) -> String {
    let what = |name: &str| -> String {
        match sizes.get(name) {
            Some((n, Some(what))) => format!("`{name}` = {n}, {what}"),
            Some((n, None)) => format!("`{name}` = {n}"),
            None => format!("`{name}`"),
        }
    };
    if let Some(name) = size.identifier() {
        return match sizes.get(name) {
            Some((_, Some(w))) => format!("`{name}` = {value} ({w})"),
            _ => format!("`{name}` = {value}"),
        };
    }
    let vars: Vec<String> = size.variables().into_iter().map(what).collect();
    if vars.is_empty() {
        format!("`{}` = {value}", size.text)
    } else {
        format!("`{}` = {value} ({})", size.text, vars.join("; "))
    }
}

/// A declared bound's value, when it is numbers, bound scalars and `+ - * /`
/// in parentheses. Anything else — `negative_infinity()`, a transformed-data
/// variable — is `None`: not checked here, and Stan checks it.
fn eval_bound(text: &str, lookup: &dyn Fn(&str) -> Option<f64>) -> Option<f64> {
    let tokens = bound_tokens(text)?;
    let mut parser = BoundParser {
        tokens: &tokens,
        at: 0,
        lookup,
    };
    let value = parser.sum()?;
    (parser.at == tokens.len()).then_some(value)
}

#[derive(Debug, Clone, PartialEq)]
enum BoundToken {
    Number(f64),
    Name(String),
    Op(char),
}

fn bound_tokens(text: &str) -> Option<Vec<BoundToken>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || c == '.' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_digit()
                    || chars[i] == '.'
                    || chars[i] == 'e'
                    || chars[i] == 'E'
                    || ((chars[i] == '-' || chars[i] == '+') && matches!(chars[i - 1], 'e' | 'E')))
            {
                i += 1;
            }
            out.push(BoundToken::Number(
                chars[start..i].iter().collect::<String>().parse().ok()?,
            ));
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(BoundToken::Name(chars[start..i].iter().collect()));
        } else if "+-*/()".contains(c) {
            out.push(BoundToken::Op(c));
            i += 1;
        } else {
            return None;
        }
    }
    Some(out)
}

struct BoundParser<'a> {
    tokens: &'a [BoundToken],
    at: usize,
    lookup: &'a dyn Fn(&str) -> Option<f64>,
}

impl BoundParser<'_> {
    fn peek_op(&self) -> Option<char> {
        match self.tokens.get(self.at) {
            Some(BoundToken::Op(c)) => Some(*c),
            _ => None,
        }
    }

    fn sum(&mut self) -> Option<f64> {
        let mut value = self.product()?;
        while let Some(op @ ('+' | '-')) = self.peek_op() {
            self.at += 1;
            let right = self.product()?;
            value = if op == '+' {
                value + right
            } else {
                value - right
            };
        }
        Some(value)
    }

    fn product(&mut self) -> Option<f64> {
        let mut value = self.unary()?;
        while let Some(op @ ('*' | '/')) = self.peek_op() {
            self.at += 1;
            let right = self.unary()?;
            value = if op == '*' {
                value * right
            } else {
                value / right
            };
        }
        Some(value)
    }

    fn unary(&mut self) -> Option<f64> {
        if self.peek_op() == Some('-') {
            self.at += 1;
            return Some(-self.unary()?);
        }
        let token = self.tokens.get(self.at)?.clone();
        self.at += 1;
        match token {
            BoundToken::Number(x) => Some(x),
            BoundToken::Name(name) => {
                // A function call is not evaluated here.
                if self.peek_op() == Some('(') {
                    return None;
                }
                (self.lookup)(&name)
            }
            BoundToken::Op('(') => {
                let value = self.sum()?;
                (self.peek_op() == Some(')')).then(|| self.at += 1)?;
                Some(value)
            }
            BoundToken::Op(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_evaluates_when_it_is_arithmetic_over_bound_scalars() {
        let lookup = |name: &str| match name {
            "J" => Some(85.0),
            "sigma" => Some(0.5),
            _ => None,
        };
        assert_eq!(eval_bound("1", &lookup), Some(1.0));
        assert_eq!(eval_bound(" J ", &lookup), Some(85.0));
        assert_eq!(eval_bound("-(J - 5) * 2", &lookup), Some(-160.0));
        assert_eq!(eval_bound("1e-3", &lookup), Some(0.001));
        assert_eq!(eval_bound("sigma / 2", &lookup), Some(0.25));
        assert_eq!(eval_bound("negative_infinity()", &lookup), None);
        assert_eq!(eval_bound("K", &lookup), None);
        assert_eq!(eval_bound("J +", &lookup), None);
    }

    #[test]
    fn an_element_is_named_by_its_one_based_index() {
        assert_eq!(element_index(&[5], 2), "[3]");
        assert_eq!(element_index(&[2, 3], 4), "[2, 2]");
    }
}

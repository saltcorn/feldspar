//! A notebook's `nb` surface (analytics TODO A10.4): what a cell's
//! `nb.dataset(…)`, `nb.plot(…)`, `nb.table(…)`, `nb.test(…)`, `nb.card(…)`,
//! `nb.map(…)` and `nb.show(…)` are, written once as **plans** and given a
//! JavaScript and a Python prelude in the way `db` is.
//!
//! A cell reaches the server only through `db`'s seam, as `{ op: "nb", what,
//! … }` requests, which [`answer`] answers — so a cell's `nb` is bounded by the
//! same call budget as its `db`, and both languages (and an LLM prompt cell's
//! tools) send the same plans:
//!
//! ```json
//! { "op": "nb", "what": "plot",  "spec": { "data": …, "mark": "bar", "x": "a", "y": "b" } }
//! { "op": "nb", "what": "table", "data": …, "spec": { "rows": ["a"], "cells": ["mean(b)"] } }
//! { "op": "nb", "what": "test",  "data": …, "spec": { "y": "price", "x": "neighbourhood" } }
//! { "op": "nb", "what": "card",  "data": "Houses", "spec": { "value": "mean(price)" } }
//! { "op": "nb", "what": "map",   "spec": { … a map spec … } }
//! { "op": "nb", "what": "text",  "markdown": "## Prices" }
//! { "op": "nb", "what": "rows",  "data": …, "limit": 10 }
//! { "op": "nb", "what": "columns", "data": … }
//! { "op": "nb", "what": "fit",   "model": "House prices" }   (the server's: it starts a fit)
//! ```
//!
//! Each panel request answers `{ panel }`, the panel already drawn once on the
//! server: a spec that does not draw is refused with its own sentence, which
//! the cell reports with the line it came from.
//!
//! **Data** (`data`, and a plot spec's `data`) is a dataset's or a table's
//! name, a data reference (A2.1, A10.2), or a dataset built in the cell —
//! `nb.dataset("houses").filter("price > 100000")` — which crosses as its base
//! and its steps and becomes a non-persisted dataset ([`data_ref`]):
//!
//! ```json
//! { "__nb": "dataset", "base": "houses", "name": null,
//!   "steps": [ { "method": "filter", "args": ["price > 100000"] } ] }
//! ```
//!
//! Its id is made from what it is, so the same chain is the same dataset in
//! every cell and every run — and once it is saved (**Save as dataset**), the
//! chain refers to the stored dataset of that id.
//!
//! **A cell's outputs** come back with its answer, as one envelope the
//! preludes make ([`cell_outputs`]): what it showed and printed, in order, and
//! its last value — or why it stopped, with the line.

use std::collections::BTreeSet;

use sc_catalog::Catalog;
use sc_dataset::{
    AggregateOp, Base, Caller, ColType, DatasetDef, DatasetId, GroupKey, LimitMode, LimitOp, Op,
    Operation, Page, SelectColumn, SelectOp, SortKey, SortOp, Summary, SummaryFunction,
};
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::card::{CardFunction, CardValue, StatCard};
use crate::notebook::Output;
use crate::panel::{Panel, PanelBody, render_panel};
use crate::plot::{
    AggregateFn, Bin, Cell, Channel, DataRef, FieldDef, Layer, Mark, PlotSpec, Stat, TableSpec,
};
use crate::stats::TestSpec;

/// How many rows a dataset shown as a cell's output keeps.
pub const PREVIEW_ROWS: u64 = 20;

/// The most rows `rows` answers.
pub const MAX_ROWS: u64 = 1_000;

/// The JavaScript prelude: run once in a session, it defines
/// `globalThis.__nbMake(db)`, which each cell's body calls for its `nb`
/// (`sc_expr::notebook_cell`).
pub const NB_JS: &str = include_str!("nb/nb.js");

/// The Python prelude: run once in a session's globals, it defines `nb`,
/// `print` and the session's `__session_cell__`, which runs each cell.
pub const NB_PY: &str = include_str!("nb/nb.py");

// --- data -------------------------------------------------------------------

/// The data reference `value` names: a stored dataset's name, a table's name,
/// a data reference, or a dataset built in a cell.
pub async fn data_ref(catalog: &Catalog, value: &Json) -> Result<DataRef> {
    match value {
        Json::String(name) => match sc_dataset::load_dataset_by_name(catalog, name).await? {
            Some(def) => Ok(DataRef::Dataset { dataset: def.id }),
            None if table_exists(catalog, name)? => {
                inline(
                    catalog,
                    DatasetDef::new(name.clone(), Base::table(name.clone())),
                )
                .await
            }
            None => Err(no_data(catalog, name).await),
        },
        Json::Object(fields) if fields.get("__nb").and_then(Json::as_str) == Some("dataset") => {
            inline(catalog, chain_definition(catalog, fields).await?).await
        }
        Json::Object(fields) if fields.contains_key("kind") => {
            serde_json::from_value(value.clone())
                .map_err(|e| Error::invalid(format!("this is not a data reference: {e}")))
        }
        Json::Null => Err(Error::invalid(
            "no data was given: name a dataset or a table, or pass nb.dataset(…)",
        )),
        other => Err(Error::invalid(format!(
            "`{}` is not data: name a dataset or a table, or pass nb.dataset(…)",
            short(other)
        ))),
    }
}

/// `def` as a reference: to the stored dataset of its id, once it has been
/// saved, and otherwise to itself.
async fn inline(catalog: &Catalog, mut def: DatasetDef) -> Result<DataRef> {
    def.id = content_id(&def);
    if sc_dataset::load_dataset(catalog, def.id).await?.is_some() {
        return Ok(DataRef::Dataset { dataset: def.id });
    }
    Ok(DataRef::inline(def))
}

/// The id a non-persisted dataset is given: made from its base and its
/// operations, so the same chain is the same dataset each time it is run.
pub fn content_id(def: &DatasetDef) -> DatasetId {
    let what = json!({ "base": def.base, "operations": def.operations });
    DatasetId(Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        what.to_string().as_bytes(),
    ))
}

fn table_exists(catalog: &Catalog, name: &str) -> Result<bool> {
    Ok(catalog.tables()?.iter().any(|t| t.name == name))
}

async fn no_data(catalog: &Catalog, name: &str) -> Error {
    let mut known: Vec<String> = sc_dataset::list_datasets(catalog)
        .await
        .map(|ds| ds.into_iter().map(|d| d.name).collect())
        .unwrap_or_default();
    known.sort();
    let hint = match known.is_empty() {
        true => String::new(),
        false => format!("; the datasets are {}", quoted(&known)),
    };
    Error::invalid(format!(
        "there is no dataset or table called `{name}`{hint}"
    ))
}

/// The definition a cell's dataset chain is: its base resolved — a stored
/// dataset of that name, else a table — and its steps made operations.
pub async fn chain_definition(catalog: &Catalog, chain: &Map<String, Json>) -> Result<DatasetDef> {
    let base = match chain.get("base") {
        Some(Json::String(name)) => match sc_dataset::load_dataset_by_name(catalog, name).await? {
            Some(def) => Base::dataset(def.id),
            None if table_exists(catalog, name)? => Base::table(name.clone()),
            None => return Err(no_data(catalog, name).await),
        },
        Some(base @ Json::Object(_)) => serde_json::from_value(base.clone())
            .map_err(|e| Error::invalid(format!("a dataset's base does not read: {e}")))?,
        _ => {
            return Err(Error::invalid(
                "a dataset starts from a dataset's or a table's name, as in nb.dataset(\"houses\")",
            ));
        }
    };
    let name = match (chain.get("name").and_then(Json::as_str), &base) {
        (Some(name), _) if !name.trim().is_empty() => name.trim().to_owned(),
        (_, Base::Table { table }) => table.clone(),
        (_, Base::Dataset { .. }) => chain
            .get("base")
            .and_then(Json::as_str)
            .unwrap_or("dataset")
            .to_owned(),
    };
    let mut def = DatasetDef::new(name, base);
    let steps = match chain.get("steps") {
        None | Some(Json::Null) => &[][..],
        Some(Json::Array(steps)) => steps.as_slice(),
        Some(_) => return Err(Error::invalid("a dataset's steps are a list")),
    };
    for (i, step) in steps.iter().enumerate() {
        let method = step.get("method").and_then(Json::as_str).unwrap_or("");
        let args = step
            .get("args")
            .and_then(Json::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let op = operation(method, args)
            .map_err(|e| Error::invalid(format!(".{method}(): {}", e.message())))?;
        def.operations
            .push(Operation::new(format!("op{}", i + 1), op));
    }
    Ok(def)
}

/// One step of a chain — the method and what it was called with — as the
/// operation it is.
pub fn operation(method: &str, args: &[Json]) -> Result<Op> {
    let text = |i: usize, what: &str| -> Result<String> {
        match args.get(i) {
            Some(Json::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
            _ => Err(Error::invalid(format!("takes {what}"))),
        }
    };
    let names = |from: usize| -> Result<Vec<String>> {
        let mut out = Vec::new();
        for a in &args[from.min(args.len())..] {
            match a {
                Json::String(s) => out.push(s.clone()),
                Json::Array(items) => {
                    for item in items {
                        out.push(
                            item.as_str()
                                .map(str::to_owned)
                                .ok_or_else(|| Error::invalid("takes column names"))?,
                        );
                    }
                }
                _ => return Err(Error::invalid("takes column names")),
            }
        }
        Ok(out)
    };
    match method {
        "filter" => Ok(Op::filter(text(
            0,
            "a condition, as in .filter(\"price > 100\")",
        )?)),
        "mutate" | "calculated" => {
            if method == "calculated" {
                return Ok(Op::calculated(
                    text(0, "a column's name and its formula")?,
                    text(1, "a column's name and its formula")?,
                ));
            }
            match args.first() {
                Some(Json::Object(columns)) if columns.len() == 1 => {
                    let (name, formula) = columns
                        .iter()
                        .next()
                        .ok_or_else(|| Error::invalid("takes { name: formula }"))?;
                    let formula = formula.as_str().ok_or_else(|| {
                        Error::invalid("takes { name: formula }, the formula as text")
                    })?;
                    Ok(Op::calculated(name.clone(), formula))
                }
                _ => Err(Error::invalid(
                    "takes one { name: formula }, as in .mutate({ per_m2: \"price / area\" }); \
                     chain one .mutate() per column",
                )),
            }
        }
        "select" => {
            let columns = names(0)?;
            if columns.is_empty() {
                return Err(Error::invalid("takes the columns to keep"));
            }
            Ok(Op::Select(SelectOp {
                columns: columns.into_iter().map(SelectColumn::keep).collect(),
            }))
        }
        "sort" => {
            let keys = names(0)?;
            if keys.is_empty() {
                return Err(Error::invalid(
                    "takes the columns to sort by, `-price` for descending",
                ));
            }
            Ok(Op::Sort(SortOp {
                keys: keys
                    .into_iter()
                    .map(|k| match k.strip_prefix('-') {
                        Some(rest) => SortKey {
                            formula: rest.trim().to_owned(),
                            descending: true,
                        },
                        None => SortKey {
                            formula: k,
                            descending: false,
                        },
                    })
                    .collect(),
            }))
        }
        "aggregate" => {
            let (by, summaries) = match (args.first(), args.get(1)) {
                (Some(Json::Object(s)), None) => (Vec::new(), s.clone()),
                (by, s) => (
                    group_names(by)?,
                    s.and_then(Json::as_object).cloned().unwrap_or_default(),
                ),
            };
            let mut out = Vec::new();
            for (name, source) in &summaries {
                let source = source.as_str().ok_or_else(|| {
                    Error::invalid(format!(
                        "`{name}`'s summary is written as text, like \"mean(price)\""
                    ))
                })?;
                out.push(summary(name, source)?);
            }
            if out.is_empty() {
                out.push(Summary::count("n"));
            }
            Ok(Op::Aggregate(AggregateOp {
                group_by: by.into_iter().map(GroupKey::column).collect(),
                summaries: out,
            }))
        }
        "limit" | "sample" => {
            let n = args
                .first()
                .and_then(Json::as_u64)
                .ok_or_else(|| Error::invalid("takes a whole number of rows"))?;
            Ok(Op::Limit(LimitOp {
                mode: match method {
                    "limit" => LimitMode::First,
                    _ => LimitMode::Sample,
                },
                n,
                seed: args.get(1).and_then(Json::as_i64).unwrap_or(0),
                group_by: Vec::new(),
                order: Vec::new(),
            }))
        }
        "then" => {
            let kind = text(0, "an operation's kind and its parameters")?;
            let params = args.get(1).cloned().unwrap_or_else(|| json!({}));
            serde_json::from_value(json!({ "kind": kind, "params": params }))
                .map_err(|e| Error::invalid(format!("`{kind}` does not read: {e}")))
        }
        "" => Err(Error::invalid("a step names no method")),
        other => Err(Error::invalid(format!(
            "`{other}` is not a dataset step; the steps are filter, mutate, select, sort, \
             aggregate, limit, sample and then"
        ))),
    }
}

/// The columns an aggregate groups by: none, one, or a list.
fn group_names(by: Option<&Json>) -> Result<Vec<String>> {
    match by {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::String(s)) => Ok(vec![s.clone()]),
        Some(Json::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::invalid(
                        "groups by column names, as in .aggregate([\"town\"], { n: \"count()\" })",
                    )
                })
            })
            .collect(),
        Some(_) => Err(Error::invalid(
            "takes the columns to group by first, as in .aggregate([\"town\"], { n: \"count()\" })",
        )),
    }
}

/// `mean(price)`, `count()`, `count_distinct(town)` as a summary called `name`.
fn summary(name: &str, source: &str) -> Result<Summary> {
    let (function, column) = call_of(source).ok_or_else(|| {
        Error::invalid(format!(
            "`{source}` is not a summary: write count(), mean(price), sum(area) …"
        ))
    })?;
    let function: SummaryFunction = serde_json::from_value(Json::String(function.clone()))
        .map_err(|_| {
            Error::invalid(format!(
                "`{function}` is not a summary; the summaries are count, count_distinct, sum, \
                 mean, median, min, max, sd, first, last"
            ))
        })?;
    Ok(Summary {
        name: name.to_owned(),
        function,
        column: column.filter(|c| !c.is_empty()),
        order: None,
    })
}

/// `fn(arg)` as its two parts, the argument trimmed and `None` when empty.
fn call_of(source: &str) -> Option<(String, Option<String>)> {
    let source = source.trim();
    let open = source.find('(')?;
    let inner = source.strip_suffix(')')?.get(open + 1..)?;
    let function = source[..open].trim();
    if function.is_empty()
        || !function
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let inner = inner.trim();
    Some((
        function.to_lowercase(),
        (!inner.is_empty()).then(|| inner.to_owned()),
    ))
}

// --- specs --------------------------------------------------------------------

/// A plot spec, written in full (with `layers`) or short: `{ data, mark, x, y,
/// color, size, shape, label, stat, bins }`, where `mark` may also be
/// `histogram`, `boxplot`, `density` or `smooth`, and `stat` is a stat or
/// the name of one (`count`, `mean`, `sum`, `median`, `min`, `max`,
/// `boxplot`, `density`, `smooth`, `summary`).
pub async fn plot_spec(catalog: &Catalog, spec: &Json) -> Result<PlotSpec> {
    let Json::Object(fields) = spec else {
        return Err(Error::invalid(
            "nb.plot() takes a spec, as in nb.plot({ data: d, mark: \"bar\", x: \"town\", y: \"n\" })",
        ));
    };
    let data = data_ref(catalog, fields.get("data").unwrap_or(&Json::Null)).await?;
    if fields.contains_key("layers") {
        let mut full = fields.clone();
        full.insert(
            "data".into(),
            serde_json::to_value(&data).map_err(|e| Error::msg(e.to_string()))?,
        );
        return serde_json::from_value(Json::Object(full))
            .map_err(|e| Error::invalid(format!("this plot spec does not read: {e}")));
    }
    let mark_name = fields
        .get("mark")
        .and_then(Json::as_str)
        .unwrap_or("point")
        .to_lowercase();
    let has_y = fields.get("y").is_some_and(|y| !y.is_null());
    let (mark, mut stat, bin_x) = match mark_name.as_str() {
        "histogram" => (Mark::Bar, Stat::Count, true),
        "box" | "boxplot" => (Mark::Box, Stat::boxplot(), false),
        "density" => (Mark::Area, Stat::density(), false),
        "smooth" => (
            Mark::Line,
            Stat::smooth(crate::plot::SmoothMethod::Linear),
            false,
        ),
        "bar" if !has_y => (Mark::Bar, Stat::Count, false),
        "heatmap" => (Mark::Rect, Stat::Identity, false),
        other => {
            let mark: Mark =
                serde_json::from_value(Json::String(other.to_owned())).map_err(|_| {
                    Error::invalid(format!(
                        "`{other}` is not a mark; the marks are point, line, bar, area, box, text, \
                     rect, band, errorbar, mosaic, and histogram, boxplot, density and smooth"
                    ))
                })?;
            (mark, Stat::Identity, false)
        }
    };
    if let Some(given) = fields.get("stat").filter(|s| !s.is_null()) {
        stat = stat_of(given)?;
    }
    let mut layer = Layer::new(mark, stat);
    for channel in Channel::ENCODED {
        let Some(value) = fields.get(channel.as_str()).filter(|v| !v.is_null()) else {
            continue;
        };
        let mut field = field_def(value, channel.as_str())?;
        if channel == Channel::X && bin_x && field.bin.is_none() {
            field.bin = Some(Bin {
                width: None,
                bins: fields
                    .get("bins")
                    .and_then(Json::as_u64)
                    .and_then(|b| u32::try_from(b).ok()),
            });
        }
        layer = layer.with(channel, field);
    }
    Ok(PlotSpec::single(data, layer))
}

/// A stat, or the name of one.
fn stat_of(value: &Json) -> Result<Stat> {
    if let Json::String(name) = value {
        let function = |f: AggregateFn| Ok(Stat::aggregate(f));
        return match name.as_str() {
            "identity" => Ok(Stat::Identity),
            "count" => Ok(Stat::Count),
            "sum" => function(AggregateFn::Sum),
            "mean" => function(AggregateFn::Mean),
            "median" => function(AggregateFn::Median),
            "min" => function(AggregateFn::Min),
            "max" => function(AggregateFn::Max),
            "sd" => function(AggregateFn::Sd),
            "boxplot" => Ok(Stat::boxplot()),
            "density" => Ok(Stat::density()),
            "smooth" => Ok(Stat::smooth(crate::plot::SmoothMethod::Linear)),
            "loess" => Ok(Stat::smooth(crate::plot::SmoothMethod::Loess)),
            "summary" => Ok(Stat::summary()),
            other => Err(Error::invalid(format!(
                "`{other}` is not a stat; the stats are identity, count, sum, mean, median, \
                 min, max, sd, boxplot, density, smooth, loess and summary"
            ))),
        };
    }
    serde_json::from_value(value.clone())
        .map_err(|e| Error::invalid(format!("this stat does not read: {e}")))
}

/// A channel's field: a column's name, or `{ field, bin }`.
fn field_def(value: &Json, what: &str) -> Result<FieldDef> {
    match value {
        Json::String(name) => Ok(FieldDef::of(name.clone())),
        Json::Object(_) => serde_json::from_value(value.clone())
            .map_err(|e| Error::invalid(format!("`{what}` does not read: {e}"))),
        other => Err(Error::invalid(format!(
            "`{what}` is a column's name, not `{}`",
            short(other)
        ))),
    }
}

fn field_list(value: Option<&Json>, what: &str) -> Result<Vec<FieldDef>> {
    match value {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items.iter().map(|v| field_def(v, what)).collect(),
        Some(one) => Ok(vec![field_def(one, what)?]),
    }
}

/// A summary table over `data`: `{ rows, columns, cells, totals }`, the
/// fields by name and each cell `count()` or `mean(price)`. With no spec,
/// a row per combination of the text, date and true/false columns (at most
/// two) and the mean of each number column.
pub async fn table_spec(
    catalog: &Catalog,
    caller: &Caller,
    data: &Json,
    spec: &Json,
) -> Result<TableSpec> {
    let data = data_ref(catalog, data).await?;
    let fields = spec.as_object().cloned().unwrap_or_default();
    let mut rows = field_list(fields.get("rows"), "rows")?;
    let columns = field_list(fields.get("columns"), "columns")?;
    let mut cells = Vec::new();
    match fields.get("cells") {
        None | Some(Json::Null) => {}
        Some(Json::Array(items)) => {
            for item in items {
                cells.push(cell_of(item)?);
            }
        }
        Some(one) => cells.push(cell_of(one)?),
    }
    if rows.is_empty() && columns.is_empty() && cells.is_empty() {
        // A foreign key is a category, though its values are numbers.
        let shape = stage_columns(catalog, caller, &data).await?;
        let measure = |c: &sc_dataset::StageColumn| is_number(c.ty) && c.key.is_none();
        for c in &shape {
            if !measure(c) && rows.len() < 2 {
                rows.push(FieldDef::of(c.name.clone()));
            }
        }
        for c in &shape {
            if measure(c) && cells.len() < 6 {
                cells.push(Cell::of(AggregateFn::Mean, c.name.clone()));
            }
        }
        if cells.is_empty() {
            cells.push(Cell::count());
        }
    }
    Ok(TableSpec {
        data,
        fold: None,
        rows,
        columns,
        cells,
        totals: fields.get("totals").and_then(Json::as_bool).unwrap_or(true),
    })
}

fn is_number(ty: ColType) -> bool {
    matches!(ty, ColType::Int | ColType::Float | ColType::Decimal)
}

/// `count()`, `mean(price)`, or `{ field, function }`.
fn cell_of(value: &Json) -> Result<Cell> {
    if let Json::String(source) = value {
        let (function, field) = call_of(source).ok_or_else(|| {
            Error::invalid(format!(
                "`{source}` is not a cell: write count() or mean(price)"
            ))
        })?;
        let function: AggregateFn = serde_json::from_value(Json::String(function.clone()))
            .map_err(|_| {
                Error::invalid(format!(
                    "`{function}` is not a table's summary; they are count, sum, mean, median, \
                     min, max and sd"
                ))
            })?;
        return Ok(Cell { field, function });
    }
    serde_json::from_value(value.clone())
        .map_err(|e| Error::invalid(format!("this cell does not read: {e}")))
}

/// The hypothesis tests over `data`: `{ y, x, by, paired, mu, level }`, the
/// fields by name.
pub async fn test_spec(catalog: &Catalog, data: &Json, spec: &Json) -> Result<TestSpec> {
    let data = data_ref(catalog, data).await?;
    let fields = spec.as_object().cloned().unwrap_or_default();
    let y = field_list(fields.get("y"), "y")?;
    if y.is_empty() {
        return Err(Error::invalid(
            "nb.test() needs `y`, the column (or columns) tested, as in { y: \"price\", x: \"town\" }",
        ));
    }
    let one = |key: &str| -> Result<Option<FieldDef>> {
        match fields.get(key) {
            None | Some(Json::Null) => Ok(None),
            Some(v) => field_def(v, key).map(Some),
        }
    };
    Ok(TestSpec {
        data,
        y,
        x: one("x")?,
        by: one("by")?,
        paired: fields
            .get("paired")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        mu: fields.get("mu").and_then(Json::as_f64).unwrap_or(0.0),
        level: fields.get("level").and_then(Json::as_f64).unwrap_or(0.95),
    })
}

/// A stat card over a stored dataset: `{ value: "mean(price)", filter }`, or a
/// stat card's own spec. A card reads a saved dataset, as a dashboard's does.
pub async fn card(catalog: &Catalog, data: &Json, spec: &Json) -> Result<StatCard> {
    let dataset = match data_ref(catalog, data).await? {
        DataRef::Dataset { dataset } => dataset,
        DataRef::Inline { definition } => {
            return Err(Error::invalid(format!(
                "a stat card reads a saved dataset, and `{}` is the notebook's own: save it \
                 (Save as dataset) or name a stored one",
                definition.name
            )));
        }
        DataRef::FitOutput { .. } => {
            return Err(Error::invalid(
                "a stat card reads a dataset, not a fit's output",
            ));
        }
    };
    let mut fields = spec.as_object().cloned().unwrap_or_default();
    if let Some(Json::String(source)) = fields.get("value").cloned() {
        let (function, column) = call_of(&source).ok_or_else(|| {
            Error::invalid(format!(
                "`{source}` is not a card's value: write count() or mean(price)"
            ))
        })?;
        let function: CardFunction = serde_json::from_value(Json::String(function.clone()))
            .map_err(|_| {
                Error::invalid(format!(
                    "`{function}` is not a card's value; they are count, count_distinct, sum, \
                     mean, median, min and max"
                ))
            })?;
        fields.insert(
            "value".into(),
            serde_json::to_value(CardValue { function, column })
                .map_err(|e| Error::msg(e.to_string()))?,
        );
    }
    fields.insert("dataset".into(), json!(dataset));
    serde_json::from_value(Json::Object(fields))
        .map_err(|e| Error::invalid(format!("this stat card does not read: {e}")))
}

// --- answering ------------------------------------------------------------------

/// Answer one `nb` plan, read as `caller` — every `what` but `fit`, which
/// starts a fit and is the server's.
pub async fn answer(catalog: &Catalog, caller: &Caller, plan: &Json) -> Result<Json> {
    let what = plan.get("what").and_then(Json::as_str).unwrap_or("");
    let get = |key: &str| plan.get(key).unwrap_or(&Json::Null);
    let body = match what {
        "plot" => PanelBody::Plot {
            spec: plot_spec(catalog, get("spec")).await?,
        },
        "table" => PanelBody::SummaryTable {
            spec: table_spec(catalog, caller, get("data"), get("spec")).await?,
        },
        "test" => {
            let tests = test_spec(catalog, get("data"), get("spec")).await?;
            let plot = match get("spec").get("plot") {
                None | Some(Json::Null) | Some(Json::Bool(false)) => None,
                Some(plot) => {
                    let mut plot = plot.clone();
                    if let Json::Object(p) = &mut plot
                        && !p.contains_key("data")
                    {
                        p.insert("data".into(), get("data").clone());
                    }
                    Some(plot_spec(catalog, &plot).await?)
                }
            };
            PanelBody::TestResult { tests, plot }
        }
        "card" => PanelBody::StatCard(card(catalog, get("data"), get("spec")).await?),
        "map" => PanelBody::Map {
            spec: serde_json::from_value(get("spec").clone())
                .map_err(|e| Error::invalid(format!("this map spec does not read: {e}")))?,
        },
        "text" => PanelBody::Text {
            markdown: get("markdown").as_str().unwrap_or_default().to_owned(),
        },
        "rows" => {
            let data = data_ref(catalog, get("data")).await?;
            let limit = get("limit").as_u64().unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS);
            let (columns, rows, total) = read(catalog, caller, &data, limit).await?;
            let rows: Vec<Json> = rows
                .into_iter()
                .map(|r| Json::Object(columns.iter().cloned().zip(r).collect()))
                .collect();
            return Ok(json!({ "columns": columns, "rows": rows, "total": total }));
        }
        "columns" => {
            let data = data_ref(catalog, get("data")).await?;
            let columns = columns_of(catalog, caller, &data).await?;
            return Ok(Json::Array(
                columns
                    .into_iter()
                    .map(|(name, ty)| json!({ "name": name, "type": ty }))
                    .collect(),
            ));
        }
        "" => return Err(Error::invalid("an nb request names no `what`")),
        other => {
            return Err(Error::invalid(format!(
                "nb has no `{other}`; it has dataset, plot, table, test, card, map, markdown, \
                 fit and show"
            )));
        }
    };
    let panel = checked(catalog, caller, Panel::new(body)).await?;
    Ok(json!({ "panel": panel }))
}

/// `panel`, drawn once: refused with the first sentence it draws, so a cell
/// learns at the line that made it that a column is misspelled.
pub async fn checked(catalog: &Catalog, caller: &Caller, panel: Panel) -> Result<Panel> {
    panel.check()?;
    let drawn = render_panel(catalog, caller, &panel).await?;
    let drawn = serde_json::to_value(&drawn).map_err(|e| Error::msg(e.to_string()))?;
    if let Some(error) = first_error(&drawn) {
        return Err(Error::invalid(error));
    }
    Ok(panel)
}

/// The first `error` a drawn panel carries: its own, or a part's.
fn first_error(drawn: &Json) -> Option<String> {
    if let Some(error) = drawn.get("error").and_then(Json::as_str) {
        return Some(error.to_owned());
    }
    ["plot", "table", "tests", "map", "card", "output"]
        .iter()
        .find_map(|part| drawn.get(part)?.get("error")?.as_str().map(str::to_owned))
}

/// The panel showing the output `output` of the fit `fit`: its table, or its
/// plot over the fit's output data.
pub async fn fit_panel(catalog: &Catalog, fit: Uuid, output: &str) -> Result<Panel> {
    let instance = sc_model::require_model_instance(catalog, sc_model::InstanceId(fit)).await?;
    let decls = sc_model::instance_outputs(&instance)?;
    let Some(decl) = decls.iter().find(|d| d.name == output) else {
        let names: Vec<String> = decls.iter().map(|d| d.name.clone()).collect();
        return Err(Error::invalid(format!(
            "this fit has no output `{output}`; it has {}",
            quoted(&names)
        )));
    };
    let body = match &decl.kind {
        sc_model::OutputKind::Plot { data, spec } => PanelBody::Plot {
            spec: crate::model_outputs::output_spec(instance.id, data, spec)?,
        },
        _ => PanelBody::FitTable {
            fit,
            output: output.to_owned(),
        },
    };
    Ok(Panel::new(body))
}

/// What a fit handle is built from: the fit, and a ready panel per output.
pub async fn fit_handle(catalog: &Catalog, model: &str, fit: Uuid) -> Result<Json> {
    let instance = sc_model::require_model_instance(catalog, sc_model::InstanceId(fit)).await?;
    let mut outputs = Vec::new();
    for decl in sc_model::instance_outputs(&instance)? {
        let panel = fit_panel(catalog, fit, &decl.name).await?;
        outputs.push(json!({
            "name": decl.name,
            "label": decl.label,
            "kind": panel.body.kind(),
            "panel": panel,
        }));
    }
    Ok(json!({
        "fit": fit,
        "model": model,
        "status": instance.status,
        "outputs": outputs,
    }))
}

/// The columns `data` has, with their types.
async fn columns_of(
    catalog: &Catalog,
    caller: &Caller,
    data: &DataRef,
) -> Result<Vec<(String, ColType)>> {
    Ok(stage_columns(catalog, caller, data)
        .await?
        .into_iter()
        .map(|c| (c.name, c.ty))
        .collect())
}

/// The columns `data` has, as its last stage has them.
async fn stage_columns(
    catalog: &Catalog,
    caller: &Caller,
    data: &DataRef,
) -> Result<Vec<sc_dataset::StageColumn>> {
    let def = definition(catalog, data).await?;
    let page = sc_dataset::read_stage(catalog, caller, &def, None, Page::first(1)).await?;
    Ok(page.columns)
}

/// The first `limit` rows `data` has: its column names, the rows, and how
/// many there are in all.
async fn read(
    catalog: &Catalog,
    caller: &Caller,
    data: &DataRef,
    limit: u64,
) -> Result<(Vec<String>, Vec<Vec<Json>>, u64)> {
    let def = definition(catalog, data).await?;
    let page = sc_dataset::read_stage(catalog, caller, &def, None, Page::first(limit)).await?;
    Ok((
        page.columns.into_iter().map(|c| c.name).collect(),
        page.rows
            .iter()
            .map(|r| r.iter().map(sc_types::value_to_analysis_json).collect())
            .collect(),
        page.total,
    ))
}

async fn definition(catalog: &Catalog, data: &DataRef) -> Result<DatasetDef> {
    match data {
        DataRef::Dataset { dataset } => sc_dataset::require_dataset(catalog, *dataset).await,
        DataRef::Inline { definition } => Ok((**definition).clone()),
        DataRef::FitOutput { .. } => Err(Error::invalid(
            "a fit's output data is read by plotting it, not as rows",
        )),
    }
}

// --- a cell's outputs ---------------------------------------------------------------

/// What one run of a cell made: its outputs, in order, and why it stopped
/// when it did not finish — the sentence and the line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellAnswer {
    /// What it showed and printed, then its value.
    pub outputs: Vec<Output>,
    /// Why it stopped, if it did.
    pub error: Option<(String, Option<u32>)>,
}

/// The outputs a cell's envelope carries — `{ __nb: "finish", outputs, value,
/// has_value }` or `{ __nb: "fail", outputs, message, line }` — each made an
/// [`Output`]: a panel, a dataset (its first rows read now), text, or a value.
/// The error, when there is one, is the last output.
pub async fn cell_outputs(
    catalog: &Catalog,
    caller: &Caller,
    envelope: &Json,
) -> Result<CellAnswer> {
    let mut answer = CellAnswer::default();
    for lowered in envelope
        .get("outputs")
        .and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        push_output(catalog, caller, lowered, &mut answer).await;
    }
    match envelope.get("__nb").and_then(Json::as_str) {
        Some("fail") => {
            let message = envelope
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or("the cell failed")
                .to_owned();
            let line = envelope
                .get("line")
                .and_then(Json::as_u64)
                .and_then(|l| u32::try_from(l).ok())
                .filter(|l| *l > 0);
            answer.outputs.push(Output::Error {
                message: clip(&message),
                line,
            });
            answer.error = Some((message, line));
        }
        Some("finish") => {
            if envelope.get("has_value").and_then(Json::as_bool) == Some(true) {
                let value = envelope.get("value").unwrap_or(&Json::Null);
                push_output(catalog, caller, value, &mut answer).await;
            }
        }
        // A cell that returned at its top level, past the envelope.
        _ if !envelope.is_null() => push_output(catalog, caller, envelope, &mut answer).await,
        _ => {}
    }
    answer.outputs.truncate(crate::notebook::MAX_OUTPUTS);
    Ok(answer)
}

/// `lowered` as an output, or — when it cannot be one (a dataset that does
/// not read) — the error output saying why.
async fn push_output(catalog: &Catalog, caller: &Caller, lowered: &Json, answer: &mut CellAnswer) {
    match output_of(catalog, caller, lowered).await {
        Ok(Some(output)) => answer.outputs.push(output),
        Ok(None) => {}
        Err(e) => answer.outputs.push(Output::Error {
            message: clip(e.message()),
            line: None,
        }),
    }
}

/// One shown value as an output: a panel, a dataset's first rows, a fit's
/// first table, printed text, or the value itself.
pub async fn output_of(
    catalog: &Catalog,
    caller: &Caller,
    lowered: &Json,
) -> Result<Option<Output>> {
    let kind = lowered.get("__nb").and_then(Json::as_str);
    Ok(Some(match kind {
        Some("panel") => {
            let panel = Panel::from_json(lowered.get("panel").unwrap_or(&Json::Null))?;
            Output::Panel {
                panel: Box::new(panel),
            }
        }
        Some("text") => Output::Text {
            text: clip(lowered.get("text").and_then(Json::as_str).unwrap_or("")),
        },
        Some("dataset") => {
            let data = data_ref(catalog, lowered).await?;
            let (columns, rows, total) = read(catalog, caller, &data, PREVIEW_ROWS).await?;
            Output::Dataset {
                data,
                columns,
                rows,
                total,
            }
        }
        Some("fit") => {
            let first = lowered
                .get("outputs")
                .and_then(Json::as_array)
                .and_then(|outputs| {
                    outputs
                        .iter()
                        .find(|o| o.get("kind").and_then(Json::as_str) == Some("fit_table"))
                        .or_else(|| outputs.first())
                })
                .and_then(|o| o.get("panel"));
            match first {
                Some(panel) => Output::Panel {
                    panel: Box::new(Panel::from_json(panel)?),
                },
                None => Output::Value {
                    value: json!({ "fit": lowered.get("fit"), "status": lowered.get("status") }),
                },
            }
        }
        _ if lowered.is_null() => return Ok(None),
        _ => {
            let text = lowered.to_string();
            match text.len() > crate::panel::MAX_TEXT {
                true => Output::Text {
                    text: format!(
                        "(a value of {} bytes, too long to keep: show part of it)",
                        text.len()
                    ),
                },
                false => Output::Value {
                    value: lowered.clone(),
                },
            }
        }
    }))
}

// --- words ---------------------------------------------------------------------------

/// `text`, cut to what an output may keep.
fn clip(text: &str) -> String {
    if text.len() <= crate::panel::MAX_TEXT {
        return text.to_owned();
    }
    let mut end = crate::panel::MAX_TEXT - 3;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn short(value: &Json) -> String {
    let text = value.to_string();
    match text.chars().count() > 60 {
        true => format!("{}…", text.chars().take(60).collect::<String>()),
        false => text,
    }
}

/// The stored and table names an LLM prompt cell's agent is told about: each
/// stored dataset's name, and each table's.
pub async fn known_data(catalog: &Catalog) -> Result<(Vec<String>, Vec<String>)> {
    let mut datasets: Vec<String> = sc_dataset::list_datasets(catalog)
        .await?
        .into_iter()
        .map(|d| d.name)
        .collect();
    datasets.sort();
    let mut tables: BTreeSet<String> = BTreeSet::new();
    for t in catalog.tables()? {
        if !t.name.starts_with("_fd_") && !t.name.starts_with("_sc_") {
            tables.insert(t.name.clone());
        }
    }
    Ok((datasets, tables.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chains_steps_are_the_operations_of_the_dataset_editor() {
        let op = operation("filter", &[json!("price > 100000")]).unwrap();
        assert_eq!(op, Op::filter("price > 100000"));
        let op = operation(
            "aggregate",
            &[
                json!(["neighbourhood"]),
                json!({ "mean_price": "mean(price)", "n": "count()" }),
            ],
        )
        .unwrap();
        let Op::Aggregate(agg) = op else {
            panic!("{op:?}")
        };
        assert_eq!(agg.group_by, vec![GroupKey::column("neighbourhood")]);
        assert_eq!(
            agg.summaries,
            vec![
                Summary::of("mean_price", SummaryFunction::Mean, "price"),
                Summary::count("n"),
            ]
        );
        let Op::Sort(sort) = operation("sort", &[json!("-price"), json!("area")]).unwrap() else {
            panic!()
        };
        assert!(sort.keys[0].descending && !sort.keys[1].descending);
        assert_eq!(sort.keys[0].formula, "price");
        let err =
            operation("aggregate", &[json!("town"), json!({ "m": "avg(price)" })]).unwrap_err();
        assert!(err.to_string().contains("`avg` is not a summary"), "{err}");
        let err = operation("pivot", &[]).unwrap_err();
        assert!(
            err.to_string().contains("`pivot` is not a dataset step"),
            "{err}"
        );
        let op = operation("mutate", &[json!({ "per_m2": "price / area" })]).unwrap();
        assert_eq!(op, Op::calculated("per_m2", "price / area"));
        let op = operation(
            "then",
            &[json!("filter"), json!({ "formula": "area > 10" })],
        )
        .unwrap();
        assert_eq!(op, Op::filter("area > 10"));
    }

    #[test]
    fn the_same_chain_is_the_same_dataset() {
        let a = DatasetDef::new("a", Base::table("houses")).then(Op::filter("price > 1"));
        let mut b = DatasetDef::new("b", Base::table("houses")).then(Op::filter("price > 1"));
        assert_eq!(content_id(&a), content_id(&b));
        b = b.then(Op::filter("area > 1"));
        assert_ne!(content_id(&a), content_id(&b));
    }

    #[test]
    fn a_summary_is_a_call() {
        assert_eq!(call_of("count()"), Some(("count".into(), None)));
        assert_eq!(
            call_of(" Mean( price ) "),
            Some(("mean".into(), Some("price".into())))
        );
        assert_eq!(call_of("price"), None);
        assert_eq!(call_of("a b(c)"), None);
    }
}

//! What the binding table on the model form asks of the binder beyond
//! [`bind_data`]: **Preview data**, with each variable's shape, first values
//! and error on its own row, and **Bind automatically** (Stan TODO §18).
//!
//! Both are for a configuration that is still being written, so neither
//! refuses the whole of it for one variable's mistake: [`preview_data`] binds
//! what it can and puts each sentence on the row it is about, and
//! [`suggest_bindings`] proposes bindings only for the variables that have
//! none.

use std::collections::{BTreeMap, BTreeSet};

use sc_error::{Error, Repr};
use sc_expr::SchemaShape;
use sc_types::Attrs;
use serde_json::{Map, Value as Json};

use super::BINDINGS_KEY;
use super::resolve::{BindReport, bind_data};
use super::spec::{Binding, parse_binding};
use crate::frame::Frame;
use crate::interface::{Declaration, Element, Interface};
use crate::model::{MAIN_DATASET, Model};

/// One `data` variable's row of the preview.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct VariablePreview {
    /// The variable.
    pub name: String,
    /// Its declared type.
    pub stan_type: String,
    /// Its binding, as a sentence reads it, when it has one that parses.
    pub binding: Option<String>,
    /// The bound shape, when it bound.
    pub shape: Option<Vec<usize>>,
    /// The first few values, when it bound.
    pub first: Vec<Json>,
    /// Why it did not bind.
    pub error: Option<String>,
}

/// What Preview data answers.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DataPreview {
    /// One row per `data` variable, in declaration order.
    pub variables: Vec<VariablePreview>,
    /// The datasets, dimensions, drops and warnings of the variables that
    /// bound — `None` when a sentence not about one variable stopped it.
    pub report: Option<BindReport>,
    /// The sentences that are not about one variable: a binding of a variable
    /// the program does not declare, a dimension that does not resolve.
    pub errors: Vec<String>,
}

/// An error's sentence without its kind's prefix.
fn sentence(e: &Error) -> String {
    match e.repr() {
        Repr::Invalid(m) | Repr::NotFound(m) | Repr::Config(m) => m.clone(),
        _ => e.to_string(),
    }
}

/// Bind as much of `config` as binds, and say of every other variable why not.
///
/// Each variable's own mistake — a binding that does not parse, a column that
/// is not there, a size that does not match, a null under `refuse` — is on its
/// row, and the variables that depend on it (a `width` of its `design`, a
/// `segment_*` of its `index`) say so; the rest are bound as a fit would bind
/// them. Only a sentence about no one variable stops the preview, and it is in
/// [`DataPreview::errors`].
pub fn preview_data(
    interface: &Interface,
    config: &Attrs,
    datasets: &[(String, Frame)],
    max_values: u64,
) -> DataPreview {
    let mut errors = Vec::new();
    let empty = Map::new();
    let bindings = match config.get(BINDINGS_KEY) {
        None | Some(Json::Null) => &empty,
        Some(Json::Object(map)) => map,
        Some(_) => {
            errors.push(format!(
                "`{BINDINGS_KEY}` must be an object from each data variable's name to its binding"
            ));
            &empty
        }
    };
    for name in bindings.keys() {
        if interface.data_variable(name).is_none() {
            errors.push(format!(
                "`{BINDINGS_KEY}` binds `{name}`, which the program's `data` block does not \
                 declare"
            ));
        }
    }

    let mut rows: Vec<VariablePreview> = interface
        .data
        .iter()
        .map(|d| VariablePreview {
            name: d.name.clone(),
            stan_type: d.stan_type.clone(),
            binding: None,
            shape: None,
            first: Vec::new(),
            error: None,
        })
        .collect();
    let mut parsed: BTreeMap<&str, Binding> = BTreeMap::new();
    for (row, decl) in rows.iter_mut().zip(&interface.data) {
        match bindings.get(&decl.name) {
            None => row.error = Some("it has no binding".to_owned()),
            Some(json) => match parse_binding(&decl.name, json) {
                Ok(binding) => {
                    row.binding = Some(binding.describe());
                    parsed.insert(decl.name.as_str(), binding);
                }
                Err(e) => row.error = Some(sentence(&e)),
            },
        }
    }

    // Bind what is left; take out the variable each refusal is about, and go
    // again. Every round removes one, so this ends.
    let mut report = None;
    loop {
        let reduced = Interface {
            data: interface
                .data
                .iter()
                .filter(|d| parsed.contains_key(d.name.as_str()))
                .cloned()
                .collect(),
            ..Interface::default()
        };
        let mut reduced_config = config.clone();
        reduced_config.insert(
            BINDINGS_KEY.to_owned(),
            Json::Object(
                reduced
                    .data
                    .iter()
                    .map(|d| (d.name.clone(), bindings[&d.name].clone()))
                    .collect(),
            ),
        );
        match bind_data(&reduced, &reduced_config, datasets, max_values) {
            Ok(bound) => {
                for v in &bound.report.variables {
                    if let Some(row) = rows.iter_mut().find(|r| r.name == v.name) {
                        row.shape = Some(v.shape.clone());
                        row.first = v.first.clone();
                    }
                }
                report = Some(bound.report);
                break;
            }
            Err(e) => {
                let text = sentence(&e);
                let about = parsed
                    .keys()
                    .find(|name| text.starts_with(&format!("`{name}`, declared ")))
                    .map(|name| (*name).to_owned());
                let Some(name) = about else {
                    errors.push(text);
                    break;
                };
                let mut failed = vec![(name.clone(), text)];
                while let Some((name, why)) = failed.pop() {
                    parsed.remove(name.as_str());
                    if let Some(row) = rows.iter_mut().find(|r| r.name == name) {
                        row.error = Some(why);
                    }
                    let dependents: Vec<String> = parsed
                        .iter()
                        .filter(|(_, b)| depends_on(b, &name))
                        .map(|(n, _)| (*n).to_owned())
                        .collect();
                    for d in dependents {
                        failed.push((d, format!("it depends on `{name}`, which did not bind")));
                    }
                }
            }
        }
    }
    DataPreview {
        variables: rows,
        report,
        errors,
    }
}

/// Whether `binding` is computed from the variable `name`'s binding.
fn depends_on(binding: &Binding, name: &str) -> bool {
    match binding {
        Binding::Width { of } => of == name,
        Binding::SegmentStart { index, .. } | Binding::SegmentSize { index, .. } => index == name,
        _ => false,
    }
}

/// What Bind automatically proposes.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct Suggestions {
    /// Each unbound variable it has a proposal for, and the binding — as the
    /// configuration writes it.
    pub bindings: Map<String, Json>,
    /// Why, per variable: "`main` has a column `y`".
    pub reasons: BTreeMap<String, String>,
}

/// One dataset as the suggestions see it.
struct Source<'a> {
    name: &'a str,
    table: &'a str,
    /// Column name → its formula.
    columns: Vec<(&'a str, &'a str)>,
}

/// Propose a binding for every `data` variable of `interface` that `model`'s
/// configuration leaves unbound (§18), from:
///
/// - **a column named like the variable** — a `column`, or an `index` when it
///   is an int array and the column is a foreign key into the table of another
///   dataset (`county` → `index(main.county → counties)`);
/// - **the variables an int sizes** — `size(d)` when an `index` into `d` is
///   declared `upper=` it, `count(ds)` when a per-row binding of `ds` is
///   declared with it as its first size, `width(X)` when a `design` `X` has it
///   as its second.
///
/// Only empty rows are filled: a variable the admin bound is never
/// second-guessed, though what it is bound to informs the sizes.
pub fn suggest_bindings(interface: &Interface, model: &Model, schema: &SchemaShape) -> Suggestions {
    let existing: BTreeMap<String, Binding> = match model.configuration.get(BINDINGS_KEY) {
        Some(Json::Object(map)) => map
            .iter()
            .filter_map(|(name, json)| Some((name.clone(), parse_binding(name, json).ok()?)))
            .collect(),
        _ => BTreeMap::new(),
    };
    let taken: BTreeSet<&str> = match model.configuration.get(BINDINGS_KEY) {
        Some(Json::Object(map)) => map.keys().map(String::as_str).collect(),
        _ => BTreeSet::new(),
    };
    let mut sources = vec![Source {
        name: MAIN_DATASET,
        table: model.dataset.table.as_str(),
        columns: model
            .dataset
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.expr.as_str()))
            .collect(),
    }];
    for related in &model.related {
        sources.push(Source {
            name: related.name.as_str(),
            table: related.dataset.table.as_str(),
            columns: related
                .dataset
                .columns
                .iter()
                .map(|c| (c.name.as_str(), c.expr.as_str()))
                .collect(),
        });
    }

    let mut out: BTreeMap<String, (Binding, String)> = BTreeMap::new();
    let unbound = |d: &&Declaration| !taken.contains(d.name.as_str());

    // A column named like the variable.
    for decl in interface.data.iter().filter(unbound) {
        if decl.rank() != 1 || !matches!(decl.element, Element::Int | Element::Real) {
            continue;
        }
        let Some((source, expr)) = sources.iter().find_map(|s| {
            s.columns
                .iter()
                .find(|(name, _)| *name == decl.name)
                .map(|(_, expr)| (s, *expr))
        }) else {
            continue;
        };
        let target = (decl.element == Element::Int)
            .then(|| key_target(schema, source.table, expr))
            .flatten()
            .and_then(|table| {
                sources
                    .iter()
                    .find(|s| s.name != source.name && s.table == table)
            });
        let proposal = match target {
            Some(target) => (
                Binding::Index {
                    dataset: source.name.to_owned(),
                    column: decl.name.clone(),
                    dimension: target.name.to_owned(),
                    match_column: None,
                },
                format!(
                    "`{}` has a column `{}`, a key into `{}`, which is the dataset `{}`",
                    source.name, decl.name, target.table, target.name
                ),
            ),
            None => (
                Binding::Column {
                    dataset: source.name.to_owned(),
                    column: decl.name.clone(),
                    time: None,
                },
                format!("`{}` has a column `{}`", source.name, decl.name),
            ),
        };
        out.insert(decl.name.clone(), proposal);
    }

    // The sizes, from what they size.
    let binding_of = |name: &str| -> Option<Binding> {
        existing
            .get(name)
            .or(out.get(name).map(|(b, _)| b))
            .cloned()
    };
    let mut sizes: Vec<(String, (Binding, String))> = Vec::new();
    for decl in interface.data.iter().filter(unbound) {
        if decl.rank() != 0 || decl.element != Element::Int || out.contains_key(&decl.name) {
            continue;
        }
        let name = decl.name.as_str();
        let mut proposal = None;
        for other in &interface.data {
            if other.upper.as_deref().map(str::trim) != Some(name) {
                continue;
            }
            if let Some(Binding::Index { dimension, .. }) = binding_of(&other.name) {
                proposal = Some((
                    Binding::Size {
                        dimension: dimension.clone(),
                    },
                    format!(
                        "`{}` indexes into `{dimension}` and is declared `upper={name}`",
                        other.name
                    ),
                ));
                break;
            }
        }
        if proposal.is_none() {
            for other in &interface.data {
                if other.dims.first().and_then(|s| s.identifier()) != Some(name) {
                    continue;
                }
                let per_row = match binding_of(&other.name) {
                    Some(
                        Binding::Column { dataset, .. }
                        | Binding::Columns { dataset, .. }
                        | Binding::Design { dataset, .. }
                        | Binding::Index { dataset, .. },
                    ) => Some(dataset),
                    _ => None,
                };
                if let Some(dataset) = per_row {
                    proposal = Some((
                        Binding::Count {
                            dataset: dataset.clone(),
                        },
                        format!(
                            "`{}` has one value per row of `{dataset}` and is declared `{}`",
                            other.name, other.stan_type
                        ),
                    ));
                    break;
                }
            }
        }
        if proposal.is_none() {
            for other in &interface.data {
                if other.dims.get(1).and_then(|s| s.identifier()) != Some(name) {
                    continue;
                }
                if let Some(Binding::Design { .. }) = binding_of(&other.name) {
                    proposal = Some((
                        Binding::Width {
                            of: other.name.clone(),
                        },
                        format!(
                            "`{}` is a design matrix declared `{}`",
                            other.name, other.stan_type
                        ),
                    ));
                    break;
                }
            }
        }
        if let Some(p) = proposal {
            sizes.push((decl.name.clone(), p));
        }
    }
    out.extend(sizes);

    let mut suggestions = Suggestions::default();
    for decl in &interface.data {
        if let Some((binding, reason)) = out.remove(&decl.name) {
            if let Ok(json) = serde_json::to_value(&binding) {
                suggestions.bindings.insert(decl.name.clone(), json);
                suggestions.reasons.insert(decl.name.clone(), reason);
            }
        }
    }
    suggestions
}

/// The table a dataset column's formula points into, when the formula is a
/// bare field of `table` that is a key.
fn key_target(schema: &SchemaShape, table: &str, expr: &str) -> Option<String> {
    let field = expr.trim();
    let shape = schema.tables.get(table)?.fields.get(field)?;
    shape.key.as_ref().map(|k| k.target_table.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::Dataset;
    use crate::frame::Column;
    use crate::interface::SizeExpr;
    use crate::model::NamedDataset;
    use sc_expr::TableShape;
    use serde_json::json;

    /// The radon program's `data` block.
    fn radon() -> Interface {
        let n = || vec![SizeExpr::var("N")];
        Interface {
            data: vec![
                Declaration::new("N", Element::Int, vec![], "int<lower=1>")
                    .bounded(Some("1"), None),
                Declaration::new("J", Element::Int, vec![], "int<lower=1>")
                    .bounded(Some("1"), None),
                Declaration::new(
                    "county",
                    Element::Int,
                    n(),
                    "array[N] int<lower=1, upper=J>",
                )
                .bounded(Some("1"), Some("J")),
                Declaration::new("x", Element::Real, n(), "vector[N]"),
                Declaration::new("u", Element::Real, vec![SizeExpr::var("J")], "vector[J]"),
                Declaration::new("y", Element::Real, n(), "vector[N]"),
            ],
            ..Interface::default()
        }
    }

    fn model(bindings: Json) -> Model {
        let mut model = Model::new(
            "radon",
            "stan",
            Dataset::new("homes")
                .column("county", "county")
                .column("floor", "floor")
                .column("y", "log_radon"),
        );
        model.related = vec![NamedDataset {
            name: "counties".into(),
            dataset: Dataset::new("counties").column("log_uranium", "log_uranium"),
            label: Some("name".into()),
        }];
        model
            .configuration
            .insert(BINDINGS_KEY.to_owned(), bindings);
        model
    }

    fn schema() -> SchemaShape {
        SchemaShape::new()
            .table(
                "homes",
                TableShape::new()
                    .field("id")
                    .key_field("county", "counties", "id")
                    .field("floor")
                    .field("log_radon"),
            )
            .table(
                "counties",
                TableShape::new()
                    .field("id")
                    .field("name")
                    .field("log_uranium"),
            )
    }

    #[test]
    fn radon_is_bound_automatically_except_for_what_the_names_do_not_say() {
        let s = suggest_bindings(&radon(), &model(json!({})), &schema());
        assert_eq!(
            s.bindings,
            json!({
                "N": { "kind": "count", "dataset": "main" },
                "J": { "kind": "size", "dimension": "counties" },
                "county": { "kind": "index", "dataset": "main", "column": "county",
                            "dimension": "counties" },
                "y": { "kind": "column", "dataset": "main", "column": "y" },
            })
            .as_object()
            .unwrap()
            .clone()
        );
        assert!(
            s.reasons["county"].contains("a key into `counties`"),
            "{s:?}"
        );
        assert!(s.reasons["J"].contains("declared `upper=J`"), "{s:?}");
        assert!(!s.bindings.contains_key("x") && !s.bindings.contains_key("u"));
    }

    #[test]
    fn a_bound_variable_is_never_second_guessed_but_informs_the_sizes() {
        let s = suggest_bindings(
            &radon(),
            &model(json!({
                "county": { "kind": "column", "dataset": "main", "column": "county" },
                "x": { "kind": "column", "dataset": "main", "column": "floor" },
            })),
            &schema(),
        );
        assert!(!s.bindings.contains_key("county"));
        // `county` is no index now, so `J` has nothing to be the size of; `N`
        // still counts `main` through `x`.
        assert!(!s.bindings.contains_key("J"));
        assert_eq!(
            s.bindings["N"],
            json!({ "kind": "count", "dataset": "main" })
        );
    }

    fn frames() -> Vec<(String, Frame)> {
        vec![
            (
                MAIN_DATASET.to_owned(),
                Frame::new(
                    vec![
                        (
                            "county".into(),
                            Column::Int(vec![Some(1), Some(2), Some(2)]),
                        ),
                        (
                            "floor".into(),
                            Column::Float(vec![Some(0.0), Some(1.0), None]),
                        ),
                        (
                            "y".into(),
                            Column::Float(vec![Some(1.0), Some(2.0), Some(3.0)]),
                        ),
                    ],
                    vec!["1".into(), "2".into(), "3".into()],
                )
                .unwrap(),
            ),
            (
                "counties".to_owned(),
                Frame::new(
                    vec![(
                        "log_uranium".into(),
                        Column::Float(vec![Some(0.5), Some(-0.5)]),
                    )],
                    vec!["1".into(), "2".into()],
                )
                .unwrap(),
            ),
        ]
    }

    #[test]
    fn the_preview_puts_each_error_on_its_own_row_and_binds_the_rest() {
        let mut config = Attrs::new();
        config.insert(
            BINDINGS_KEY.to_owned(),
            json!({
                "N": { "kind": "count", "dataset": "main" },
                "J": { "kind": "size", "dimension": "counties" },
                "county": { "kind": "index", "dataset": "main", "column": "county",
                            "dimension": "counties" },
                "x": { "kind": "column", "dataset": "main", "column": "floor" },
                "u": { "kind": "colum", "dataset": "counties" },
                "w": { "kind": "count", "dataset": "main" },
            }),
        );
        let p = preview_data(&radon(), &config, &frames(), 1000);
        let row = |name: &str| p.variables.iter().find(|v| v.name == name).unwrap();
        // Bound, with its shape and first values.
        assert_eq!(row("N").shape, Some(vec![]));
        assert_eq!(row("N").first, vec![json!(3)]);
        assert_eq!(row("county").shape, Some(vec![3]));
        assert_eq!(row("J").first, vec![json!(2)]);
        // A null under `refuse` is on `x`'s row, and nothing else is affected.
        let x = row("x").error.as_deref().unwrap();
        assert!(x.starts_with("`x`, declared `vector[N]`"), "{x}");
        assert!(x.contains("is null"), "{x}");
        // A binding that does not parse, and a variable with none.
        assert!(
            row("u")
                .error
                .as_deref()
                .unwrap()
                .contains("the binding of `u`"),
            "{:?}",
            row("u")
        );
        assert_eq!(row("y").error.as_deref(), Some("it has no binding"));
        // A binding of nothing declared is not any row's.
        assert_eq!(p.errors.len(), 1, "{:?}", p.errors);
        assert!(p.errors[0].contains("`w`"), "{:?}", p.errors);
        assert_eq!(p.report.unwrap().dimensions["counties"], 2);
    }
}

//! Labels: an output variable's axes tied back to the dimensions (Stan TODO
//! §15), and the configuration that decides which draws are kept (§14).
//!
//! `vector[J] alpha` says `alpha`'s one axis has size `J`. If `J` is bound by
//! `size(counties)` or `count(counties)`, `alpha[j]` is about county `j`, and
//! the summary, the warnings and (later) the draws API all say
//! `alpha[Aitkin]`, never `alpha[1]`. A `width(X)` size labels its axis with
//! the design's column names. Anything else — `J + 1`, a literal, a function
//! call — stays numbered.
//!
//! The configuration's [`LABELS_KEY`] overrides that per variable, one entry
//! per axis: a dimension's name, a `design`-bound variable's name, or `null`
//! for "numbered" — `"y_future": ["day.future"]`, for a forecast whose size is
//! a literal horizon. It is checked three times, each as early as it can be:
//! the names on save ([`check_outputs`]), the lengths before sampling where
//! the declared size evaluates ([`Labeller::check_lengths`]), and the lengths
//! of the draws that came back after — where a mismatch leaves the axis
//! numbered and says so, rather than failing an hour's sampling over a label.

use std::collections::{BTreeMap, BTreeSet};

use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;

use super::LABELS_KEY;
use super::dimension::Coordinates;
use super::spec::{Binding, Spec};
use crate::interface::{Declaration, Interface};

/// The configuration key listing output variables whose draws are not kept.
/// Their summary is kept when it would be anyway (every parameter, and a
/// generated quantity within `--stan-summary-max-elements`).
pub const EXCLUDE_VARIABLES_KEY: &str = "exclude_variables";

/// The configuration key saying whether the draws are kept at all (default
/// `true`). `false` keeps the summary and the diagnostics, computed from the
/// draws before they are discarded.
pub const KEEP_DRAWS_KEY: &str = "keep_draws";

/// What one axis of an output variable is labelled by.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Axis {
    /// The heading of its column in a summary table: the dimension's name, or
    /// `index` for a numbered axis — made unique within the variable.
    pub name: String,
    /// The dimension (or `design`-bound variable) whose positions these are;
    /// `None` for a numbered axis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<String>,
    /// Position `i + 1`'s label; empty for a numbered axis.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Position `i + 1`'s key; empty for a numbered axis and for a design's
    /// columns, which have names but no keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<Json>,
}

impl Axis {
    /// The cell for position `i` (1-based): its label, or the number.
    pub fn cell(&self, i: usize) -> Json {
        match self.labels.get(i.wrapping_sub(1)) {
            Some(label) => Json::from(label.clone()),
            None => Json::from(i),
        }
    }

    /// The text for position `i` (1-based), for a sentence: `Aitkin`, `3`.
    pub fn text(&self, i: usize) -> String {
        match self.labels.get(i.wrapping_sub(1)) {
            Some(label) => label.clone(),
            None => i.to_string(),
        }
    }
}

/// `alpha[Aitkin]`, `Sigma[1,2]`, `lp__` — an element named by its axes'
/// labels.
pub fn element_label(variable: &str, element: &[usize], axes: &[Axis]) -> String {
    if element.is_empty() {
        return variable.to_owned();
    }
    let parts: Vec<String> = element
        .iter()
        .enumerate()
        .map(|(k, i)| axes.get(k).map_or_else(|| i.to_string(), |a| a.text(*i)))
        .collect();
    format!("{variable}[{}]", parts.join(","))
}

/// Labels the axes of output variables from a configuration and an instance's
/// coordinates.
pub struct Labeller<'a> {
    bindings: BTreeMap<String, Binding>,
    overrides: BTreeMap<String, Vec<Option<String>>>,
    coordinates: &'a Coordinates,
}

impl<'a> Labeller<'a> {
    /// A labeller for the model configured by `config`, fitted with
    /// `coordinates`.
    pub fn new(config: &Attrs, coordinates: &'a Coordinates) -> Result<Labeller<'a>> {
        Ok(Labeller {
            bindings: Spec::parse(config)?.bindings,
            overrides: overrides(config)?,
            coordinates,
        })
    }

    /// The labels and keys of the dimension or design called `name`.
    fn source(&self, name: &str) -> Option<(Vec<String>, Vec<Json>)> {
        if let Some(d) = self.coordinates.dimension(name) {
            return Some((d.labels.clone(), d.keys.clone()));
        }
        self.coordinates
            .designs
            .get(name)
            .map(|d| (d.columns.clone(), Vec::new()))
    }

    /// What the size `identifier` is the size of, through its binding.
    fn sized_by(&self, identifier: &str) -> Option<&str> {
        match self.bindings.get(identifier)? {
            Binding::Size { dimension } => Some(dimension),
            Binding::Count { dataset } => Some(dataset),
            Binding::Width { of } => Some(of),
            _ => None,
        }
    }

    /// The axes of `variable`, whose draws came back with `lengths` positions
    /// per axis, declared as `decl` when the program's interface is known.
    /// Answers the axes and, when the configured labels did not fit the draws,
    /// the sentence saying which axis was left numbered.
    pub fn axes(
        &self,
        variable: &str,
        decl: Option<&Declaration>,
        lengths: &[usize],
    ) -> (Vec<Axis>, Vec<String>) {
        let overridden = self.overrides.get(variable);
        let mut problems = Vec::new();
        let mut axes: Vec<Axis> = lengths
            .iter()
            .enumerate()
            .map(|(k, &length)| {
                let numbered = Axis {
                    name: String::new(),
                    dimension: None,
                    labels: Vec::new(),
                    keys: Vec::new(),
                };
                let chosen = match overridden {
                    Some(entries) => entries.get(k).cloned().flatten(),
                    None => decl
                        .and_then(|d| d.dims.get(k))
                        .and_then(|size| size.identifier())
                        .and_then(|id| self.sized_by(id))
                        .map(str::to_owned),
                };
                let Some(name) = chosen else {
                    return numbered;
                };
                match self.source(&name) {
                    Some((labels, keys)) if labels.len() == length => Axis {
                        name: name.clone(),
                        dimension: Some(name),
                        labels,
                        keys,
                    },
                    // A size bound to a dimension always has its length; an
                    // override may not, and is the admin's to hear about.
                    found if overridden.is_some() => {
                        problems.push(format!(
                            "the labels of `{variable}` name `{name}` for its {} axis, which {}, \
                             but the draws have {length} positions there: it is left numbered",
                            ordinal(k),
                            match found {
                                Some((labels, _)) => format!("has {} positions", labels.len()),
                                None => "this fit has no coordinates for".to_owned(),
                            }
                        ));
                        numbered
                    }
                    _ => numbered,
                }
            })
            .collect();
        name_columns(&mut axes);
        (axes, problems)
    }

    /// The overrides whose declared size evaluates against `sizes` (the bound
    /// data's integers) to a length their dimension does not have — refused
    /// before anything is sampled.
    pub fn check_lengths(
        &self,
        interface: &Interface,
        sizes: &dyn Fn(&str) -> Option<i64>,
    ) -> Result<()> {
        for (variable, entries) in &self.overrides {
            let Some(decl) = interface.output(variable) else {
                continue;
            };
            for (k, entry) in entries.iter().enumerate() {
                let (Some(name), Some(size)) = (entry, decl.dims.get(k)) else {
                    continue;
                };
                let (Some(n), Some((labels, _))) = (size.eval(sizes), self.source(name)) else {
                    continue;
                };
                if usize::try_from(n).ok() != Some(labels.len()) {
                    return Err(Error::invalid(format!(
                        "`{LABELS_KEY}` labels the {} axis of `{variable}` (declared `{}`) with \
                         `{name}`, which has {} positions, but `{size}` is {n} here",
                        ordinal(k),
                        decl.stan_type,
                        labels.len()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// One output variable of a fitted posterior: its shape, and what labels each
/// axis — recorded at fit time ([`ATTR_AXES`](crate::ATTR_AXES)), so the
/// instance is read back by the configuration it was fitted with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordedAxes {
    /// Positions per axis, outer to inner: `[85]` for `alpha`, `[]` for a
    /// scalar.
    pub dims: Vec<usize>,
    /// Per axis, the dimension (or `design`-bound variable) whose labels it
    /// takes, or `None` for a numbered axis.
    pub dimensions: Vec<Option<String>>,
}

/// Every variable of `draws`, with its shape as the draws have it and its axes
/// as `labeller` labels them — what a fitted instance records.
pub fn recorded_axes(
    draws: &[crate::posterior::DrawSeries],
    interface: Option<&Interface>,
    labeller: &Labeller<'_>,
) -> BTreeMap<String, RecordedAxes> {
    let mut lengths: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for series in draws {
        let dims = lengths.entry(series.variable.as_str()).or_default();
        if dims.len() < series.element.len() {
            dims.resize(series.element.len(), 0);
        }
        for (k, i) in series.element.iter().enumerate() {
            dims[k] = dims[k].max(*i);
        }
    }
    lengths
        .into_iter()
        .map(|(name, dims)| {
            let decl = interface.and_then(|i| i.output(name));
            let (axes, _) = labeller.axes(name, decl, &dims);
            (
                name.to_owned(),
                RecordedAxes {
                    dims,
                    dimensions: axes.into_iter().map(|a| a.dimension).collect(),
                },
            )
        })
        .collect()
}

/// The axes of a variable recorded as `recorded`, labelled from
/// `coordinates` — the reading half of [`recorded_axes`]. An axis whose
/// dimension this instance has no coordinates of, or whose length no longer
/// matches them, is numbered.
pub fn named_axes(coordinates: &Coordinates, recorded: &RecordedAxes) -> Vec<Axis> {
    let mut axes: Vec<Axis> = recorded
        .dims
        .iter()
        .enumerate()
        .map(|(k, &length)| {
            let numbered = Axis {
                name: String::new(),
                dimension: None,
                labels: Vec::new(),
                keys: Vec::new(),
            };
            let Some(name) = recorded.dimensions.get(k).cloned().flatten() else {
                return numbered;
            };
            let found = coordinates
                .dimension(&name)
                .map(|d| (d.labels.clone(), d.keys.clone()))
                .or_else(|| {
                    coordinates
                        .designs
                        .get(&name)
                        .map(|d| (d.columns.clone(), Vec::new()))
                });
            match found {
                Some((labels, keys)) if labels.len() == length => Axis {
                    name: name.clone(),
                    dimension: Some(name),
                    labels,
                    keys,
                },
                _ => numbered,
            }
        })
        .collect();
    name_columns(&mut axes);
    axes
}

/// Headings: the dimension's name, or `index` (`index 1`, `index 2` for a
/// variable with several numbered axes); a second axis over the same
/// dimension gets ` (2)`.
fn name_columns(axes: &mut [Axis]) {
    let numbered = axes.iter().filter(|a| a.dimension.is_none()).count();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (k, axis) in axes.iter_mut().enumerate() {
        let base = match &axis.dimension {
            Some(d) => d.clone(),
            None if numbered == 1 => "index".to_owned(),
            None => format!("index {}", k + 1),
        };
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        axis.name = if *n == 1 {
            base
        } else {
            format!("{base} ({n})")
        };
    }
}

/// "first", "second", … for sentences about axes.
fn ordinal(k: usize) -> String {
    match k {
        0 => "first".to_owned(),
        1 => "second".to_owned(),
        2 => "third".to_owned(),
        n => format!("{}th", n + 1),
    }
}

/// The configuration's label overrides: each variable's list of axis names.
fn overrides(config: &Attrs) -> Result<BTreeMap<String, Vec<Option<String>>>> {
    let mut out = BTreeMap::new();
    let map = match config.get(LABELS_KEY) {
        None | Some(Json::Null) => return Ok(out),
        Some(Json::Object(map)) => map,
        Some(_) => {
            return Err(Error::invalid(format!(
                "`{LABELS_KEY}` must be an object from an output variable's name to a list with \
                 one dimension (or null) per axis"
            )));
        }
    };
    for (variable, entries) in map {
        let wrong = || {
            Error::invalid(format!(
                "`{LABELS_KEY}` for `{variable}` must be a list with one dimension name (or null, \
                 for a numbered axis) per axis — `[\"counties\"]`"
            ))
        };
        let Json::Array(entries) = entries else {
            return Err(wrong());
        };
        let axes = entries
            .iter()
            .map(|e| match e {
                Json::Null => Ok(None),
                Json::String(s) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
                _ => Err(wrong()),
            })
            .collect::<Result<Vec<_>>>()?;
        out.insert(variable.clone(), axes);
    }
    Ok(out)
}

/// The configured `exclude_variables`, empty when absent.
pub fn excluded_variables(config: &Attrs) -> Result<BTreeSet<String>> {
    match config.get(EXCLUDE_VARIABLES_KEY) {
        None | Some(Json::Null) => Ok(BTreeSet::new()),
        Some(Json::Array(names)) => names
            .iter()
            .map(|n| {
                n.as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        Error::invalid(format!(
                            "`{EXCLUDE_VARIABLES_KEY}` must be a list of variable names, and \
                             holds `{n}`"
                        ))
                    })
            })
            .collect(),
        Some(other) => Err(Error::invalid(format!(
            "`{EXCLUDE_VARIABLES_KEY}` must be a list of variable names, not `{other}`"
        ))),
    }
}

/// Whether the configuration keeps the draws (the default) or only their
/// summary.
pub fn keeps_draws(config: &Attrs) -> Result<bool> {
    match config.get(KEEP_DRAWS_KEY) {
        None | Some(Json::Null) => Ok(true),
        Some(Json::Bool(keep)) => Ok(*keep),
        Some(other) => Err(Error::invalid(format!(
            "`{KEEP_DRAWS_KEY}` must be true or false, not `{other}`"
        ))),
    }
}

/// The save-time checks of the output configuration against the program:
/// every excluded variable and every labelled one is an output variable, a
/// label list has one entry per axis, and each entry names a dimension
/// (`is_dimension`) or a `design`-bound variable.
pub(crate) fn check_outputs(
    interface: &Interface,
    config: &Attrs,
    spec: &Spec,
    is_dimension: &dyn Fn(&str) -> bool,
) -> Result<()> {
    let outputs = || {
        let names: Vec<String> = interface
            .outputs()
            .map(|d| format!("`{}`", d.name))
            .collect();
        if names.is_empty() {
            "it declares none".to_owned()
        } else {
            format!("it declares {}", names.join(", "))
        }
    };
    for name in excluded_variables(config)? {
        if interface.output(&name).is_none() {
            return Err(Error::invalid(format!(
                "`{EXCLUDE_VARIABLES_KEY}` names `{name}`, which is not a parameter, transformed \
                 parameter or generated quantity of the program ({})",
                outputs()
            )));
        }
    }
    keeps_draws(config)?;
    for (variable, entries) in overrides(config)? {
        let Some(decl) = interface.output(&variable) else {
            return Err(Error::invalid(format!(
                "`{LABELS_KEY}` labels `{variable}`, which is not a parameter, transformed \
                 parameter or generated quantity of the program ({})",
                outputs()
            )));
        };
        if entries.len() != decl.rank() {
            return Err(Error::invalid(format!(
                "`{LABELS_KEY}` gives `{variable}` {} but it is declared `{}`, which has {}",
                plural(entries.len(), "label"),
                decl.stan_type,
                plural(decl.rank(), "axis")
            )));
        }
        for name in entries.iter().flatten() {
            let is_design = matches!(spec.bindings.get(name), Some(Binding::Design { .. }));
            if !is_dimension(name) && !is_design {
                return Err(Error::invalid(format!(
                    "`{LABELS_KEY}` labels `{variable}` with `{name}`, which is neither a \
                     dimension (a dataset, or one declared under `dimensions`) nor a \
                     `design`-bound variable"
                )));
            }
        }
    }
    Ok(())
}

fn plural(n: usize, what: &str) -> String {
    match (n, what) {
        (1, _) => format!("1 {what}"),
        (_, "axis") => format!("{n} axes"),
        _ => format!("{n} {what}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bind::dimension::{DesignCoordinates, DimensionCoordinates, DimensionKind};
    use crate::interface::{Element, SizeExpr};
    use serde_json::json;

    fn coordinates() -> Coordinates {
        let dim = |name: &str, labels: &[&str], keys: Vec<Json>| DimensionCoordinates {
            name: name.to_owned(),
            kind: DimensionKind::Rows,
            dataset: name.to_owned(),
            column: None,
            keys,
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        };
        let mut c = Coordinates {
            dimensions: vec![
                dim(
                    "main",
                    &["1", "2", "3", "4"],
                    (1..=4).map(Json::from).collect(),
                ),
                dim(
                    "counties",
                    &["Aitkin", "Anoka", "Becker"],
                    vec![json!(27001), json!(27003), json!(27005)],
                ),
                dim("day.future", &["2024-01-04", "2024-01-05"], vec![]),
            ],
            ..Coordinates::default()
        };
        c.designs.insert(
            "X".to_owned(),
            DesignCoordinates {
                columns: vec!["floor".to_owned(), "region=north".to_owned()],
                encoding: Json::Null,
            },
        );
        c
    }

    fn config(extra: Json) -> Attrs {
        let mut config: Attrs = serde_json::from_value(json!({
            "bindings": {
                "N": {"kind": "count", "dataset": "main"},
                "J": {"kind": "size", "dimension": "counties"},
                "X": {"kind": "design", "dataset": "main", "columns": ["floor", "region"]},
                "K": {"kind": "width", "of": "X"},
                "H": {"kind": "value", "value": 2},
            },
        }))
        .unwrap();
        if let Json::Object(extra) = extra {
            config.extend(extra);
        }
        config
    }

    fn decl(name: &str, dims: &[&str]) -> Declaration {
        Declaration::new(
            name,
            Element::Real,
            dims.iter().map(|d| SizeExpr::var(*d)).collect(),
            "…",
        )
    }

    #[test]
    fn an_axis_sized_by_a_dimension_is_labelled_by_it() {
        let coords = coordinates();
        let config = config(json!({}));
        let labeller = Labeller::new(&config, &coords).unwrap();
        let (axes, problems) = labeller.axes("alpha", Some(&decl("alpha", &["J"])), &[3]);
        assert!(problems.is_empty());
        assert_eq!(axes[0].name, "counties");
        assert_eq!(axes[0].labels, ["Aitkin", "Anoka", "Becker"]);
        assert_eq!(axes[0].keys[1], json!(27003));
        assert_eq!(element_label("alpha", &[2], &axes), "alpha[Anoka]");
        // `count(main)` labels by main's rows, `width(X)` by the design's
        // columns; a literal-valued size stays numbered.
        let (axes, _) = labeller.axes("b", Some(&decl("b", &["N", "K", "H"])), &[4, 2, 2]);
        assert_eq!(
            axes.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["main", "X", "index"]
        );
        assert_eq!(axes[1].cell(2), json!("region=north"));
        assert_eq!(axes[2].cell(2), json!(2));
        assert!(axes[1].keys.is_empty());
        // Two axes over one dimension get distinct headings.
        let (axes, _) = labeller.axes("Omega", Some(&decl("Omega", &["J", "J"])), &[3, 3]);
        assert_eq!(axes[0].name, "counties");
        assert_eq!(axes[1].name, "counties (2)");
        assert_eq!(
            element_label("Omega", &[1, 3], &axes),
            "Omega[Aitkin,Becker]"
        );
        // No interface: numbered.
        let (axes, _) = labeller.axes("Sigma", None, &[2, 2]);
        assert_eq!(axes[0].name, "index 1");
        assert_eq!(element_label("Sigma", &[2, 1], &axes), "Sigma[2,1]");
    }

    #[test]
    fn an_override_labels_what_the_size_does_not_and_is_checked_against_lengths() {
        let coords = coordinates();
        let config = config(json!({"labels": {"y_future": ["day.future"]}}));
        let labeller = Labeller::new(&config, &coords).unwrap();
        let future = Declaration::new(
            "y_future",
            Element::Real,
            vec![SizeExpr::literal(2)],
            "vector[2]",
        );
        let (axes, problems) = labeller.axes("y_future", Some(&future), &[2]);
        assert!(problems.is_empty());
        assert_eq!(axes[0].labels, ["2024-01-04", "2024-01-05"]);
        // Draws of another length: numbered, and said.
        let (axes, problems) = labeller.axes("y_future", Some(&future), &[3]);
        assert!(axes[0].dimension.is_none());
        assert!(
            problems[0].contains("name `day.future` for its first axis, which has 2 positions"),
            "{problems:?}"
        );
        // Before sampling, a size that evaluates is checked.
        let wrong = Interface {
            generated: vec![Declaration::new(
                "y_future",
                Element::Real,
                vec![SizeExpr::var("H")],
                "vector[H]",
            )],
            ..Interface::default()
        };
        labeller.check_lengths(&wrong, &|_| Some(2)).unwrap();
        let err = labeller.check_lengths(&wrong, &|_| Some(7)).unwrap_err();
        assert!(
            err.to_string().contains(
                "labels the first axis of `y_future` (declared `vector[H]`) with `day.future`, \
                 which has 2 positions, but `H` is 7 here"
            ),
            "{err}"
        );
    }

    #[test]
    fn the_output_configuration_is_checked_against_the_program_on_save() {
        let interface = Interface {
            parameters: vec![decl("alpha", &["J"])],
            generated: vec![decl("y_rep", &["N"])],
            ..Interface::default()
        };
        let is_dimension = |n: &str| ["main", "counties"].contains(&n);
        let check = |extra: Json| {
            let config = config(extra);
            let spec = Spec::parse(&config).unwrap();
            check_outputs(&interface, &config, &spec, &is_dimension)
        };
        check(json!({"exclude_variables": ["y_rep"], "keep_draws": false})).unwrap();
        check(json!({"labels": {"alpha": ["counties"], "y_rep": [null]}})).unwrap();
        check(json!({"labels": {"y_rep": ["X"]}})).unwrap();
        let err = check(json!({"exclude_variables": ["y_rpe"]})).unwrap_err();
        assert!(
            err.to_string().contains(
                "names `y_rpe`, which is not a parameter, transformed parameter or generated \
                 quantity of the program (it declares `alpha`, `y_rep`)"
            ),
            "{err}"
        );
        let err = check(json!({"labels": {"alpha": ["counties", "main"]}})).unwrap_err();
        assert!(err.to_string().contains("gives `alpha` 2 labels"), "{err}");
        let err = check(json!({"labels": {"alpha": ["regions"]}})).unwrap_err();
        assert!(
            err.to_string().contains("with `regions`, which is neither"),
            "{err}"
        );
        let err = check(json!({"labels": {"N": ["main"]}})).unwrap_err();
        assert!(
            err.to_string().contains("labels `N`, which is not"),
            "{err}"
        );
        let err = check(json!({"keep_draws": "no"})).unwrap_err();
        assert!(err.to_string().contains("true or false"), "{err}");
        let err = check(json!({"labels": {"alpha": "counties"}})).unwrap_err();
        assert!(err.to_string().contains("must be a list"), "{err}");
    }
}

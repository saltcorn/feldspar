//! The binder: a program's `data` block tied to the tables (Stan TODO §§8–11).
//!
//! A provider that [binds data](crate::ModelProvider::binds_data) declares an
//! [`Interface`], and its configuration says how each `data` variable is
//! computed from the datasets: one **binding** per variable, under
//! [`BINDINGS_KEY`], with the extra dimensions under [`DIMENSIONS_KEY`], each
//! dataset's `nulls` and `unknown` policies under [`POLICIES_KEY`], and the
//! output labels under [`LABELS_KEY`]. The key names are the host's, not the
//! provider's, because the binder that reads them is the host's (§4): a second
//! Bayesian provider must not be able to spell them differently.
//!
//! - [`Binding`] and [`DimensionSpec`] are the configuration, as types.
//! - A [`Dimension`](DimensionCoordinates) maps the positions `1..n` a Stan
//!   index lives in to the keys a database has: a dataset's rows, a column's
//!   values, a time grid's steps. Its [`Coordinates`] are stored on the
//!   instance, because positions are the instance's private business and
//!   everything that leaves the host speaks keys.
//! - [`check_bindings`] is §10's save-time half — structure only, no data.
//! - [`bind_data`] is the whole of it at preview and fit time: the policies,
//!   the resolution order, every binding evaluated, every declaration checked
//!   against its value, and the result as [`BoundData`] — CmdStan's JSON, the
//!   coordinates and the report.
//!
//! Every refusal is a sentence naming the variable, its declaration and its
//! binding, because the admin reading it is looking at a table with one row
//! per variable.

mod assist;
mod dimension;
mod labels;
mod resolve;
mod spec;
mod structured;
#[cfg(test)]
mod structured_tests;
mod tensor;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;

use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;

use crate::dataset::Dataset;
use crate::interface::Interface;
use crate::model::{MAIN_DATASET, Model, NamedDataset};

pub use assist::{DataPreview, Suggestions, VariablePreview, preview_data, suggest_bindings};
pub use dimension::{
    Coordinates, DesignCoordinates, DimensionCoordinates, DimensionKind, MAX_GRID_STEPS,
};
pub use labels::{
    Axis, EXCLUDE_VARIABLES_KEY, KEEP_DRAWS_KEY, Labeller, RecordedAxes, element_label,
    excluded_variables, keeps_draws, named_axes, recorded_axes,
};
pub use resolve::{
    BindReport, BoundData, DEFAULT_MAX_DATA_VALUES, DatasetReport, DropReport, VariableReport,
    bind_data,
};
pub use spec::{
    Aggregate, Along, Binding, DimensionSpec, Edges, Points, Policies, Policy, Symmetric, TimeScale,
};
pub use structured::{MAX_DISTANCE_SITES, MAX_ICAR_NODES};

/// The configuration key holding the bindings: an object from each `data`
/// variable's name to its binding.
pub const BINDINGS_KEY: &str = "bindings";
/// The configuration key holding the declared dimensions — values and time
/// grids; every dataset is a rows dimension without being declared (§8).
pub const DIMENSIONS_KEY: &str = "dimensions";
/// The configuration key holding each dataset's policies: an object from a
/// dataset's name to `{ "nulls": "refuse" | "drop", "unknown": "refuse" |
/// "drop" }`, both `refuse` when absent (§10).
pub const POLICIES_KEY: &str = "policies";
/// The configuration key holding explicit labels for output variables (§15).
pub const LABELS_KEY: &str = "labels";

/// The column a related dataset's read carries its label formula's value in
/// (see [`binding_dataset`]). Reserved, like the split key.
pub const LABEL_COLUMN: &str = "_fd_label";

/// The dataset a related dataset is **read** as for binding: its own, plus its
/// label formula as [`LABEL_COLUMN`] when it has one — so the labels are the
/// row layer's answer, read with the rows, and [`bind_data`] turns them into
/// the labels of that dataset's rows dimension.
pub fn binding_dataset(related: &NamedDataset) -> Dataset {
    match &related.label {
        Some(label) => related.dataset.clone().column(LABEL_COLUMN, label.clone()),
        None => related.dataset.clone(),
    }
}

/// §10's save-time checks of `model`'s bindings against `interface`, with no
/// data read: every variable bound and every binding declared; the datasets,
/// columns and dimensions each names exist; each kind can produce its
/// declaration's rank and element type; a `width` names a `design` and a
/// `segment_*` an `index`; and the datasets have an order to be resolved in.
/// And the outputs' configuration (§§14–15): `exclude_variables` and `labels`
/// name output variables, and each label names a dimension.
pub fn check_bindings(interface: &Interface, model: &Model) -> Result<()> {
    let spec = spec::Spec::parse(&model.configuration)?;
    let mut datasets = vec![resolve::DatasetColumns {
        name: MAIN_DATASET,
        columns: model
            .dataset
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect(),
    }];
    for related in &model.related {
        datasets.push(resolve::DatasetColumns {
            name: related.name.as_str(),
            columns: related
                .dataset
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect(),
        });
    }
    resolve::check_structure(interface, &model.configuration, &spec, &datasets)?;
    labels::check_outputs(interface, &model.configuration, &spec, &|name| {
        resolve::dimension_source(&spec, &datasets, name).is_some()
    })
}

/// Every `data` variable of `interface` has a binding in `config`, and every
/// binding names one — each refused with a sentence naming the variables.
pub fn check_bindings_declared(interface: &Interface, config: &Attrs) -> Result<()> {
    let empty = serde_json::Map::new();
    let bindings = match config.get(BINDINGS_KEY) {
        None | Some(Json::Null) => &empty,
        Some(Json::Object(map)) => map,
        Some(_) => {
            return Err(Error::invalid(format!(
                "`{BINDINGS_KEY}` must be an object from each data variable's name to its binding"
            )));
        }
    };
    let declared: BTreeSet<&str> = interface.data.iter().map(|d| d.name.as_str()).collect();
    let listed = || {
        interface
            .data
            .iter()
            .map(|d| format!("`{}`", d.name))
            .collect::<Vec<_>>()
            .join(", ")
    };

    for name in bindings.keys() {
        if !declared.contains(name.as_str()) {
            return Err(Error::invalid(if declared.is_empty() {
                format!(
                    "`{BINDINGS_KEY}` binds `{name}`, but the program's `data` block declares \
                     nothing"
                )
            } else {
                format!(
                    "`{BINDINGS_KEY}` binds `{name}`, which the program's `data` block does not \
                     declare (it declares {})",
                    listed()
                )
            }));
        }
    }
    for (name, binding) in bindings {
        let has_kind = binding
            .get("kind")
            .and_then(Json::as_str)
            .is_some_and(|k| !k.trim().is_empty());
        if !has_kind {
            return Err(Error::invalid(format!(
                "the binding of `{name}` must be an object with a `kind`"
            )));
        }
    }

    let missing: Vec<String> = interface
        .data
        .iter()
        .filter(|d| !bindings.contains_key(&d.name))
        .map(|d| format!("`{}` ({})", d.name, d.stan_type))
        .collect();
    match missing.len() {
        0 => Ok(()),
        1 => Err(Error::invalid(format!(
            "the data variable {} has no binding",
            missing[0]
        ))),
        _ => Err(Error::invalid(format!(
            "the data variables {} have no binding",
            missing.join(", ")
        ))),
    }
}

#[cfg(test)]
mod declared_tests {
    use super::*;
    use crate::interface::{Declaration, Element, SizeExpr};
    use serde_json::json;

    fn radon() -> Interface {
        Interface {
            data: vec![
                Declaration::new("N", Element::Int, vec![], "int<lower=1>"),
                Declaration::new("y", Element::Real, vec![SizeExpr::var("N")], "vector[N]"),
            ],
            ..Interface::default()
        }
    }

    fn config(bindings: Json) -> Attrs {
        let mut attrs = Attrs::new();
        attrs.insert(BINDINGS_KEY.to_owned(), bindings);
        attrs
    }

    #[test]
    fn every_variable_bound_and_nothing_else_is_accepted() {
        let ok = config(json!({
            "N": {"kind": "count", "dataset": "main"},
            "y": {"kind": "column", "dataset": "main", "column": "log_radon"},
        }));
        check_bindings_declared(&radon(), &ok).unwrap();
        // A program with no data needs no bindings at all.
        check_bindings_declared(&Interface::default(), &Attrs::new()).unwrap();
    }

    #[test]
    fn a_variable_with_no_binding_is_named_with_its_type() {
        let err = check_bindings_declared(&radon(), &Attrs::new()).unwrap_err();
        assert!(
            err.to_string()
                .contains("the data variables `N` (int<lower=1>), `y` (vector[N]) have no binding"),
            "{err}"
        );
        let err = check_bindings_declared(&radon(), &config(json!({"N": {"kind": "count"}})))
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("the data variable `y` (vector[N]) has no binding"),
            "{err}"
        );
    }

    #[test]
    fn a_typo_lists_the_declared_variables() {
        let err = check_bindings_declared(
            &radon(),
            &config(json!({"N": {"kind": "count"}, "yy": {"kind": "column"}})),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains(
                "binds `yy`, which the program's `data` block does not declare (it declares `N`, \
                 `y`)"
            ),
            "{err}"
        );
    }

    #[test]
    fn a_binding_is_an_object_with_a_kind() {
        let err = check_bindings_declared(&radon(), &config(json!({"N": 3, "y": {"kind": "x"}})))
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("the binding of `N` must be an object with a `kind`")
        );
        let err = check_bindings_declared(&radon(), &config(json!(["N"]))).unwrap_err();
        assert!(err.to_string().contains("must be an object"));
    }
}

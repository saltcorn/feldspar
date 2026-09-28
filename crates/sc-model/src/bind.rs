//! The binder's vocabulary, and the first of its checks (Stan TODO §§9–10).
//!
//! A provider that [binds data](crate::ModelProvider::binds_data) declares an
//! [`Interface`], and its configuration says how each `data` variable is
//! computed from the datasets: one **binding** per variable, under
//! [`BINDINGS_KEY`], with the extra dimensions under [`DIMENSIONS_KEY`] and the
//! output labels under [`LABELS_KEY`]. The key names are the host's, not the
//! provider's, because the binder that reads them is the host's (§4): a second
//! Bayesian provider must not be able to spell them differently.
//!
//! What is here is the part of §10's **save-time** checks that needs only the
//! interface and the configuration's keys: every declared `data` variable has a
//! binding, and every binding names a declared variable. The binding kinds, and
//! the checks of each against its declaration and the datasets, are Phase 3's.

use std::collections::BTreeSet;

use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;

use crate::interface::Interface;

/// The configuration key holding the bindings: an object from each `data`
/// variable's name to its binding.
pub const BINDINGS_KEY: &str = "bindings";
/// The configuration key holding the declared dimensions — values and time
/// grids; every dataset is a rows dimension without being declared (§8).
pub const DIMENSIONS_KEY: &str = "dimensions";
/// The configuration key holding explicit labels for output variables (§15).
pub const LABELS_KEY: &str = "labels";

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
mod tests {
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

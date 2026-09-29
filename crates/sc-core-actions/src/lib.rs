//! The core built-in actions (layer 9; technical design §10.1, TODO Phase 3).
//!
//! Every action Saltcorn ships with, in one crate: [`InsertRow`], [`UpdateRows`],
//! [`DeleteRows`], [`Fetch`], [`RunJsCode`], [`RunPythonCode`], [`SendEmail`],
//! and [`FitModel`]. The set is **deliberately small** — GOALS asks for a
//! minimal one, because control flow belongs to the workflow engine (§10.3)
//! rather than to a proliferation of actions — and [`builtin_actions`] is the
//! single constructor that assembles it.
//!
//! ## Why a crate of its own
//!
//! Three of these must write rows through **`sc-api::rows`**, so that a trigger's
//! write is the same write an API caller makes: the same type coercion, the same
//! rich-type and `File`-field validation, and — from Phase 4 — the same emitted
//! events, which is the recursion decision 5 bounds rather than forbids. That
//! fixes them *above* the row layer (layer 8). `fetch` needs none of that — but
//! splitting the built-in set across two crates by that accident of implementation
//! made "where does an action live?" a question with two answers and a paragraph
//! of explanation. One crate above the row layer makes it one answer.
//!
//! What stays **below**, in `sc-action` (layer 6), is everything that is not an
//! action: the [`Action`](sc_action::Action) trait, the event model, the run
//! context, the registry, the [`Trigger`](sc_action::Trigger) and its storage — and
//! the machinery every action's configuration goes through (`sc_action`'s formula
//! scope, settings parsers and [event bindings](sc_action::EventBindings)). So a
//! plugin's action needs `sc-action` alone, and this crate is the first consumer of
//! that seam rather than a privileged one.
//!
//! Layering is one-way as it always was: nothing in `sc-action` names this crate,
//! and a write reaches a trigger through the seam the catalog holds (Phase 4),
//! never through a dependency.
//!
//! ## One scope rule, two typings
//!
//! Every configuration value that reads the event is a **formula** in the same
//! `sc-expr` language as an ownership rule and a trigger's `only_if`, under
//! decision 7's scope rule, which `sc-action` defines. The `rows_scope` module adds
//! only what a *row* action needs on top: values typed by their columns (which only
//! the translated `where` cares about), the two `where` strategies, the per-row
//! prefetching, and the authority a write runs under.

mod code_body;
mod code_fetch;
mod delete_rows;
mod fetch;
mod fit_model;
mod insert_row;
mod rows_scope;
mod run_js_code;
mod run_python_code;
mod send_email;
mod update_rows;

use std::sync::Arc;

use sc_action::ActionRegistry;
use sc_error::Result;

pub use code_body::{CodeSurfaces, Hosts as CodeBodyHosts};
pub use delete_rows::DeleteRows;
pub use fetch::Fetch;
pub use fit_model::FitModel;
pub use insert_row::InsertRow;
pub use run_js_code::RunJsCode;
pub use run_python_code::RunPythonCode;
pub use send_email::SendEmail;
pub use update_rows::UpdateRows;

/// The built-in action set a server installs.
///
/// One constructor, so a deployment cannot end up with half the built-ins
/// depending on what it remembered to register. Fallible because [`Fetch`] builds
/// an HTTP client, and therefore a TLS stack: a deployment where that cannot be
/// initialised should hear about it at boot rather than at the first firing.
pub fn builtin_actions() -> Result<ActionRegistry> {
    let mut registry = ActionRegistry::new();
    register_builtin_actions(&mut registry)?;
    Ok(registry)
}

/// Add the built-in actions to an existing registry — for a deployment (or a
/// test) that assembles its own set from these plus its plugins'.
///
/// Fails if one of the names is already taken, as any duplicate registration
/// does: which implementation answers to `insert_row` must not depend on load
/// order.
pub fn register_builtin_actions(registry: &mut ActionRegistry) -> Result<()> {
    registry.register(Arc::new(InsertRow))?;
    registry.register(Arc::new(UpdateRows))?;
    registry.register(Arc::new(DeleteRows))?;
    registry.register(Arc::new(Fetch::new()?))?;
    registry.register(Arc::new(RunJsCode::new()?))?;
    registry.register(Arc::new(RunPythonCode::new()?))?;
    registry.register(Arc::new(SendEmail))?;
    Ok(())
}

/// Add the model action to a registry — `fit_model`, the one model action,
/// because it is the one that means the same thing for every provider
/// (milestone 31 §1). Predicting and writing a posterior back are methods of
/// a model, reached from code, and `predict("…")` in a formula.
///
/// Separate from [`register_builtin_actions`] for the same reason
/// `sc_core_traits::register_agent_actions` is separate: it holds the model
/// provider registry a fit is validated against and the seam a fit is started
/// through, and neither exists until a server has assembled them. A process
/// with no model support registers the other seven and this one is simply
/// absent — which is what makes a trigger naming it report "unknown action"
/// rather than fail silently.
pub fn register_model_actions(
    registry: &mut ActionRegistry,
    providers: Arc<sc_model::ModelRegistry>,
    fits: Arc<dyn sc_model::FitStarter>,
) -> Result<()> {
    registry.register(Arc::new(FitModel::new(providers, fits)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtins_are_registered_under_their_stored_names() {
        let registry = builtin_actions().unwrap();
        assert_eq!(
            registry.names(),
            vec![
                "delete_rows",
                "fetch",
                "insert_row",
                "run_js_code",
                "run_python_code",
                "send_email",
                "update_rows"
            ]
        );
        // Every one of them describes itself and its configuration as data, which
        // is what lets the admin UI render a form it has never heard of — and
        // every one names something required, so a blank form cannot be saved.
        for action in registry.all() {
            assert!(!action.description().is_empty(), "{}", action.name());
            let spec = action.config_spec();
            assert!(!spec.is_empty(), "{}", action.name());
            assert!(spec.iter().any(|f| f.required), "{}", action.name());
        }
    }

    /// A `FitStarter` that is never called: the registration is under test.
    struct NoFits;

    #[async_trait::async_trait]
    impl sc_model::FitStarter for NoFits {
        async fn start_fit(
            &self,
            _model: &sc_model::Model,
            _instance: sc_model::ModelInstance,
            _activation: sc_model::Activation,
        ) -> Result<sc_model::ModelInstance> {
            unreachable!("nothing is fitted here")
        }
    }

    #[test]
    fn the_model_action_set_is_exactly_fit_model() {
        // Milestone 31 §1: a model's one action is the generic one. Predicting
        // is `predict("…")` in a formula and `m.predict` in code; writing a
        // posterior back is `m.writePosterior`.
        let mut registry = sc_action::ActionRegistry::new();
        register_model_actions(
            &mut registry,
            Arc::new(sc_model::ModelRegistry::new()),
            Arc::new(NoFits),
        )
        .unwrap();
        assert_eq!(registry.names(), vec!["fit_model"]);
        let spec: Vec<String> = registry
            .require("fit_model")
            .unwrap()
            .config_spec()
            .iter()
            .map(|f| f.name().to_owned())
            .collect();
        assert_eq!(spec, vec!["model", "activate", "wait", "name"]);
    }

    #[test]
    fn registering_the_builtins_twice_is_refused() {
        let mut registry = builtin_actions().unwrap();
        let err = register_builtin_actions(&mut registry).unwrap_err();
        assert!(err.to_string().contains("insert_row"), "{err}");
    }

    #[test]
    fn each_action_declares_the_settings_its_semantics_need() {
        let registry = builtin_actions().unwrap();
        let names = |action: &str| -> Vec<String> {
            registry
                .require(action)
                .unwrap()
                .config_spec()
                .iter()
                .map(|f| f.name().to_owned())
                .collect()
        };
        assert_eq!(names("insert_row"), vec!["table", "values"]);
        assert_eq!(names("update_rows"), vec!["table", "where", "assignments"]);
        assert_eq!(names("delete_rows"), vec!["table", "where"]);
        assert_eq!(
            names("fetch"),
            vec!["url", "method", "headers", "body", "timeout_ms"]
        );
        assert_eq!(names("run_js_code"), vec!["code", "timeout_ms"]);
        assert_eq!(
            names("send_email"),
            vec!["to", "cc", "bcc", "from", "subject", "html", "mjml", "text"]
        );
    }
}

//! A module's **model providers**, as `sc-model`'s [`ModelProviderHost`] (TODO
//! "Predictive models" §14).
//!
//! A module exports `modelproviders` beside its `actions` and
//! `table_providers`:
//!
//! ```js
//! modelproviders: {
//!   ridge: {
//!     description: "Linear regression with an L2 penalty",
//!     config_fields: [{ name: "label", label: "Label", type: "String", required: true }],
//!     hyperparameters: [{ name: "alpha", type: "Float", default: 1 }],
//!     outcome: { kind: "regression", label: "label" },
//!     standardise: true,
//!     fit: async ({ frame, configuration, hyperparameters }) => ({ state, parameters, warnings }),
//!     predict: async ({ state, frame }) => [1.2, 3.4],
//!   },
//! }
//! ```
//!
//! [`ModuleModelProviders`] is [`ModuleTableProviders`](crate::
//! ModuleTableProviders)' sibling in every respect: built whole on every module
//! change, routed to the worker the named module is loaded on, and checking
//! every name again on this side because the set can be rebuilt between a model
//! being saved and a fit of it starting.
//!
//! # The frame crosses as columns
//!
//! [`Frame::to_json`] is twelve arrays for a 50 000 × 12 dataset, not 50 000
//! objects with the same twelve keys repeated — which is the difference between
//! a Python provider being usable and being a curiosity. What comes back is
//! `sc_model::FitResult` and `sc_model::Prediction`, read leniently in the two
//! places a plugin author would otherwise be writing tagged JSON by hand: a
//! prediction may be a bare number, a bare string or a bare array, and only a
//! cluster number or a class probability needs the explicit form.
//!
//! # Metrics are not asked for, and cannot be sent
//!
//! # Outputs
//!
//! A provider may declare `outputs` (analytics TODO A3.2): what a fit of it
//! shows, as `sc_model::OutputDecl`'s JSON — `{ name, label, kind:
//! "parameters", block }`, `{ kind: "metrics" }`, or `{ kind: "plot", data,
//! spec }` with a plot spec over output data. The data may be the host's
//! (`rows`, each scored row with `actual`, `fitted`, …) or the provider's own,
//! answered by `fit` as `outputs: { name: frame }` in the frame's JSON.
//! Without a declaration a fit shows the standard outputs of its outcome.
//!
//! A provider answers its state, its parameters and, optionally, `warnings`:
//! sentences the admin should read before trusting the fit, which make it
//! not "clean" for `fit_model`'s `activate: if_clean`. Everything scored — R²,
//! RMSE, accuracy, the confusion matrix — is computed by `sc-model` over the
//! same splits with the same code for every provider, which is what makes a
//! scikit-learn estimator's number comparable with a smartcore regression's.

use std::sync::Arc;

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_model::{
    FitResult, Frame, ModelProviderHost, ModelProviderKind, OutcomeSpec, OutputDecl, Prediction,
};
use sc_types::Attrs;
use serde_json::Value as Json;

use crate::host::ModuleHost;
use crate::modules::ModuleSet;
use crate::spec::config_fields_to_form_fields;

/// The model providers this server's JavaScript modules supply.
pub struct ModuleModelProviders {
    host: Arc<ModuleHost>,
    providers: Vec<ModelProviderKind>,
}

impl ModuleModelProviders {
    /// Every model provider of every loaded module in `set`, over `host`.
    ///
    /// A module that would not load supplies none, and a provider whose
    /// `outcome` cannot be read supplies nothing either: the host script has
    /// already reported that one as an issue on the module's card, so repeating
    /// it here would put a module's problem in front of an admin fitting an
    /// unrelated model.
    #[must_use]
    pub fn new(host: &Arc<ModuleHost>, set: &ModuleSet) -> ModuleModelProviders {
        let mut providers = Vec::new();
        for loaded in set.modules() {
            let Some(manifest) = &loaded.manifest else {
                continue;
            };
            for provider in &manifest.model_providers {
                let Ok(outcome) = serde_json::from_value::<OutcomeSpec>(provider.outcome.clone())
                else {
                    continue;
                };
                let owner = format!("the model provider `{}`", provider.name);
                let (config_spec, _) =
                    config_fields_to_form_fields(&provider.config_fields, &owner);
                let (hyperparameters, _) =
                    config_fields_to_form_fields(&provider.hyperparameters, &owner);
                let mut kind =
                    ModelProviderKind::new(&provider.name, &provider.description, outcome)
                        .config(config_spec)
                        .hyperparameters(hyperparameters)
                        .module(loaded.module.name.clone());
                kind.standardise = provider.standardise;
                kind.outputs = declared_outputs(&provider.outputs);
                providers.push(kind);
            }
        }
        ModuleModelProviders {
            host: Arc::clone(host),
            providers,
        }
    }

    /// An empty set — a server with no modules, and the starting point for a
    /// test that wants the seam without a worker.
    #[must_use]
    pub fn empty(host: &Arc<ModuleHost>) -> ModuleModelProviders {
        ModuleModelProviders {
            host: Arc::clone(host),
            providers: Vec::new(),
        }
    }

    /// Refuse a name this set does not have, before a worker is reached.
    ///
    /// Re-checked here even though the registry resolved the provider through
    /// this same list: a module can be uninstalled between a model being opened
    /// and its Fit button being pressed, and the honest answer then is a
    /// sentence naming what went away.
    fn require(&self, module: &str, provider: &str) -> Result<()> {
        if self
            .providers
            .iter()
            .any(|p| p.name == provider && p.module.as_deref() == Some(module))
        {
            return Ok(());
        }
        Err(Error::not_found(format!(
            "no installed module supplies the model provider `{provider}` of `{module}`; it may \
             have been uninstalled, or failed to load"
        )))
    }

    /// Every provider, in the order the module set has them.
    #[must_use]
    pub fn providers(&self) -> &[ModelProviderKind] {
        &self.providers
    }
}

#[async_trait]
impl ModelProviderHost for ModuleModelProviders {
    fn providers(&self) -> Vec<ModelProviderKind> {
        self.providers.clone()
    }

    async fn fit(
        &self,
        module: &str,
        provider: &str,
        frame: &Frame,
        config: &Attrs,
        hyper: &Attrs,
    ) -> Result<FitResult> {
        self.require(module, provider)?;
        let answer = self
            .host
            .model_fit(
                module,
                provider,
                &frame.to_json(),
                &Json::Object(config.clone()),
                &Json::Object(hyper.clone()),
            )
            .await?;
        read_fit(module, provider, answer)
    }

    async fn predict(
        &self,
        module: &str,
        provider: &str,
        state: &Json,
        frame: &Frame,
    ) -> Result<Vec<Prediction>> {
        self.require(module, provider)?;
        let answer = self
            .host
            .model_predict(module, provider, state, &frame.to_json())
            .await?;
        read_predictions(module, provider, answer, frame.rows)
    }
}

/// The outputs a module's provider declares (analytics TODO A3.2), or `None`
/// — the standard ones — when it declares none or declares them in a shape
/// this server cannot read.
pub fn declared_outputs(json: &Json) -> Option<Vec<OutputDecl>> {
    if json.is_null() {
        return None;
    }
    serde_json::from_value(json.clone()).ok()
}

/// What a module answered a fit with, as a [`FitResult`].
///
/// Named rather than defaulted: a fit whose state did not survive the seam is a
/// model that will predict nonsense later, and "the provider answered something
/// this server could not read" at the moment of the fit is the only place the
/// admin can act on it.
pub fn read_fit(module: &str, provider: &str, answer: Json) -> Result<FitResult> {
    serde_json::from_value(answer).map_err(|e| {
        Error::config(format!(
            "the model provider `{provider}` of `{module}` answered a fit this server could not \
             read: {e}"
        ))
    })
}

/// What a module answered a prediction with, as one [`Prediction`] per row.
///
/// **Lenient in three shapes, and only three.** A bare number is a regression's
/// answer, a bare string is a class, and a bare array is a vector — which is
/// what a provider written over numpy will naturally produce, and demanding
/// `{"prediction":"number","value":1.2}` for each of 50 000 rows would be a tax
/// on the common case. A cluster number and a class probability carry
/// information a bare value cannot, so they are written out in full.
///
/// The **count** is checked, because a provider that answered 49 999 predictions
/// for 50 000 rows would otherwise put every row after the missing one against
/// its neighbour's answer.
pub fn read_predictions(
    module: &str,
    provider: &str,
    answer: Json,
    rows: usize,
) -> Result<Vec<Prediction>> {
    let Json::Array(values) = answer else {
        return Err(Error::config(format!(
            "the model provider `{provider}` of `{module}` answered its predictions with \
             {answer}, which is not a list"
        )));
    };
    if values.len() != rows {
        return Err(Error::config(format!(
            "the model provider `{provider}` of `{module}` answered {} predictions for {rows} \
             rows",
            values.len()
        )));
    }
    values
        .into_iter()
        .enumerate()
        .map(|(i, value)| {
            read_prediction(value).map_err(|e| {
                Error::config(format!(
                    "the model provider `{provider}` of `{module}`: prediction {} could not be \
                     read: {e}",
                    i + 1
                ))
            })
        })
        .collect()
}

/// One prediction, in the tagged form or in one of the three bare ones.
fn read_prediction(value: Json) -> Result<Prediction> {
    match value {
        Json::Number(n) => n
            .as_f64()
            .map(Prediction::number)
            .ok_or_else(|| Error::invalid("a prediction that is not a finite number")),
        Json::String(s) => Ok(Prediction::class(s, None)),
        Json::Array(values) => {
            let mut out = Vec::with_capacity(values.len());
            for value in values {
                out.push(value.as_f64().ok_or_else(|| {
                    Error::invalid(format!("a vector component that is not a number: {value}"))
                })?);
            }
            Ok(Prediction::Vector { values: out })
        }
        other => serde_json::from_value(other).map_err(|e| Error::invalid(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_nothing_supplies_is_refused_before_a_worker_is_reached() {
        let host = Arc::new(ModuleHost::new("modules"));
        let providers = ModuleModelProviders::empty(&host);
        let err = providers
            .require("@saltcorn-test/echo", "echo_ridge")
            .expect_err("nothing supplies it");
        assert!(err.to_string().contains("echo_ridge"), "{err}");
        assert!(ModelProviderHost::providers(&providers).is_empty());
    }

    #[test]
    fn the_three_bare_prediction_shapes_are_read_and_the_count_is_checked() {
        let read = read_predictions(
            "m",
            "p",
            serde_json::json!([1.5, "north", [1.0, 2.0], { "prediction": "cluster", "cluster": 2 }]),
            4,
        )
        .expect("four predictions");
        assert_eq!(read[0], Prediction::number(1.5));
        assert_eq!(read[1], Prediction::class("north", None));
        assert_eq!(
            read[2],
            Prediction::Vector {
                values: vec![1.0, 2.0]
            }
        );
        assert_eq!(read[3], Prediction::Cluster { cluster: 2 });

        // One short is a wrong answer per row, not a shorter list.
        let err = read_predictions("m", "p", serde_json::json!([1.0]), 2).expect_err("one short");
        assert!(
            err.to_string().contains("1 predictions for 2 rows"),
            "{err}"
        );
        let err = read_predictions("m", "p", serde_json::json!({}), 0).expect_err("not a list");
        assert!(err.to_string().contains("not a list"), "{err}");
    }

    #[test]
    fn a_fit_that_answers_only_a_state_is_read_as_having_no_parameters() {
        let fit = read_fit("m", "p", serde_json::json!({ "state": { "b": [1.0] } }))
            .expect("state alone is a fit");
        assert!(fit.parameters.is_empty());
        let err = read_fit("m", "p", serde_json::json!({ "parameters": [] }))
            .expect_err("a fit with no state cannot predict");
        assert!(err.to_string().contains("could not read"), "{err}");
    }
}

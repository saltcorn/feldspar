//! A Python module's **model providers**, as `sc-model`'s [`ModelProviderHost`]
//! (TODO "Predictive models" §14).
//!
//! [`sc_module::ModuleModelProviders`]' sibling, and the same shape for the same
//! reasons: built whole on every module change, and every name checked again on
//! this side because a module can be uninstalled between a model being saved and
//! a fit of it starting.
//!
//! A plugin declares one with a decorator:
//!
//! ```python
//! @sc.model_provider(
//!     "ridge",
//!     config=[sc.Field.string("label", label="Label", required=True)],
//!     hyperparameters=[sc.Field.float("alpha", label="Alpha", default=1.0)],
//!     outcome=sc.Outcome.regression("label"),
//!     standardise=True,
//! )
//! class Ridge:
//!     def fit(self, frame, configuration, hyperparameters): ...
//!     def predict(self, state, frame): ...
//! ```
//!
//! The frame crosses as **columns**, which is what makes a Python provider
//! usable at all: it lands on the far side as lists `numpy.asarray` takes
//! directly, and a 50 000 × 12 dataset is twelve arrays rather than 50 000
//! objects with the same twelve keys repeated. What comes back is read by
//! `sc_module`'s own reader, because the two languages answer one wire format
//! and two readers would be two ways for a provider to be misread.

use std::sync::Arc;

use async_trait::async_trait;
use sc_error::Result;
use sc_model::{FitResult, Frame, ModelProviderHost, ModelProviderKind, OutcomeSpec, Prediction};
use sc_module::LoadedModule;
use sc_module::model_providers::{declared_outputs, read_fit, read_predictions};
use sc_types::Attrs;
use serde_json::Value as Json;

use super::fields;
use super::host::PyModuleHost;

/// The model providers this server's Python modules supply.
pub struct PyModuleModelProviders {
    host: Arc<PyModuleHost>,
    providers: Vec<ModelProviderKind>,
}

impl PyModuleModelProviders {
    /// Every model provider of every loaded Python module, over `host`.
    ///
    /// A provider whose `outcome` cannot be read supplies nothing: the decorator
    /// has already refused it on the Python side, and that refusal is the
    /// module's own issue on its card.
    #[must_use]
    pub fn new(host: &Arc<PyModuleHost>, modules: &[LoadedModule]) -> PyModuleModelProviders {
        let mut providers = Vec::new();
        for loaded in modules {
            let Some(manifest) = &loaded.manifest else {
                continue;
            };
            for provider in &manifest.model_providers {
                let Ok(outcome) = serde_json::from_value::<OutcomeSpec>(provider.outcome.clone())
                else {
                    continue;
                };
                let mut kind =
                    ModelProviderKind::new(&provider.name, &provider.description, outcome)
                        .config(fields::form_fields(&provider.config_fields))
                        .hyperparameters(fields::form_fields(&provider.hyperparameters))
                        .module(loaded.module.name.clone());
                kind.standardise = provider.standardise;
                kind.outputs = declared_outputs(&provider.outputs);
                providers.push(kind);
            }
        }
        PyModuleModelProviders {
            host: Arc::clone(host),
            providers,
        }
    }

    /// An empty set — a server with no Python modules.
    #[must_use]
    pub fn empty(host: &Arc<PyModuleHost>) -> PyModuleModelProviders {
        PyModuleModelProviders {
            host: Arc::clone(host),
            providers: Vec::new(),
        }
    }

    /// Refuse a name this set does not have, before the interpreter is reached.
    fn require(&self, module: &str, provider: &str) -> Result<()> {
        if self
            .providers
            .iter()
            .any(|p| p.name == provider && p.module.as_deref() == Some(module))
        {
            return Ok(());
        }
        Err(sc_error::Error::not_found(format!(
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
impl ModelProviderHost for PyModuleModelProviders {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PythonRuntime;

    #[tokio::test]
    async fn a_provider_nothing_supplies_is_refused_before_the_interpreter() {
        let host = Arc::new(PyModuleHost::new(Arc::new(PythonRuntime::new())));
        let providers = PyModuleModelProviders::empty(&host);
        let err = providers
            .predict("feldspar-sklearn", "ridge", &Json::Null, &Frame::default())
            .await
            .expect_err("nothing supplies it")
            .to_string();
        assert!(
            err.contains("ridge") && err.contains("feldspar-sklearn"),
            "{err}"
        );
        assert!(ModelProviderHost::providers(&providers).is_empty());
    }
}

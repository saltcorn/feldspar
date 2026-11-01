//! `fit_model` — refit a named model from a trigger or a workflow (Stan TODO
//! 7.5; milestone 31 §2).
//!
//! The one model action, because it is the one that is **generic**: every
//! provider fits, and "fit it again, and maybe make the new fit the one that
//! answers" means the same thing for a linear regression, a random forest and
//! a Stan program. What a model does after that is a method of the model,
//! reached from code (`models.get("…")`), or a `predict("…")` in a formula.
//!
//! "Clean" is provider-neutral too: a fit is clean when it is fitted and
//! nothing warned — a posterior's diagnostics or a provider's own
//! [`FitResult::warnings`](sc_model::FitResult) — which is what
//! [`Activation::IfClean`] reads.

use std::sync::Arc;
use std::time::Duration;

use sc_action::{Action, ActionContext, ConfigCheck};
use sc_error::{Error, Result};
use sc_model::{Activation, FitStarter, ModelInstance, ModelRegistry};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

/// The model, by name.
pub const CFG_MODEL: &str = "model";
/// When the new fit becomes active: `never`, `if_clean` or `always`.
pub const CFG_ACTIVATE: &str = "activate";
/// Whether the action waits for the fit to finish.
pub const CFG_WAIT: &str = "wait";
/// The new fit's name.
pub const CFG_NAME: &str = "name";

/// Whether `fit_model` waits for the fit by default.
const DEFAULT_WAIT: bool = true;

/// How often a waiting `fit_model` reads its instance's row back.
const WAIT_POLL: Duration = Duration::from_millis(250);

/// Start a fit of a named model.
///
/// By default it **waits** for the fit and answers how it ended —
/// `{ instance, status, active, error, warnings, metrics }` — so the next step
/// of a workflow sees the new fit, and can branch on its `metrics` (say
/// `metrics.test.r2`) where no activation rule would do. `activate` decides
/// whether the new fit becomes the model's active one: `never` (the default),
/// `if_clean` (fitted, with no warnings) or `always` (fitted, warnings or
/// not). Not waiting starts the fit and answers at once, and the job applies
/// `activate` when it finishes.
pub struct FitModel {
    providers: Arc<ModelRegistry>,
    fits: Arc<dyn FitStarter>,
}

impl FitModel {
    /// The action, validating against `providers` and starting fits through
    /// `fits`.
    pub fn new(providers: Arc<ModelRegistry>, fits: Arc<dyn FitStarter>) -> FitModel {
        FitModel { providers, fits }
    }
}

/// A required, non-empty configuration string.
fn configured(config: &Attrs, key: &str) -> Result<String> {
    optional(config, key).ok_or_else(|| Error::invalid(format!("`{key}` is required")))
}

/// An optional configuration string, empty treated as absent.
fn optional(config: &Attrs, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A boolean setting, `default` when unset.
fn flag(config: &Attrs, key: &str, default: bool) -> Result<bool> {
    match config.get(key) {
        None | Some(Json::Null) => Ok(default),
        Some(Json::Bool(b)) => Ok(*b),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` must be true or false, not `{other}`"
        ))),
    }
}

/// The configured [`Activation`], `never` when unset.
fn activation(config: &Attrs) -> Result<Activation> {
    match config.get(CFG_ACTIVATE) {
        None | Some(Json::Null) => Ok(Activation::Never),
        Some(Json::String(raw)) if raw.trim().is_empty() => Ok(Activation::Never),
        Some(Json::String(raw)) => Activation::parse(raw),
        Some(other) => Err(Error::invalid(format!(
            "`{CFG_ACTIVATE}` must be one of `never`, `if_clean` or `always`, not `{other}`"
        ))),
    }
}

/// What the action answers about `instance`.
fn answer(instance: &ModelInstance) -> Json {
    json!({
        "instance": instance.id.to_string(),
        "status": instance.status.as_str(),
        "active": instance.active,
        "error": instance.error(),
        "warnings": instance
            .attributes
            .get(sc_model::ATTR_WARNINGS)
            .cloned()
            .unwrap_or_else(|| json!([])),
        "metrics": instance.metrics.clone(),
    })
}

#[async_trait::async_trait]
impl Action for FitModel {
    fn name(&self) -> &str {
        "fit_model"
    }

    fn description(&self) -> &str {
        "Fit a model again, and optionally make the new fit active"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_MODEL, BasicType::Text)
                .label("Model")
                .server_query(sc_model::MODELS_QUERY)
                .required(),
            FormField::new(CFG_ACTIVATE, BasicType::Text)
                .label("Make the new fit active")
                .options(Activation::ALL)
                .default_value(Activation::Never.as_str()),
            FormField::new(CFG_WAIT, BasicType::Bool)
                .label("Wait for the fit to finish")
                .default_value(DEFAULT_WAIT),
            FormField::new(CFG_NAME, BasicType::Text).label("Name of the new fit"),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        let name = configured(check.config, CFG_MODEL)?;
        sc_model::load_model_by_name(check.catalog, &name)
            .await?
            .ok_or_else(|| Error::invalid(format!("no model named `{name}`")))?;
        activation(check.config)?;
        flag(check.config, CFG_WAIT, DEFAULT_WAIT)?;
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let named = |e: Error| Error::invalid(format!("trigger `{}`: {e}", ctx.trigger));
        let name = configured(ctx.config, CFG_MODEL).map_err(named)?;
        let activate = activation(ctx.config).map_err(named)?;
        let wait = flag(ctx.config, CFG_WAIT, DEFAULT_WAIT).map_err(named)?;
        let model = sc_model::require_model(ctx.catalog, &name)
            .await
            .map_err(named)?;
        // Checked before the row exists, as the Fit button checks.
        sc_model::validate_model(ctx.catalog, &self.providers, &model, None)
            .await
            .map_err(named)?;
        let mut instance = ModelInstance::starting(model.id);
        instance.name = optional(ctx.config, CFG_NAME)
            .unwrap_or_else(|| format!("fitted by trigger `{}`", ctx.trigger));
        // Not waiting, the job activates; waiting, this does, once the row
        // says how the fit ended — so the answer can say whether it did.
        let started = self
            .fits
            .start_fit(
                &model,
                instance,
                if wait { Activation::Never } else { activate },
            )
            .await
            .map_err(named)?;
        if !wait {
            return Ok(answer(&started));
        }
        let mut finished = loop {
            let row = sc_model::require_model_instance(ctx.catalog, started.id)
                .await
                .map_err(named)?;
            if row.status != sc_model::FitStatus::Fitting {
                break row;
            }
            tokio::time::sleep(WAIT_POLL).await;
        };
        if activate.activates(&finished) {
            finished.active = true;
            sc_model::save_model_instance(ctx.catalog, &finished)
                .await
                .map_err(named)?;
        }
        Ok(answer(&finished))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, Json)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn fit_model_waits_by_default_and_takes_its_flags_as_booleans() {
        assert!(flag(&Attrs::new(), CFG_WAIT, DEFAULT_WAIT).unwrap());
        assert!(!flag(&config(&[(CFG_WAIT, json!(false))]), CFG_WAIT, true).unwrap());
        let err = flag(&config(&[(CFG_WAIT, json!("yes"))]), CFG_WAIT, true).unwrap_err();
        assert!(err.to_string().contains("true or false"), "{err}");
    }

    #[test]
    fn activate_is_one_of_three_words_and_never_by_default() {
        assert_eq!(activation(&Attrs::new()).unwrap(), Activation::Never);
        assert_eq!(
            activation(&config(&[(CFG_ACTIVATE, json!("if_clean"))])).unwrap(),
            Activation::IfClean
        );
        assert_eq!(
            activation(&config(&[(CFG_ACTIVATE, json!("always"))])).unwrap(),
            Activation::Always
        );
        for wrong in [json!(true), json!("sometimes")] {
            let err = activation(&config(&[(CFG_ACTIVATE, wrong)])).unwrap_err();
            assert!(
                err.to_string()
                    .contains("must be one of `never`, `if_clean` or `always`"),
                "{err}"
            );
        }
    }

    #[test]
    fn if_clean_reads_the_warnings_and_always_only_the_status() {
        let fitted = sc_model::fitted(
            ModelInstance::starting(sc_model::ModelId::new()),
            json!(null),
            Vec::new(),
        );
        let mut warned = fitted.clone();
        warned
            .attributes
            .insert(sc_model::ATTR_WARNINGS.to_owned(), json!(["check it"]));
        let failed = ModelInstance::starting(sc_model::ModelId::new()).failed("no");

        assert!(Activation::IfClean.activates(&fitted));
        assert!(!Activation::IfClean.activates(&warned));
        assert!(Activation::Always.activates(&warned));
        assert!(!Activation::Always.activates(&failed));
        assert!(!Activation::Never.activates(&fitted));
    }
}

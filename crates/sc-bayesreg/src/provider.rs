//! `BayesRegProvider` (TODO-bayesian-regression R1.7): the formula as a
//! [`ModelProvider`].
//!
//! Its configuration is the formula, the family and its links, the explicit
//! prior rows and `center`, beside the Stan sampler's settings (the same keys
//! as the `stan` provider's, `sc_stan::config_keys`). It **lowers** that
//! configuration (§7) into the generated program and its bindings, and the
//! host binds, checks and labels exactly as it does for a hand-written Stan
//! model. A fit hands the generated program to the shared
//! [`StanProvider::run_program`], so compiling, sampling, the compile cache
//! and the process budget are the `stan` provider's own — nothing about
//! CmdStan is duplicated.

use std::sync::Arc;

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_model::{
    BINDINGS_KEY, DIMENSIONS_KEY, DatasetShape, DrawPlan, FitContext, FitResult, Frame, Interface,
    LABELS_KEY, LowerInput, ModelProvider, Outcome, OutcomeSpec, POLICIES_KEY, PosteriorInput,
    PosteriorResult, Prediction,
};
use sc_stan::StanProvider;
use sc_stan::config_keys::{PROGRAM, PROGRAM_STORE};
use sc_stan::program::{Program, ProgramFile};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::family::FAMILIES;
use crate::generate::PROGRAM_PATH;
use crate::lower::{Built, build, keys};
use crate::resolve::Data;
use crate::{PROVIDER, Structure};

/// What a prediction reads: the expected response (`posterior_epred`).
pub const EPRED: &str = "epred";

/// The name a generated program's snapshot gives as its store: it was read
/// from no file store.
pub const GENERATED_STORE: &str = "(generated)";

/// The provider.
pub struct BayesRegProvider {
    stan: Arc<StanProvider>,
    description: String,
}

impl BayesRegProvider {
    /// The provider, fitting through `stan` (the server's, shared with the
    /// `stan` provider).
    pub fn new(stan: Arc<StanProvider>) -> BayesRegProvider {
        let mut description = "Bayesian regression from a formula in lme4/brms syntax \
                               (`y ~ x + (1 | g)`): a generated Stan program, sampled with \
                               CmdStan"
            .to_owned();
        if let Some(why) = stan.unavailable() {
            description.push_str(&format!(" — CmdStan was not found: {why}"));
        }
        BayesRegProvider { stan, description }
    }

    /// The Stan provider it fits through.
    pub fn stan(&self) -> &Arc<StanProvider> {
        &self.stan
    }
}

/// The program `text` as a program the Stan machinery takes: one file at the
/// fixed path, so the compile cache's key is the text's hash (§5).
pub fn program_of(text: &str) -> Result<Program> {
    Program::from_files(GENERATED_STORE, vec![ProgramFile::new(PROGRAM_PATH, text)])
}

/// The generated program in a lowered `config`, or the one its formula gives.
fn program_text(config: &Attrs) -> Result<String> {
    if let Some(text) = config.get(keys::GENERATED_PROGRAM).and_then(Json::as_str) {
        return Ok(text.to_owned());
    }
    Ok(build(config, Data::Nothing)?.program)
}

/// What a lowering knows of the data.
fn data_of<'a>(input: &LowerInput<'a>) -> (Data<'a>, Option<&'a Frame>) {
    match input {
        LowerInput::Shape { shape: Some(s), .. } => (Data::Shape(s), None),
        LowerInput::Shape { shape: None, .. } => (Data::Nothing, None),
        LowerInput::Frames { .. } => match input.main_frame() {
            Some(f) => (Data::Frame(f), Some(f)),
            None => (Data::Nothing, None),
        },
    }
}

/// Lower `config` for `input` — the free function behind
/// [`ModelProvider::lower`], for the editor's API too.
pub fn lower_config(config: &Attrs, input: LowerInput<'_>) -> Result<(Built, Attrs)> {
    let (data, frame) = data_of(&input);
    let ctx = crate::canon::PlanContext::of(input.model());
    let built = build(config, data)?;
    let lowered = built.lower_in(config, frame, &ctx)?;
    Ok((built, lowered))
}

#[async_trait]
impl ModelProvider for BayesRegProvider {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn config_declaration(&self) -> Vec<FormField> {
        let mut fields = vec![
            FormField::new(keys::FORMULA, BasicType::Text)
                .label("Formula")
                .required(),
            FormField::new(keys::FAMILY, BasicType::Text)
                .label("Family")
                .options(FAMILIES.iter().map(|f| f.name))
                .default_value("gaussian"),
            FormField::new(keys::LINKS, BasicType::Json).label("Links by parameter"),
            FormField::new(keys::PRIORS, BasicType::Json).label("Priors"),
            FormField::new(keys::CENTER, BasicType::Bool)
                .label("Centre the predictors")
                .default_value(true),
        ];
        // The sampler's settings are the Stan provider's own keys; where the
        // program is and how it is bound are this provider's to derive.
        let derived = [
            PROGRAM_STORE,
            PROGRAM,
            DIMENSIONS_KEY,
            BINDINGS_KEY,
            POLICIES_KEY,
            LABELS_KEY,
        ];
        fields.extend(
            self.stan
                .config_declaration()
                .into_iter()
                .filter(|f| !derived.contains(&f.name())),
        );
        fields
    }

    fn outcome_spec(&self) -> OutcomeSpec {
        // Prediction in Rust from the draws (§8, R2.4): the posterior mean
        // of the expected response, a number.
        OutcomeSpec::Posterior {
            prediction: Some(EPRED.to_owned()),
        }
    }

    /// A posterior that predicts `epred` — the expected response — except a
    /// multivariate or categorical model, which predicts no one number.
    fn outcome(&self, shape: &DatasetShape, config: &Attrs) -> Result<Outcome> {
        let built = build(config, Data::Shape(shape))?;
        Ok(Outcome::Posterior {
            prediction: crate::predict::predicts(built.structure())
                .is_ok()
                .then(|| EPRED.to_owned()),
        })
    }

    fn binds_data(&self) -> bool {
        true
    }

    fn cancellable(&self) -> bool {
        true
    }

    /// The formula against the dataset's shape (every name a column, the
    /// response the family's, every prior row a target the formula has), and
    /// the sampler's settings as the `stan` provider checks them.
    fn validate(&self, shape: &DatasetShape, config: &Attrs) -> Result<()> {
        for key in [PROGRAM_STORE, PROGRAM, BINDINGS_KEY, DIMENSIONS_KEY] {
            // Empty is unset: a form may send `{}` for a key it does not edit.
            let set = config.get(key).is_some_and(|v| match v {
                Json::Null => false,
                Json::String(s) => !s.trim().is_empty(),
                Json::Object(m) => !m.is_empty(),
                _ => true,
            });
            if set {
                return Err(Error::invalid(format!(
                    "`{key}` is derived from the formula, so it cannot be set; to write the \
                     program yourself, convert the model to a Stan model"
                )));
            }
        }
        let built = build(config, Data::Shape(shape))?;
        let table = built.prior_table(None)?.0;
        table.require_matched()?;
        crate::lower::require_nl_priors(&table)?;
        self.stan.validate(shape, config)
    }

    async fn fit(&self, _frame: &Frame, _config: &Attrs, _hyper: &Attrs) -> Result<FitResult> {
        Err(Error::invalid(
            "a Bayesian regression is sampled, not fitted to one frame: its outcome is a \
             posterior",
        ))
    }

    /// The posterior mean of each row's expected response, in Rust from the
    /// draws the fit kept (§8).
    async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>> {
        Ok(crate::predict::predict(state, frame)?
            .into_iter()
            .map(Prediction::number)
            .collect())
    }

    /// The expected response's posterior distribution (analytics TODO A7.2).
    fn uncertainty(&self) -> sc_model::Uncertainty {
        sc_model::Uncertainty::Distribution
    }

    /// Each row's draws of its expected response (analytics TODO A7.5).
    async fn predict_draws(&self, state: &Json, frame: &Frame) -> Result<Option<Vec<Vec<f64>>>> {
        crate::predict::predict_draws(state, frame).map(Some)
    }

    /// The formula's predictors (analytics TODO A7.4): the main dataset's
    /// columns the fit's bindings read, but for the response's own — the
    /// response, its bounds, censoring, weights, standard errors and subset.
    fn inputs(&self, state: &Json) -> Option<Vec<sc_model::PredictionInput>> {
        let config: Attrs = serde_json::from_value(
            state
                .get(crate::predict::PACK_KEY)?
                .get("config")
                .cloned()?,
        )
        .ok()?;
        let response_side = |name: &str| {
            ["Y", "lb_Y", "ub_Y", "weights", "cens", "se", "J_sub"]
                .iter()
                .any(|p| name.starts_with(p))
        };
        sc_model::bound_columns(&config, sc_model::MAIN_DATASET, response_side).ok()
    }

    /// The posterior of each row's expected response, from the kept draws,
    /// summarised at `level`.
    async fn predict_uncertain(
        &self,
        state: &Json,
        frame: &Frame,
        level: f64,
    ) -> Result<Vec<Prediction>> {
        crate::predict::predict_draws(state, frame)?
            .iter()
            .enumerate()
            .map(|(i, draws)| {
                sc_model::distribution_of(draws, level).ok_or_else(|| {
                    Error::invalid(format!(
                        "no draw of the expected response of row {} is a finite number",
                        i + 1
                    ))
                })
            })
            .collect()
    }

    fn lower(&self, config: &Attrs, input: LowerInput<'_>) -> Result<Option<Attrs>> {
        Ok(Some(lower_config(config, input)?.1))
    }

    /// The generated program's declarations, read by the Stan provider's own
    /// parser, so every host check runs on it unchanged.
    async fn interface(&self, config: &Attrs) -> Result<Option<Interface>> {
        Ok(Some(program_of(&program_text(config)?)?.interface()?))
    }

    async fn fit_posterior(
        &self,
        input: &PosteriorInput,
        config: &Attrs,
        ctx: &FitContext<'_>,
    ) -> Result<PosteriorResult> {
        let program = program_of(&program_text(config)?)?;
        let structure: Structure = match config.get(keys::STRUCTURE) {
            Some(s) => serde_json::from_value(s.clone())
                .map_err(|e| Error::msg(format!("the lowered structure is unreadable: {e}")))?,
            None => build(config, Data::Nothing)?.canonical.structure,
        };
        let mut result = self.stan.run_program(&program, input, config, ctx).await?;
        let layout = config.get(keys::LAYOUT).cloned().unwrap_or(Json::Null);
        let tables = crate::output::tables(&result.draws, &input.coordinates, &layout, &structure)?;
        // The provider's table first: it is what a brms user looks for.
        let host = std::mem::take(&mut result.parameters);
        result.parameters = tables;
        result.parameters.extend(host);
        if let Json::Object(state) = &mut result.state {
            state.insert(
                keys::STRUCTURE.to_owned(),
                serde_json::to_value(&structure).unwrap_or(Json::Null),
            );
            state.insert("generator".to_owned(), Json::from(crate::GENERATOR_VERSION));
            if let Some(p) = config.get(keys::RESOLVED_PRIORS) {
                state.insert(keys::RESOLVED_PRIORS.to_owned(), p.clone());
            }
            if let Some(f) = config.get(keys::FORMULA) {
                state.insert(keys::FORMULA.to_owned(), f.clone());
            }
            match crate::predict::pack(config, &structure, &input.coordinates, &result.draws) {
                Ok(pack) => {
                    state.insert(crate::predict::PACK_KEY.to_owned(), pack);
                }
                Err(why) => {
                    state.insert(crate::predict::NO_PACK_KEY.to_owned(), Json::from(why));
                }
            }
        }
        Ok(result)
    }

    fn draw_plan(&self, config: &Attrs) -> Result<Option<DrawPlan>> {
        self.stan.draw_plan(config)
    }

    async fn run_files(&self, state: &Json) -> Result<Option<Vec<(String, Vec<u8>)>>> {
        self.stan.run_files(state).await
    }

    /// The program the configuration generates now against the snapshot:
    /// another structure, or another generator version, is another program
    /// (its header names both). `None` when the formula does not build now.
    async fn program_changed(&self, config: &Attrs, state: &Json) -> Option<bool> {
        let snapshot = state
            .get("program")?
            .get("files")?
            .as_array()?
            .first()?
            .get("sha256")?
            .as_str()?
            .to_owned();
        let now = build(config, Data::Nothing).ok()?.program;
        Some(ProgramFile::new(PROGRAM_PATH, now).sha256 != snapshot)
    }

    async fn discard(&self, state: &Json) -> Result<()> {
        self.stan.discard(state).await
    }
}

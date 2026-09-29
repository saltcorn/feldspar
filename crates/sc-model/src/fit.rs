//! The orchestration: materialise, split, encode, search, fit, score, write
//! (TODO §8, §11, task 3.3).
//!
//! Everything else in this crate is a piece; this is the order they go in, and
//! the order is the design:
//!
//! 1. **Materialise** the dataset through the [`DatasetSource`] seam, bounded by
//!    the row cap.
//! 2. **Resolve the outcome** from the provider and the configuration, because
//!    everything after this branches on it — a hypothesis test has no split, no
//!    encoding and no metrics, and saying so once here is better than five
//!    `if`s later.
//! 3. **Split** by the hash of the primary key (§5).
//! 4. **Fit the encoding on the training rows only** (§6). Categories and
//!    standardisation constants computed over everything would leak the held-out
//!    rows into the fit, quietly, in the one place nobody looks.
//! 5. **Search the grid** on the validation rows, if the model declares any
//!    lists (§11), and keep the point with the best primary metric.
//! 6. **Fit the winner** on the training rows.
//! 7. **Score every split** by running the fitted state back over it — the
//!    host's job, not the provider's (§7).
//! 8. **Write the instance.**
//!
//! ## A fit is a job, and the row is the registry (§8)
//!
//! [`fit_model`] is the body of that job: the instance row already exists,
//! saying `fitting`, and the id has already been returned to whoever asked. So
//! this function's contract is *unusual on purpose* — **a fit that fails is
//! `Ok`**, carrying an instance whose status is `failed` and whose sentence says
//! why. An `Err` from it means the failure could not be *recorded*, which is a
//! different and much worse thing. A caller that treated "the optimiser did not
//! converge" and "the database is gone" the same way would leave rows saying
//! `fitting` for ever, which is the state boot has to reap.
//!
//! ## What the instance records beyond its columns
//!
//! Three things go in `attributes` rather than columns, by §9's rule — they are
//! present on some rows and not others, and nothing filters a list by them:
//! [`ATTR_OUTCOME`] (what this fit produces, so a prediction does not have to
//! re-read the dataset to find out), [`ATTR_ROWS`] (what the split came to and
//! what the encoding dropped) and [`ATTR_SEARCH`] (every grid point and its
//! score, so the search is inspectable and not a number that appeared).
//! [`ATTR_WARNINGS`] holds what the provider warned about, for any provider,
//! and a posterior's diagnostics when they say it should not be trusted as it
//! stands. A posterior adds two more: [`ATTR_PROGRESS`] while it runs, and
//! [`ATTR_CANCEL_REQUESTED`] when somebody asks it to stop.
//!
//! ## A posterior takes another road (Stan TODO §2)
//!
//! After the outcome is resolved, a [`Posterior`](Outcome::Posterior) leaves
//! the path above at step 3. There is no split (every row is data, and the
//! diagnostics are the draws'), no encoding (the program says what it wants,
//! variable by variable), no grid (it is sampled, not searched) and no scoring.
//! Instead: materialise the **related** datasets beside the main one, ask the
//! provider for the program's interface, **bind** the data to it, sample, and
//! keep the draws — which [`fit_model`] writes in the transaction that marks the
//! instance fitted. The summary, the diagnostics and the warnings are the
//! host's, computed from the draws before `exclude_variables` and `keep_draws`
//! decide which of them are kept (Stan TODO §15).

use std::collections::{BTreeMap, BTreeSet};

use sc_catalog::Catalog;
use sc_error::{Context, Error, Result};
use sc_types::Attrs;
use serde_json::Value as Json;

use crate::bind::{
    BindReport, Coordinates, Labeller, RecordedAxes, bind_data, binding_dataset,
    excluded_variables, keeps_draws, recorded_axes,
};
use crate::dataset::DatasetShape;
use crate::diagnose;
use crate::draws::{check_planned_draws, declared_elements, human_bytes, plan_draws, stored_bytes};
use crate::encode::{Encoded, Encoding, apply_encoding_dropping, fit_encoding};
use crate::frame::Frame;
use crate::instance::{InstanceId, ModelInstance};
use crate::instance_store::{require_model_instance, save_fitted_instance, save_model_instance};
use crate::metrics::{Metrics, SplitMetrics};
use crate::model::{MAIN_DATASET, Model};
use crate::posterior::{DrawSeries, FitContext, FitStage, PosteriorInput, Progress};
use crate::provider::{ModelProvider, Outcome, ParameterBlock};
use crate::registry::ModelRegistry;
use crate::source::DatasetSource;
use crate::split::{Part, SplitCounts};
use crate::validate::NO_POSTERIOR_SEARCH;

/// The attribute holding this fit's resolved [`Outcome`].
///
/// Stored because a prediction needs it and re-deriving it would mean reading
/// the dataset again just to learn its column types — and worse, would answer
/// with *today's* data rather than the data this instance was fitted over.
pub const ATTR_OUTCOME: &str = "outcome";

/// The attribute holding the row counts: what the dataset selected, what each
/// split came to, and what the encoding dropped.
pub const ATTR_ROWS: &str = "rows";

/// The attribute holding every hyperparameter point tried and what it scored.
pub const ATTR_SEARCH: &str = "search";

/// The attribute a running posterior fit's [`Progress`] is written to, at most
/// once a second, for the screen to poll (Stan TODO §13).
pub const ATTR_PROGRESS: &str = "progress";

/// The attribute `cancelModelFit` sets on a running instance (Stan TODO §13).
///
/// On the row rather than in memory for the reason the row is the job
/// registry: a cancel then works from any node, and the job reads it back each
/// time it writes its progress.
pub const ATTR_CANCEL_REQUESTED: &str = "cancel_requested";

/// The attribute holding a posterior's [`Coordinates`]: every dimension's keys
/// and labels as this instance numbered them (Stan TODO §8). Positions are the
/// instance's private business, so the only way to speak of `alpha[37]` outside
/// it is through these.
pub const ATTR_COORDINATES: &str = "coordinates";

/// The attribute holding a posterior's [`BindReport`]: rows read and bound,
/// what the policies dropped, and a line per bound variable (Stan TODO §10).
pub const ATTR_BINDING: &str = "binding";

/// The attribute holding a fit's warnings, as sentences that say what to do:
/// a posterior's diagnostics (Stan TODO §15) and whatever any provider
/// reported in [`FitResult::warnings`](crate::provider::FitResult). A fit
/// with warnings is still `fitted`, and is not [`fitted_cleanly`].
pub const ATTR_WARNINGS: &str = "warnings";

/// The attribute holding, for each output variable a posterior drew, its
/// shape and the dimension each axis is labelled by ([`RecordedAxes`]) —
/// decided once, at fit time, by the configuration the fit ran with. The draws
/// API, the summary on demand and the write-back label by these and this
/// instance's own coordinates, so editing the model's labels afterwards changes
/// the model and never an existing instance (Stan TODO §§8, 16).
pub const ATTR_AXES: &str = "axes";

/// One point of the hyperparameter grid and what it scored on the validation
/// rows (§11).
///
/// A point that *failed* is recorded with its sentence rather than dropped: "I
/// tried `k = 12` and it could not be fitted" is the answer to "why did it pick
/// `k = 8`", and a search that silently skipped its failures would look like a
/// search that never tried them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GridPoint {
    /// The values tried.
    pub hyperparameters: Attrs,
    /// The primary metric on the validation rows, where it fitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Why it did not fit, for a point that did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How many rows went where — what the instance reports and the screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RowCounts {
    /// Rows the dataset selected.
    pub selected: usize,
    /// Rows assigned to each split.
    #[serde(flatten)]
    pub split: SplitCounts,
    /// Rows the encoding could not represent and dropped, across all splits
    /// (§6). Reported rather than swallowed: a fit over 900 of 1 000 rows is a
    /// different claim from a fit over 1 000.
    pub dropped: usize,
}

/// Everything one fit produced, before it is a row.
#[derive(Debug, Clone, PartialEq)]
pub struct Fit {
    /// What this fit produces, with a classification's classes filled in.
    pub outcome: Outcome,
    /// The provider's serialised fit.
    pub state: Json,
    /// The provider's parameters.
    pub parameters: Vec<ParameterBlock>,
    /// The encoding, for everything but a hypothesis test.
    pub encoding: Option<Encoding>,
    /// The host's metrics, per split.
    pub metrics: SplitMetrics,
    /// The hyperparameter point this fit used — always values, never lists.
    pub hyperparameters: Attrs,
    /// Where the rows went.
    pub rows: RowCounts,
    /// Every grid point tried, empty when there was no search.
    pub search: Vec<GridPoint>,
    /// A posterior's draws, empty for every other outcome. Not written by
    /// [`apply`](Fit::apply): they go to `_fd_model_draws`, in the transaction
    /// that saves the instance ([`save_fitted_instance`]).
    pub draws: Vec<DrawSeries>,
    /// A posterior's binding: every dimension's coordinates and the report.
    /// Written to [`ATTR_COORDINATES`] and [`ATTR_BINDING`].
    pub binding: Option<(Coordinates, BindReport)>,
    /// The fit's warnings, as sentences: a posterior's diagnostics, or what
    /// the provider reported. Written to [`ATTR_WARNINGS`] when there are any.
    pub warnings: Vec<String>,
    /// A posterior's output variables: their shapes and what labels each
    /// axis. Written to [`ATTR_AXES`].
    pub axes: BTreeMap<String, RecordedAxes>,
}

impl Fit {
    /// This fit written onto `instance`: status, state, parameters, metrics,
    /// encoding, the chosen point, and the three attributes.
    pub fn apply(&self, mut instance: ModelInstance) -> Result<ModelInstance> {
        instance =
            crate::instance_store::fitted(instance, self.state.clone(), self.parameters.clone());
        instance.metrics = self.metrics.to_json()?;
        instance.encoding = match &self.encoding {
            Some(encoding) => encoding.to_json()?,
            None => Json::Null,
        };
        instance.hyperparameters = self.hyperparameters.clone();
        instance.attributes.insert(
            ATTR_OUTCOME.to_owned(),
            serde_json::to_value(&self.outcome).map_err(|e| Error::msg(format!("outcome: {e}")))?,
        );
        instance.attributes.insert(
            ATTR_ROWS.to_owned(),
            serde_json::to_value(self.rows).map_err(|e| Error::msg(format!("row counts: {e}")))?,
        );
        if let Some((coordinates, report)) = &self.binding {
            instance.attributes.insert(
                ATTR_COORDINATES.to_owned(),
                serde_json::to_value(coordinates)
                    .map_err(|e| Error::msg(format!("coordinates: {e}")))?,
            );
            instance.attributes.insert(
                ATTR_BINDING.to_owned(),
                serde_json::to_value(report)
                    .map_err(|e| Error::msg(format!("binding report: {e}")))?,
            );
        }
        if !self.axes.is_empty() {
            instance.attributes.insert(
                ATTR_AXES.to_owned(),
                serde_json::to_value(&self.axes).map_err(|e| Error::msg(format!("axes: {e}")))?,
            );
        }
        if !self.warnings.is_empty() {
            instance
                .attributes
                .insert(ATTR_WARNINGS.to_owned(), Json::from(self.warnings.clone()));
        }
        if !self.search.is_empty() {
            instance.attributes.insert(
                ATTR_SEARCH.to_owned(),
                serde_json::to_value(&self.search)
                    .map_err(|e| Error::msg(format!("search: {e}")))?,
            );
        }
        Ok(instance)
    }
}

/// Run the fit named by `instance` and record what happened — **the body of the
/// job** (§8).
///
/// The instance row already exists and says `fitting`. This loads it, runs the
/// fit, and saves it as `fitted` or as `failed` with the sentence. A fit that
/// fails is `Ok` carrying the failed instance; an `Err` means the failure could
/// not be *recorded*, which is the only kind of trouble a caller can do anything
/// about. See the module docs.
pub async fn fit_model(
    catalog: &Catalog,
    registry: &ModelRegistry,
    source: &dyn DatasetSource,
    model: &Model,
    instance: InstanceId,
    cap: u64,
) -> Result<ModelInstance> {
    fit_model_with(
        catalog,
        registry,
        source,
        model,
        instance,
        cap,
        &FitContext::detached(),
    )
    .await
}

/// [`fit_model`], reporting progress to and cancelled through `ctx` — what a
/// posterior fit, which is minutes long, is run with.
pub async fn fit_model_with(
    catalog: &Catalog,
    registry: &ModelRegistry,
    source: &dyn DatasetSource,
    model: &Model,
    instance: InstanceId,
    cap: u64,
    ctx: &FitContext<'_>,
) -> Result<ModelInstance> {
    let row = require_model_instance(catalog, instance).await?;
    let failed_to_record = |row: ModelInstance, e: &Error| {
        row.failed(format!("the fit finished but could not be recorded: {e}"))
    };
    let ctx = ctx.with_instance(instance);
    let finished = match run_fit_with(registry, source, model, cap, &ctx).await {
        Ok(fit) => match fit.apply(row.clone()) {
            // The draws and the row in one transaction: a fitted instance has
            // all of its draws, and a write that failed half way has none of
            // them and is recorded as the failure it is.
            Ok(finished) => match save_fitted_instance(catalog, &finished, &fit.draws).await {
                Ok(()) => return Ok(finished),
                Err(e) => {
                    // What the fit kept outside the database (a published raw
                    // run) belongs to an instance that will never be fitted.
                    if let Some(provider) = registry.get(model.provider.trim()) {
                        let _ = provider.discard(&fit.state).await;
                    }
                    failed_to_record(row, &e)
                }
            },
            // The fit itself worked and only writing it down did not — which is
            // still a failed instance, and the sentence should say which half
            // broke rather than pretending the optimiser was at fault.
            Err(e) => failed_to_record(row, &e),
        },
        // The **chain**, not just the outermost sentence: a fit fails at the
        // bottom of a stack of contexts ("counting the rows of dataset table
        // `houses`"), and the row is the only place the reason will ever be
        // read — so it carries the cause the context was wrapped around.
        Err(e) => row.failed(sc_error::format_chain(&e)),
    };
    save_model_instance(catalog, &finished).await?;
    Ok(finished)
}

/// Run a fit and answer what it produced, touching no store.
///
/// The half worth testing, and the half a caller with its own instance handling
/// wants: [`fit_model`] is this plus the row.
pub async fn run_fit(
    registry: &ModelRegistry,
    source: &dyn DatasetSource,
    model: &Model,
    cap: u64,
) -> Result<Fit> {
    run_fit_with(registry, source, model, cap, &FitContext::detached()).await
}

/// [`run_fit`] with a [`FitContext`], which only a posterior reads.
pub async fn run_fit_with(
    registry: &ModelRegistry,
    source: &dyn DatasetSource,
    model: &Model,
    cap: u64,
    ctx: &FitContext<'_>,
) -> Result<Fit> {
    let provider = registry.require(model.provider.trim())?;
    let frame = source.materialise(&model.dataset, cap).await?;
    if frame.rows == 0 {
        return Err(Error::invalid(
            "this dataset selects no rows, so there is nothing to fit",
        ));
    }
    let shape = DatasetShape::of_frame(model.table(), &frame);
    provider.validate(&shape, &model.configuration)?;
    let outcome = provider.outcome(&shape, &model.configuration)?;
    let points = grid(&model.hyperparameters)?;

    // Before `predicts()`: a posterior that names no prediction does not
    // predict, and is still not a hypothesis test.
    if outcome.is_posterior() {
        return fit_posterior(
            provider.as_ref(),
            source,
            model,
            frame,
            outcome,
            points,
            cap,
            ctx,
        )
        .await;
    }

    if !outcome.predicts() {
        return fit_test(provider.as_ref(), model, &frame, outcome, points).await;
    }

    let splits = frame.split(&model.split)?;
    let encoding = fit_encoding(&splits.train, &outcome, provider.standardise())?;
    let train = apply_encoding_dropping(&encoding, &splits.train)?;
    if train.is_empty() {
        return Err(Error::invalid(format!(
            "every one of the {} training rows was dropped by the encoding: they have a null in \
             a feature or in the label",
            splits.counts.train
        )));
    }
    let validation = apply_encoding_dropping(&encoding, &splits.validation)?;
    let test = apply_encoding_dropping(&encoding, &splits.test)?;
    // The classes are the *data's*, so the outcome only becomes complete once
    // the encoding has seen them.
    let outcome = with_classes(outcome, &encoding);

    let (chosen, search) = search_grid(
        provider.as_ref(),
        &model.configuration,
        &outcome,
        &train,
        &validation,
        points,
    )
    .await?;

    let result = provider
        .fit(&train.frame(), &model.configuration, &chosen)
        .await?;

    let mut metrics = SplitMetrics::default();
    for (part, encoded) in [
        (Part::Train, &train),
        (Part::Validation, &validation),
        (Part::Test, &test),
    ] {
        if encoded.is_empty() {
            continue;
        }
        let predictions = provider
            .predict(&result.state, &encoded.features_frame())
            .await?;
        metrics.set(part, Metrics::of(&outcome, &predictions, encoded)?);
    }

    Ok(Fit {
        outcome,
        state: result.state,
        parameters: result.parameters,
        encoding: Some(encoding),
        metrics,
        hyperparameters: chosen,
        rows: RowCounts {
            selected: frame.rows,
            split: splits.counts,
            dropped: train.dropped + validation.dropped + test.dropped,
        },
        search,
        draws: Vec::new(),
        binding: None,
        warnings: result.warnings,
        axes: BTreeMap::new(),
    })
}

/// A hypothesis test: the whole frame, unencoded, and the parameters are the
/// answer (§7, §13).
///
/// No split, because there is nothing to hold out from a test statistic; no
/// encoding, because a t-test's configuration names *this* column as the value
/// and *that* one as the group, and a one-hot would leave neither addressable;
/// no metrics, because the parameters are the result.
async fn fit_test(
    provider: &dyn ModelProvider,
    model: &Model,
    frame: &Frame,
    outcome: Outcome,
    points: Vec<Attrs>,
) -> Result<Fit> {
    if points.len() > 1 {
        return Err(Error::invalid(
            "a hypothesis test produces no per-row prediction, so there is nothing to score a \
             hyperparameter search against: give each hyperparameter one value",
        ));
    }
    let chosen = points.into_iter().next().unwrap_or_default();
    let result = provider.fit(frame, &model.configuration, &chosen).await?;
    Ok(Fit {
        outcome,
        state: result.state,
        parameters: result.parameters,
        encoding: None,
        metrics: SplitMetrics::default(),
        hyperparameters: chosen,
        rows: RowCounts {
            selected: frame.rows,
            split: SplitCounts {
                train: frame.rows,
                validation: 0,
                test: 0,
            },
            dropped: 0,
        },
        search: Vec::new(),
        draws: Vec::new(),
        binding: None,
        warnings: result.warnings,
        axes: BTreeMap::new(),
    })
}

/// A posterior (Stan TODO §2): every dataset, the program's interface, the data
/// bound to it, the provider's draws.
///
/// No split and no encoding — see the module docs. The main frame has already
/// been read (the outcome needed its shape); the related ones are read here,
/// each under the same row cap, each named in the sentence when it fails.
#[allow(clippy::too_many_arguments)]
async fn fit_posterior(
    provider: &dyn ModelProvider,
    source: &dyn DatasetSource,
    model: &Model,
    main: Frame,
    outcome: Outcome,
    points: Vec<Attrs>,
    cap: u64,
    ctx: &FitContext<'_>,
) -> Result<Fit> {
    if points.len() > 1 {
        return Err(Error::invalid(NO_POSTERIOR_SEARCH));
    }
    let chosen = points.into_iter().next().unwrap_or_default();
    let selected = main.rows;

    let mut datasets = Vec::with_capacity(1 + model.related.len());
    datasets.push((MAIN_DATASET.to_owned(), main));
    for related in &model.related {
        // Read with its label formula beside its columns, so the labels of its
        // rows are the row layer's answer (the binder takes the column back
        // out).
        let frame = source
            .materialise(&binding_dataset(related), cap)
            .await
            .with_context(|| format!("reading the related dataset `{}`", related.name))?;
        datasets.push((related.name.clone(), frame));
    }

    let config = &model.configuration;
    let limits = ctx.limits();
    let interface = provider.interface(config).await?;
    let bound = match &interface {
        Some(interface) => Some(bind_data(
            interface,
            config,
            &datasets,
            limits.max_data_values,
        )?),
        None => None,
    };
    let dropped = bound.as_ref().map_or(0, |b| b.report.dropped(MAIN_DATASET));
    let data = bound
        .as_ref()
        .map_or_else(|| Json::Object(serde_json::Map::new()), |b| b.json.clone());
    let coordinates = bound
        .as_ref()
        .map(|b| b.coordinates.clone())
        .unwrap_or_default();
    // Every size the program declares its outputs with is a bound integer.
    let sizes = |name: &str| data.get(name).and_then(Json::as_i64);

    // Before anything is compiled or sampled (Stan TODO §§14–15): the labels
    // fit, the draws fit, and which variables nobody will read.
    let excluded = excluded_variables(config)?;
    let keep = keeps_draws(config)?;
    let labeller = Labeller::new(config, &coordinates)?;
    let mut unread = BTreeSet::new();
    if let Some(interface) = &interface {
        labeller.check_lengths(interface, &sizes)?;
        if keep {
            if let Some(plan) = provider.draw_plan(config)? {
                check_planned_draws(
                    &plan_draws(interface, &sizes, &excluded, plan),
                    plan,
                    limits.max_draws_bytes,
                )?;
            }
        }
        // A generated quantity that is neither kept nor small enough to be
        // summarised need not even be read back.
        for decl in &interface.generated {
            let too_big = declared_elements(decl, &sizes)
                .is_some_and(|n| n > limits.summary_max_elements as u64);
            if too_big && (!keep || excluded.contains(&decl.name)) {
                unread.insert(decl.name.clone());
            }
        }
    }
    let input = PosteriorInput {
        model: model.name.clone(),
        instance: ctx.instance(),
        datasets,
        interface,
        data: data.clone(),
        coordinates: coordinates.clone(),
        unread,
    };

    let result = provider.fit_posterior(&input, config, ctx).await?;
    ctx.report(&Progress::stage(FitStage::Summarising));

    // The host's: the summary, the diagnostics and the warnings, from every
    // draw — before any of them is discarded.
    let report = diagnose::report(
        &result.draws,
        &result.run,
        input.interface.as_ref(),
        &labeller,
        limits.summary_max_elements,
    )?;
    let mut warnings = report.warnings;
    let axes = recorded_axes(&result.draws, input.interface.as_ref(), &labeller);
    let mut draws = result.draws;
    if keep {
        draws.retain(|s| !excluded.contains(&s.variable));
        // A size the plan could not evaluate is measured now. Over the limit,
        // the draws go and the summary stays: an hour of sampling is not
        // thrown away over its storage.
        let bytes = stored_bytes(&draws);
        if bytes > limits.max_draws_bytes {
            draws.clear();
            warnings.push(format!(
                "the draws came to about {}, over the limit of {} (`--stan-max-draws-bytes`), \
                 so they were not kept — the summary and the diagnostics were computed from them \
                 first; to keep them, thin them (`thin`) or leave the largest variables out \
                 (`exclude_variables`)",
                human_bytes(bytes),
                human_bytes(limits.max_draws_bytes)
            ));
        }
    } else {
        draws.clear();
    }
    let mut metrics = SplitMetrics::default();
    metrics.set(Part::Train, report.metrics);
    let mut parameters = report.tables;
    parameters.extend(result.parameters);

    Ok(Fit {
        outcome,
        state: result.state,
        parameters,
        encoding: None,
        metrics,
        hyperparameters: chosen,
        rows: RowCounts {
            selected,
            split: SplitCounts {
                train: selected - dropped,
                validation: 0,
                test: 0,
            },
            dropped,
        },
        search: Vec::new(),
        draws,
        binding: bound.map(|b| (b.coordinates, b.report)),
        warnings,
        axes,
    })
}

/// When a new fit becomes its model's active one — `fit_model`'s `activate`
/// (milestone 31 §2).
///
/// The same three words for every provider, because "clean" is: a fit is
/// clean when it is fitted and nothing warned, whether the warning was a
/// posterior's diagnostic or a provider's own ([`FitResult::warnings`](crate::
/// provider::FitResult)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Activation {
    /// Keep the new fit beside the active one, for the admin to activate.
    #[default]
    Never,
    /// Activate it when it is [`fitted_cleanly`].
    IfClean,
    /// Activate it whenever it is fitted, warnings or not. A failed fit is
    /// never activated: it cannot answer a prediction.
    Always,
}

impl Activation {
    /// Every setting, as stored.
    pub const ALL: [&'static str; 3] = ["never", "if_clean", "always"];

    /// The setting as stored.
    pub fn as_str(self) -> &'static str {
        match self {
            Activation::Never => "never",
            Activation::IfClean => "if_clean",
            Activation::Always => "always",
        }
    }

    /// A stored setting, refused by name when it is none of the three.
    pub fn parse(raw: &str) -> Result<Activation> {
        match raw.trim() {
            "never" => Ok(Activation::Never),
            "if_clean" => Ok(Activation::IfClean),
            "always" => Ok(Activation::Always),
            other => Err(Error::invalid(format!(
                "`activate` must be one of `never`, `if_clean` or `always`, not `{other}`"
            ))),
        }
    }

    /// Whether a finished `instance` becomes active under this setting.
    pub fn activates(self, instance: &ModelInstance) -> bool {
        match self {
            Activation::Never => false,
            Activation::IfClean => fitted_cleanly(instance),
            Activation::Always => instance.status == crate::instance::FitStatus::Fitted,
        }
    }
}

/// Starting a fit as a job, from below the layer that owns the jobs — the seam
/// the `fit_model` action reaches the server's fits through (Stan TODO 7.5),
/// as `DatasetSource` is the one a dataset is read through.
#[async_trait::async_trait]
pub trait FitStarter: Send + Sync {
    /// Write `instance` (saying `fitting`) and start fitting `model` into it,
    /// answering as soon as the row exists. When it finishes, the job makes
    /// the instance active if `activation` [`activates`](Activation::activates)
    /// it.
    async fn start_fit(
        &self,
        model: &Model,
        instance: ModelInstance,
        activation: Activation,
    ) -> Result<ModelInstance>;
}

/// Whether a finished instance may be made active by a fit that asked for it
/// only when clean: fitted, and without warnings.
pub fn fitted_cleanly(instance: &ModelInstance) -> bool {
    instance.status == crate::instance::FitStatus::Fitted
        && instance
            .attributes
            .get(ATTR_WARNINGS)
            .and_then(Json::as_array)
            .is_none_or(Vec::is_empty)
}

/// Pick the grid point that scores best on the validation rows (§11).
///
/// With one point there is no search and nothing is scored — the common case,
/// which must not pay for the uncommon one: a search costs one extra fit per
/// point, and doing it for a model with no lists would double every fit in the
/// system for nothing.
async fn search_grid(
    provider: &dyn ModelProvider,
    config: &Attrs,
    outcome: &Outcome,
    train: &Encoded,
    validation: &Encoded,
    points: Vec<Attrs>,
) -> Result<(Attrs, Vec<GridPoint>)> {
    if points.len() < 2 {
        return Ok((points.into_iter().next().unwrap_or_default(), Vec::new()));
    }
    if validation.is_empty() {
        return Err(Error::invalid(
            "this model searches over a list of hyperparameters, but its split holds out no \
             validation rows to score the points on: give the split a validation fraction",
        ));
    }
    let mut search = Vec::with_capacity(points.len());
    for point in points {
        let scored = match score_point(provider, config, outcome, train, validation, &point).await {
            Ok(Some(score)) if score.is_finite() => GridPoint {
                hyperparameters: point,
                score: Some(score),
                error: None,
            },
            // A point that fitted but scored nothing comparable — an R² over a
            // label that is constant on the validation rows, say. Recorded with
            // the reason rather than as a score of 0, which would be a number
            // the search could rank.
            Ok(_) => GridPoint {
                hyperparameters: point,
                score: None,
                error: Some(UNSCORABLE.to_owned()),
            },
            Err(e) => GridPoint {
                hyperparameters: point,
                score: None,
                error: Some(e.to_string()),
            },
        };
        search.push(scored);
    }
    let best = search
        .iter()
        .filter(|p| p.score.is_some_and(f64::is_finite))
        .max_by(|a, b| {
            a.score
                .unwrap_or(f64::NEG_INFINITY)
                .total_cmp(&b.score.unwrap_or(f64::NEG_INFINITY))
        });
    match best {
        Some(point) => Ok((point.hyperparameters.clone(), search)),
        // Every point failed. The first sentence is the useful one — they are
        // usually the same failure — and reporting "no point could be scored"
        // alone would hide it.
        None => Err(Error::invalid(format!(
            "no point of this hyperparameter grid could be fitted and scored; the first said: {}",
            search
                .first()
                .and_then(|p| p.error.clone())
                .unwrap_or_else(|| "nothing".to_owned())
        ))),
    }
}

/// Fit one grid point on the training rows and score it on the validation rows.
async fn score_point(
    provider: &dyn ModelProvider,
    config: &Attrs,
    outcome: &Outcome,
    train: &Encoded,
    validation: &Encoded,
    point: &Attrs,
) -> Result<Option<f64>> {
    let result = provider.fit(&train.frame(), config, point).await?;
    let predictions = provider
        .predict(&result.state, &validation.features_frame())
        .await?;
    Ok(Metrics::of(outcome, &predictions, validation)?.primary())
}

/// The outcome with a classification's classes filled in from the fitted
/// encoding.
///
/// The classes are the *data's* — no amount of reading the configuration
/// discovers them — so this is the one moment they become known.
fn with_classes(outcome: Outcome, encoding: &Encoding) -> Outcome {
    match outcome {
        Outcome::Classification { label, .. } => Outcome::Classification {
            label,
            classes: encoding.classes().map(<[String]>::to_vec),
        },
        other => other,
    }
}

/// The hyperparameter grid: every combination of the lists, with the scalars
/// held fixed (§11).
///
/// A list of one and a scalar are the same search, so nothing downstream has to
/// ask which shape a setting was typed in. The product is bounded by
/// [`MAX_GRID_POINTS`] because a fit per point is minutes each and four
/// six-element lists is 1 296 of them — a number nobody typed on purpose.
pub fn grid(hyperparameters: &Attrs) -> Result<Vec<Attrs>> {
    let mut points = vec![Attrs::new()];
    for (key, value) in hyperparameters {
        let values: Vec<&Json> = match value {
            Json::Array(values) if values.is_empty() => {
                return Err(Error::invalid(format!(
                    "hyperparameter `{key}` is an empty list, so there is nothing to search over"
                )));
            }
            Json::Array(values) => values.iter().collect(),
            single => vec![single],
        };
        if points.len().saturating_mul(values.len()) > MAX_GRID_POINTS {
            return Err(Error::invalid(format!(
                "this hyperparameter grid has more than {MAX_GRID_POINTS} points, and each one \
                 is a fit: shorten the lists"
            )));
        }
        points = points
            .into_iter()
            .flat_map(|point| {
                values.iter().map(move |value| {
                    let mut next = point.clone();
                    next.insert(key.clone(), (*value).clone());
                    next
                })
            })
            .collect();
    }
    Ok(points)
}

/// Why a grid point that fitted still could not be ranked.
const UNSCORABLE: &str = "this point produced no comparable score on the validation rows";

/// The most grid points a fit will run. A fit per point, and each is seconds at
/// best — see [`grid`].
pub const MAX_GRID_POINTS: usize = 200;

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use sc_expr::SchemaShape;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::dataset::Dataset;
    use crate::frame::Column;
    use crate::provider::{FitResult, OutcomeSpec, Prediction};
    use crate::split::Split;
    use sc_types::{BasicType, FormField};

    /// A deterministic provider that predicts the mean of its training label,
    /// shifted by the `bias` hyperparameter.
    ///
    /// Enough to test the orchestration and nothing more: the grid can only pick
    /// the right point if the scoring pass is wired to the validation rows, and
    /// the mean can only be right if the label reached the fit.
    struct Mean {
        fits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelProvider for Mean {
        fn name(&self) -> &str {
            "mean"
        }

        fn description(&self) -> &str {
            "predicts the training mean"
        }

        fn config_declaration(&self) -> Vec<FormField> {
            vec![crate::provider::numeric_column_field("label", "Label")]
        }

        fn hyperparameters(&self) -> Vec<FormField> {
            vec![FormField::new("bias", BasicType::Float)]
        }

        fn outcome_spec(&self) -> OutcomeSpec {
            OutcomeSpec::Regression {
                label: "label".to_owned(),
            }
        }

        async fn fit(&self, frame: &Frame, config: &Attrs, hyper: &Attrs) -> Result<FitResult> {
            self.fits.fetch_add(1, Ordering::SeqCst);
            let label = config.get("label").and_then(Json::as_str).unwrap_or("");
            let Some(Column::Float(values)) = frame.column(label) else {
                return Err(Error::msg(format!(
                    "no label column `{label}` in the fit frame"
                )));
            };
            let n = values.len() as f64;
            let mean = values.iter().flatten().sum::<f64>() / n;
            let bias = hyper.get("bias").and_then(Json::as_f64).unwrap_or(0.0);
            let mut result = FitResult::new(serde_json::json!({ "prediction": mean + bias }))
                .parameter(ParameterBlock::scalar("mean", mean));
            // A configured `warn` is reported, the way sklearn's
            // `ConvergenceWarning` is by a Python provider.
            if let Some(warning) = config.get("warn").and_then(Json::as_str) {
                result = result.warning(warning);
            }
            Ok(result)
        }

        async fn predict(&self, state: &Json, frame: &Frame) -> Result<Vec<Prediction>> {
            let value = state
                .get("prediction")
                .and_then(Json::as_f64)
                .ok_or_else(|| Error::msg("no fitted mean in the state".to_owned()))?;
            Ok(vec![Prediction::number(value); frame.rows])
        }
    }

    use crate::source::Read;

    /// A source that answers one fixed frame — the seam, stubbed.
    struct Fixed(Frame);

    #[async_trait]
    impl DatasetSource for Fixed {
        async fn read(&self, _ds: &Dataset, _how: &Read<'_>) -> Result<Frame> {
            Ok(self.0.clone())
        }
    }

    /// `n` rows whose `x` is the row number and whose `y` cycles through 0..7.
    ///
    /// The label has to *vary*: R² over a constant label is undefined, and a
    /// grid that cannot rank its points is a different test from this one.
    fn rows(n: usize) -> Frame {
        Frame::new(
            vec![
                (
                    "x".to_owned(),
                    Column::Float((0..n).map(|i| Some(i as f64)).collect()),
                ),
                (
                    "y".to_owned(),
                    Column::Float((0..n).map(|i| Some((i % 7) as f64)).collect()),
                ),
            ],
            (0..n).map(|i| format!("int:{i}")).collect(),
        )
        .expect("frame")
    }

    fn registry(fits: &Arc<AtomicUsize>) -> ModelRegistry {
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(Mean {
                fits: Arc::clone(fits),
            }))
            .expect("register");
        registry
    }

    fn model() -> Model {
        Model::new(
            "m",
            "mean",
            Dataset::new("t").column("x", "x").column("y", "y"),
        )
        .config("label", "y")
    }

    #[tokio::test]
    async fn a_fit_with_no_search_fits_once_and_scores_every_split_it_has() {
        let fits = Arc::new(AtomicUsize::new(0));
        let fit = run_fit(
            &registry(&fits),
            &Fixed(rows(200)),
            &model().split(Split::new(0.8, 0.0, 0.2, 7)),
            1000,
        )
        .await
        .expect("fit");
        // One fit, because there is no grid: the common case does not pay for
        // the uncommon one.
        assert_eq!(fits.load(Ordering::SeqCst), 1);
        assert!(fit.search.is_empty());
        assert_eq!(fit.rows.selected, 200);
        assert_eq!(fit.rows.split.train + fit.rows.split.test, 200);
        assert_eq!(fit.rows.split.validation, 0);
        assert!(fit.metrics.train.is_some());
        assert!(fit.metrics.test.is_some());
        assert!(fit.metrics.validation.is_none());
        // The label reached the fit: the mean of a column cycling 0..7 is in it.
        let [ParameterBlock::Scalar { name, value }] = fit.parameters.as_slice() else {
            panic!("expected one scalar, got {:?}", fit.parameters);
        };
        assert_eq!(name, "mean");
        assert!((0.0..=6.0).contains(value), "{value}");
        // The encoding is fitted over the features and not the label.
        let encoding = fit.encoding.expect("encoding");
        assert_eq!(encoding.feature_names(), vec!["x"]);
        assert_eq!(encoding.target.expect("target").column, "y".to_owned());
    }

    #[tokio::test]
    async fn a_providers_warnings_are_the_instances_and_a_clean_fit_has_none() {
        let fits = Arc::new(AtomicUsize::new(0));
        let sentence = "the optimiser stopped without converging: raise `max_iter`";
        let warned = run_fit(
            &registry(&fits),
            &Fixed(rows(100)),
            &model().config("warn", sentence),
            1000,
        )
        .await
        .expect("fit");
        assert_eq!(warned.warnings, vec![sentence.to_owned()]);
        let instance = warned
            .apply(ModelInstance::starting(crate::model::ModelId::new()))
            .expect("apply");
        assert_eq!(
            instance.attributes.get(ATTR_WARNINGS),
            Some(&serde_json::json!([sentence]))
        );
        assert!(!fitted_cleanly(&instance));

        let clean = run_fit(&registry(&fits), &Fixed(rows(100)), &model(), 1000)
            .await
            .expect("fit");
        assert!(clean.warnings.is_empty());
        let instance = clean
            .apply(ModelInstance::starting(crate::model::ModelId::new()))
            .expect("apply");
        assert!(!instance.attributes.contains_key(ATTR_WARNINGS));
        assert!(fitted_cleanly(&instance));
    }

    #[tokio::test]
    async fn the_grid_picks_the_point_that_scores_best_on_the_validation_rows() {
        let fits = Arc::new(AtomicUsize::new(0));
        let model = model()
            .split(Split::new(0.6, 0.2, 0.2, 7))
            .hyperparameter("bias", serde_json::json!([0.0, 5.0, -3.0]));
        let fit = run_fit(&registry(&fits), &Fixed(rows(300)), &model, 1000)
            .await
            .expect("fit");
        // A bias of 0 predicts the truth exactly, so it wins.
        assert_eq!(fit.hyperparameters.get("bias"), Some(&Json::from(0.0)));
        assert_eq!(fit.search.len(), 3);
        assert!(fit.search.iter().all(|p| p.error.is_none()));
        // Three scoring fits plus the winner's refit.
        assert_eq!(fits.load(Ordering::SeqCst), 4);
        assert!(fit.metrics.validation.is_some());
    }

    #[tokio::test]
    async fn a_search_with_no_validation_rows_is_refused_rather_than_scored_on_the_training_ones() {
        let fits = Arc::new(AtomicUsize::new(0));
        let model = model()
            .split(Split::new(0.8, 0.0, 0.2, 7))
            .hyperparameter("bias", serde_json::json!([0.0, 5.0]));
        let err = run_fit(&registry(&fits), &Fixed(rows(100)), &model, 1000)
            .await
            .expect_err("no validation rows");
        assert!(err.to_string().contains("validation rows"), "{err}");
    }

    #[tokio::test]
    async fn the_encoding_is_fitted_on_train_and_a_test_only_category_is_dropped_and_counted() {
        // `region` is `north` on every training row and `west` on exactly one
        // row that the seed puts in the test set.
        let n = 200;
        let split = Split::new(0.8, 0.0, 0.2, 7);
        let regions: Vec<Option<String>> = (0..n)
            .map(|i| {
                let key = format!("int:{i}");
                Some(if split.assign(&key) == Part::Test && i % 37 == 0 {
                    "west".to_owned()
                } else {
                    "north".to_owned()
                })
            })
            .collect();
        let unseen = regions.iter().flatten().filter(|r| *r == "west").count();
        assert!(
            unseen > 0,
            "the fixture needs at least one test-only region"
        );
        let mut frame = rows(n);
        frame
            .columns
            .push(("region".to_owned(), Column::Str(regions)));
        let fits = Arc::new(AtomicUsize::new(0));
        let fit = run_fit(&registry(&fits), &Fixed(frame), &model().split(split), 1000)
            .await
            .expect("fit");
        // Fitted on the training rows only, so `west` is not in the encoding.
        let encoding = fit.encoding.as_ref().expect("encoding");
        assert_eq!(encoding.feature_names(), vec!["x"]);
        // And the test rows carrying it were dropped, and counted.
        assert_eq!(fit.rows.dropped, unseen);
        assert_eq!(
            fit.metrics.test.as_ref().map(Metrics::rows),
            Some(fit.rows.split.test - unseen)
        );
    }

    #[tokio::test]
    async fn a_dataset_that_selects_no_rows_is_refused_by_name() {
        let fits = Arc::new(AtomicUsize::new(0));
        let err = run_fit(&registry(&fits), &Fixed(rows(0)), &model(), 1000)
            .await
            .expect_err("no rows");
        assert!(err.to_string().contains("selects no rows"), "{err}");
    }

    #[test]
    fn the_grid_is_the_product_of_the_lists_with_the_scalars_held_fixed() {
        let mut hyper = Attrs::new();
        hyper.insert("k".to_owned(), serde_json::json!([2, 3]));
        hyper.insert("seed".to_owned(), Json::from(1));
        hyper.insert("d".to_owned(), serde_json::json!(["a", "b"]));
        let points = grid(&hyper).expect("grid");
        assert_eq!(points.len(), 4);
        assert!(points.iter().all(|p| p.get("seed") == Some(&Json::from(1))));
        assert!(
            points
                .iter()
                .any(|p| p.get("k") == Some(&Json::from(3)) && p.get("d") == Some(&Json::from("b")))
        );
        // No lists at all is one point, not zero.
        assert_eq!(grid(&Attrs::new()).expect("grid").len(), 1);
    }

    #[test]
    fn a_grid_nobody_typed_on_purpose_is_refused() {
        let mut hyper = Attrs::new();
        for key in ["a", "b", "c", "d"] {
            hyper.insert(key.to_owned(), serde_json::json!([1, 2, 3, 4, 5, 6]));
        }
        let err = grid(&hyper).expect_err("too big");
        assert!(err.to_string().contains("each one"), "{err}");
    }

    #[tokio::test]
    async fn a_classification_learns_its_classes_from_the_data_and_records_them() {
        // The stub is a regressor, so this exercises `with_classes` directly:
        // the classes are the encoding's, and no configuration discovers them.
        let frame = Frame::new(
            vec![
                ("x".to_owned(), Column::Float(vec![Some(1.0), Some(2.0)])),
                (
                    "sold".to_owned(),
                    Column::Str(vec![Some("no".into()), Some("yes".into())]),
                ),
            ],
            vec!["int:1".to_owned(), "int:2".to_owned()],
        )
        .expect("frame");
        let encoding = fit_encoding(
            &frame,
            &Outcome::Classification {
                label: "sold".to_owned(),
                classes: None,
            },
            false,
        )
        .expect("encoding");
        let outcome = with_classes(
            Outcome::Classification {
                label: "sold".to_owned(),
                classes: None,
            },
            &encoding,
        );
        assert_eq!(
            outcome,
            Outcome::Classification {
                label: "sold".to_owned(),
                classes: Some(vec!["no".to_owned(), "yes".to_owned()]),
            }
        );
    }

    #[tokio::test]
    async fn a_fit_writes_its_outcome_and_row_counts_onto_the_instance() {
        let fits = Arc::new(AtomicUsize::new(0));
        let model = model();
        let fit = run_fit(&registry(&fits), &Fixed(rows(50)), &model, 1000)
            .await
            .expect("fit");
        let instance = fit.apply(ModelInstance::starting(model.id)).expect("apply");
        assert!(instance.is_usable());
        assert_eq!(instance.attributes[ATTR_OUTCOME]["outcome"], "regression");
        assert_eq!(instance.attributes[ATTR_ROWS]["selected"], 50);
        assert!(!instance.attributes.contains_key(ATTR_SEARCH));
        assert!(!instance.encoding.is_null());
        assert!(!instance.metrics.is_null());
    }

    /// The dataset validation this crate already has is a separate concern from
    /// the fit; this only asserts that the fit reaches the provider's own check.
    #[tokio::test]
    async fn a_configuration_the_provider_refuses_stops_the_fit_before_it_reads_anything() {
        let fits = Arc::new(AtomicUsize::new(0));
        let model =
            Model::new("m", "mean", Dataset::new("t").column("y", "y")).config("label", "nope");
        let err = run_fit(&registry(&fits), &Fixed(rows(10)), &model, 1000)
            .await
            .expect_err("bad label");
        assert!(err.to_string().contains("`nope`"), "{err}");
        assert_eq!(fits.load(Ordering::SeqCst), 0);
        // And the schema-level validation is unchanged by any of this.
        let _ = SchemaShape::default();
    }

    /// A sampler that answers canned draws: `alpha[j]` for each row of the
    /// related `groups` dataset, in each of two chains, and `lp__`.
    ///
    /// Enough to test the orchestration: the draws can only be sized by the
    /// groups if the related dataset reached the provider, and the progress
    /// can only arrive if the context did.
    struct Sampler {
        interface: Option<Interface>,
        plan: Option<crate::posterior::DrawPlan>,
    }

    #[async_trait]
    impl ModelProvider for Sampler {
        fn name(&self) -> &str {
            "sampler"
        }
        fn description(&self) -> &str {
            "answers canned draws"
        }
        fn config_declaration(&self) -> Vec<FormField> {
            Vec::new()
        }
        fn hyperparameters(&self) -> Vec<FormField> {
            vec![FormField::new("bias", BasicType::Float)]
        }
        fn outcome_spec(&self) -> OutcomeSpec {
            OutcomeSpec::Posterior { prediction: None }
        }
        fn binds_data(&self) -> bool {
            true
        }
        async fn fit(&self, _f: &Frame, _c: &Attrs, _h: &Attrs) -> Result<FitResult> {
            Err(Error::msg("a sampler is never fitted".to_owned()))
        }
        async fn predict(&self, _s: &Json, _f: &Frame) -> Result<Vec<Prediction>> {
            Err(Error::msg("a sampler is never asked to predict".to_owned()))
        }
        async fn interface(&self, _config: &Attrs) -> Result<Option<Interface>> {
            Ok(self.interface.clone())
        }
        fn draw_plan(&self, _config: &Attrs) -> Result<Option<crate::posterior::DrawPlan>> {
            Ok(self.plan)
        }
        async fn fit_posterior(
            &self,
            input: &PosteriorInput,
            _config: &Attrs,
            ctx: &FitContext<'_>,
        ) -> Result<PosteriorResult> {
            ctx.report(&Progress::stage(FitStage::Sampling));
            let names: Vec<&str> = input.datasets.iter().map(|(n, _)| n.as_str()).collect();
            let groups = input
                .dataset("groups")
                .ok_or_else(|| Error::msg(format!("no `groups` among {names:?}")))?;
            let mut draws = Vec::new();
            for chain in 1..=2u32 {
                // Six draws a chain, drifting: enough for a split-R̂, and
                // one that says the chains have not mixed.
                for j in 1..=groups.rows {
                    let drift = (0..6).map(|i| j as f64 + 0.1 * f64::from(i)).collect();
                    draws.push(DrawSeries::new("alpha", vec![j], chain, drift));
                }
                let lp = (0..6).map(|i| -f64::from(i)).collect();
                draws.push(DrawSeries::new("lp__", vec![], chain, lp));
            }
            if self.interface.is_some() {
                for chain in 1..=2u32 {
                    let y = (0..6).map(|i| f64::from(i % 2)).collect();
                    draws.push(DrawSeries::new("y_rep", vec![1], chain, y));
                }
            }
            Ok(PosteriorResult {
                state: serde_json::json!({
                    "datasets": names,
                    "data": input.data,
                    "unread": input.unread,
                }),
                draws,
                parameters: Vec::new(),
                run: crate::posterior::PosteriorRun::default(),
            })
        }
    }

    use crate::interface::{Declaration, Element, Interface, SizeExpr};
    use crate::model::NamedDataset;
    use crate::posterior::{DrawSeries, NoProgress, PosteriorLimits, PosteriorResult};
    use crate::provider::OutcomeSpec as Spec;
    use std::sync::Mutex;

    /// A source that answers a different frame per table — `groups` has three
    /// rows and everything else fifty.
    struct ByTable;

    #[async_trait]
    impl DatasetSource for ByTable {
        async fn read(&self, ds: &Dataset, _how: &Read<'_>) -> Result<Frame> {
            match ds.table.as_str() {
                "groups" => Frame::new(
                    vec![(
                        "u".to_owned(),
                        Column::Float(vec![Some(0.1), Some(0.2), Some(0.3)]),
                    )],
                    vec!["int:1".into(), "int:2".into(), "int:3".into()],
                ),
                "missing" => Err(Error::invalid("no such table".to_owned())),
                _ => Ok(rows(50)),
            }
        }
    }

    /// Collects every report.
    #[derive(Default)]
    struct Reports(Mutex<Vec<FitStage>>);

    impl crate::posterior::FitProgress for Reports {
        fn report(&self, progress: &Progress) {
            self.0.lock().expect("lock").push(progress.stage);
        }
    }

    fn sampler(interface: Option<Interface>) -> ModelRegistry {
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(Sampler {
                interface,
                plan: None,
            }))
            .expect("register");
        registry
    }

    fn radon() -> Model {
        Model::new(
            "radon",
            "sampler",
            Dataset::new("homes").column("x", "x").column("y", "y"),
        )
        .related(NamedDataset::new(
            "groups",
            Dataset::new("groups").column("u", "u"),
        ))
    }

    #[tokio::test]
    async fn a_posterior_is_sampled_over_every_dataset_and_keeps_its_draws() {
        let reports = Reports::default();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let ctx = FitContext::new(&reports, &cancel);
        let fit = run_fit_with(&sampler(None), &ByTable, &radon(), 1000, &ctx)
            .await
            .expect("fit");

        assert_eq!(fit.outcome, Outcome::Posterior { prediction: None });
        // The main dataset first and under its reserved name, then the related
        // one under its own; and no program data declared means an empty data
        // object, not a refusal.
        assert_eq!(
            fit.state,
            serde_json::json!({ "datasets": ["main", "groups"], "data": {}, "unread": [] })
        );
        // Three groups × two chains of `alpha`, plus `lp__` per chain.
        assert_eq!(fit.draws.len(), 8);
        assert!(
            fit.draws
                .iter()
                .any(|d| d.label() == "alpha[3]" && d.chain == 2)
        );
        // No split, no encoding, no search: a posterior is none of those.
        assert!(fit.encoding.is_none());
        assert!(fit.search.is_empty());
        assert_eq!(fit.rows.selected, 50);
        assert_eq!(fit.rows.split.train, 50);
        // The provider's progress and then the host's.
        assert_eq!(
            *reports.0.lock().expect("lock"),
            vec![FitStage::Sampling, FitStage::Summarising]
        );
        // The instance records the outcome; the draws go elsewhere.
        let instance = fit
            .apply(ModelInstance::starting(radon().id))
            .expect("apply");
        assert_eq!(instance.attributes[ATTR_OUTCOME]["outcome"], "posterior");
        assert!(instance.encoding.is_null());
    }

    #[tokio::test]
    async fn a_posterior_refuses_a_grid_as_a_test_does() {
        let model = radon().hyperparameter("bias", serde_json::json!([0.0, 1.0]));
        let err = run_fit(&sampler(None), &ByTable, &model, 1000)
            .await
            .expect_err("a grid over a posterior");
        assert!(err.to_string().contains("sampled, not searched"), "{err}");
        // One value is not a search, and is fine.
        let model = radon().hyperparameter("bias", 0.0);
        run_fit(&sampler(None), &ByTable, &model, 1000)
            .await
            .expect("one point");
    }

    #[tokio::test]
    async fn a_related_dataset_that_cannot_be_read_is_named() {
        let model = radon().related(NamedDataset::new("gone", Dataset::new("missing")));
        let err = run_fit(&sampler(None), &ByTable, &model, 1000)
            .await
            .expect_err("unreadable related dataset");
        let said = sc_error::format_chain(&err);
        assert!(said.contains("related dataset `gone`"), "{said}");
    }

    fn grouped_program() -> Interface {
        Interface {
            data: vec![
                Declaration::new("N", Element::Int, vec![], "int"),
                Declaration::new("J", Element::Int, vec![], "int"),
                Declaration::new("u", Element::Real, vec![SizeExpr::var("J")], "vector[J]"),
            ],
            parameters: vec![Declaration::new(
                "alpha",
                Element::Real,
                vec![SizeExpr::var("J")],
                "vector[J]",
            )],
            generated: vec![Declaration::new(
                "y_rep",
                Element::Real,
                vec![SizeExpr::var("N")],
                "vector[N]",
            )],
            ..Interface::default()
        }
    }

    fn bound_radon() -> Model {
        radon().config(
            crate::bind::BINDINGS_KEY,
            serde_json::json!({
                "N": {"kind": "count", "dataset": "main"},
                "J": {"kind": "size", "dimension": "groups"},
                "u": {"kind": "column", "dataset": "groups", "column": "u"},
            }),
        )
    }

    #[tokio::test]
    async fn a_program_whose_data_is_unbound_is_refused_by_name() {
        let err = run_fit(&sampler(Some(grouped_program())), &ByTable, &radon(), 1000)
            .await
            .expect_err("no bindings");
        assert!(
            err.to_string().contains(
                "the data variables `N` (int), `J` (int), `u` (vector[J]) have no binding"
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_bound_program_is_handed_its_data_and_the_instance_keeps_the_coordinates() {
        let model = bound_radon();
        let fit = run_fit(&sampler(Some(grouped_program())), &ByTable, &model, 1000)
            .await
            .expect("fit");
        assert_eq!(
            fit.state["data"],
            serde_json::json!({ "N": 50, "J": 3, "u": [0.1, 0.2, 0.3] })
        );
        let instance = fit.apply(ModelInstance::starting(model.id)).expect("apply");
        let groups = &instance.attributes[ATTR_COORDINATES]["dimensions"][1];
        assert_eq!(groups["name"], "groups");
        assert_eq!(groups["keys"], serde_json::json!([1, 2, 3]));
        assert_eq!(
            instance.attributes[ATTR_BINDING]["datasets"][0],
            serde_json::json!({ "name": "main", "read": 50, "bound": 50 })
        );
    }

    #[tokio::test]
    async fn the_host_summarises_the_draws_by_label_and_scores_them() {
        let model = bound_radon();
        let fit = run_fit(&sampler(Some(grouped_program())), &ByTable, &model, 1000)
            .await
            .expect("fit");
        // One table per variable, the program's order, the groups' labels
        // (their keys, as `groups` has no label formula) first.
        let names: Vec<&str> = fit.parameters.iter().map(ParameterBlock::name).collect();
        assert_eq!(names, ["alpha", "y_rep"]);
        let ParameterBlock::Table { columns, rows, .. } = &fit.parameters[0] else {
            panic!("{:?}", fit.parameters[0]);
        };
        assert_eq!(columns[0], "groups");
        assert_eq!(columns[1], "mean");
        assert_eq!(rows[2].cells[0], serde_json::json!("3"));
        // alpha[3] runs 3.0 … 3.5 in both chains.
        let mean = rows[2].cells[1].as_f64().expect("mean");
        assert!((mean - 3.25).abs() < 1e-12, "{mean}");
        let Some(Metrics::Posterior(m)) = fit.metrics.get(Part::Train) else {
            panic!("{:?}", fit.metrics);
        };
        assert_eq!((m.chains, m.draws_per_chain), (2, 6));
        // Chains that drift disagree with their own halves: said, and still
        // fitted.
        assert!(m.max_rhat > 1.01, "{m:?}");
        assert!(
            fit.warnings.iter().any(|w| w.contains("for `alpha[")),
            "{:?}",
            fit.warnings
        );
        let instance = fit.apply(ModelInstance::starting(model.id)).expect("apply");
        assert!(instance.is_usable());
        assert!(instance.attributes[ATTR_WARNINGS].as_array().is_some());
        assert_eq!(instance.metrics["train"]["metrics"], "posterior");
    }

    #[tokio::test]
    async fn excluded_variables_and_keep_draws_decide_what_is_kept_but_not_what_is_summarised() {
        let excluded = bound_radon().config(
            crate::bind::EXCLUDE_VARIABLES_KEY,
            serde_json::json!(["y_rep"]),
        );
        let fit = run_fit(&sampler(Some(grouped_program())), &ByTable, &excluded, 1000)
            .await
            .expect("fit");
        assert!(fit.draws.iter().all(|d| d.variable != "y_rep"));
        assert!(fit.draws.iter().any(|d| d.variable == "alpha"));
        assert!(fit.parameters.iter().any(|p| p.name() == "y_rep"));
        // Small enough to summarise, so still read.
        assert_eq!(fit.state["unread"], serde_json::json!([]));

        let summary_only = bound_radon().config(crate::bind::KEEP_DRAWS_KEY, false);
        let fit = run_fit(
            &sampler(Some(grouped_program())),
            &ByTable,
            &summary_only,
            1000,
        )
        .await
        .expect("fit");
        assert!(fit.draws.is_empty());
        assert_eq!(fit.parameters.len(), 2);
        assert!(fit.metrics.get(Part::Train).is_some());

        // `y_rep` has N = 50 elements: over a summary cap of 10, and neither
        // kept nor summarised, so the provider is told it need not read it.
        let reports = Reports::default();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let ctx = FitContext::new(&reports, &cancel).with_limits(PosteriorLimits {
            summary_max_elements: 10,
            ..PosteriorLimits::default()
        });
        let fit = run_fit_with(
            &sampler(Some(grouped_program())),
            &ByTable,
            &summary_only,
            1000,
            &ctx,
        )
        .await
        .expect("fit");
        assert_eq!(fit.state["unread"], serde_json::json!(["y_rep"]));
    }

    #[tokio::test]
    async fn draws_over_the_limit_are_refused_before_sampling_or_dropped_after() {
        let mut registry = ModelRegistry::new();
        registry
            .register(Arc::new(Sampler {
                interface: Some(grouped_program()),
                plan: Some(crate::posterior::DrawPlan {
                    chains: 4,
                    draws_per_chain: 1000,
                    sampler_variables: 7,
                }),
            }))
            .expect("register");
        let limited = |bytes| PosteriorLimits {
            max_draws_bytes: bytes,
            ..PosteriorLimits::default()
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let ctx = FitContext::new(&NoProgress, &cancel).with_limits(limited(100_000));
        // 7 + 3 + 50 elements × 4 chains × 1 000 draws is about 2.9 MB.
        let err = run_fit_with(&registry, &ByTable, &bound_radon(), 1000, &ctx)
            .await
            .expect_err("over the limit");
        assert!(
            err.to_string()
                .contains("4 chains × 1 000 draws × 60 elements"),
            "{err}"
        );
        // Keeping only the summary is always within it.
        let summary_only = bound_radon().config(crate::bind::KEEP_DRAWS_KEY, false);
        run_fit_with(&registry, &ByTable, &summary_only, 1000, &ctx)
            .await
            .expect("nothing stored");

        // A provider with no plan is measured afterwards: the canned draws
        // (10 series of 6) are about 1.9 kB, so a limit of 1 kB drops them and
        // says so, and the summary stays.
        let ctx = FitContext::new(&NoProgress, &cancel).with_limits(limited(1_000));
        let fit = run_fit_with(
            &sampler(Some(grouped_program())),
            &ByTable,
            &bound_radon(),
            1000,
            &ctx,
        )
        .await
        .expect("fitted without its draws");
        assert!(fit.draws.is_empty());
        assert!(!fit.parameters.is_empty());
        assert!(
            fit.warnings
                .iter()
                .any(|w| w.contains("so they were not kept")),
            "{:?}",
            fit.warnings
        );
    }

    #[tokio::test]
    async fn labels_that_do_not_fit_the_bound_sizes_are_refused_before_sampling() {
        let model = bound_radon().config(
            crate::bind::LABELS_KEY,
            serde_json::json!({ "alpha": ["main"] }),
        );
        let err = run_fit(&sampler(Some(grouped_program())), &ByTable, &model, 1000)
            .await
            .expect_err("alpha has J = 3 positions, main has 50");
        assert!(
            err.to_string()
                .contains("with `main`, which has 50 positions, but `J` is 3 here"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_provider_that_is_not_a_sampler_refuses_to_sample() {
        let fits = Arc::new(AtomicUsize::new(0));
        let mean = Mean { fits };
        let input = PosteriorInput {
            model: "m".into(),
            instance: None,
            datasets: Vec::new(),
            interface: None,
            data: Json::Null,
            coordinates: Default::default(),
            unread: Default::default(),
        };
        let err = mean
            .fit_posterior(&input, &Attrs::new(), &FitContext::detached())
            .await
            .expect_err("not a sampler");
        assert!(
            err.to_string().contains("does not sample a posterior"),
            "{err}"
        );
        assert!(!mean.kind().binds_data);
        let sampler = Sampler {
            interface: None,
            plan: None,
        };
        assert!(sampler.kind().binds_data);
        assert_eq!(sampler.outcome_spec(), Spec::Posterior { prediction: None });
    }
}

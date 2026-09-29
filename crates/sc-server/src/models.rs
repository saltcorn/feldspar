//! Predictive models: the pieces `sc-model` declares but cannot supply, and the
//! services a running server holds them in (TODO "Predictive models", §4, §8).
//!
//! `sc-model` is at layer 6 so a module can supply a model provider — the same
//! placement argument `sc-action` carries — and the price is that it cannot read
//! a row: `sc_api::rows` is layer 8. So it declares
//! [`DatasetSource`](sc_model::DatasetSource) and this is where the seam is
//! filled in, exactly as `sc-agent` declares `ProviderConnector` and
//! [`agents`](crate::agents) supplies it.
//!
//! Reading **through the row layer** rather than around it is the whole point of
//! the seam. A dataset that issued its own `SELECT` would see no non-stored
//! calculated field, would ignore ownership and row-level security, and could
//! not read a table a module provides at all — three ways for a fit to be
//! computed over rows that are not the rows the application has.
//!
//! [`ModelServices`] is the assembly [`AgentServices`](crate::AgentServices) and
//! the trigger dispatcher already are: the registry of providers, the source, the
//! row cap this process was started with, and the one method that **starts a
//! fit** ([`ModelServices::start_fit`]). It rides on
//! [`AppMounts`](crate::AppMounts) with the other four for the reason they do —
//! the admin handlers and the action registry both already hold that handle.
//!
//! ## A fit is a job, and the row is the registry (§8)
//!
//! [`start_fit`](ModelServices::start_fit) writes the instance row saying
//! `fitting`, returns it, and spawns the work. There is no in-memory job
//! registry, so nothing survives a restart — which is why
//! [`install_models`] reaps every row still `fitting` at boot rather than
//! leaving an admin looking at a fit in progress that is not.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use sc_api::rows::{RowQuery, count_rows_where, list_row_values};
use sc_catalog::Catalog;
use sc_error::{Context, Error, Result};
use sc_model::{
    Activation, Column, Dataset, DatasetSource, FitContext, FitProgress, Frame, InstanceId, Model,
    ModelInstance, ModelProvider, ModelRegistry, PosteriorLimits, Progress, ProgressWrite, Read,
    SPLIT_KEY, bootstrap_model_draws, bootstrap_model_instances, bootstrap_models,
    builtin_registry, canonical_key, fit_model_with, reap_fitting_instances, record_fit_progress,
    save_model_instance,
};
use sc_query::{Expr, Projection, Value};
use sc_stan::StanProvider;
use sc_stan::cmdstan::{Locations, Toolchain, discover};
use sc_stan::compile::CompileCache;
use sc_stan::run::ProcessBudget;

/// The [`DatasetSource`] a running server has: the catalog, read through
/// `sc_api::rows`.
pub struct CatalogDatasetSource {
    catalog: Arc<Catalog>,
}

impl CatalogDatasetSource {
    /// A source over `catalog`.
    pub fn new(catalog: Arc<Catalog>) -> CatalogDatasetSource {
        CatalogDatasetSource { catalog }
    }
}

#[async_trait]
impl DatasetSource for CatalogDatasetSource {
    async fn read(&self, ds: &Dataset, how: &Read<'_>) -> Result<Frame> {
        let table = self.catalog.require(&ds.table)?;
        let shape = self.catalog.schema_shape()?;
        // The dataset's own filter, and the caller's restriction anded onto it:
        // a prediction about one row is that row's primary key in the `WHERE`,
        // not a full read filtered afterwards, because "afterwards" would mean
        // materialising the whole table to answer about one row of it.
        //
        // A prediction reads `unfiltered`, and that is deliberate: the dataset's
        // filter says which rows the model was *fitted from*, not which rows it
        // may be asked about. A model of what houses sell for is fitted on the
        // sold ones and asked about the unsold one a trigger just inserted.
        let own = if how.filtered {
            ds.filter_expr(&shape)?
        } else {
            None
        };
        let filter = match (own, how.restrict) {
            (Some(own), Some(extra)) => Some(own.and(extra.clone())),
            (Some(own), None) => Some(own),
            (None, Some(extra)) => Some(extra.clone()),
            (None, None) => None,
        };

        // The count first, and on purpose: a dataset is a `SELECT` an admin
        // wrote and the server has to hold the answer in memory, so the refusal
        // costs one `COUNT(*)` rather than a partial read that has already
        // allocated most of what it would have refused.
        //
        // A **limited** read skips it, because it is bounded by construction:
        // the preview screen exists to show an admin the dataset that is too
        // big to fit, and refusing it for being too big would refuse the one
        // answer they came for.
        if how.limit.is_none() {
            let count = count_rows_where(&self.catalog, &table, filter.clone(), None)
                .await
                .with_context(|| format!("counting the rows of dataset table `{}`", ds.table))?;
            if count > 0 && how.cap < count as u64 {
                return Err(Error::invalid(format!(
                    "the dataset selects more than {} rows (it selects {count}); \
                     add a filter or raise `--model-max-rows`",
                    how.cap
                )));
            }
        }

        // The split key rides along as a reserved projection rather than as the
        // primary-key column's own name: a dataset column may legitimately be
        // *called* `id` while computing something else, and a split that hashed
        // that would be a split over the wrong thing. A table with a composite
        // or absent primary key simply has no key column — reads are unaffected,
        // and only the split refuses (§5).
        let mut projections = ds.projections(&shape)?;
        let key_column = ds.primary_key(&shape).ok();
        if let Some(pk) = &key_column {
            projections.push(Projection::expr_as(
                Expr::qcol(ds.table.clone(), pk.clone()),
                SPLIT_KEY,
            ));
        }

        // The dataset's order, then its primary key, on every read — a fit, a
        // preview and a prediction alike. A posterior needs it (the same seed
        // over the same rows in another order is another set of draws, Stan
        // TODO §7), and a hash-split provider is indifferent to it.
        // `sql_only`: the dataset's columns are its own projections, and a
        // calculated field of the table that predicts with this very model
        // would otherwise read this dataset to compute itself.
        let mut query = RowQuery::new()
            .where_(filter)
            .projecting(projections)
            .order_by(ds.order_by(&shape)?)
            .sql_only();
        if how.limit.is_some() {
            // Never above the cap, even when the caller asked for more: the cap
            // is what this process can hold, and a limit is what this caller
            // wants.
            query = query.limit(how.ceiling());
        }
        let rows = list_row_values(&self.catalog, &table, &query, None)
            .await
            .with_context(|| format!("reading dataset table `{}`", ds.table))?;

        // A frame of no rows is a real answer — a filter that matched nothing —
        // and it keeps its columns: a frame with none would fail later as a
        // shape error rather than here as an empty fit. There is also nothing to
        // check translation against, since a column is "present" only by having
        // arrived on some row.
        if rows.is_empty() {
            return Frame::new(
                ds.columns
                    .iter()
                    .map(|c| (c.name.clone(), Column::Null(0)))
                    .collect(),
                Vec::new(),
            );
        }

        let mut columns = Vec::with_capacity(ds.columns.len());
        let mut arrived: BTreeSet<&str> = BTreeSet::new();
        for column in &ds.columns {
            // A column absent from every row is one whose formula did not
            // translate: `Dataset::projections` leaves those to the reified
            // evaluator, which the read path does not yet run.
            if rows.iter().all(|row| !row.contains_key(&column.name)) {
                continue;
            }
            arrived.insert(column.name.as_str());
            columns.push((
                column.name.clone(),
                Column::from_values(
                    rows.iter()
                        .map(|row| row.get(&column.name).cloned().unwrap_or(Value::Null))
                        .collect(),
                ),
            ));
        }
        // Saying which column and why, rather than handing back a frame that is
        // quietly missing it — a fit over the columns that happened to arrive
        // would not be the fit the model asked for.
        let missing = ds.missing(&arrived);
        if !missing.is_empty() {
            return Err(Error::invalid(format!(
                "dataset on `{}`: the formula for {} does not translate to SQL, and the \
                 dataset read has no reified fallback",
                ds.table,
                missing
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        let keys = match &key_column {
            Some(_) => rows
                .iter()
                .map(|row| canonical_key(row.get(SPLIT_KEY).unwrap_or(&Value::Null)))
                .collect(),
            None => Vec::new(),
        };
        Frame::new(columns, keys)
    }
}

/// The model machinery a running server holds: the provider registry, the
/// dataset seam, and the bound a read must stay under.
///
/// Cloneable and cheap, like [`AgentServices`](crate::AgentServices), because
/// four places need the same one: the admin handlers, the `fit_model` action
/// in the trigger registry, the module reload that rebuilds the registry, and
/// the spawned fit itself.
#[derive(Clone)]
pub struct ModelServices {
    catalog: Arc<Catalog>,
    /// The providers, **replaced whole** rather than mutated when the module set
    /// changes — so a fit that is running keeps the registry it started with,
    /// which is the same rule the action registry follows.
    registry: Arc<RwLock<Arc<ModelRegistry>>>,
    source: Arc<dyn DatasetSource>,
    max_rows: u64,
    /// The Stan provider, built once: CmdStan is discovered when the server
    /// starts, not on every module change that rebuilds the registry.
    stan: Arc<StanProvider>,
    /// The most numbers one draws response may carry
    /// (`--stan-max-draws-response`, Stan TODO §16). Shared by every clone,
    /// so it is set once for the process.
    max_draws_response: Arc<AtomicU64>,
    /// The ceilings on a posterior fit (`--stan-max-data-values`,
    /// `--stan-max-draws-bytes`, `--stan-summary-max-elements`), handed to
    /// every fit this node runs.
    limits: PosteriorLimits,
}

/// This node's Stan settings: the `--cmdstan` and `--stan-*` flags (Stan TODO
/// §20), each `None` or the engine's own default when not given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StanSettings {
    /// `--cmdstan`: the CmdStan to use, before `$CMDSTAN` and `~/.cmdstan`.
    pub cmdstan: Option<PathBuf>,
    /// `--stan-cache-dir`: where compiled programs are kept. `None` is beside
    /// the modules root in the platform's data directory.
    pub cache_dir: Option<PathBuf>,
    /// `--stan-max-processes`: the chain processes this node runs at once,
    /// across every fit. `None` is half the available CPUs, at least one.
    pub max_processes: Option<usize>,
    /// `--stan-max-draws-response`: the most numbers one draws response may
    /// carry.
    pub max_draws_response: u64,
    /// `--stan-max-data-values`, `--stan-max-draws-bytes` and
    /// `--stan-summary-max-elements`.
    pub limits: PosteriorLimits,
}

impl Default for StanSettings {
    fn default() -> StanSettings {
        StanSettings {
            cmdstan: None,
            cache_dir: None,
            max_processes: None,
            max_draws_response: sc_model::DEFAULT_MAX_DRAWS_RESPONSE,
            limits: PosteriorLimits::default(),
        }
    }
}

impl ModelServices {
    /// The services over `catalog`, with the built-in providers and the catalog
    /// as the dataset source.
    pub fn new(catalog: &Arc<Catalog>, max_rows: u64) -> Result<ModelServices> {
        ModelServices::with_settings(catalog, max_rows, &StanSettings::default())
    }

    /// [`new`](ModelServices::new), with this node's Stan flags: the CmdStan
    /// discovered from `--cmdstan`, the cache, the process budget and the
    /// ceilings.
    pub fn with_settings(
        catalog: &Arc<Catalog>,
        max_rows: u64,
        settings: &StanSettings,
    ) -> Result<ModelServices> {
        let services =
            ModelServices::with_stan(catalog, max_rows, stan_provider(catalog, settings))?
                .with_posterior_limits(settings.limits);
        services.set_max_draws_response(settings.max_draws_response);
        Ok(services)
    }

    /// The services with `stan` as the Stan provider — a server's is
    /// [`new`](ModelServices::new)'s; a test's has a CmdStan of its own.
    pub fn with_stan(
        catalog: &Arc<Catalog>,
        max_rows: u64,
        stan: StanProvider,
    ) -> Result<ModelServices> {
        let stan = Arc::new(stan);
        Ok(ModelServices {
            catalog: Arc::clone(catalog),
            registry: Arc::new(RwLock::new(Arc::new(
                base_registry(&stan).context("registering the built-in model providers")?,
            ))),
            source: Arc::new(CatalogDatasetSource::new(Arc::clone(catalog))),
            max_rows,
            stan,
            max_draws_response: Arc::new(AtomicU64::new(sc_model::DEFAULT_MAX_DRAWS_RESPONSE)),
            limits: PosteriorLimits::default(),
        })
    }

    /// The same services giving every posterior fit `limits`.
    pub fn with_posterior_limits(mut self, limits: PosteriorLimits) -> ModelServices {
        self.limits = limits;
        self
    }

    /// The ceilings a posterior fit and a data preview on this node work
    /// within.
    pub fn posterior_limits(&self) -> PosteriorLimits {
        self.limits
    }

    /// The providers this server has before any module adds its own: the
    /// built-ins and Stan. What a module change rebuilds the registry from.
    pub fn base_registry(&self) -> Result<ModelRegistry> {
        base_registry(&self.stan)
    }

    /// The Stan provider — for what only it answers, such as checking a
    /// program with `stanc`.
    pub fn stan(&self) -> &Arc<StanProvider> {
        &self.stan
    }

    /// The provider registry as it stands.
    pub fn registry(&self) -> Arc<ModelRegistry> {
        match self.registry.read() {
            Ok(guard) => Arc::clone(&guard),
            // A poisoned lock means a panic while a *read* was in progress,
            // which cannot have left the value half-written: the registry is
            // replaced whole. Recovering is therefore correct and refusing
            // would take the models tab down for the life of the process.
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Replace the provider registry — what a module change does (Phase 7).
    pub fn set_registry(&self, registry: Arc<ModelRegistry>) {
        match self.registry.write() {
            Ok(mut guard) => *guard = registry,
            Err(poisoned) => *poisoned.into_inner() = registry,
        }
    }

    /// How a dataset becomes rows here.
    pub fn source(&self) -> Arc<dyn DatasetSource> {
        Arc::clone(&self.source)
    }

    /// The ceiling on one dataset read (`--model-max-rows`).
    pub fn max_rows(&self) -> u64 {
        self.max_rows
    }

    /// The most numbers one draws response may carry.
    pub fn max_draws_response(&self) -> u64 {
        self.max_draws_response.load(Ordering::Relaxed)
    }

    /// Set [`max_draws_response`](Self::max_draws_response) for every clone.
    pub fn set_max_draws_response(&self, max: u64) {
        self.max_draws_response.store(max, Ordering::Relaxed);
    }

    /// **Start a fit** (§8): write the instance row saying `fitting`, and spawn
    /// the work.
    ///
    /// Returns as soon as the row exists, because that is the contract the whole
    /// milestone is built on — a fit reads every row of a dataset and runs an
    /// optimiser over it, which is seconds at best and minutes at worst, and it
    /// must not be an HTTP request a proxy times out halfway through while the
    /// work carries on invisibly. The screen polls the row.
    ///
    /// The spawned task's only failure mode worth handling is "the failure could
    /// not be recorded", which [`fit_model_with`] reports as `Err`; a fit that simply
    /// did not work is `Ok` carrying a failed instance. So the task logs the
    /// former and nothing else: there is nobody left to return it to.
    pub async fn start_fit(&self, model: &Model, instance: ModelInstance) -> Result<ModelInstance> {
        self.start_fit_activating(model, instance, Activation::Never)
            .await
    }

    /// [`start_fit`](Self::start_fit), making the instance the model's active
    /// one when it finishes and `activation` says it should — what `fit_model`
    /// does when it does not wait.
    pub async fn start_fit_activating(
        &self,
        model: &Model,
        instance: ModelInstance,
        activation: Activation,
    ) -> Result<ModelInstance> {
        save_model_instance(&self.catalog, &instance)
            .await
            .context("recording the start of the fit")?;
        let id: InstanceId = instance.id;
        let catalog = Arc::clone(&self.catalog);
        let registry = self.registry();
        let source = Arc::clone(&self.source);
        let model = model.clone();
        let cap = self.max_rows;
        let limits = self.limits;
        tokio::spawn(async move {
            let progress = JobProgress::default();
            let cancel = AtomicBool::new(false);
            let ctx = FitContext::new(&progress, &cancel).with_limits(limits);
            let fit = fit_model_with(&catalog, &registry, source.as_ref(), &model, id, cap, &ctx);
            // The fit and its row's upkeep, side by side on one task: the
            // upkeep never finishes, so this is over when the fit is.
            let finished = tokio::select! {
                finished = fit => finished,
                () = keep_row(&catalog, id, &progress, &cancel, PROGRESS_INTERVAL) => {
                    unreachable!("the upkeep of a fit's row never finishes")
                }
            };
            let finished = match finished {
                Ok(finished) => finished,
                Err(e) => {
                    eprintln!(
                        "feldspar: the fit of model `{}` could not be recorded: {}",
                        model.name,
                        sc_error::format_chain(&e)
                    );
                    return;
                }
            };
            if activation.activates(&finished) {
                let mut active = finished;
                active.active = true;
                if let Err(e) = save_model_instance(&catalog, &active).await {
                    eprintln!(
                        "feldspar: the new fit of model `{}` could not be made active: {}",
                        model.name,
                        sc_error::format_chain(&e)
                    );
                }
            }
        });
        Ok(instance)
    }
}

#[async_trait]
impl sc_model::FitStarter for ModelServices {
    async fn start_fit(
        &self,
        model: &Model,
        instance: ModelInstance,
        activation: Activation,
    ) -> Result<ModelInstance> {
        self.start_fit_activating(model, instance, activation).await
    }
}

/// The models a formula's `predict("…")` and a code body's model handle reach
/// (milestone 31 §4), installed on the catalog by [`install_models`] and again
/// on every module rebuild.
///
/// Both halves are `sc_api::models`' — this supplies the registry as it stands
/// at the call (so a module's provider is used from the moment it is loaded),
/// the dataset source and the row cap.
#[async_trait]
impl sc_catalog::ModelHost for ModelServices {
    async fn predict(
        &self,
        model: &str,
        fit: Option<&str>,
        table: &str,
        rows: sc_catalog::PredictRows<'_>,
        detail: bool,
    ) -> Result<Vec<serde_json::Value>> {
        sc_api::models::predict_for(
            &self.catalog,
            &self.registry(),
            self.source.as_ref(),
            self.max_rows,
            model,
            fit,
            table,
            rows,
            detail,
        )
        .await
    }

    async fn describe(&self, model: &str) -> Result<sc_catalog::ModelSummary> {
        sc_api::models::describe_model(&self.catalog, &self.registry(), model).await
    }
}

/// Install `services` as the catalog's [`ModelHost`](sc_catalog::ModelHost).
pub fn install_model_host(catalog: &Catalog, services: &ModelServices) -> Result<()> {
    catalog.set_model_host(Arc::new(services.clone()))
}

/// How often a running fit's progress reaches its row, and the row's cancel
/// is read back (Stan TODO §13): at most once a second.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// A running fit's latest progress, held until the next write to its row. A
/// provider may report as often as it likes; the row hears of it at most once
/// per [`PROGRESS_INTERVAL`].
#[derive(Default)]
pub struct JobProgress(Mutex<Option<Progress>>);

impl JobProgress {
    /// The progress reported since the last call, if any.
    fn take(&self) -> Option<Progress> {
        match self.0.lock() {
            Ok(mut latest) => latest.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }
}

impl FitProgress for JobProgress {
    fn report(&self, progress: &Progress) {
        match self.0.lock() {
            Ok(mut latest) => *latest = Some(progress.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(progress.clone()),
        }
    }
}

/// The upkeep of a running fit's row, every `interval` until it is dropped:
/// write the latest progress, and read back `cancel_requested` — setting
/// `cancel`, which the provider polls, when somebody has asked. Reading it
/// from the row rather than from memory is what makes a cancel work from any
/// node (Stan TODO §13).
///
/// A write that fails is reported and retried next time: a fit is not failed
/// because its progress bar could not be updated.
pub async fn keep_row(
    catalog: &Catalog,
    id: InstanceId,
    progress: &JobProgress,
    cancel: &AtomicBool,
    interval: Duration,
) {
    let mut unwritten: Option<Progress> = None;
    loop {
        tokio::time::sleep(interval).await;
        if let Some(latest) = progress.take() {
            unwritten = Some(latest);
        }
        match record_fit_progress(catalog, id, unwritten.as_ref()).await {
            Ok(ProgressWrite::Running) => unwritten = None,
            Ok(ProgressWrite::CancelRequested) => cancel.store(true, Ordering::SeqCst),
            // The fit is finishing: the final save has been made or is about
            // to be, and it is the one that counts.
            Ok(ProgressWrite::Finished) => {}
            Err(e) => eprintln!(
                "feldspar: the progress of model fit {id} could not be recorded: {}",
                sc_error::format_chain(&e)
            ),
        }
    }
}

/// The built-in providers and `stan`.
fn base_registry(stan: &Arc<StanProvider>) -> Result<ModelRegistry> {
    let mut registry = builtin_registry()?;
    registry.register(Arc::clone(stan) as Arc<dyn ModelProvider>)?;
    Ok(registry)
}

/// The Stan provider over `catalog`'s file stores, with the CmdStan this
/// process finds (`--cmdstan`, else `$CMDSTAN`, else the newest
/// `~/.cmdstan/cmdstan-*`). Not finding one is not an error: the provider is
/// listed saying so (Stan TODO §4, §20).
fn stan_provider(catalog: &Arc<Catalog>, settings: &StanSettings) -> StanProvider {
    let stores = Arc::clone(catalog);
    let locations = Locations::from_env(settings.cmdstan.clone());
    let make = Toolchain::find(&locations)
        .make
        .unwrap_or_else(|| PathBuf::from("make"));
    StanProvider::new(
        Arc::new(move |name: &str| stores.require_file_store(name)),
        discover(&locations),
    )
    .with_scratch(std::env::temp_dir().join("feldspar-stan"))
    .with_cache(CompileCache::new(
        settings.cache_dir.clone().unwrap_or_else(stan_cache_dir),
        make,
    ))
    .with_budget(
        settings
            .max_processes
            .map_or_else(ProcessBudget::for_this_machine, ProcessBudget::new),
    )
}

/// Where compiled Stan programs are kept: beside the modules root, in the
/// platform's data directory (Stan TODO §13) — a compile is a minute, and a
/// temporary directory cleared at reboot would pay it again. The system
/// temporary directory when there is no data directory.
fn stan_cache_dir() -> PathBuf {
    match sc_module::default_modules_root() {
        Ok(modules) => modules
            .parent()
            .map_or_else(|| modules.join("stan-cache"), |app| app.join("stan-cache")),
        Err(_) => std::env::temp_dir().join("feldspar-stan-cache"),
    }
}

/// Ensure the three model tables exist, **reap every fit that was running when
/// this process last stopped** (§8), and assemble the services.
///
/// A fit is a job whose registry is its row: `fitModel` writes the instance
/// first, returns its id, and runs the work on a spawned task. Nothing survives
/// a restart, so an instance still saying `fitting` at boot is one nothing will
/// ever finish — and leaving it that way would show an admin a fit in progress
/// that is not. It is failed by name instead, with the sentence saying what
/// happened. Making a fit durable is the workflow engine's job and would mean
/// expressing a fit as a workflow, which is a bigger claim than this milestone
/// makes.
///
/// Runs before anything can read an instance, for that reason.
pub async fn install_models(catalog: &Arc<Catalog>, max_rows: u64) -> Result<ModelServices> {
    install_models_with(catalog, max_rows, &StanSettings::default()).await
}

/// [`install_models`] with this node's Stan flags — what `serve` calls.
pub async fn install_models_with(
    catalog: &Arc<Catalog>,
    max_rows: u64,
    stan: &StanSettings,
) -> Result<ModelServices> {
    bootstrap_models(catalog)
        .await
        .context("ensuring the models table exists")?;
    bootstrap_model_instances(catalog)
        .await
        .context("ensuring the model instances table exists")?;
    bootstrap_model_draws(catalog)
        .await
        .context("ensuring the model draws table exists")?;
    let reaped = reap_fitting_instances(catalog)
        .await
        .context("failing the fits that were running at the last shutdown")?;
    if reaped > 0 {
        eprintln!(
            "feldspar: {reaped} model fit(s) were running when the server last stopped and \
             have been marked failed"
        );
    }
    let services = ModelServices::with_settings(catalog, max_rows, stan)?;
    install_model_host(catalog, &services).context("installing the model host")?;
    // The run directories of those fits, and any compile they were in the
    // middle of, on this node (Stan TODO §13).
    let stan = services.stan();
    let removed =
        sc_stan::run_dir::clean_stale_scratch(stan.scratch()) + stan.compile_cache().clean_stale();
    if removed > 0 {
        eprintln!("feldspar: removed {removed} stale Stan run or compile directories");
    }
    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_split_key_is_a_reserved_alias_and_not_a_dataset_column_name() {
        // A dataset column called `id` computes whatever its formula says; the
        // key is projected separately so the two cannot be confused.
        assert!(SPLIT_KEY.starts_with("_fd_"));
        let ds = Dataset::new("houses").column("id", "bedrooms");
        assert!(ds.columns.iter().all(|c| c.name != SPLIT_KEY));
    }
}

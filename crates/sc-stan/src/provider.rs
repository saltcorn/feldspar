//! `StanProvider`: a Stan program as a [`ModelProvider`] (TODO §§4–6, 13, 14).
//!
//! What it declares is the whole of the Stan model form: where the program is
//! (a file store and a path in it — §6, the only place a program lives), how
//! its `data` block is bound (`dimensions`, `bindings`, `policies`, `labels`, whose keys
//! are the host's, [`sc_model::BINDINGS_KEY`] and its siblings), the sampler's
//! settings (§13 — the provider's configuration, not hyperparameters, since
//! there is no grid search over a posterior) and where the raw run and the
//! draws go (§14). The datasets are the model's own, not configuration.
//!
//! It [binds data](ModelProvider::binds_data), and its
//! [`interface`](ModelProvider::interface) reads the program out of its store
//! and parses it, so the host's save-time checks run against the program as
//! it is now.
//!
//! **CmdStan's availability is a runtime fact** (§4): the provider is always
//! registered, and when no CmdStan was found its description says so and why,
//! so the picker shows it rather than hiding it. A model can still be saved —
//! on our parse alone, with a notice ([`StanProvider::check_program`]) — and a
//! fit is refused with the same sentence.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sc_error::{Context, Error, Result};
use sc_files::FileStore;
use sc_model::{
    BINDINGS_KEY, DIMENSIONS_KEY, DatasetShape, DrawPlan, FitContext, FitResult, FitStage, Frame,
    Interface, LABELS_KEY, ModelProvider, OutcomeSpec, POLICIES_KEY, PosteriorInput,
    PosteriorMethod, PosteriorResult, PosteriorRun, Prediction, Progress,
};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::cmdstan::{CmdStan, Locations, Toolchain};
use crate::compile::CompileCache;
use crate::output::{optimizer_iterations, read_output_file};
use crate::process::Watch;
use crate::program::Program;
use crate::run::{DATA_FILE, Method, ProcessBudget, Run, RunSettings, run_chains};
use crate::run_dir::{DEFAULT_RUNS_DIR, RunDir, RunLocation, publish, read_published};
use crate::stanc::{ProgramCheck, check_program};

/// The name the provider is registered and stored under.
pub const STAN_PROVIDER: &str = "stan";

/// The configuration keys, beside the host's binding keys.
pub mod config_keys {
    /// The file store the program is in.
    pub const PROGRAM_STORE: &str = "program_store";
    /// The program's path in that store.
    pub const PROGRAM: &str = "program";
    /// `sample`, `optimize` or `pathfinder`.
    pub const METHOD: &str = "method";
    /// How many chains.
    pub const CHAINS: &str = "chains";
    /// How many chains run at once (default: all of them, within the node's
    /// process budget).
    pub const PARALLEL_CHAINS: &str = "parallel_chains";
    /// Warmup iterations per chain.
    pub const ITER_WARMUP: &str = "iter_warmup";
    /// Sampling iterations per chain.
    pub const ITER_SAMPLING: &str = "iter_sampling";
    /// Keep every `thin`-th draw.
    pub const THIN: &str = "thin";
    /// NUTS's target acceptance rate.
    pub const ADAPT_DELTA: &str = "adapt_delta";
    /// NUTS's maximum tree depth.
    pub const MAX_TREEDEPTH: &str = "max_treedepth";
    /// The random seed; empty means a fresh one per fit, recorded on the
    /// instance.
    pub const SEED: &str = "seed";
    /// Initial values are uniform(−init, init) on the unconstrained scale.
    pub const INIT: &str = "init";
    /// Whether warmup draws are kept.
    pub const SAVE_WARMUP: &str = "save_warmup";
    /// A fit running longer is killed and failed.
    pub const MAX_RUNTIME_MINUTES: &str = "max_runtime_minutes";
    /// The file store the raw CmdStan run is published to, if any.
    pub const RUNS_STORE: &str = "runs_store";
    /// The directory in that store.
    pub const RUNS_DIR: &str = "runs_dir";
    /// Output variables whose draws are not kept — the host's key, since the
    /// host decides what is kept after it has summarised everything.
    pub const EXCLUDE_VARIABLES: &str = sc_model::EXCLUDE_VARIABLES_KEY;
    /// Whether the draws are kept at all (the summary always is) — the
    /// host's key.
    pub const KEEP_DRAWS: &str = sc_model::KEEP_DRAWS_KEY;
}

use config_keys::*;

/// The sampling methods (§13). ADVI is not offered: Pathfinder replaces it.
const METHODS: [&str; 3] = ["sample", "optimize", "pathfinder"];

/// How a provider reaches a file store by name — the catalog's registry in a
/// server, a directory in a test.
pub trait StoreLookup: Send + Sync {
    /// The connected store called `name`, or why not.
    fn store(&self, name: &str) -> Result<Arc<dyn FileStore>>;
}

impl<F> StoreLookup for F
where
    F: Fn(&str) -> Result<Arc<dyn FileStore>> + Send + Sync,
{
    fn store(&self, name: &str) -> Result<Arc<dyn FileStore>> {
        self(name)
    }
}

/// The Stan model provider.
pub struct StanProvider {
    stores: Arc<dyn StoreLookup>,
    /// The CmdStan found at startup, or the sentence saying why none was.
    cmdstan: std::result::Result<CmdStan, String>,
    /// Where `stanc` lays programs out and fits keep their run directories
    /// while they run.
    scratch: PathBuf,
    /// Where compiled programs are kept.
    cache: CompileCache,
    /// The node's chain processes, shared by every fit.
    budget: ProcessBudget,
    description: String,
}

impl StanProvider {
    /// A provider reading programs through `stores`, with the CmdStan
    /// discovery found (or the error it gave).
    pub fn new(stores: Arc<dyn StoreLookup>, cmdstan: Result<CmdStan>) -> StanProvider {
        let cmdstan = cmdstan.map_err(|e| sentence(&e));
        let mut description = "Bayesian inference: a Stan program whose data block is bound to \
                               the datasets, sampled with CmdStan"
            .to_owned();
        if let Err(why) = &cmdstan {
            description.push_str(&format!(" — CmdStan was not found: {why}"));
        }
        let make = Toolchain::find(&Locations::from_env(None))
            .make
            .unwrap_or_else(|| PathBuf::from("make"));
        StanProvider {
            stores,
            cmdstan,
            scratch: std::env::temp_dir(),
            cache: CompileCache::new(std::env::temp_dir().join("feldspar-stan-cache"), make),
            budget: ProcessBudget::for_this_machine(),
            description,
        }
    }

    /// The same provider laying programs out, and running fits, under `dir`
    /// rather than the system temporary directory.
    pub fn with_scratch(mut self, dir: impl Into<PathBuf>) -> StanProvider {
        self.scratch = dir.into();
        self
    }

    /// The same provider keeping compiled programs in `cache`.
    pub fn with_cache(mut self, cache: CompileCache) -> StanProvider {
        self.cache = cache;
        self
    }

    /// The same provider running its chains within `budget` — the node's,
    /// which the server owns.
    pub fn with_budget(mut self, budget: ProcessBudget) -> StanProvider {
        self.budget = budget;
        self
    }

    /// Where fits keep their run directories while they run.
    pub fn scratch(&self) -> &std::path::Path {
        &self.scratch
    }

    /// The compile cache.
    pub fn compile_cache(&self) -> &CompileCache {
        &self.cache
    }

    /// The node's chain-process budget.
    pub fn budget(&self) -> &ProcessBudget {
        &self.budget
    }

    /// The CmdStan this provider compiles with, when one was found.
    pub fn cmdstan(&self) -> Option<&CmdStan> {
        self.cmdstan.as_ref().ok()
    }

    /// Why there is no CmdStan, when there is none.
    pub fn unavailable(&self) -> Option<&str> {
        self.cmdstan.as_ref().err().map(String::as_str)
    }

    /// Read the program `config` names, and everything it includes.
    pub async fn program(&self, config: &Attrs) -> Result<Program> {
        let store_name = text(config, PROGRAM_STORE).ok_or_else(|| {
            Error::invalid(format!(
                "`{PROGRAM_STORE}`: no file store is chosen for the program"
            ))
        })?;
        let path = text(config, PROGRAM).ok_or_else(|| {
            Error::invalid(format!(
                "`{PROGRAM}`: no program is chosen in `{store_name}`"
            ))
        })?;
        let store = self.stores.store(store_name)?;
        Program::load(store.as_ref(), store_name, path).await
    }

    /// The "Check program" button and the check on save (§5): `stanc` and our
    /// parse when CmdStan is available; our parse alone, with a notice saying
    /// the program has not been checked, when it is not.
    pub async fn check_program(&self, config: &Attrs) -> Result<ProgramCheck> {
        let program = self.program(config).await?;
        match &self.cmdstan {
            Ok(cmdstan) => check_program(&cmdstan.stanc(), &program, &self.scratch).await,
            Err(why) => Ok(ProgramCheck {
                interface: program.interface()?,
                warnings: String::new(),
                notice: Some(format!(
                    "the program has not been checked by stanc, because CmdStan was not found: \
                     {why}"
                )),
            }),
        }
    }
}

/// What `compileModel` answers: the compiled program's cache key, and whether
/// it was already there.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CompileAnswer {
    /// The key it is cached under.
    pub key: String,
    /// Whether it was compiled before this request.
    pub cached: bool,
    /// The CmdStan it was compiled with.
    pub cmdstan: String,
}

impl StanProvider {
    /// The "Compile" button (§13): the program `config` names, compiled into
    /// the cache without fitting — at once when it is there already, else one
    /// compile at a time on this node, within `limit`.
    pub async fn compile(
        &self,
        config: &Attrs,
        limit: std::time::Duration,
    ) -> Result<CompileAnswer> {
        let cmdstan = match &self.cmdstan {
            Ok(cmdstan) => cmdstan,
            Err(why) => {
                return Err(Error::config(format!(
                    "this Stan program cannot be compiled because CmdStan was not found: {why}"
                )));
            }
        };
        let program = self.program(config).await?;
        let never = || false;
        let watch = Watch::new(limit, &never);
        let compiled = self
            .cache
            .compile(cmdstan, &program, &watch, &|| {})
            .await?;
        Ok(CompileAnswer {
            key: compiled.key,
            cached: compiled.cached,
            cmdstan: cmdstan.version.to_string(),
        })
    }
}

/// An error's message without its kind's prefix ("not found: …"): discovery's
/// sentences are written to be read on their own, after "CmdStan was not
/// found:".
fn sentence(e: &Error) -> String {
    match e.repr() {
        sc_error::Repr::NotFound(m) | sc_error::Repr::Config(m) | sc_error::Repr::Invalid(m) => {
            m.clone()
        }
        _ => e.to_string(),
    }
}

/// A trimmed, non-empty string setting.
fn text<'a>(config: &'a Attrs, key: &str) -> Option<&'a str> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// An integer setting, if it is set; refused when it is not a whole number in
/// `range`.
fn int_in(config: &Attrs, key: &str, range: std::ops::RangeInclusive<i64>) -> Result<Option<i64>> {
    let Some(value) = config.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    match value.as_i64() {
        Some(n) if range.contains(&n) => Ok(Some(n)),
        _ => Err(Error::invalid(format!(
            "`{key}` must be a whole number from {} to {}, got {value}",
            range.start(),
            range.end()
        ))),
    }
}

#[async_trait]
impl ModelProvider for StanProvider {
    fn name(&self) -> &str {
        STAN_PROVIDER
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn config_declaration(&self) -> Vec<FormField> {
        vec![
            FormField::new(PROGRAM_STORE, BasicType::Text)
                .label("Program file store")
                .required()
                .server_query(sc_catalog::QUERY_FILE_STORES),
            FormField::new(PROGRAM, BasicType::Text)
                .label("Program (a .stan file in that store)")
                .required(),
            FormField::new(DIMENSIONS_KEY, BasicType::Json).label("Dimensions"),
            FormField::new(BINDINGS_KEY, BasicType::Json).label("Bindings"),
            FormField::new(POLICIES_KEY, BasicType::Json)
                .label("Per dataset: what a null, or an unknown key, does"),
            FormField::new(LABELS_KEY, BasicType::Json).label("Labels"),
            FormField::new(METHOD, BasicType::Text)
                .label("Method")
                .options(METHODS)
                .default_value("sample"),
            FormField::new(CHAINS, BasicType::Int)
                .label("Chains")
                .default_value(4),
            FormField::new(PARALLEL_CHAINS, BasicType::Int)
                .label("Chains run at once (empty for all of them)"),
            FormField::new(ITER_WARMUP, BasicType::Int)
                .label("Warmup iterations per chain")
                .default_value(1000),
            FormField::new(ITER_SAMPLING, BasicType::Int)
                .label("Sampling iterations per chain")
                .default_value(1000),
            FormField::new(THIN, BasicType::Int)
                .label("Keep every n-th draw")
                .default_value(1),
            FormField::new(ADAPT_DELTA, BasicType::Float)
                .label("Target acceptance rate (adapt_delta)")
                .default_value(0.8),
            FormField::new(MAX_TREEDEPTH, BasicType::Int)
                .label("Maximum tree depth")
                .default_value(10),
            FormField::new(SEED, BasicType::Int)
                .label("Random seed (empty for a fresh one each fit)"),
            FormField::new(INIT, BasicType::Float)
                .label("Initial values within ±init on the unconstrained scale")
                .default_value(2),
            FormField::new(SAVE_WARMUP, BasicType::Bool)
                .label("Keep warmup draws")
                .default_value(false),
            FormField::new(MAX_RUNTIME_MINUTES, BasicType::Int)
                .label("Stop a fit after this many minutes")
                .default_value(60),
            FormField::new(RUNS_STORE, BasicType::Text)
                .label("File store for the raw CmdStan run (optional)")
                .server_query(sc_catalog::QUERY_FILE_STORES),
            FormField::new(RUNS_DIR, BasicType::Text).label("Directory in that store"),
            FormField::new(EXCLUDE_VARIABLES, BasicType::Json)
                .label("Variables whose draws are not kept"),
            FormField::new(KEEP_DRAWS, BasicType::Bool)
                .label("Keep the draws")
                .default_value(true),
        ]
    }

    fn outcome_spec(&self) -> OutcomeSpec {
        // Prediction for new rows (§19) is carried past this milestone, so a
        // posterior names none and `predict()` over one is refused on save.
        OutcomeSpec::Posterior { prediction: None }
    }

    fn binds_data(&self) -> bool {
        true
    }

    /// A fit is compiles and chain processes, which are killed when the
    /// instance's row asks (§13).
    fn cancellable(&self) -> bool {
        true
    }

    /// The configuration's own consistency — ranges, shapes, pairs of settings
    /// that go together. The checks against the program are the host's, in
    /// `validate_model`, through [`interface`](ModelProvider::interface).
    fn validate(&self, _shape: &DatasetShape, config: &Attrs) -> Result<()> {
        if let Some(path) = text(config, PROGRAM) {
            if path.starts_with('/') || path.split(['/', '\\']).any(|s| s == "..") {
                return Err(Error::invalid(format!(
                    "`{PROGRAM}`: `{path}` must be a path inside the file store, with no `..`"
                )));
            }
        }
        let chains = int_in(config, CHAINS, 1..=64)?.unwrap_or(4);
        if let Some(parallel) = int_in(config, PARALLEL_CHAINS, 1..=64)? {
            if parallel > chains {
                return Err(Error::invalid(format!(
                    "`{PARALLEL_CHAINS}` is {parallel}, but there are only {chains} chains"
                )));
            }
        }
        int_in(config, ITER_WARMUP, 0..=1_000_000)?;
        int_in(config, ITER_SAMPLING, 1..=1_000_000)?;
        int_in(config, THIN, 1..=1_000_000)?;
        int_in(config, MAX_TREEDEPTH, 1..=30)?;
        // CmdStan's seed is an unsigned 32-bit integer.
        int_in(config, SEED, 0..=i64::from(u32::MAX))?;
        int_in(config, MAX_RUNTIME_MINUTES, 1..=7 * 24 * 60)?;
        if let Some(delta) = config.get(ADAPT_DELTA).and_then(Json::as_f64) {
            if !(delta > 0.0 && delta < 1.0) {
                return Err(Error::invalid(format!(
                    "`{ADAPT_DELTA}` must be between 0 and 1 (exclusive), got {delta}"
                )));
            }
        }
        if let Some(init) = config.get(INIT).and_then(Json::as_f64) {
            if !(init >= 0.0 && init.is_finite()) {
                return Err(Error::invalid(format!(
                    "`{INIT}` must be zero or a positive number, got {init}"
                )));
            }
        }
        for key in [DIMENSIONS_KEY, BINDINGS_KEY, POLICIES_KEY, LABELS_KEY] {
            if config
                .get(key)
                .is_some_and(|v| !v.is_null() && !v.is_object())
            {
                return Err(Error::invalid(format!("`{key}` must be an object")));
            }
        }
        if let Some(excluded) = config.get(EXCLUDE_VARIABLES).filter(|v| !v.is_null()) {
            let names = excluded
                .as_array()
                .filter(|a| a.iter().all(Json::is_string));
            if names.is_none() {
                return Err(Error::invalid(format!(
                    "`{EXCLUDE_VARIABLES}` must be a list of variable names"
                )));
            }
        }
        if text(config, RUNS_DIR).is_some() && text(config, RUNS_STORE).is_none() {
            return Err(Error::invalid(format!(
                "`{RUNS_DIR}` is set but `{RUNS_STORE}` is not: choose the file store the runs \
                 directory is in"
            )));
        }
        Ok(())
    }

    async fn fit(&self, _frame: &Frame, _config: &Attrs, _hyper: &Attrs) -> Result<FitResult> {
        Err(Error::invalid(
            "a Stan model is sampled, not fitted to one frame: its outcome is a posterior",
        ))
    }

    async fn predict(&self, _state: &Json, _frame: &Frame) -> Result<Vec<Prediction>> {
        Err(Error::invalid(
            "this Stan model does not predict rows: it is inspected through its posterior",
        ))
    }

    async fn interface(&self, config: &Attrs) -> Result<Option<Interface>> {
        let program = self.program(config).await?;
        Ok(Some(program.interface()?))
    }

    /// Compile (or find in the cache), run one process per chain, read the
    /// draws back, and publish the raw run when the model keeps one (§§13–14).
    async fn fit_posterior(
        &self,
        input: &PosteriorInput,
        config: &Attrs,
        ctx: &FitContext<'_>,
    ) -> Result<PosteriorResult> {
        let cmdstan = match &self.cmdstan {
            Ok(cmdstan) => cmdstan,
            Err(why) => {
                return Err(Error::config(format!(
                    "this Stan model cannot be fitted because CmdStan was not found: {why}"
                )));
            }
        };
        let settings = RunSettings::from_config(config, fresh_seed)?;
        let cancelled = || ctx.cancelled();
        let watch = Watch::new(settings.max_runtime(), &cancelled);
        let program = self.program(config).await?;
        let run_id = input
            .instance
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), |id| id.to_string());

        // The run directory, as it will be published (§14).
        let dir = RunDir::create(&self.scratch, &run_id)?;
        program.layout(&dir.path().join("program"))?;
        dir.write(DATA_FILE, &to_json(&input.data)?)?;
        dir.write("coordinates.json", &to_json(&input.coordinates)?)?;
        dir.write(
            "config.json",
            &to_json(&json!({
                "program": program.main_path(),
                "method": settings.method,
                "settings": settings,
                "arguments": settings.arguments(1),
                "cmdstan": cmdstan.version.to_string(),
            }))?,
        )?;

        let compiled = self
            .cache
            .compile(cmdstan, &program, &watch, &|| {
                ctx.report(&Progress::stage(FitStage::Compiling));
            })
            .await?;
        let report = |progress: &Progress| ctx.report(progress);
        let chains = run_chains(
            &Run {
                exe: &compiled.exe,
                src: &compiled.src,
                program: program.main_path(),
                dir: dir.path(),
                settings: &settings,
            },
            &self.budget,
            &watch,
            &report,
        )
        .await?;

        ctx.report(&Progress::stage(FitStage::Summarising));
        let warmup_rows = match settings.method {
            Method::Sample if settings.save_warmup => {
                usize::try_from(settings.iter_warmup.div_ceil(settings.thin)).unwrap_or(0)
            }
            _ => 0,
        };
        // Chain by chain, each file a line at a time, the variables the host
        // will not read skipped as they go past (§14).
        let skip = |variable: &str| input.unread.contains(variable);
        let mut draws = Vec::new();
        let mut records = Vec::with_capacity(chains.len());
        for chain in &chains {
            let output = read_output_file(&chain.csv, chain.chain, warmup_rows, &skip)?;
            draws.extend(output.draws);
            records.push(json!({
                "chain": chain.chain,
                "wall_seconds": chain.wall.as_secs_f64(),
                "step_size": output.adaptation.as_ref().map(|a| a.step_size),
                "timing": output.timing,
            }));
        }
        let iterations = match settings.method {
            Method::Optimize => chains
                .first()
                .and_then(|c| std::fs::read_to_string(&c.log).ok())
                .and_then(|log| optimizer_iterations(&log)),
            Method::Sample | Method::Pathfinder => None,
        };

        let run = match text(config, RUNS_STORE) {
            None => None,
            Some(store_name) => {
                let store = self.stores.store(store_name)?;
                let runs_dir = text(config, RUNS_DIR).unwrap_or(DEFAULT_RUNS_DIR);
                let path = publish(dir.path(), store.as_ref(), runs_dir, &input.model, &run_id)
                    .await
                    .with_context(|| {
                        format!("publishing the raw run to the file store `{store_name}`")
                    })?;
                Some(RunLocation {
                    store: store_name.to_owned(),
                    path,
                })
            }
        };

        // Small, by design (§14): the program snapshot and its hashes, the
        // seed, the toolchain, the interface, the cache key and the run's
        // location. The draws are rows of their own.
        let state = json!({
            "program": {
                "store": program.store,
                "main": program.main_path(),
                "files": program.files.iter().map(|f| json!({
                    "path": f.path, "text": f.text, "sha256": f.sha256,
                })).collect::<Vec<_>>(),
            },
            "hashes": program.hashes(),
            "seed": settings.seed,
            "method": settings.method,
            "settings": settings,
            "cmdstan": cmdstan.version.to_string(),
            "compiled": compiled.key,
            "interface": input.interface,
            "chains": records,
            "run": run,
        });
        Ok(PosteriorResult {
            state,
            draws,
            parameters: Vec::new(),
            run: PosteriorRun {
                method: match settings.method {
                    Method::Sample => PosteriorMethod::Mcmc,
                    Method::Optimize => PosteriorMethod::Mode,
                    Method::Pathfinder => PosteriorMethod::Approximation,
                },
                max_treedepth: (settings.method == Method::Sample)
                    .then_some(settings.max_treedepth),
                wall_seconds: chains.iter().map(|c| c.wall.as_secs_f64()).collect(),
                iterations,
            },
        })
    }

    /// Chains × kept iterations × (the program's outputs + CmdStan's own
    /// columns), from the settings alone (§14).
    fn draw_plan(&self, config: &Attrs) -> Result<Option<DrawPlan>> {
        let settings = RunSettings::from_config(config, || 0)?;
        Ok(Some(match settings.method {
            Method::Sample => DrawPlan {
                chains: u64::from(settings.chains),
                draws_per_chain: settings.iter_sampling.div_ceil(settings.thin)
                    + if settings.save_warmup {
                        settings.iter_warmup.div_ceil(settings.thin)
                    } else {
                        0
                    },
                // lp__, accept_stat__, stepsize__, treedepth__, n_leapfrog__,
                // divergent__, energy__.
                sampler_variables: 7,
            },
            // lp__ and converged__.
            Method::Optimize => DrawPlan {
                chains: 1,
                draws_per_chain: 1,
                sampler_variables: 2,
            },
            // lp_approx__, lp__ and path__.
            Method::Pathfinder => DrawPlan {
                chains: 1,
                draws_per_chain: settings.iter_sampling,
                sampler_variables: 3,
            },
        }))
    }

    /// The published raw run, read back for `downloadModelRun` (§16).
    async fn run_files(&self, state: &Json) -> Result<Option<Vec<(String, Vec<u8>)>>> {
        let Some(run) = state.get("run").filter(|r| !r.is_null()) else {
            return Ok(None);
        };
        let run: RunLocation = serde_json::from_value(run.clone())
            .map_err(|e| Error::msg(format!("the instance's run location is unreadable: {e}")))?;
        let store = self.stores.store(&run.store)?;
        let files = read_published(store.as_ref(), &run.path)
            .await
            .with_context(|| {
                format!(
                    "the raw run {} in the file store `{}` could not be read",
                    run.path, run.store
                )
            })?;
        if files.is_empty() {
            return Err(Error::not_found(format!(
                "the raw run {} is gone from the file store `{}`",
                run.path, run.store
            )));
        }
        Ok(Some(files))
    }

    /// The program `config` names now against the snapshot in `state` (§6):
    /// another store, another main file, a file added or dropped from the
    /// includes, or any file's SHA-256. A program that cannot be read now is
    /// `None` — a disconnected store says nothing about the file in it.
    async fn program_changed(&self, config: &Attrs, state: &Json) -> Option<bool> {
        let snapshot = state.get("program")?;
        let hashes: std::collections::BTreeMap<String, String> =
            serde_json::from_value(state.get("hashes")?.clone()).ok()?;
        let store = snapshot.get("store")?.as_str()?;
        let main = snapshot.get("main")?.as_str()?;
        if text(config, PROGRAM_STORE) != Some(store) {
            return Some(true);
        }
        let program = self.program(config).await.ok()?;
        Some(program.main_path() != main || program.hashes() != hashes)
    }

    /// Delete the published raw run, when the fit kept one (§14).
    async fn discard(&self, state: &Json) -> Result<()> {
        let Some(run) = state.get("run").filter(|r| !r.is_null()) else {
            return Ok(());
        };
        let run: RunLocation = serde_json::from_value(run.clone())
            .map_err(|e| Error::msg(format!("the instance's run location is unreadable: {e}")))?;
        let store = self.stores.store(&run.store)?;
        store.delete(&run.path).await.with_context(|| {
            format!(
                "deleting the raw run {} from the file store `{}`",
                run.path, run.store
            )
        })?;
        Ok(())
    }
}

/// A seed for a fit that configures none — recorded on the instance, so the
/// fit can be repeated.
fn fresh_seed() -> u32 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn to_json(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| Error::msg(format!("writing JSON: {e}")))
}

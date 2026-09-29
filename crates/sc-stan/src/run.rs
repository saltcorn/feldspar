//! The runner (TODO §13): a compiled program run as one process per chain.
//!
//! **One process per chain**, rather than CmdStan's `num_chains`, because
//! per-chain progress and per-chain failure are both simpler to read from
//! separate processes, and it needs no `STAN_THREADS` build. The chains of a
//! fit run in parallel up to its `parallel_chains`, and every chain of every
//! fit on the node draws from one [`ProcessBudget`]; a fit waiting for it is
//! `queued`.
//!
//! **`sig_figs` is 9**, not CmdStan's 6: six significant figures is a visible
//! error in a posterior standard deviation computed from them.
//!
//! **Progress** is CmdStan's own `Iteration: 400 / 2000 [ 20%]  (Warmup)`
//! lines, read as they arrive and reported per chain.
//!
//! **Failures are sentences** (§13): a data error names CmdStan's variable, an
//! initialisation failure says so and what to try, and anything else carries
//! the last 40 lines of that chain's output. A chain that fails stops the
//! others: they are dropped, and a dropped chain's process group is killed.
//!
//! `optimize` and `pathfinder` go through the same runner as one process each
//! (§13, task 4.6): the arguments differ and nothing else does.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sc_error::{Context, Error, Result};
use sc_model::{ChainPhase, ChainProgress, FitStage, Progress};
use sc_types::Attrs;
use serde_json::Value as Json;
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::compile::{map_src, stop_error};
use crate::process::{self, KillGroupOnDrop, Tail, Watch};
use crate::provider::config_keys::*;

/// CmdStan's `sig_figs` for every run (§13).
pub const SIG_FIGS: u32 = 9;

/// The file CmdStan reads the bound data from, in the run directory.
pub const DATA_FILE: &str = "data.json";

/// What CmdStan is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// NUTS: the posterior, by MCMC.
    Sample,
    /// The posterior mode: one "draw".
    Optimize,
    /// Pathfinder's approximate draws.
    Pathfinder,
}

impl Method {
    /// The name in the configuration and on CmdStan's command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Sample => "sample",
            Method::Optimize => "optimize",
            Method::Pathfinder => "pathfinder",
        }
    }
}

/// Every setting a run is made with, read from the configuration with its
/// defaults filled in — and recorded on the instance and in `config.json`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunSettings {
    /// `sample`, `optimize` or `pathfinder`.
    pub method: Method,
    /// Chains (for `pathfinder`, paths).
    pub chains: u32,
    /// How many of them run at once, within the node's budget.
    pub parallel_chains: u32,
    /// Warmup iterations per chain.
    pub iter_warmup: u64,
    /// Sampling iterations per chain (for `pathfinder`, draws).
    pub iter_sampling: u64,
    /// Keep every `thin`-th iteration.
    pub thin: u64,
    /// NUTS's target acceptance rate.
    pub adapt_delta: f64,
    /// NUTS's maximum tree depth.
    pub max_treedepth: u32,
    /// The seed: the configured one, or a fresh one drawn for this fit.
    pub seed: u32,
    /// Initial values are uniform(−init, init) on the unconstrained scale.
    pub init: f64,
    /// Whether warmup iterations are written out.
    pub save_warmup: bool,
    /// A fit running longer is killed.
    pub max_runtime_minutes: u64,
    /// Significant figures in CmdStan's output: [`SIG_FIGS`].
    pub sig_figs: u32,
}

impl RunSettings {
    /// The settings `config` asks for, with `fresh_seed` drawn when it names
    /// none. The ranges were checked on save ([`StanProvider`]'s
    /// `validate`); a value of the wrong type here is refused all the same.
    ///
    /// [`StanProvider`]: crate::StanProvider
    pub fn from_config(config: &Attrs, fresh_seed: impl FnOnce() -> u32) -> Result<RunSettings> {
        let method = match config.get(METHOD).and_then(Json::as_str).map(str::trim) {
            None | Some("") | Some("sample") => Method::Sample,
            Some("optimize") => Method::Optimize,
            Some("pathfinder") => Method::Pathfinder,
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`{METHOD}` must be `sample`, `optimize` or `pathfinder`, not `{other}`"
                )));
            }
        };
        let int = |key: &str, default: u64| -> Result<u64> {
            match config.get(key).filter(|v| !v.is_null()) {
                None => Ok(default),
                Some(v) => v.as_u64().ok_or_else(|| {
                    Error::invalid(format!("`{key}` must be a whole number, got {v}"))
                }),
            }
        };
        let float = |key: &str, default: f64| -> Result<f64> {
            match config.get(key).filter(|v| !v.is_null()) {
                None => Ok(default),
                Some(v) => v
                    .as_f64()
                    .ok_or_else(|| Error::invalid(format!("`{key}` must be a number, got {v}"))),
            }
        };
        let small = |key: &str, default: u64| -> Result<u32> {
            u32::try_from(int(key, default)?)
                .map_err(|_| Error::invalid(format!("`{key}` is too large")))
        };
        let chains = small(CHAINS, 4)?.max(1);
        let parallel_chains = small(PARALLEL_CHAINS, u64::from(chains))?.clamp(1, chains);
        let seed = match config.get(SEED).filter(|v| !v.is_null()) {
            None => fresh_seed(),
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "`{SEED}` must be a whole number from 0 to {}, got {v}",
                        u32::MAX
                    ))
                })?,
        };
        Ok(RunSettings {
            method,
            chains,
            parallel_chains,
            iter_warmup: int(ITER_WARMUP, 1000)?,
            iter_sampling: int(ITER_SAMPLING, 1000)?.max(1),
            thin: int(THIN, 1)?.max(1),
            adapt_delta: float(ADAPT_DELTA, 0.8)?,
            max_treedepth: small(MAX_TREEDEPTH, 10)?,
            seed,
            init: float(INIT, 2.0)?,
            save_warmup: config
                .get(SAVE_WARMUP)
                .and_then(Json::as_bool)
                .unwrap_or(false),
            max_runtime_minutes: int(MAX_RUNTIME_MINUTES, 60)?.max(1),
            sig_figs: SIG_FIGS,
        })
    }

    /// How many processes the run is: a chain each for `sample`, one for the
    /// others.
    pub fn processes(&self) -> u32 {
        match self.method {
            Method::Sample => self.chains,
            Method::Optimize | Method::Pathfinder => 1,
        }
    }

    /// How long the fit may run.
    pub fn max_runtime(&self) -> Duration {
        Duration::from_secs(self.max_runtime_minutes * 60)
    }

    /// Iterations a chain runs, counting warmup — what its progress counts to.
    pub fn total_iterations(&self) -> u64 {
        self.iter_warmup + self.iter_sampling
    }

    /// How often CmdStan prints its progress line: about every 2 %.
    fn refresh(&self) -> u64 {
        (self.total_iterations() / 50).max(1)
    }

    /// The CSV a chain writes, relative to the run directory.
    pub fn csv_name(chain: u32) -> String {
        format!("chain-{chain}.csv")
    }

    /// The file a chain's output is kept in, relative to the run directory.
    pub fn log_name(chain: u32) -> String {
        format!("chain-{chain}.log")
    }

    /// CmdStan's command line for `chain` (from 1), relative to the run
    /// directory it is started in. cmdstanpy's order: the common arguments,
    /// then the method's.
    pub fn arguments(&self, chain: u32) -> Vec<String> {
        let mut args = vec![
            format!("id={chain}"),
            "random".to_owned(),
            format!("seed={}", self.seed),
            "data".to_owned(),
            format!("file={DATA_FILE}"),
            format!("init={}", number(self.init)),
            "output".to_owned(),
            format!("file={}", Self::csv_name(chain)),
            format!("refresh={}", self.refresh()),
            format!("sig_figs={}", self.sig_figs),
            format!("method={}", self.method.as_str()),
        ];
        match self.method {
            Method::Sample => args.extend([
                format!("num_samples={}", self.iter_sampling),
                format!("num_warmup={}", self.iter_warmup),
                format!("save_warmup={}", u8::from(self.save_warmup)),
                format!("thin={}", self.thin),
                "algorithm=hmc".to_owned(),
                "engine=nuts".to_owned(),
                format!("max_depth={}", self.max_treedepth),
                "adapt".to_owned(),
                "engaged=1".to_owned(),
                format!("delta={}", number(self.adapt_delta)),
            ]),
            Method::Optimize => {}
            Method::Pathfinder => args.extend([
                format!("num_paths={}", self.chains),
                format!("num_draws={}", self.iter_sampling),
                format!("num_psis_draws={}", self.iter_sampling),
            ]),
        }
        args
    }
}

/// `2` rather than `2.0`, `0.95` as itself.
fn number(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// CmdStan's progress line, read: `Iteration:  400 / 2000 [ 20%]  (Warmup)`
/// as (400, 2000, warmup).
pub fn parse_iteration(line: &str) -> Option<(u64, u64, ChainPhase)> {
    let rest = line.trim().strip_prefix("Iteration:")?;
    let (count, rest) = rest.split_once('/')?;
    let iteration = count.trim().parse().ok()?;
    let total = rest.split_whitespace().next()?.parse().ok()?;
    let phase = if rest.contains("(Warmup)") {
        ChainPhase::Warmup
    } else if rest.contains("(Sampling)") {
        ChainPhase::Sampling
    } else {
        return None;
    };
    Some((iteration, total, phase))
}

/// The node's process budget (§13): every chain of every fit holds one permit
/// while it runs. Owned by the server; cheap to clone.
#[derive(Debug, Clone)]
pub struct ProcessBudget(Arc<Semaphore>, usize);

impl ProcessBudget {
    /// A budget of `processes` (at least one).
    pub fn new(processes: usize) -> ProcessBudget {
        let processes = processes.max(1);
        ProcessBudget(Arc::new(Semaphore::new(processes)), processes)
    }

    /// How many chain processes the budget allows at once, in all.
    pub fn processes(&self) -> usize {
        self.1
    }

    /// The default: half the available CPUs, at least one.
    pub fn for_this_machine() -> ProcessBudget {
        let cpus = std::thread::available_parallelism().map_or(2, usize::from);
        ProcessBudget::new(cpus / 2)
    }

    /// Permits free right now.
    pub fn available(&self) -> usize {
        self.0.available_permits()
    }
}

/// One chain that finished.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainRun {
    /// The chain, from 1.
    pub chain: u32,
    /// Its CSV.
    pub csv: PathBuf,
    /// Its output.
    pub log: PathBuf,
    /// How long its process ran.
    pub wall: Duration,
}

/// Where the fit's chains have got to, reported whole on every change.
struct Tracker<'a> {
    progress: Mutex<Progress>,
    report: &'a (dyn Fn(&Progress) + Sync),
}

impl Tracker<'_> {
    fn update(&self, f: impl FnOnce(&mut Progress)) {
        let snapshot = {
            let mut progress = match self.progress.lock() {
                Ok(p) => p,
                Err(poisoned) => poisoned.into_inner(),
            };
            f(&mut progress);
            progress.clone()
        };
        (self.report)(&snapshot);
    }

    /// A chain is waiting for the budget: the fit is `queued` if nothing of
    /// it is running yet.
    fn queued(&self) {
        self.update(|p| {
            if p.chains.is_empty() {
                p.stage = FitStage::Queued;
            }
        });
    }

    fn chain(&self, chain: u32, iteration: u64, total: u64, phase: ChainPhase) {
        self.update(|p| {
            p.stage = FitStage::Sampling;
            let now = ChainProgress {
                chain,
                iteration,
                total,
                phase,
            };
            match p.chains.iter_mut().find(|c| c.chain == chain) {
                Some(c) => *c = now,
                None => {
                    p.chains.push(now);
                    p.chains.sort_by_key(|c| c.chain);
                }
            }
        });
    }
}

/// A compiled program and the directory it runs in.
pub struct Run<'a> {
    /// The executable.
    pub exe: &'a Path,
    /// Where the program was compiled from — the paths its messages carry.
    pub src: &'a Path,
    /// The main file's store path, for sentences.
    pub program: &'a str,
    /// The run directory, holding [`DATA_FILE`]; the CSVs and logs are
    /// written into it.
    pub dir: &'a Path,
    /// The settings.
    pub settings: &'a RunSettings,
}

/// Run every process of `run`, reporting progress through `report`, stopped
/// by `watch`. Answers each chain's files in chain order, or the first
/// failure's sentence.
pub async fn run_chains(
    run: &Run<'_>,
    budget: &ProcessBudget,
    watch: &Watch<'_>,
    report: &(dyn Fn(&Progress) + Sync),
) -> Result<Vec<ChainRun>> {
    let tracker = Tracker {
        progress: Mutex::new(Progress::stage(FitStage::Sampling)),
        report,
    };
    let local = Semaphore::new(run.settings.parallel_chains as usize);
    let chains = (1..=run.settings.processes())
        .map(|chain| run_chain(run, chain, budget, &local, watch, &tracker));
    futures::future::try_join_all(chains).await
}

async fn run_chain(
    run: &Run<'_>,
    chain: u32,
    budget: &ProcessBudget,
    local: &Semaphore,
    watch: &Watch<'_>,
    tracker: &Tracker<'_>,
) -> Result<ChainRun> {
    let _this_fit = tokio::select! {
        permit = local.acquire() => permit.map_err(|_| Error::msg("the fit's chain limit closed"))?,
        why = watch.until_stopped() => return Err(stop_error(why, watch)),
    };
    let _this_node: OwnedSemaphorePermit = match Arc::clone(&budget.0).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            tracker.queued();
            tokio::select! {
                permit = Arc::clone(&budget.0).acquire_owned() => {
                    permit.map_err(|_| Error::msg("the process budget closed"))?
                }
                why = watch.until_stopped() => return Err(stop_error(why, watch)),
            }
        }
    };
    let total = run.settings.total_iterations();
    tracker.chain(chain, 0, total, ChainPhase::Warmup);

    let log_path = run.dir.join(RunSettings::log_name(chain));
    let mut log = tokio::fs::File::create(&log_path)
        .await
        .with_context(|| format!("creating {}", log_path.display()))?;
    let started = Instant::now();
    let mut child = process::command(run.exe)
        .args(run.settings.arguments(chain))
        .current_dir(run.dir)
        .spawn()
        .with_context(|| format!("running the compiled program {}", run.exe.display()))?;
    let group = KillGroupOnDrop(child.id());
    let mut lines = process::output_lines(&mut child);
    let mut tail = Tail::default();
    loop {
        tokio::select! {
            line = lines.recv() => match line {
                Some(line) => {
                    log.write_all(line.as_bytes()).await.ok();
                    log.write_all(b"\n").await.ok();
                    if let Some((iteration, total, phase)) = parse_iteration(&line) {
                        tracker.chain(chain, iteration, total, phase);
                    }
                    tail.push(&line);
                }
                None => break,
            },
            why = watch.until_stopped() => {
                group.kill();
                return Err(stop_error(why, watch));
            }
        }
    }
    let status = tokio::select! {
        status = child.wait() => status.context("waiting for a chain")?,
        why = watch.until_stopped() => {
            group.kill();
            return Err(stop_error(why, watch));
        }
    };
    log.flush().await.ok();
    let wall = started.elapsed();
    let csv = run.dir.join(RunSettings::csv_name(chain));
    if !status.success() {
        return Err(chain_failure(run, chain, &status, &tail));
    }
    if !csv.is_file() {
        return Err(Error::msg(format!(
            "chain {chain} finished but wrote no output file; the last lines of its output:\n{}",
            map_src(&tail.text(), run.src)
        )));
    }
    Ok(ChainRun {
        chain,
        csv,
        log: log_path,
        wall,
    })
}

/// The sentence for a chain that exited with a failure (§13).
fn chain_failure(
    run: &Run<'_>,
    chain: u32,
    status: &std::process::ExitStatus,
    tail: &Tail,
) -> Error {
    let lines: Vec<String> = tail.lines().map(|l| map_src(l, run.src)).collect();
    let find = |needle: &str| lines.iter().find(|l| l.contains(needle));

    // A data error the binder somehow missed: CmdStan names the variable.
    if let Some(line) = lines
        .iter()
        .find(|l| l.contains("processing stage=data initialization"))
    {
        let variable = line
            .split("variable name=")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .map(str::trim);
        return Error::invalid(match variable {
            Some(name) => format!(
                "CmdStan refused the data for `{name}` — which the binder should have caught, \
                 so this is worth reporting: {}",
                line.trim()
            ),
            None => format!("CmdStan refused the data: {}", line.trim()),
        });
    }
    if let Some(line) = find("Error reading").or_else(|| find("Error in JSON parsing")) {
        return Error::invalid(format!(
            "CmdStan could not read the data file: {}",
            line.trim()
        ));
    }
    // Every initial value rejected.
    if find("Initialization failed").is_some()
        || lines
            .iter()
            .any(|l| l.contains("Initialization between") && l.contains("failed"))
    {
        let reason = lines
            .iter()
            .position(|l| l.contains("Rejecting initial value"))
            .and_then(|at| lines.get(at + 1))
            .map(|l| {
                format!(
                    " The last reason given: {}.",
                    l.trim().trim_end_matches('.')
                )
            })
            .unwrap_or_default();
        return Error::invalid(format!(
            "chain {chain} could not find a starting point: every initial value it tried was \
             rejected.{reason} Try `{INIT}: 0`, tighter priors, or check the constraints the \
             parameters declare"
        ));
    }
    let what = match run.settings.method {
        Method::Sample => format!("chain {chain} of `{}`", run.program),
        method => format!("`{}` ({})", run.program, method.as_str()),
    };
    Error::msg(format!(
        "{what} exited with {status}; the last lines of its output:\n{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(config: Json) -> RunSettings {
        let config: Attrs = serde_json::from_value(config).expect("config");
        RunSettings::from_config(&config, || 1234).expect("settings")
    }

    #[test]
    fn the_defaults_are_the_documented_ones_with_a_fresh_seed() {
        let s = settings(json!({}));
        assert_eq!(s.method, Method::Sample);
        assert_eq!((s.chains, s.parallel_chains), (4, 4));
        assert_eq!((s.iter_warmup, s.iter_sampling, s.thin), (1000, 1000, 1));
        assert_eq!((s.adapt_delta, s.max_treedepth), (0.8, 10));
        assert_eq!((s.seed, s.init, s.save_warmup), (1234, 2.0, false));
        assert_eq!((s.max_runtime_minutes, s.sig_figs), (60, 9));
        assert_eq!(settings(json!({ "seed": 7 })).seed, 7);
    }

    /// `--stan-max-processes` (§20) is a budget's size, and a size of nothing
    /// would queue every fit for ever — so it is one at least.
    #[test]
    fn a_budget_is_the_processes_it_was_given_and_never_none() {
        let budget = ProcessBudget::new(6);
        assert_eq!((budget.processes(), budget.available()), (6, 6));
        assert_eq!(ProcessBudget::new(0).processes(), 1);
        assert!(ProcessBudget::for_this_machine().processes() >= 1);
    }

    #[test]
    fn a_chain_is_run_with_cmdstans_arguments() {
        let s = settings(json!({
            "chains": 2, "iter_warmup": 100, "iter_sampling": 200, "thin": 2,
            "adapt_delta": 0.95, "max_treedepth": 12, "seed": 42, "init": 0.5,
            "save_warmup": true,
        }));
        assert_eq!(
            s.arguments(2).join(" "),
            "id=2 random seed=42 data file=data.json init=0.5 output file=chain-2.csv refresh=6 \
             sig_figs=9 method=sample num_samples=200 num_warmup=100 save_warmup=1 thin=2 \
             algorithm=hmc engine=nuts max_depth=12 adapt engaged=1 delta=0.95"
        );
        assert_eq!(s.processes(), 2);
    }

    #[test]
    fn optimize_and_pathfinder_are_one_process_each() {
        let o = settings(json!({ "method": "optimize", "seed": 1 }));
        assert_eq!(o.processes(), 1);
        assert!(
            o.arguments(1)
                .join(" ")
                .ends_with("sig_figs=9 method=optimize")
        );
        let p = settings(
            json!({ "method": "pathfinder", "chains": 4, "iter_sampling": 500, "seed": 1 }),
        );
        assert_eq!(p.processes(), 1);
        assert!(
            p.arguments(1)
                .join(" ")
                .ends_with("method=pathfinder num_paths=4 num_draws=500 num_psis_draws=500")
        );
        let bad: Attrs = serde_json::from_value(json!({ "method": "advi" })).expect("config");
        assert!(RunSettings::from_config(&bad, || 0).is_err());
    }

    #[test]
    fn progress_lines_are_read_and_everything_else_is_not() {
        assert_eq!(
            parse_iteration("Iteration:  400 / 2000 [ 20%]  (Warmup)"),
            Some((400, 2000, ChainPhase::Warmup))
        );
        assert_eq!(
            parse_iteration("Iteration: 2000 / 2000 [100%]  (Sampling)"),
            Some((2000, 2000, ChainPhase::Sampling))
        );
        assert_eq!(
            parse_iteration("Gradient evaluation took 1e-05 seconds"),
            None
        );
        assert_eq!(parse_iteration("Iteration: x / 2000 [ 0%] (Warmup)"), None);
    }

    #[test]
    fn parallel_chains_cannot_exceed_the_chains() {
        assert_eq!(
            settings(json!({ "chains": 2, "parallel_chains": 9 })).parallel_chains,
            2
        );
        assert_eq!(
            settings(json!({ "chains": 4, "parallel_chains": 1 })).parallel_chains,
            1
        );
    }
}

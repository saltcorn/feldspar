//! Compiling and running against a **fake model executable** (TODO 4.7): a
//! fake `make` that "compiles" a program by copying a shell script into
//! place, and that script, which reads CmdStan's arguments, prints CmdStan's
//! progress lines and writes a canned CmdStan CSV — or sleeps, or fails the
//! way a real model fails, as the test asks. Everything of Phase 4 that does
//! not need a C++ compiler is exercised here: the compile cache, the
//! arguments, progress, the process budget, cancel, the timeout, the failure
//! sentences, the raw run and its discard. The same fit against a real
//! CmdStan is in `real_cmdstan.rs`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sc_error::{Error, Result};
use sc_files::{FileStore, LocalFileStore};
use sc_model::{
    ChainPhase, DrawSeries, FitContext, FitProgress, FitStage, InstanceId, ModelProvider,
    PosteriorInput, PosteriorMethod, PosteriorResult, Progress,
};
use sc_stan::cmdstan::{CmdStan, Source};
use sc_stan::compile::CompileCache;
use sc_stan::run::{ProcessBudget, Run, RunSettings, run_chains};
use sc_stan::{StanProvider, Watch, config_keys};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

const PROGRAM: &str = "parameters { real mu; vector[2] theta; }\n\
                       model { mu ~ normal(0, 1); theta ~ normal(0, 1); }\n";

/// A directory with a fake CmdStan, a fake `make`, a fake model, a program
/// store and a runs store — and the files the fakes leave behind.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        let dir =
            std::env::temp_dir().join(format!("sc-stan-runner-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["cmdstan", "programs", "runs/.git", "scratch", "cache"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("programs/m.stan"), PROGRAM).unwrap();
        let fixture = Fixture { dir };
        fixture.script("make", &fake_make(&fixture.dir));
        fixture.script("fake-model", &fake_model(&fixture.dir));
        fixture
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// What the fake model does: `ok`, `slow` (ok, a second late, noting any
    /// overlap with another chain), `sleep`, `fail`, `init` or `data`.
    fn behave(&self, behaviour: &str) {
        std::fs::write(self.dir.join("behaviour"), behaviour).unwrap();
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    fn cmdstan(&self) -> CmdStan {
        CmdStan {
            dir: self.dir.join("cmdstan"),
            version: "2.36.0".parse().unwrap(),
            source: Source::Flag,
        }
    }

    fn provider(&self, budget: usize) -> StanProvider {
        let programs: Arc<dyn FileStore> =
            Arc::new(LocalFileStore::new("programs", self.dir.join("programs")).unwrap());
        let runs: Arc<dyn FileStore> =
            Arc::new(LocalFileStore::new("runs", self.dir.join("runs")).unwrap());
        let lookup = move |name: &str| -> Result<Arc<dyn FileStore>> {
            match name {
                "programs" => Ok(Arc::clone(&programs)),
                "runs" => Ok(Arc::clone(&runs)),
                other => Err(Error::not_found(format!("no file store `{other}`"))),
            }
        };
        StanProvider::new(Arc::new(lookup), Ok(self.cmdstan()))
            .with_scratch(self.dir.join("scratch"))
            .with_cache(CompileCache::new(
                self.dir.join("cache"),
                self.dir.join("make"),
            ))
            .with_budget(ProcessBudget::new(budget))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A `make` that logs its arguments and copies the fake model to the target
/// (its last argument) — or, when `make-fails` exists, says what `stanc` says
/// about a program that does not type-check, naming the file by the absolute
/// path it was laid out at.
fn fake_make(dir: &Path) -> String {
    format!(
        r#"#!/bin/sh
here='{dir}'
echo "$*" >> "$here/make.log"
pwd > "$here/make-cwd.log"
for a in "$@"; do target="$a"; case "$a" in STANCFLAGS=--include-paths=*) src="${{a#STANCFLAGS=--include-paths=}}";; esac; done
echo "--- Translating Stan model to C++ code ---"
if [ -e "$here/make-fails" ]; then
  echo "Semantic error in '$src/m.stan', line 2, column 7 to column 9:"
  echo "Identifier 'mu' not in scope."
  echo "make: *** [make/program:50: $target.hpp] Error 1"
  exit 2
fi
cp "$here/fake-model" "$target"
"#,
        dir = dir.display()
    )
}

/// The fake model: CmdStan's command line in, CmdStan's output out.
fn fake_model(dir: &Path) -> String {
    format!(
        r##"#!/bin/sh
here='{dir}'
echo "$*" >> "$here/calls.log"
id=1; out=; prev=; samples=10; warmup=10; save_warmup=0; opt=0
for a in "$@"; do
  case "$a" in
    id=*) id="${{a#id=}}";;
    file=*) if [ "$prev" != data ]; then out="${{a#file=}}"; fi;;
    num_samples=*) samples="${{a#num_samples=}}";;
    num_warmup=*) warmup="${{a#num_warmup=}}";;
    save_warmup=*) save_warmup="${{a#save_warmup=}}";;
    method=optimize) samples=1; warmup=0; opt=1;;
  esac
  prev="$a"
done
env | cut -d= -f1 | sort | tr '\n' ' ' > "$here/env-$id.log"
pwd > "$here/cwd-$id.log"
echo $$ > "$here/pid-$id.log"
behaviour=$(cat "$here/behaviour" 2>/dev/null || echo ok)
total=$((warmup + samples))
echo "Gradient evaluation took 1e-05 seconds"
case "$behaviour" in
  sleep) echo "Iteration: 1 / $total [  0%]  (Warmup)"; sleep 30;;
  fail) echo "chain $id is unwell"; echo "something went wrong"; exit 70;;
  init)
    echo "Rejecting initial value:"
    echo "  Log probability evaluates to log(0), i.e. negative infinity."
    echo "Initialization between (-2, 2) failed after 100 attempts."
    echo "Initialization failed."
    exit 70;;
  data)
    echo "Exception: mismatch in dimension declared and found in context; processing stage=data initialization; variable name=y; base type=double"
    exit 70;;
  slow)
    if [ -e "$here/running" ]; then echo overlap >> "$here/overlap"; fi
    touch "$here/running"; sleep 1; rm -f "$here/running";;
esac
i=1
while [ $i -le $total ]; do
  if [ $i -le $warmup ]; then ph=Warmup; else ph=Sampling; fi
  echo "Iteration: $i / $total [ 50%]  ($ph)"
  i=$((i+1))
done
if [ $opt = 1 ]; then
  echo "    Iter      log prob        ||dx||      ||grad||       alpha      alpha0  # evals  Notes "
  echo "       6      -5.00402   0.000103557   2.55062e-07           1           1        9   "
  echo "Optimization terminated normally: "
fi
rows=$samples
if [ "$save_warmup" = 1 ]; then rows=$((samples + warmup)); fi
{{
  echo "# model = fake_model"
  echo "lp__,accept_stat__,mu,theta.1,theta.2"
  echo "# Adaptation terminated"
  echo "# Step size = 0.$id"
  echo "# Diagonal elements of inverse mass matrix:"
  echo "# 1, 0.5, 0.25"
  n=1
  while [ $n -le $rows ]; do echo "-$n,0.9,$id.$n,1,2"; n=$((n+1)); done
  echo "# "
  echo "#  Elapsed Time: 0.01 seconds (Warm-up)"
  echo "#                0.02 seconds (Sampling)"
  echo "#                0.03 seconds (Total)"
}} > "$out"
"##,
        dir = dir.display()
    )
}

/// Every progress report, in order.
#[derive(Default)]
struct Reports(Mutex<Vec<Progress>>);

impl FitProgress for Reports {
    fn report(&self, progress: &Progress) {
        self.0.lock().unwrap().push(progress.clone());
    }
}

impl Reports {
    fn stages(&self) -> Vec<FitStage> {
        let mut stages: Vec<FitStage> = Vec::new();
        for p in self.0.lock().unwrap().iter() {
            if stages.last() != Some(&p.stage) {
                stages.push(p.stage);
            }
        }
        stages
    }
}

fn config(extra: Json) -> Attrs {
    let mut config = json!({
        config_keys::PROGRAM_STORE: "programs",
        config_keys::PROGRAM: "m.stan",
        config_keys::CHAINS: 2,
        config_keys::ITER_WARMUP: 3,
        config_keys::ITER_SAMPLING: 4,
        config_keys::SEED: 7,
    });
    for (k, v) in extra.as_object().unwrap() {
        config[k] = v.clone();
    }
    serde_json::from_value(config).unwrap()
}

fn input(instance: InstanceId) -> PosteriorInput {
    PosteriorInput {
        model: "Radon".into(),
        instance: Some(instance),
        datasets: vec![],
        interface: None,
        data: json!({ "N": 2, "y": [1.5, 2.5] }),
        coordinates: Default::default(),
        unread: Default::default(),
    }
}

async fn fit(
    provider: &StanProvider,
    config: &Attrs,
    reports: &Reports,
    cancel: &AtomicBool,
) -> Result<PosteriorResult> {
    provider
        .fit_posterior(
            &input(InstanceId::new()),
            config,
            &FitContext::new(reports, cancel),
        )
        .await
}

fn series<'a>(draws: &'a [DrawSeries], label: &str, chain: u32) -> &'a DrawSeries {
    draws
        .iter()
        .find(|s| s.label() == label && s.chain == chain && !s.warmup)
        .unwrap_or_else(|| panic!("no {label} in chain {chain}"))
}

#[tokio::test]
async fn a_fit_compiles_once_runs_every_chain_with_cmdstans_arguments_and_reads_the_draws() {
    let fx = Fixture::new("fit");
    fx.behave("ok");
    let provider = fx.provider(4);
    let reports = Reports::default();
    let cancel = AtomicBool::new(false);
    let result = fit(&provider, &config(json!({})), &reports, &cancel)
        .await
        .unwrap();

    // Compiled once, by our make, from the CmdStan directory, with nothing
    // but the include path and the target.
    let make = fx.read("make.log");
    assert_eq!(make.lines().count(), 1, "{make}");
    let args: Vec<&str> = make.split_whitespace().collect();
    assert_eq!(args.len(), 2, "{make}");
    assert!(args[0].starts_with("STANCFLAGS=--include-paths="));
    assert!(args[1].ends_with("/src/m"), "{make}");
    assert_eq!(
        fx.read("make-cwd.log").trim(),
        fx.dir.join("cmdstan").display().to_string()
    );

    // One process per chain, with CmdStan's arguments, in the run directory.
    let calls = fx.read("calls.log");
    let mut calls: Vec<&str> = calls.lines().collect();
    calls.sort_unstable();
    assert_eq!(
        calls,
        [
            "id=1 random seed=7 data file=data.json init=2 output file=chain-1.csv refresh=1 \
             sig_figs=9 method=sample num_samples=4 num_warmup=3 save_warmup=0 thin=1 \
             algorithm=hmc engine=nuts max_depth=10 adapt engaged=1 delta=0.8",
            "id=2 random seed=7 data file=data.json init=2 output file=chain-2.csv refresh=1 \
             sig_figs=9 method=sample num_samples=4 num_warmup=3 save_warmup=0 thin=1 \
             algorithm=hmc engine=nuts max_depth=10 adapt engaged=1 delta=0.8",
        ]
    );
    assert!(fx.read("cwd-1.log").trim().contains("/scratch/stan-run-"));
    // The environment is scrubbed: what the shell adds itself, and what we
    // pass on, and nothing else.
    let allowed = [
        "PATH", "HOME", "TMPDIR", "CMDSTAN", "CXX", "PWD", "OLDPWD", "SHLVL", "_",
    ];
    for var in fx.read("env-1.log").split_whitespace() {
        assert!(
            allowed.contains(&var) || var.starts_with("STAN_"),
            "the model saw `{var}`"
        );
    }

    // The draws: every column of every chain, elements by name.
    assert_eq!(result.draws.len(), 2 * 5);
    assert_eq!(
        series(&result.draws, "mu", 2).draws,
        vec![2.1, 2.2, 2.3, 2.4]
    );
    assert_eq!(series(&result.draws, "theta[2]", 1).draws, vec![2.0; 4]);
    assert!(result.draws.iter().all(|s| !s.warmup));

    // Progress: compiling, then sampling with each chain's iterations, then
    // summarising; every chain reached its last iteration.
    assert_eq!(
        reports.stages(),
        [
            FitStage::Compiling,
            FitStage::Sampling,
            FitStage::Summarising
        ]
    );
    let last_sampling = reports
        .0
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|p| p.stage == FitStage::Sampling)
        .cloned()
        .unwrap();
    assert_eq!(last_sampling.chains.len(), 2);
    for chain in &last_sampling.chains {
        assert_eq!((chain.iteration, chain.total), (7, 7));
        assert_eq!(chain.phase, ChainPhase::Sampling);
    }

    // What the host is told about the run, to diagnose it by.
    assert_eq!(result.run.method, PosteriorMethod::Mcmc);
    assert_eq!(result.run.max_treedepth, Some(10));
    assert_eq!(result.run.wall_seconds.len(), 2);
    assert_eq!(result.run.iterations, None);

    // The state: the seed, the toolchain, the cache key, the program, each
    // chain's adaptation and timing; no run.
    assert_eq!(result.state["chains"][1]["chain"], 2);
    assert_eq!(result.state["chains"][1]["step_size"], 0.2);
    assert_eq!(result.state["chains"][0]["timing"]["Total"], 0.03);
    assert_eq!(result.state["chains"][0]["timing"]["Warm-up"], 0.01);
    assert_eq!(result.state["seed"], 7);
    assert_eq!(result.state["cmdstan"], "2.36.0");
    assert_eq!(result.state["compiled"].as_str().unwrap().len(), 64);
    assert_eq!(result.state["program"]["files"][0]["text"], PROGRAM);
    assert!(result.state["run"].is_null());
    // And the scratch run directory is gone.
    assert_eq!(
        std::fs::read_dir(fx.dir.join("scratch")).unwrap().count(),
        0
    );

    // A second fit of the same program is a cache hit: no make, no compiling.
    let again = Reports::default();
    fit(&provider, &config(json!({})), &again, &cancel)
        .await
        .unwrap();
    assert_eq!(fx.read("make.log").lines().count(), 1);
    assert!(!again.stages().contains(&FitStage::Compiling));
    // And an edited program is a new key.
    std::fs::write(
        fx.dir.join("programs/m.stan"),
        format!("{PROGRAM}// edited\n"),
    )
    .unwrap();
    fit(&provider, &config(json!({})), &Reports::default(), &cancel)
        .await
        .unwrap();
    assert_eq!(fx.read("make.log").lines().count(), 2);
}

#[tokio::test]
async fn warmup_draws_are_kept_as_warmup_when_asked_for() {
    let fx = Fixture::new("warmup");
    fx.behave("ok");
    let result = fit(
        &fx.provider(4),
        &config(json!({ "chains": 1, "save_warmup": true })),
        &Reports::default(),
        &AtomicBool::new(false),
    )
    .await
    .unwrap();
    let warm = result
        .draws
        .iter()
        .find(|s| s.label() == "mu" && s.warmup)
        .unwrap();
    assert_eq!(warm.draws, vec![1.1, 1.2, 1.3]);
    assert_eq!(
        series(&result.draws, "mu", 1).draws,
        vec![1.4, 1.5, 1.6, 1.7]
    );
}

#[tokio::test]
async fn two_fits_share_a_process_budget_of_one_and_the_second_says_queued() {
    let fx = Fixture::new("budget");
    fx.behave("slow");
    let provider = fx.provider(1);
    let cancel = AtomicBool::new(false);
    let (a, b) = (Reports::default(), Reports::default());
    let one = config(json!({ "chains": 1 }));
    let (ra, rb) = tokio::join!(
        fit(&provider, &one, &a, &cancel),
        fit(&provider, &one, &b, &cancel)
    );
    ra.unwrap();
    rb.unwrap();
    assert_eq!(fx.read("overlap"), "", "two chains ran at once");
    assert!(
        a.stages().contains(&FitStage::Queued) || b.stages().contains(&FitStage::Queued),
        "neither fit said it was queued: {:?} / {:?}",
        a.stages(),
        b.stages()
    );
    // One compile between them: the second waited for it and found it cached.
    assert_eq!(fx.read("make.log").lines().count(), 1);
}

/// Whether the process is gone (or a zombie nobody has reaped yet).
fn gone(pid: &str) -> bool {
    match std::fs::read_to_string(format!("/proc/{}/status", pid.trim())) {
        Err(_) => true,
        Ok(status) => status
            .lines()
            .any(|l| l.starts_with("State:") && l.contains('Z')),
    }
}

#[tokio::test]
async fn cancel_kills_a_sleeping_chain_and_says_so() {
    let fx = Fixture::new("cancel");
    fx.behave("sleep");
    let provider = fx.provider(4);
    let reports = Reports::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let pid_file = fx.dir.join("pid-1.log");
    tokio::spawn(async move {
        // Once the chain is running.
        while !pid_file.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        flag.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let err = fit(
        &provider,
        &config(json!({ "chains": 1 })),
        &reports,
        &cancel,
    )
    .await
    .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the chain was not killed"
    );
    assert!(
        err.to_string().contains("the fit was cancelled after"),
        "{err}"
    );
    let pid = fx.read("pid-1.log");
    for _ in 0..50 {
        if gone(&pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone(&pid), "chain process {pid} is still running");
    assert_eq!(
        std::fs::read_dir(fx.dir.join("scratch")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn a_run_past_its_time_is_killed_and_says_how_long_it_ran() {
    let fx = Fixture::new("timeout");
    fx.behave("sleep");
    let dir = fx.dir.join("run");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("data.json"), "{}").unwrap();
    let never = || false;
    let watch = Watch::new(Duration::from_millis(300), &never);
    let config = config(json!({ "chains": 1 }));
    let settings = RunSettings::from_config(&config, || 1).unwrap();
    let exe = fx.dir.join("fake-model");
    let started = Instant::now();
    let err = run_chains(
        &Run {
            exe: &exe,
            src: &fx.dir,
            program: "m.stan",
            dir: &dir,
            settings: &settings,
        },
        &ProcessBudget::new(1),
        &watch,
        &|_: &Progress| {},
    )
    .await
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        err.to_string().contains(
            "the fit was stopped after 0 seconds, because it ran longer than its \
                       `max_runtime_minutes`"
        ),
        "{err}"
    );
}

#[tokio::test]
async fn a_failing_chain_is_a_sentence_with_its_last_lines() {
    let fx = Fixture::new("fail");
    let provider = fx.provider(4);
    let cancel = AtomicBool::new(false);
    let one = config(json!({ "chains": 1 }));

    fx.behave("fail");
    let err = fit(&provider, &one, &Reports::default(), &cancel)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(
            "chain 1 of `m.stan` exited with exit status: 70; the last lines of its output:"
        ),
        "{err}"
    );
    assert!(
        err.ends_with("chain 1 is unwell\nsomething went wrong"),
        "{err}"
    );

    fx.behave("init");
    let err = fit(&provider, &one, &Reports::default(), &cancel)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(
            "chain 1 could not find a starting point: every initial value it tried was rejected. \
             The last reason given: Log probability evaluates to log(0), i.e. negative infinity. \
             Try `init: 0`"
        ),
        "{err}"
    );

    fx.behave("data");
    let err = fit(&provider, &one, &Reports::default(), &cancel)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("CmdStan refused the data for `y`"), "{err}");
}

#[tokio::test]
async fn a_program_stanc_refuses_is_quoted_with_its_store_path() {
    let fx = Fixture::new("stanc");
    std::fs::write(fx.dir.join("make-fails"), "").unwrap();
    let err = fit(
        &fx.provider(4),
        &config(json!({})),
        &Reports::default(),
        &AtomicBool::new(false),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid: stanc refused the program `m.stan`:\n\
         Semantic error in 'm.stan', line 2, column 7 to column 9:\n\
         Identifier 'mu' not in scope."
    );
    // Nothing half-built is left in the cache.
    assert_eq!(std::fs::read_dir(fx.dir.join("cache")).unwrap().count(), 0);
}

fn gunzip(bytes: &[u8]) -> String {
    use std::io::Read as _;
    let mut out = String::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_string(&mut out)
        .unwrap();
    out
}

#[tokio::test]
async fn a_raw_run_is_published_to_the_runs_store_and_discarded_with_the_instance() {
    let fx = Fixture::new("publish");
    fx.behave("ok");
    let provider = fx.provider(4);
    let instance = InstanceId::new();
    let cfg = config(json!({ "chains": 2, "runs_store": "runs" }));
    let result = provider
        .fit_posterior(
            &input(instance),
            &cfg,
            &FitContext::new(&Reports::default(), &AtomicBool::new(false)),
        )
        .await
        .unwrap();

    let path = format!("stan-runs/Radon/{instance}");
    assert_eq!(
        result.state["run"],
        json!({ "store": "runs", "path": path })
    );
    let runs = fx.dir.join("runs");
    // A git store gets a `.gitignore` of everything in the runs directory.
    assert_eq!(
        std::fs::read_to_string(runs.join("stan-runs/.gitignore")).unwrap(),
        "*\n"
    );
    let run = runs.join(&path);
    let mut files: Vec<String> = Vec::new();
    let mut stack = vec![run.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files.push(p.strip_prefix(&run).unwrap().display().to_string());
            }
        }
    }
    files.sort();
    assert_eq!(
        files,
        [
            "chain-1.csv.gz",
            "chain-1.log",
            "chain-2.csv.gz",
            "chain-2.log",
            "config.json",
            "coordinates.json",
            "data.json",
            "program/m.stan",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(run.join("program/m.stan")).unwrap(),
        PROGRAM
    );
    let data: Json =
        serde_json::from_str(&std::fs::read_to_string(run.join("data.json")).unwrap()).unwrap();
    assert_eq!(data, json!({ "N": 2, "y": [1.5, 2.5] }));
    let config: Json =
        serde_json::from_str(&std::fs::read_to_string(run.join("config.json")).unwrap()).unwrap();
    assert_eq!(config["settings"]["seed"], 7);
    assert_eq!(config["cmdstan"], "2.36.0");
    let csv = gunzip(&std::fs::read(run.join("chain-2.csv.gz")).unwrap());
    assert!(
        csv.starts_with(
            "# model = fake_model\nlp__,accept_stat__,mu,theta.1,theta.2\n\
             # Adaptation terminated\n# Step size = 0.2\n\
             # Diagonal elements of inverse mass matrix:\n# 1, 0.5, 0.25\n-1,0.9,2.1,1,2\n"
        ),
        "{csv}"
    );
    assert!(
        std::fs::read_to_string(run.join("chain-1.log"))
            .unwrap()
            .contains("Iteration: 7 / 7")
    );
    // The scratch copy is gone once published.
    assert_eq!(
        std::fs::read_dir(fx.dir.join("scratch")).unwrap().count(),
        0
    );

    // Deleting the instance discards the run.
    provider.discard(&result.state).await.unwrap();
    assert!(!run.exists());
    assert!(runs.join("stan-runs/.gitignore").exists());
}

#[tokio::test]
async fn optimize_and_pathfinder_run_as_one_process_through_the_same_runner() {
    let fx = Fixture::new("methods");
    fx.behave("ok");
    let provider = fx.provider(4);
    let cancel = AtomicBool::new(false);

    let result = fit(
        &provider,
        &config(json!({ "method": "optimize", "chains": 4 })),
        &Reports::default(),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(fx.read("calls.log").lines().count(), 1);
    assert!(fx.read("calls.log").trim_end().ends_with("method=optimize"));
    assert_eq!(series(&result.draws, "mu", 1).draws, vec![1.1]);
    assert_eq!(result.state["method"], "optimize");
    assert_eq!(result.run.method, PosteriorMethod::Mode);
    assert_eq!(result.run.iterations, Some(6));
    assert_eq!(result.run.max_treedepth, None);

    std::fs::remove_file(fx.dir.join("calls.log")).unwrap();
    let result = fit(
        &provider,
        &config(json!({ "method": "pathfinder", "chains": 4 })),
        &Reports::default(),
        &cancel,
    )
    .await
    .unwrap();
    let calls = fx.read("calls.log");
    assert_eq!(calls.lines().count(), 1);
    assert!(calls.contains("method=pathfinder num_paths=4 num_draws=4 num_psis_draws=4"));
    assert!(result.draws.iter().all(|s| s.chain == 1));
    assert_eq!(result.run.method, PosteriorMethod::Approximation);
}

#[tokio::test]
async fn a_variable_the_host_will_not_read_is_skipped_as_the_csv_is_read() {
    let fx = Fixture::new("unread");
    fx.behave("ok");
    let provider = fx.provider(4);
    let cancel = AtomicBool::new(false);
    let mut unread = input(InstanceId::new());
    unread.unread.insert("theta".to_owned());
    let result = provider
        .fit_posterior(
            &unread,
            &config(json!({})),
            &FitContext::new(&Reports::default(), &cancel),
        )
        .await
        .unwrap();
    let variables: std::collections::BTreeSet<&str> =
        result.draws.iter().map(|s| s.variable.as_str()).collect();
    assert_eq!(
        variables,
        std::collections::BTreeSet::from(["lp__", "accept_stat__", "mu"])
    );
}

#[test]
fn the_draw_plan_counts_what_the_settings_will_store() {
    let fx = Fixture::new("plan");
    let provider = fx.provider(1);
    let plan = |extra| provider.draw_plan(&config(extra)).unwrap().unwrap();
    let sample = plan(json!({
        "chains": 4, "iter_warmup": 500, "iter_sampling": 1000, "thin": 2, "save_warmup": true,
    }));
    assert_eq!(
        (
            sample.chains,
            sample.draws_per_chain,
            sample.sampler_variables
        ),
        (4, 250 + 500, 7)
    );
    let optimum = plan(json!({ "method": "optimize", "chains": 4 }));
    assert_eq!((optimum.chains, optimum.draws_per_chain), (1, 1));
    let pathfinder = plan(json!({ "method": "pathfinder", "iter_sampling": 300 }));
    assert_eq!(
        (
            pathfinder.chains,
            pathfinder.draws_per_chain,
            pathfinder.sampler_variables
        ),
        (1, 300, 3)
    );
}

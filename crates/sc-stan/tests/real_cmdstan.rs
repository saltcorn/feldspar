//! Against a real CmdStan: `#[ignore]`d, because building one takes a while.
//!
//! The CmdStan is the one `discover` finds from this process's environment —
//! `$CMDSTAN`, else the newest built `~/.cmdstan/cmdstan-*` — so on a machine
//! where `feldspar cmdstan install` has been run these need nothing set:
//!
//! ```text
//! cargo test -p sc-stan -- --ignored
//! CMDSTAN=/opt/cmdstan-2.40.0 cargo test -p sc-stan -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use sc_stan::cmdstan::{CmdStan, Locations, Toolchain, discover};
use sc_stan::program::{Program, ProgramFile};
use sc_stan::stanc::check_program;

use crate::programs::{AR1, BYM2, EIGHT_SCHOOLS, EVERY_TYPE, RADON};

/// The CmdStan on this machine, ready to compile with; a failure says why not.
fn cmdstan() -> CmdStan {
    let locations = Locations::from_env(None);
    let found = discover(&locations).unwrap_or_else(|e| {
        panic!("{e}\n(run `feldspar cmdstan install`, or set $CMDSTAN, to run these tests)")
    });
    assert!(found.built(), "{} is not built", found.dir.display());
    let tools = Toolchain::find(&locations);
    assert!(tools.ready(), "{:?}", tools.missing());
    found
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-stan-real-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The draws of `column` in a CmdStan output CSV (comment lines skipped).
fn column(csv: &Path, name: &str) -> Vec<f64> {
    let text = std::fs::read_to_string(csv).unwrap();
    let mut rows = text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty());
    let header: Vec<&str> = rows.next().unwrap().split(',').collect();
    let at = header.iter().position(|h| *h == name).unwrap();
    rows.map(|row| row.split(',').nth(at).unwrap().parse().unwrap())
        .collect()
}

/// The `bernoulli` example that ships with CmdStan, compiled outside the
/// CmdStan tree (as the compile cache will, TODO §13) and sampled: 2 successes
/// in 10 trials under a uniform prior is Beta(3, 9), whose mean is 0.25.
#[test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
fn the_bernoulli_example_compiles_and_samples() {
    let cmdstan = cmdstan();
    let work = scratch("bernoulli");
    let example = cmdstan.dir.join("examples").join("bernoulli");
    for file in ["bernoulli.stan", "bernoulli.data.json"] {
        std::fs::copy(example.join(file), work.join(file)).unwrap();
    }

    let exe = work.join(format!("bernoulli{}", std::env::consts::EXE_SUFFIX));
    run(Command::new("make").arg(&exe).current_dir(&cmdstan.dir));
    assert!(exe.is_file());

    let output = work.join("output.csv");
    let stdout = run(Command::new(&exe)
        .arg("sample")
        .arg("num_samples=1000")
        .arg("random")
        .arg("seed=4711")
        .arg("data")
        .arg(format!(
            "file={}",
            work.join("bernoulli.data.json").display()
        ))
        .arg("output")
        .arg(format!("file={}", output.display()))
        .current_dir(&work));
    // The progress lines TODO §13's runner will parse.
    assert!(
        stdout.contains("Iteration: 2000 / 2000 [100%]  (Sampling)"),
        "{stdout}"
    );

    let theta = column(&output, "theta");
    assert_eq!(theta.len(), 1000);
    let mean = theta.iter().sum::<f64>() / theta.len() as f64;
    assert!(
        (mean - 0.25).abs() < 0.03,
        "posterior mean of theta was {mean}"
    );
    std::fs::remove_dir_all(&work).unwrap();
}

/// Every program of `programs.rs` through the real `stanc`: each accepted, and
/// its `--info` in agreement with our parse (TODO 2.4). A disagreement here is
/// a bug in our parser that the fake-`stanc` tests cannot see.
#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn stanc_agrees_with_our_reading_of_every_test_program() {
    let cmdstan = cmdstan();
    let work = scratch("stanc-agrees");
    for (name, text) in [
        ("radon", RADON),
        ("eight_schools", EIGHT_SCHOOLS),
        ("ar1", AR1),
        ("bym2", BYM2),
        ("every_type", EVERY_TYPE),
    ] {
        let program = Program::from_files(
            "models",
            vec![ProgramFile::new(format!("models/{name}.stan"), text)],
        )
        .unwrap();
        let checked = check_program(&cmdstan.stanc(), &program, &work)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(checked.interface, program.interface().unwrap(), "{name}");
    }

    // With an include, laid out and rewritten so stanc finds it where we did.
    let program = Program::from_files(
        "models",
        vec![
            ProgramFile::new(
                "models/m.stan",
                "functions {\n#include lib/f.stan\n}\ndata {\n#include data.stan\n}\n\
                 parameters { real mu; }\nmodel { mu ~ normal(f(0.0), 1); }\n",
            ),
            ProgramFile::new(
                "models/lib/f.stan",
                "#include ../g.stan\nreal f(real x) { return g(x); }\n",
            ),
            ProgramFile::new("models/g.stan", "real g(real x) { return x; }\n"),
            ProgramFile::new("models/data.stan", "int<lower=0> N;\nvector[N] y;\n"),
        ],
    )
    .unwrap();
    check_program(&cmdstan.stanc(), &program, &work)
        .await
        .unwrap();

    // A refusal, in stanc's words, naming the included file by its store path.
    let broken = Program::from_files(
        "models",
        vec![
            ProgramFile::new("models/m.stan", "functions {\n#include lib/f.stan\n}\n"),
            ProgramFile::new("models/lib/f.stan", "real f(real x) { return x }\n"),
        ],
    )
    .unwrap();
    let err = check_program(&cmdstan.stanc(), &broken, &work)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("stanc refused the program `models/m.stan`"),
        "{err}"
    );
    assert!(err.contains("'models/lib/f.stan', line 1"), "{err}");
    assert!(!err.contains(&work.display().to_string()), "{err}");
    std::fs::remove_dir_all(&work).unwrap();
}

/// A program fitted **through the provider** against the real CmdStan (TODO
/// Phase 4): compiled into the cache (with an `#include`, so the include path
/// reaches `stanc` through `make`), run one process per chain with our
/// command line, the draws read back by column name, and the raw run
/// published. Then `optimize` and `pathfinder` over the same executable,
/// which proves their command lines too — and a compile error in `stanc`'s
/// words.
#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn a_program_is_compiled_sampled_and_published_through_the_provider() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use sc_files::{FileStore, LocalFileStore};
    use sc_model::{FitContext, FitStage, InstanceId, ModelProvider, PosteriorInput, Progress};
    use sc_stan::compile::CompileCache;
    use sc_stan::run::ProcessBudget;
    use sc_stan::{StanProvider, config_keys};
    use serde_json::json;

    struct Stages(std::sync::Mutex<Vec<FitStage>>);
    impl sc_model::FitProgress for Stages {
        fn report(&self, p: &Progress) {
            let mut stages = self.0.lock().unwrap();
            if stages.last() != Some(&p.stage) {
                stages.push(p.stage);
            }
        }
    }

    let cmdstan = cmdstan();
    let work = scratch("provider");
    let programs = work.join("programs");
    std::fs::create_dir_all(programs.join("lib")).unwrap();
    std::fs::write(
        programs.join("bernoulli.stan"),
        "data {\n#include lib/data.stan\n}\nparameters { real<lower=0, upper=1> theta; }\n\
         model { theta ~ beta(1, 1); y ~ bernoulli(theta); }\n",
    )
    .unwrap();
    std::fs::write(
        programs.join("lib/data.stan"),
        "int<lower=0> N;\narray[N] int<lower=0, upper=1> y;\n",
    )
    .unwrap();
    std::fs::create_dir_all(work.join("runs")).unwrap();
    let stores: Vec<Arc<dyn FileStore>> = vec![
        Arc::new(LocalFileStore::new("programs", &programs).unwrap()),
        Arc::new(LocalFileStore::new("runs", work.join("runs")).unwrap()),
    ];
    let lookup = move |name: &str| -> sc_error::Result<Arc<dyn FileStore>> {
        stores
            .iter()
            .find(|s| s.name() == name)
            .cloned()
            .ok_or_else(|| sc_error::Error::not_found(name.to_owned()))
    };
    let provider = StanProvider::new(Arc::new(lookup), Ok(cmdstan))
        .with_scratch(work.join("scratch"))
        .with_cache(CompileCache::new(work.join("cache"), "make"))
        .with_budget(ProcessBudget::new(2));
    let input = |instance| PosteriorInput {
        model: "Bernoulli".into(),
        instance: Some(instance),
        datasets: vec![],
        interface: None,
        data: json!({ "N": 10, "y": [0, 1, 0, 0, 0, 0, 0, 0, 0, 1] }),
        coordinates: Default::default(),
        unread: Default::default(),
    };
    let config = |extra: serde_json::Value| -> sc_types::Attrs {
        let mut c = json!({
            config_keys::PROGRAM_STORE: "programs",
            config_keys::PROGRAM: "bernoulli.stan",
            config_keys::CHAINS: 4,
            config_keys::ITER_WARMUP: 500,
            config_keys::ITER_SAMPLING: 1000,
            config_keys::SEED: 4711,
            config_keys::RUNS_STORE: "runs",
        });
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        serde_json::from_value(c).unwrap()
    };
    let cancel = AtomicBool::new(false);

    let stages = Stages(Default::default());
    let instance = InstanceId::new();
    let fitted = provider
        .fit_posterior(
            &input(instance),
            &config(json!({})),
            &FitContext::new(&stages, &cancel),
        )
        .await
        .unwrap();
    // Two of the four chains wait for the budget of two, but the fit is
    // sampling, not queued, once its first chain runs.
    assert_eq!(
        *stages.0.lock().unwrap(),
        [
            FitStage::Compiling,
            FitStage::Sampling,
            FitStage::Summarising
        ]
    );
    let theta: Vec<f64> = fitted
        .draws
        .iter()
        .filter(|s| s.variable == "theta")
        .flat_map(|s| s.draws.iter().copied())
        .collect();
    assert_eq!(theta.len(), 4000);
    let mean = theta.iter().sum::<f64>() / theta.len() as f64;
    // Beta(3, 9): mean 0.25.
    assert!(
        (mean - 0.25).abs() < 0.02,
        "posterior mean of theta was {mean}"
    );
    // The sampler's columns are variables like any other.
    for sampler in ["lp__", "divergent__", "treedepth__", "energy__"] {
        assert_eq!(
            fitted
                .draws
                .iter()
                .filter(|s| s.variable == sampler)
                .count(),
            4,
            "{sampler}"
        );
    }
    // The host's summary and diagnostics over CmdStan's real output: a
    // Beta(3, 9) posterior sampled well mixes, and says nothing is wrong.
    let coordinates = sc_model::Coordinates::default();
    let labeller = sc_model::Labeller::new(&sc_types::Attrs::new(), &coordinates).unwrap();
    let report =
        sc_model::diagnose_posterior(&fitted.draws, &fitted.run, None, &labeller, 1000).unwrap();
    let sc_model::Metrics::Posterior(m) = &report.metrics else {
        panic!("{:?}", report.metrics);
    };
    assert_eq!((m.chains, m.draws_per_chain), (4, 1000));
    assert_eq!(m.divergent, 0);
    assert!(m.max_rhat < 1.01, "{m:?}");
    assert!(m.min_ess_bulk > 400.0 && m.min_ess_tail > 400.0, "{m:?}");
    assert!(m.ebfmi.iter().all(|e| *e > 0.3), "{m:?}");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let sc_model::ParameterBlock::Table { columns, rows, .. } = &report.tables[0] else {
        panic!("{:?}", report.tables);
    };
    assert_eq!(columns[0], "mean");
    let summary_mean = rows[0].cells[0].as_f64().unwrap();
    assert!((summary_mean - mean).abs() < 1e-12);
    // Each chain's adaptation and CmdStan's own timing.
    let chain = &fitted.state["chains"][0];
    assert!(chain["step_size"].as_f64().unwrap() > 0.0, "{chain}");
    assert!(chain["timing"]["Total"].as_f64().is_some(), "{chain}");

    let run = work
        .join("runs/stan-runs/Bernoulli")
        .join(instance.to_string());
    for file in [
        "chain-4.csv.gz",
        "chain-1.log",
        "data.json",
        "program/lib/data.stan",
    ] {
        assert!(run.join(file).is_file(), "{file} was not published");
    }

    // The same executable, two other methods.
    let optimum = provider
        .fit_posterior(
            &input(InstanceId::new()),
            &config(json!({ "method": "optimize" })),
            &FitContext::new(&Stages(Default::default()), &cancel),
        )
        .await
        .unwrap();
    let mode = optimum
        .draws
        .iter()
        .find(|s| s.variable == "theta")
        .unwrap();
    // Beta(3, 9)'s mode is 0.2.
    assert!((mode.draws[0] - 0.2).abs() < 1e-3, "mode {:?}", mode.draws);
    assert_eq!(optimum.run.method, sc_model::PosteriorMethod::Mode);
    assert!(
        optimum.run.iterations.is_some_and(|n| n > 0),
        "{:?}",
        optimum.run
    );
    let approx = provider
        .fit_posterior(
            &input(InstanceId::new()),
            &config(json!({ "method": "pathfinder" })),
            &FitContext::new(&Stages(Default::default()), &cancel),
        )
        .await
        .unwrap();
    assert!(
        approx
            .draws
            .iter()
            .any(|s| s.variable == "theta" && s.draws.len() == 1000)
    );
    let report =
        sc_model::diagnose_posterior(&approx.draws, &approx.run, None, &labeller, 1000).unwrap();
    assert!(
        matches!(report.metrics, sc_model::Metrics::PosteriorApproximation(_)),
        "{:?}",
        report.metrics
    );
    assert_eq!(
        std::fs::read_dir(work.join("cache")).unwrap().count(),
        1,
        "the program was compiled more than once"
    );

    // A program stanc refuses, through make, in stanc's words.
    std::fs::write(
        programs.join("broken.stan"),
        "parameters { real theta; }\nmodel { thetaa ~ normal(0, 1); }\n",
    )
    .unwrap();
    let err = provider
        .fit_posterior(
            &input(InstanceId::new()),
            &config(json!({ "program": "broken.stan" })),
            &FitContext::new(&Stages(Default::default()), &cancel),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("stanc refused the program `broken.stan`"),
        "{err}"
    );
    assert!(err.contains("'broken.stan', line 2"), "{err}");
    std::fs::remove_dir_all(&work).unwrap();
}

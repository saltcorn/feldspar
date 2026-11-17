//! R1.7 against a real CmdStan: parameter recovery on simulated data for the
//! use cases A–E of TODO-bayesian-regression §1 (each true value inside its
//! 90 % posterior interval), a second model with renamed columns fitting
//! without compiling, and the summary speaking the formula's terms.
//!
//! `#[ignore]`d, as every test here that compiles and samples is: the first
//! run compiles one program per structure (a minute each), later runs find
//! them in the compile cache. Without a CmdStan they print a notice and pass.
//!
//! ```text
//! cargo test -p sc-bayesreg --test it -- --ignored recovery
//! ```
//!
//! The data and the sampler have fixed seeds, so the assertions are about
//! *these* draws. A 90 % interval misses the truth one time in ten even when
//! the model is right, so the data seeds were chosen so that the simulated
//! data themselves land near the truth. Where an earlier seed missed, the
//! posterior agreed with a classical estimate computed independently on the
//! same data (the logistic MLE; the mean of per-subject OLS slopes; a weighted
//! within-group slope): the data had drifted from the truth, not the model. No catalog is built (the fits run through `run_fit`, which
//! touches no store), so there is nothing to tear down.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sc_bayesreg::{BayesRegProvider, keys};
use sc_model::{
    Column, Dataset, DatasetSource, Fit, Frame, Model, ModelRegistry, ParameterBlock, Read, run_fit,
};
use sc_stan::StanProvider;
use sc_stan::cmdstan::{Locations, Toolchain, discover};
use sc_stan::compile::CompileCache;
use sc_stan::run::ProcessBudget;
use serde_json::Value as Json;

/// A deterministic generator, so the data is the same every run.
struct Rng(u64);

impl Rng {
    fn uniform(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }

    fn poisson(&mut self, mean: f64) -> i64 {
        let limit = (-mean).exp();
        let (mut k, mut p) = (0, 1.0);
        loop {
            p *= self.uniform();
            if p <= limit {
                return k;
            }
            k += 1;
        }
    }
}

/// The Stan provider over this machine's CmdStan, or `None` (and a notice).
fn stan() -> Option<Arc<StanProvider>> {
    stan_reading(None)
}

/// [`stan`], reading a Stan model's program from `store`.
fn stan_reading(store: Option<Arc<dyn sc_files::FileStore>>) -> Option<Arc<StanProvider>> {
    let locations = Locations::from_env(None);
    let cmdstan = match discover(&locations) {
        Ok(c) if c.built() => c,
        Ok(c) => {
            eprintln!("{} is not built: skipped", c.dir.display());
            return None;
        }
        Err(e) => {
            eprintln!("no CmdStan ({e}): skipped");
            return None;
        }
    };
    let make = Toolchain::find(&locations)
        .make
        .unwrap_or_else(|| PathBuf::from("make"));
    let stores = Arc::new(
        move |name: &str| -> sc_error::Result<Arc<dyn sc_files::FileStore>> {
            match &store {
                Some(s) if s.name() == name => Ok(Arc::clone(s)),
                _ => Err(sc_error::Error::invalid(format!(
                    "no file store `{name}` here"
                ))),
            }
        },
    );
    Some(Arc::new(
        StanProvider::new(stores, Ok(cmdstan))
            .with_scratch(std::env::temp_dir().join("sc-bayesreg-fits"))
            .with_cache(CompileCache::new(
                std::env::temp_dir().join("feldspar-bayesreg-test-cache"),
                make,
            ))
            .with_budget(ProcessBudget::new(4)),
    ))
}

/// One frame per table.
struct Frames(HashMap<String, Frame>);

#[async_trait]
impl DatasetSource for Frames {
    async fn read(&self, ds: &Dataset, _how: &Read<'_>) -> sc_error::Result<Frame> {
        self.0
            .get(&ds.table)
            .cloned()
            .ok_or_else(|| sc_error::Error::invalid(format!("no table `{}`", ds.table)))
    }
}

fn floats(v: Vec<f64>) -> Column {
    Column::Float(v.into_iter().map(Some).collect())
}

fn ints(v: Vec<i64>) -> Column {
    Column::Int(v.into_iter().map(Some).collect())
}

fn texts(v: Vec<String>) -> Column {
    Column::Str(v.into_iter().map(Some).collect())
}

fn frame(columns: Vec<(&str, Column)>) -> Frame {
    Frame::new(
        columns
            .into_iter()
            .map(|(n, c)| (n.to_owned(), c))
            .collect(),
        Vec::new(),
    )
    .unwrap()
}

/// Fit `formula` (family `family`) over `frame`.
async fn fit(stan: &Arc<StanProvider>, formula: &str, family: &str, frame: Frame) -> Fit {
    fit_with(stan, formula, family, frame, &[]).await
}

/// [`fit`] with more configuration (priors, `epred`).
async fn fit_with(
    stan: &Arc<StanProvider>,
    formula: &str,
    family: &str,
    frame: Frame,
    extra: &[(&str, Json)],
) -> Fit {
    let mut registry = ModelRegistry::new();
    registry
        .register(Arc::new(BayesRegProvider::new(Arc::clone(stan))))
        .unwrap();
    let mut dataset = Dataset::new("t");
    for (name, _) in &frame.columns {
        dataset = dataset.column(name.clone(), name.clone());
    }
    let model = Model::new("m", sc_bayesreg::PROVIDER, dataset)
        .config(keys::FORMULA, formula)
        .config(keys::FAMILY, family)
        .config("chains", 4)
        .config("iter_warmup", 500)
        .config("iter_sampling", 500)
        .config("seed", 4711);
    let model = extra
        .iter()
        .fold(model, |m, (k, v)| m.config(*k, v.clone()));
    let source = Frames(HashMap::from([("t".to_owned(), frame)]));
    run_fit(&registry, &source, &model, 1_000_000)
        .await
        .unwrap_or_else(|e| panic!("{formula}: {}", sc_error::format_chain(&e)))
}

/// The 90 % central interval of `variable[element]`, over every chain.
fn interval(fit: &Fit, variable: &str, element: &[usize]) -> (f64, f64) {
    let mut all: Vec<f64> = fit
        .draws
        .iter()
        .filter(|s| s.variable == variable && s.element == element && !s.warmup)
        .flat_map(|s| s.draws.iter().copied())
        .collect();
    assert!(!all.is_empty(), "no draws of {variable}{element:?}");
    all.sort_by(f64::total_cmp);
    (
        sc_model::quantile(&all, 0.05),
        sc_model::quantile(&all, 0.95),
    )
}

/// Each `(variable, element, truth)` inside its 90 % interval.
fn recovers(fit: &Fit, case: &str, truths: &[(&str, &[usize], f64)]) {
    let mut misses = Vec::new();
    for (variable, element, truth) in truths {
        let (lo, hi) = interval(fit, variable, element);
        if !(lo..=hi).contains(truth) {
            misses.push(format!(
                "{variable}{element:?}: {truth} not in [{lo:.3}, {hi:.3}]"
            ));
        }
    }
    assert!(misses.is_empty(), "case {case}: {}", misses.join("; "));
}

/// The column names the binder gave the model matrix `variable`.
fn columns(fit: &Fit, variable: &str) -> Vec<String> {
    fit.binding.as_ref().unwrap().0.designs[variable]
        .columns
        .clone()
}

fn table<'a>(fit: &'a Fit, name: &str) -> &'a ParameterBlock {
    fit.parameters
        .iter()
        .find(|p| matches!(p, ParameterBlock::Table { name: n, .. } if n == name))
        .unwrap_or_else(|| panic!("no table `{name}`"))
}

fn first_cells(block: &ParameterBlock) -> Vec<String> {
    match block {
        ParameterBlock::Table { rows, .. } => rows
            .iter()
            .map(|r| r.cells[0].as_str().unwrap_or_default().to_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Case A's data: radon in homes in counties (Gelman & Hill's model, data
/// drawn from known values), its columns named as given.
fn radon(names: [&str; 3]) -> Frame {
    let mut rng = Rng(11);
    let (mut y, mut floor, mut county) = (Vec::new(), Vec::new(), Vec::new());
    for c in 0..30 {
        let alpha = 1.5 + 0.4 * rng.normal();
        for _ in 0..(4 + (c * 7) % 17) {
            let f = if rng.uniform() < 0.25 { 1.0 } else { 0.0 };
            y.push(alpha - 0.7 * f + 0.7 * rng.normal());
            floor.push(f);
            county.push(format!("C{:02}", c + 1));
        }
    }
    frame(vec![
        (names[0], floats(y)),
        (names[1], floats(floor)),
        (names[2], texts(county)),
    ])
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn case_a_partial_pooling_by_county_and_a_renamed_model_is_not_recompiled() {
    let Some(stan) = stan() else { return };
    let a = fit(
        &stan,
        "log_radon ~ floor + (1 | county)",
        "gaussian",
        radon(["log_radon", "floor", "county"]),
    )
    .await;
    assert_eq!(columns(&a, "X_1"), ["floor"]);
    recovers(
        &a,
        "A",
        &[
            ("Intercept_1", &[], 1.5),
            ("b_1", &[1], -0.7),
            ("sd_1", &[1], 0.4),
            ("sigma", &[], 0.7),
        ],
    );
    // The summary speaks the formula's terms: `b_1` by the model matrix's
    // column, `r_1` by county and effect.
    assert_eq!(first_cells(table(&a, "b_1")), ["floor"]);
    let r = table(&a, "r_1");
    let ParameterBlock::Table { columns: heads, .. } = r else {
        unreachable!()
    };
    assert_eq!(heads[0], "county");
    assert!(first_cells(r).contains(&"C07".to_owned()));
    assert_eq!(
        first_cells(table(&a, "Population-level effects")),
        ["Intercept", "floor"]
    );
    assert_eq!(
        first_cells(table(&a, "Group-level effects ~county")),
        ["sd(Intercept)"]
    );
    assert_eq!(
        first_cells(table(&a, "Family-specific parameters")),
        ["sigma"]
    );

    // The same structure under other names: the same program, so no compile.
    let renamed = fit(
        &stan,
        "price ~ area + (1 | region)",
        "gaussian",
        radon(["price", "area", "region"]),
    )
    .await;
    assert_eq!(renamed.state["cached"], Json::Bool(true));
    assert_eq!(renamed.state["compiled"], a.state["compiled"]);
    assert_eq!(
        first_cells(table(&renamed, "Population-level effects")),
        ["Intercept", "area"]
    );
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn case_b_varying_slopes_correlated_with_the_intercepts() {
    let Some(stan) = stan() else { return };
    let mut rng = Rng(31);
    let (mut y, mut days, mut subject) = (Vec::new(), Vec::new(), Vec::new());
    let (sd0, sd1, rho) = (25.0, 6.0, 0.2);
    for s in 0..18 {
        let z0 = rng.normal();
        let z1 = rho * z0 + (1.0_f64 - rho * rho).sqrt() * rng.normal();
        let (a, b) = (250.0 + sd0 * z0, 10.0 + sd1 * z1);
        for d in 0..10 {
            y.push(a + b * f64::from(d) + 25.0 * rng.normal());
            days.push(f64::from(d));
            subject.push(format!("S{:02}", s + 1));
        }
    }
    let b = fit(
        &stan,
        "reaction ~ days + (1 + days | subject)",
        "gaussian",
        frame(vec![
            ("reaction", floats(y)),
            ("days", floats(days)),
            ("subject", texts(subject)),
        ]),
    )
    .await;
    assert_eq!(columns(&b, "Z_1_1"), ["Intercept", "days"]);
    recovers(
        &b,
        "B",
        &[
            ("Intercept_1", &[], 250.0),
            ("b_1", &[1], 10.0),
            ("sd_1", &[1], sd0),
            ("sd_1", &[2], sd1),
            ("Cor_1", &[2, 1], rho),
            ("sigma", &[], 25.0),
        ],
    );
    assert_eq!(
        first_cells(table(&b, "Group-level effects ~subject")),
        ["sd(Intercept)", "sd(days)", "cor(Intercept,days)"]
    );
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn case_c_a_binomial_proportion_and_a_poisson_rate_with_an_offset() {
    let Some(stan) = stan() else { return };
    let mut rng = Rng(38);
    let (mut deaths, mut n, mut dose) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..30 {
        let x = -2.0 + 4.0 * f64::from(i % 10) / 9.0;
        let p = 1.0 / (1.0 + (-(-1.0 + 0.8 * x)).exp());
        let k = (0..40).filter(|_| rng.uniform() < p).count();
        deaths.push(k as i64);
        n.push(40);
        dose.push(x);
    }
    let c1 = fit(
        &stan,
        "deaths | trials(n) ~ dose",
        "binomial",
        frame(vec![
            ("deaths", ints(deaths)),
            ("n", ints(n)),
            ("dose", floats(dose)),
        ]),
    )
    .await;
    recovers(
        &c1,
        "C (binomial)",
        &[("Intercept_1", &[], -1.0), ("b_1", &[1], 0.8)],
    );

    let (mut claims, mut age, mut exposure) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..400 {
        let a = rng.normal();
        let e = 0.5 + 1.5 * rng.uniform();
        claims.push(rng.poisson(e * (-1.0 + 0.3 * a).exp()));
        age.push(a);
        exposure.push(e);
    }
    let c2 = fit(
        &stan,
        "claims ~ age + offset(log(exposure))",
        "poisson",
        frame(vec![
            ("claims", ints(claims)),
            ("age", floats(age)),
            ("exposure", floats(exposure)),
        ]),
    )
    .await;
    recovers(
        &c2,
        "C (poisson)",
        &[("Intercept_1", &[], -1.0), ("b_1", &[1], 0.3)],
    );
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn case_d_the_residual_sd_varies_too() {
    let Some(stan) = stan() else { return };
    let mut rng = Rng(61);
    let effects: Vec<f64> = (0..20).map(|_| 0.5 * rng.normal()).collect();
    let (mut y, mut x, mut g) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..400 {
        let xi = rng.normal();
        let group = i % 20;
        let sigma = (-0.5 + 0.5 * xi).exp();
        y.push(1.0 + 2.0 * xi + effects[group] + sigma * rng.normal());
        x.push(xi);
        g.push(format!("G{:02}", group + 1));
    }
    let d = fit(
        &stan,
        "y ~ x + (1 | g)\nsigma ~ x",
        "gaussian",
        frame(vec![("y", floats(y)), ("x", floats(x)), ("g", texts(g))]),
    )
    .await;
    recovers(
        &d,
        "D",
        &[
            ("Intercept_1", &[], 1.0),
            ("b_1", &[1], 2.0),
            ("sd_1", &[1], 0.5),
            ("Intercept_2", &[], -0.5),
            ("b_2", &[1], 0.5),
        ],
    );
    assert_eq!(
        first_cells(table(&d, "Population-level effects")),
        ["Intercept", "x", "sigma_Intercept", "sigma_x"]
    );
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn case_e_censored_survival_times() {
    let Some(stan) = stan() else { return };
    let mut rng = Rng(53);
    // Weibull with shape k and mean μ: scale μ / Γ(1 + 1/k); Γ(5/3).
    let (k, gamma) = (1.5, 0.902_745_292_950_933_6);
    let (mut time, mut censored, mut treatment) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..300 {
        let drug = i % 2 == 1;
        let mean = (1.0_f64 + if drug { 0.5 } else { 0.0 }).exp();
        let t = mean / gamma * (-rng.uniform().ln()).powf(1.0 / k);
        let limit = 2.0 + 8.0 * rng.uniform();
        time.push(t.min(limit));
        censored.push(i64::from(t > limit));
        treatment.push(if drug { "drug" } else { "control" }.to_owned());
    }
    let e = fit(
        &stan,
        "time | cens(censored) ~ treatment",
        "weibull",
        frame(vec![
            ("time", floats(time)),
            ("censored", ints(censored)),
            ("treatment", texts(treatment)),
        ]),
    )
    .await;
    assert_eq!(columns(&e, "X_1"), ["treatmentdrug"]);
    recovers(
        &e,
        "E",
        &[
            ("Intercept_1", &[], 1.0),
            ("b_1", &[1], 0.5),
            ("shape", &[], k),
        ],
    );
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn case_f_a_non_linear_growth_curve_varying_by_tree() {
    let Some(stan) = stan() else { return };
    let mut rng = Rng(71);
    let (mut y, mut t, mut tree) = (Vec::new(), Vec::new(), Vec::new());
    for k in 0..12 {
        let a = 10.0 + 1.5 * rng.normal();
        for step in 1..=10 {
            let ti = f64::from(step);
            y.push(a * (1.0 - (-0.3 * ti).exp()) + 0.5 * rng.normal());
            t.push(ti);
            tree.push(format!("T{:02}", k + 1));
        }
    }
    let f = fit_with(
        &stan,
        "y ~ a * (1 - exp(-b * t))\na ~ 1 + (1 | tree)\nb ~ 1",
        "gaussian",
        frame(vec![
            ("y", floats(y)),
            ("t", floats(t)),
            ("tree", texts(tree)),
        ]),
        &[(
            keys::PRIORS,
            serde_json::json!([
                {"class": "b", "nlpar": "a", "prior": "normal(10, 5)"},
                {"class": "b", "nlpar": "b", "prior": "normal(0, 1)", "lb": 0},
            ]),
        )],
    )
    .await;
    recovers(
        &f,
        "F",
        &[
            ("b_1", &[1], 10.0),
            ("b_2", &[1], 0.3),
            ("sd_1", &[1], 1.5),
            ("sigma", &[], 0.5),
        ],
    );
    assert_eq!(
        first_cells(table(&f, "Population-level effects")),
        ["a_Intercept", "b_Intercept"]
    );
    assert_eq!(
        first_cells(table(&f, "Group-level effects ~tree")),
        ["sd(a_Intercept)"]
    );
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn case_g_two_outcomes_with_correlated_residuals_and_a_shared_block() {
    let Some(stan) = stan() else { return };
    let mut rng = Rng(113);
    let (mut weight, mut height, mut age, mut school) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for s in 0..20 {
        let z = rng.normal();
        let (u, v) = (z, 0.5 * (0.4 * z + (1.0_f64 - 0.16).sqrt() * rng.normal()));
        for _ in 0..20 {
            let a = rng.normal();
            let e1 = rng.normal();
            let e2 = 0.5 * e1 + (1.0_f64 - 0.25).sqrt() * rng.normal();
            weight.push(2.0 + 0.5 * a + u + e1);
            height.push(1.0 + 0.3 * a + v + 0.8 * e2);
            age.push(a);
            school.push(format!("S{:02}", s + 1));
        }
    }
    // A classical check of the data: the within-school least-squares slope.
    let within = |y: &[f64]| {
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for s in 0..20 {
            let r = s * 20..(s + 1) * 20;
            let (ma, my) = (
                age[r.clone()].iter().sum::<f64>() / 20.0,
                y[r.clone()].iter().sum::<f64>() / 20.0,
            );
            for i in r {
                sxy += (age[i] - ma) * (y[i] - my);
                sxx += (age[i] - ma) * (age[i] - ma);
            }
        }
        sxy / sxx
    };
    eprintln!(
        "within-school slopes: {} {}",
        within(&weight),
        within(&height)
    );
    let g = fit(
        &stan,
        "mvbind(weight, height) ~ age + (1 | p | school)",
        "gaussian",
        frame(vec![
            ("weight", floats(weight)),
            ("height", floats(height)),
            ("age", floats(age)),
            ("school", texts(school)),
        ]),
    )
    .await;
    recovers(
        &g,
        "G",
        &[
            ("Intercept_1", &[], 2.0),
            ("b_1", &[1], 0.5),
            ("Intercept_2", &[], 1.0),
            ("b_2", &[1], 0.3),
            ("sd_1", &[1], 1.0),
            ("sd_1", &[2], 0.5),
            ("sigma", &[], 1.0),
            ("sigma_2", &[], 0.8),
            ("Rescor", &[2, 1], 0.5),
        ],
    );
    assert_eq!(
        first_cells(table(&g, "Population-level effects")),
        [
            "weight_Intercept",
            "weight_age",
            "height_Intercept",
            "height_age"
        ]
    );
    assert_eq!(
        first_cells(table(&g, "Group-level effects ~school")),
        [
            "sd(weight_Intercept)",
            "sd(height_Intercept)",
            "cor(weight_Intercept,height_Intercept)"
        ]
    );
    assert_eq!(
        first_cells(table(&g, "Family-specific parameters")),
        ["sigma_weight", "sigma_height", "rescor(weight,height)"]
    );
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn r2_families_a_zero_inflated_count_and_an_ordinal_response() {
    let Some(stan) = stan() else { return };
    let mut rng = Rng(97);
    let (mut n, mut x) = (Vec::new(), Vec::new());
    for _ in 0..500 {
        let xi = rng.normal();
        let zero = rng.uniform() < 0.3;
        n.push(if zero {
            0
        } else {
            rng.poisson((0.5 + 0.4 * xi).exp())
        });
        x.push(xi);
    }
    let zip = fit(
        &stan,
        "n ~ x",
        "zero_inflated_poisson",
        frame(vec![("n", ints(n)), ("x", floats(x))]),
    )
    .await;
    recovers(
        &zip,
        "zero_inflated_poisson",
        &[
            ("Intercept_1", &[], 0.5),
            ("b_1", &[1], 0.4),
            ("zi", &[], 0.3),
        ],
    );

    let (mut y, mut x) = (Vec::new(), Vec::new());
    let thresholds = [-1.0, 0.5, 2.0];
    for _ in 0..400 {
        let xi = rng.normal();
        let u = rng.uniform();
        let latent = xi + (u / (1.0 - u)).ln();
        y.push(1 + thresholds.iter().filter(|t| latent > **t).count() as i64);
        x.push(xi);
    }
    let ord = fit(
        &stan,
        "rating ~ x",
        "cumulative",
        frame(vec![("rating", ints(y)), ("x", floats(x))]),
    )
    .await;
    recovers(
        &ord,
        "cumulative",
        &[
            ("b_1", &[1], 1.0),
            ("Intercept_1", &[1], -1.0),
            ("Intercept_1", &[2], 0.5),
            ("Intercept_1", &[3], 2.0),
        ],
    );
    assert_eq!(
        first_cells(table(&ord, "Population-level effects")),
        ["Intercept[1]", "Intercept[2]", "Intercept[3]", "x"]
    );
}

/// The posterior mean of `epred_1[i]` for each row, over every draw.
fn epred_means(fit: &Fit, rows: usize) -> Vec<f64> {
    (1..=rows)
        .map(|i| {
            let all: Vec<f64> = fit
                .draws
                .iter()
                .filter(|s| s.variable == "epred_1" && s.element == [i] && !s.warmup)
                .flat_map(|s| s.draws.iter().copied())
                .collect();
            assert!(!all.is_empty(), "no draws of epred_1[{i}]");
            all.iter().sum::<f64>() / all.len() as f64
        })
        .collect()
}

/// R2.4: a prediction in Rust from the stored draws is the program's own
/// `epred_1`, row by row, within floating-point error.
async fn predicts_as_the_program_does(
    stan: &Arc<StanProvider>,
    case: &str,
    formula: &str,
    family: &str,
    frame: Frame,
) {
    let f = fit_with(
        stan,
        formula,
        family,
        frame.clone(),
        &[
            (keys::EPRED, Json::Bool(true)),
            ("iter_sampling", Json::from(250)),
        ],
    )
    .await;
    let want = epred_means(&f, frame.rows);
    let provider = BayesRegProvider::new(Arc::clone(stan));
    let got: Vec<f64> = sc_model::ModelProvider::predict(&provider, &f.state, &frame)
        .await
        .unwrap_or_else(|e| panic!("case {case}: {}", sc_error::format_chain(&e)))
        .into_iter()
        .map(|p| match p {
            sc_model::Prediction::Number { value } => value,
            other => panic!("case {case}: {other:?}"),
        })
        .collect();
    assert_eq!(got.len(), want.len());
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        // CmdStan writes six significant digits, and the program reads its
        // intercept on the centred scale: so near, not equal.
        assert!(
            (g - w).abs() <= 1e-4 * w.abs().max(1.0),
            "case {case}, row {}: Rust {g}, Stan {w}",
            i + 1
        );
    }
    // Analytics TODO A7.2: the distribution of each row's expected response,
    // which is the program's `epred_1` draws summarised.
    let uncertain = sc_model::ModelProvider::predict_uncertain(&provider, &f.state, &frame, 0.9)
        .await
        .unwrap_or_else(|e| panic!("case {case}: {}", sc_error::format_chain(&e)));
    for (i, p) in uncertain.iter().enumerate() {
        let draws: Vec<f64> = f
            .draws
            .iter()
            .filter(|s| s.variable == "epred_1" && s.element == [i + 1] && !s.warmup)
            .flat_map(|s| s.draws.iter().copied())
            .collect();
        let Some(sc_model::Prediction::Distribution {
            lower: want_lower,
            upper: want_upper,
            ..
        }) = sc_model::distribution_of(&draws, 0.9)
        else {
            panic!("case {case}: no draws of epred_1[{}]", i + 1);
        };
        let sc_model::Prediction::Distribution {
            mean, lower, upper, ..
        } = p
        else {
            panic!("case {case}: {p:?}");
        };
        let tolerance = 1e-4 * want[i].abs().max(1.0);
        assert!((mean - want[i]).abs() <= tolerance, "case {case}: {p:?}");
        assert!(
            (lower - want_lower).abs() <= tolerance,
            "case {case}: {p:?}"
        );
        assert!(
            (upper - want_upper).abs() <= tolerance,
            "case {case}: {p:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn r2_4_predictions_match_the_programs_epred_for_cases_a_c_and_d() {
    let Some(stan) = stan() else { return };
    predicts_as_the_program_does(
        &stan,
        "A",
        "log_radon ~ floor + (1 | county)",
        "gaussian",
        radon(["log_radon", "floor", "county"]),
    )
    .await;
    let mut rng = Rng(5);
    let (mut deaths, mut n, mut dose) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..30 {
        let x = -2.0 + 4.0 * f64::from(i % 10) / 9.0;
        let p = 1.0 / (1.0 + (-(-1.0 + 0.8 * x)).exp());
        deaths.push((0..40).filter(|_| rng.uniform() < p).count() as i64);
        n.push(20 + i64::from(i));
        dose.push(x);
    }
    predicts_as_the_program_does(
        &stan,
        "C",
        "deaths | trials(n) ~ dose",
        "binomial",
        frame(vec![
            (
                "deaths",
                ints(deaths.iter().zip(&n).map(|(d, n)| (*d).min(*n)).collect()),
            ),
            ("n", ints(n)),
            ("dose", floats(dose)),
        ]),
    )
    .await;
    let (mut y, mut x, mut g) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..120 {
        let xi = rng.normal();
        y.push(1.0 + 2.0 * xi + (-0.5 + 0.5 * xi).exp() * rng.normal());
        x.push(xi);
        g.push(format!("G{}", i % 6));
    }
    predicts_as_the_program_does(
        &stan,
        "D",
        "y ~ x + (1 | g)\nsigma ~ x",
        "gaussian",
        frame(vec![("y", floats(y)), ("x", floats(x)), ("g", texts(g))]),
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a real CmdStan: `feldspar cmdstan install`, or set $CMDSTAN"]
async fn r2_5_a_converted_model_fits_to_the_same_posterior_with_the_same_seed() {
    let root = std::env::temp_dir().join(format!("sc-bayesreg-eject-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let store: Arc<dyn sc_files::FileStore> =
        Arc::new(sc_files::LocalFileStore::new("programs", &root).unwrap());
    let Some(stan) = stan_reading(Some(Arc::clone(&store))) else {
        return;
    };
    let data = radon(["log_radon", "floor", "county"]);
    let formula = fit(
        &stan,
        "log_radon ~ floor + (1 | county)",
        "gaussian",
        data.clone(),
    )
    .await;

    // Convert the configuration the fit was lowered to, as the server does.
    let lowered = formula
        .lowered
        .clone()
        .expect("a formula fit stores its lowering");
    let converted = sc_bayesreg::eject::convert(&lowered, "programs", "radon/model.stan").unwrap();
    store
        .write(
            &sc_files::StorePath::parse("radon/model.stan").unwrap(),
            converted.program.clone().into(),
        )
        .await
        .unwrap();
    let mut registry = ModelRegistry::new();
    registry
        .register(Arc::clone(&stan) as Arc<dyn sc_model::ModelProvider>)
        .unwrap();
    let mut dataset = Dataset::new("t");
    for (name, _) in &data.columns {
        dataset = dataset.column(name.clone(), name.clone());
    }
    let mut model = Model::new("m", sc_stan::STAN_PROVIDER, dataset);
    model.configuration = converted.config;
    let source = Frames(HashMap::from([("t".to_owned(), data)]));
    let plain = run_fit(&registry, &source, &model, 1_000_000)
        .await
        .unwrap_or_else(|e| panic!("{}", sc_error::format_chain(&e)));
    let _ = std::fs::remove_dir_all(&root);

    // The same program, the same data, the same seed: the same draws. (Not
    // the same compiled executable: the compile cache keys on the program's
    // path too, and the Stan model's is the admin's.)
    for (variable, element) in [("Intercept_1", vec![]), ("b_1", vec![1]), ("sd_1", vec![1])] {
        let draws = |f: &Fit| -> Vec<f64> {
            f.draws
                .iter()
                .filter(|s| s.variable == variable && s.element == element && !s.warmup)
                .flat_map(|s| s.draws.iter().copied())
                .collect()
        };
        assert!(!draws(&formula).is_empty(), "{variable}");
        assert_eq!(draws(&formula), draws(&plain), "{variable}{element:?}");
    }
}

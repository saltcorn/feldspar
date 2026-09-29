//! From a posterior's draws to what its instance shows: one summary table per
//! variable, the metrics, and the warnings (Stan TODO §15).
//!
//! **Metrics are the host's**, as ever: a second Bayesian provider hands back
//! draws in the same shape and is diagnosed by this code. What it needs beyond
//! the program's variables are the sampler's own — `divergent__`,
//! `treedepth__`, `energy__` — stored as variables like any other (§14), so
//! everything here could be recomputed from `_fd_model_draws` alone; and the
//! little the draws cannot say, the [`PosteriorRun`].
//!
//! **Warnings are sentences that say what to do**, because the admin reading
//! them is usually not a statistician: the diagnostic *and* the remedy. A fit
//! with warnings is still `fitted` — a posterior is not wrong because it is
//! hard to sample, and the warning is the honest output.

use std::collections::BTreeMap;

use sc_error::Result;
use serde_json::Value as Json;

use crate::bind::{Axis, Labeller, element_label};
use crate::interface::Interface;
use crate::metrics::{ApproximationMetrics, Metrics, ModeMetrics, PosteriorMetrics};
use crate::posterior::{DrawSeries, PosteriorMethod, PosteriorRun};
use crate::provider::{ParameterBlock, ParameterRow};
use crate::summary::ElementSummary;

/// R̂ above this says the chains have not mixed (Vehtari et al. 2021).
pub const RHAT_THRESHOLD: f64 = 1.01;
/// Bulk- and tail-ESS below this many **per chain** are too few to trust the
/// quantity they are about.
pub const ESS_PER_CHAIN_THRESHOLD: f64 = 100.0;
/// E-BFMI below this says the sampler moved poorly between energy levels.
pub const EBFMI_THRESHOLD: f64 = 0.3;

/// The summary columns of a table, after the label columns.
pub(crate) const MCMC_COLUMNS: [&str; 9] = [
    "mean", "sd", "mcse", "q5", "q50", "q95", "rhat", "ess_bulk", "ess_tail",
];
/// A mode has one number per element.
pub(crate) const MODE_COLUMNS: [&str; 1] = ["estimate"];

/// What the host computed from a posterior's draws.
#[derive(Debug, Clone, PartialEq)]
pub struct PosteriorReport {
    /// The method's metrics.
    pub metrics: Metrics,
    /// The warnings, as sentences.
    pub warnings: Vec<String>,
    /// One table per summarised variable, in the program's order.
    pub tables: Vec<ParameterBlock>,
    /// The variables left out of the stored summary for their size, with how
    /// many elements each has.
    pub unsummarised: Vec<(String, usize)>,
}

/// One variable's post-warmup draws: element → chain → draws.
struct Variable<'a> {
    elements: BTreeMap<&'a [usize], Vec<(u32, &'a [f64])>>,
}

impl<'a> Variable<'a> {
    /// The chains of `element`, in chain order.
    fn chains(&self, element: &[usize]) -> Vec<&'a [f64]> {
        self.elements
            .get(element)
            .map(|c| c.iter().map(|(_, d)| *d).collect())
            .unwrap_or_default()
    }

    /// Positions per axis: the largest index seen on each.
    fn lengths(&self) -> Vec<usize> {
        let rank = self.elements.keys().map(|e| e.len()).max().unwrap_or(0);
        (0..rank)
            .map(|k| {
                self.elements
                    .keys()
                    .filter_map(|e| e.get(k))
                    .copied()
                    .max()
                    .unwrap_or(0)
            })
            .collect()
    }
}

/// The post-warmup draws by variable, in the order the variables first
/// appear.
fn group(draws: &[DrawSeries]) -> (Vec<&str>, BTreeMap<&str, Variable<'_>>) {
    let mut order = Vec::new();
    let mut vars: BTreeMap<&str, Variable<'_>> = BTreeMap::new();
    for series in draws.iter().filter(|s| !s.warmup) {
        let var = vars.entry(series.variable.as_str()).or_insert_with(|| {
            order.push(series.variable.as_str());
            Variable {
                elements: BTreeMap::new(),
            }
        });
        var.elements
            .entry(series.element.as_slice())
            .or_default()
            .push((series.chain, series.draws.as_slice()));
    }
    for var in vars.values_mut() {
        for chains in var.elements.values_mut() {
            chains.sort_by_key(|(chain, _)| *chain);
        }
    }
    (order, vars)
}

/// Whether a variable is the sampler's own (`lp__`, `divergent__`) rather than
/// the program's.
fn is_sampler_variable(name: &str) -> bool {
    name.ends_with("__")
}

/// The worst value of one statistic across the diagnosed elements, and where.
struct Worst {
    value: f64,
    at: String,
    /// How many elements were past the threshold.
    past: usize,
}

impl Worst {
    fn new() -> Worst {
        Worst {
            value: f64::NAN,
            at: String::new(),
            past: 0,
        }
    }

    /// Consider `value` at `at`: kept when it is worse (`worse(value,
    /// current)`), counted when it is past the threshold.
    fn see(&mut self, value: f64, at: &str, worse: impl Fn(f64, f64) -> bool, past: bool) {
        if value.is_nan() {
            return;
        }
        if past {
            self.past += 1;
        }
        if self.value.is_nan() || worse(value, self.value) {
            self.value = value;
            self.at = at.to_owned();
        }
    }
}

/// Summarise, diagnose and warn about `draws` (see the module docs).
///
/// With an `interface`, every parameter and transformed parameter is
/// summarised and diagnosed, and each generated quantity of at most
/// `max_elements` elements is summarised; without one, every variable of the
/// program's (not `…__`) of at most `max_elements` elements is both.
pub fn report(
    draws: &[DrawSeries],
    run: &PosteriorRun,
    interface: Option<&Interface>,
    labeller: &Labeller<'_>,
    max_elements: usize,
) -> Result<PosteriorReport> {
    let (order, vars) = group(draws);
    // (variable, diagnosed?) in the order they are shown.
    let chosen: Vec<(&str, bool)> = match interface {
        Some(interface) => interface
            .parameters
            .iter()
            .chain(&interface.transformed)
            .map(|d| (d.name.as_str(), true))
            .chain(interface.generated.iter().map(|d| (d.name.as_str(), false)))
            .collect(),
        None => order
            .iter()
            .filter(|v| !is_sampler_variable(v))
            .map(|v| (*v, true))
            .collect(),
    };
    let chains = vars
        .values()
        .flat_map(|v| v.elements.values().flatten().map(|(c, _)| *c))
        .collect::<std::collections::BTreeSet<u32>>()
        .len()
        .max(1);

    let mut tables = Vec::new();
    let mut warnings = Vec::new();
    let mut unsummarised = Vec::new();
    let mut rhat = Worst::new();
    let mut ess_bulk = Worst::new();
    let mut ess_tail = Worst::new();
    let ess_threshold = ESS_PER_CHAIN_THRESHOLD * chains as f64;
    for (name, diagnosed) in chosen {
        let Some(var) = vars.get(name) else {
            continue;
        };
        let size = var.elements.len();
        let capped = !diagnosed || interface.is_none();
        if capped && size > max_elements {
            unsummarised.push((name.to_owned(), size));
            continue;
        }
        let decl = interface.and_then(|i| i.output(name));
        let (axes, problems) = labeller.axes(name, decl, &var.lengths());
        warnings.extend(problems);
        let columns: Vec<String> = axes
            .iter()
            .map(|a| a.name.clone())
            .chain(
                match run.method {
                    PosteriorMethod::Mode => &MODE_COLUMNS[..],
                    _ => &MCMC_COLUMNS[..],
                }
                .iter()
                .map(|c| (*c).to_owned()),
            )
            .collect();
        let mut rows = Vec::with_capacity(size);
        for element in var.elements.keys() {
            let cells = axis_cells(&axes, element);
            let chains = var.chains(element);
            let stats: Vec<Json> = match run.method {
                PosteriorMethod::Mode => {
                    vec![Json::from(chains.first().and_then(|c| c.first()).copied())]
                }
                method => {
                    let mut s = ElementSummary::of(&chains);
                    if method == PosteriorMethod::Approximation {
                        s = s.without_rhat();
                    }
                    if diagnosed {
                        let at = element_label(name, element, &axes);
                        rhat.see(s.rhat, &at, |a, b| a > b, s.rhat > RHAT_THRESHOLD);
                        ess_bulk.see(s.ess_bulk, &at, |a, b| a < b, s.ess_bulk < ess_threshold);
                        ess_tail.see(s.ess_tail, &at, |a, b| a < b, s.ess_tail < ess_threshold);
                    }
                    [
                        s.mean,
                        s.sd,
                        s.mcse_mean,
                        s.q5,
                        s.q50,
                        s.q95,
                        s.rhat,
                        s.ess_bulk,
                        s.ess_tail,
                    ]
                    .into_iter()
                    .map(Json::from)
                    .collect()
                }
            };
            rows.push(ParameterRow::new(cells.into_iter().chain(stats)));
        }
        tables.push(ParameterBlock::table(name, columns, rows)?);
    }

    let metrics = match run.method {
        PosteriorMethod::Mcmc => {
            let metrics = mcmc_metrics(&vars, run, chains, &rhat, &ess_bulk, &ess_tail);
            warnings.extend(mcmc_warnings(
                &metrics,
                run,
                &rhat,
                &ess_bulk,
                &ess_tail,
                ess_threshold,
            ));
            Metrics::Posterior(metrics)
        }
        PosteriorMethod::Mode => Metrics::PosteriorMode(ModeMetrics {
            log_density: vars
                .get("lp__")
                .map(|v| v.chains(&[]))
                .and_then(|c| c.first().and_then(|d| d.first()).copied())
                .unwrap_or(f64::NAN),
            iterations: run.iterations,
            wall_seconds: run.wall_seconds.clone(),
        }),
        PosteriorMethod::Approximation => Metrics::PosteriorApproximation(ApproximationMetrics {
            draws: draws_per_chain(&vars),
            min_ess_bulk: ess_bulk.value,
            min_ess_tail: ess_tail.value,
            wall_seconds: run.wall_seconds.clone(),
        }),
    };
    Ok(PosteriorReport {
        metrics,
        warnings,
        tables,
        unsummarised,
    })
}

/// The label cells of one element: one per axis.
fn axis_cells(axes: &[Axis], element: &[usize]) -> Vec<Json> {
    element
        .iter()
        .enumerate()
        .map(|(k, i)| axes.get(k).map_or_else(|| Json::from(*i), |a| a.cell(*i)))
        .collect()
}

/// Draws per chain: `lp__`'s, or the first variable's.
fn draws_per_chain(vars: &BTreeMap<&str, Variable<'_>>) -> usize {
    vars.get("lp__")
        .or_else(|| vars.values().next())
        .and_then(|v| v.elements.values().next())
        .and_then(|chains| chains.first())
        .map_or(0, |(_, d)| d.len())
}

/// A sampler variable's draws, chain by chain (empty when it was not written).
fn sampler<'a>(vars: &BTreeMap<&str, Variable<'a>>, name: &str) -> Vec<(u32, &'a [f64])> {
    vars.get(name)
        .and_then(|v| v.elements.get([].as_slice()))
        .cloned()
        .unwrap_or_default()
}

fn mcmc_metrics(
    vars: &BTreeMap<&str, Variable<'_>>,
    run: &PosteriorRun,
    chains: usize,
    rhat: &Worst,
    ess_bulk: &Worst,
    ess_tail: &Worst,
) -> PosteriorMetrics {
    let divergent_per_chain: Vec<usize> = sampler(vars, "divergent__")
        .iter()
        .map(|(_, d)| d.iter().filter(|x| **x > 0.5).count())
        .collect();
    let max_treedepth_hits = match run.max_treedepth {
        Some(depth) => sampler(vars, "treedepth__")
            .iter()
            .map(|(_, d)| d.iter().filter(|x| **x >= f64::from(depth)).count())
            .sum(),
        None => 0,
    };
    PosteriorMetrics {
        chains,
        draws_per_chain: draws_per_chain(vars),
        divergent: divergent_per_chain.iter().sum(),
        divergent_per_chain,
        max_treedepth_hits,
        ebfmi: sampler(vars, "energy__")
            .iter()
            .map(|(_, e)| ebfmi(e))
            .collect(),
        max_rhat: rhat.value,
        min_ess_bulk: ess_bulk.value,
        min_ess_tail: ess_tail.value,
        wall_seconds: run.wall_seconds.clone(),
    }
}

/// The energy Bayesian fraction of missing information of one chain:
/// `Σ (Eₜ − Eₜ₋₁)² / Σ (Eₜ − Ē)²` (Betancourt 2016, as CmdStan's `diagnose`
/// computes it). NaN for fewer than two draws or a constant energy.
pub fn ebfmi(energy: &[f64]) -> f64 {
    if energy.len() < 2 {
        return f64::NAN;
    }
    let mean = energy.iter().sum::<f64>() / energy.len() as f64;
    let numerator: f64 = energy.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum();
    let denominator: f64 = energy.iter().map(|e| (e - mean).powi(2)).sum();
    if denominator > 0.0 {
        numerator / denominator
    } else {
        f64::NAN
    }
}

fn mcmc_warnings(
    metrics: &PosteriorMetrics,
    run: &PosteriorRun,
    rhat: &Worst,
    ess_bulk: &Worst,
    ess_tail: &Worst,
    ess_threshold: f64,
) -> Vec<String> {
    let mut out = Vec::new();
    if metrics.divergent > 0 {
        let chains: Vec<String> = metrics
            .divergent_per_chain
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(i, _)| (i + 1).to_string())
            .collect();
        out.push(format!(
            "{} after warmup (in chain{} {}): the posterior has regions the sampler cannot \
             explore, and the draws near them are biased; raise `adapt_delta` (towards 0.99) or \
             reparameterise",
            count(metrics.divergent, "divergent transition"),
            if chains.len() == 1 { "" } else { "s" },
            chains.join(", ")
        ));
    }
    if metrics.max_treedepth_hits > 0 {
        out.push(format!(
            "{} hit the maximum tree depth of {}: the sampler stopped its trajectories short, \
             which makes it slow rather than wrong; raise `max_treedepth` or reparameterise",
            count(metrics.max_treedepth_hits, "iteration"),
            run.max_treedepth.unwrap_or_default()
        ));
    }
    let low: Vec<String> = metrics
        .ebfmi
        .iter()
        .enumerate()
        .filter(|(_, e)| **e < EBFMI_THRESHOLD)
        .map(|(i, e)| format!("{e:.2} in chain {}", i + 1))
        .collect();
    if !low.is_empty() {
        out.push(format!(
            "E-BFMI is {} (below {EBFMI_THRESHOLD}): the sampler moved poorly between energy \
             levels, usually because of heavy tails or a funnel; reparameterise (a non-centred \
             parameterisation) or tighten the priors",
            low.join(", ")
        ));
    }
    if rhat.value > RHAT_THRESHOLD {
        out.push(format!(
            "R̂ is {:.3} for `{}`{}, above {RHAT_THRESHOLD}: the chains disagree about the \
             posterior, so no summary of it can be trusted yet; run more iterations, or look at \
             the trace plots for a chain stuck somewhere the others are not",
            rhat.value,
            rhat.at,
            others(rhat.past)
        ));
    }
    for (worst, what, which) in [
        (ess_bulk, "bulk", "the posterior means and medians"),
        (ess_tail, "tail", "the 5 % and 95 % quantiles"),
    ] {
        if worst.value < ess_threshold {
            out.push(format!(
                "the {what} effective sample size is {:.0} for `{}`{}, below {ess_threshold:.0} \
                 ({ESS_PER_CHAIN_THRESHOLD:.0} per chain): {which} rest on too few effective \
                 draws; run more iterations",
                worst.value,
                worst.at,
                others(worst.past)
            ));
        }
    }
    out
}

/// " (and 11 other elements)" past the threshold — nothing when the worst is
/// the only one.
fn others(past: usize) -> String {
    match past {
        0 | 1 => String::new(),
        n => format!(" (and {})", count(n - 1, "other element")),
    }
}

/// "1 divergent transition", "12 divergent transitions".
fn count(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

#[cfg(test)]
mod tests;

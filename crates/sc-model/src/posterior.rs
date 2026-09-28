//! The posterior half of the provider seam: what a Bayesian provider is handed,
//! what it hands back, and how it reports on a fit that takes an hour (Stan
//! TODO §2, §13, §14).
//!
//! The existing seam is `fit(frame, config, hyper)` — one rectangle in, a state
//! and some parameter blocks out, in seconds. A posterior breaks it in three
//! places, and each is a type here rather than a special case in the fit:
//!
//! - **In**: several datasets, and the data the host has **bound** from them to
//!   the program's declared variables ([`PosteriorInput`]). The provider never
//!   reads a table and never numbers a county; the binder did both.
//! - **Out**: draws — every chain, every iteration, every element
//!   ([`DrawSeries`]) — which are far too many for the instance's JSON columns
//!   and get a table of their own (`_fd_model_draws`). The summary and the
//!   diagnostics are computed **by the host** from those draws, for the reason
//!   every other metric is: a second Bayesian provider must be scored by the
//!   same code.
//! - **Along the way**: a compile and four chains are minutes, so a fit reports
//!   its [`Progress`] and can be [cancelled](FitContext::cancelled) — both
//!   through a [`FitContext`], so the provider never learns how the host
//!   records either.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value as Json;

use crate::bind::Coordinates;
use crate::frame::Frame;
use crate::interface::Interface;
use crate::provider::ParameterBlock;

/// What a posterior fit is handed.
#[derive(Debug, Clone, PartialEq)]
pub struct PosteriorInput {
    /// Every dataset, materialised: the model's own first, as
    /// [`MAIN_DATASET`](crate::MAIN_DATASET), then each related one under its
    /// name, in the model's order.
    pub datasets: Vec<(String, Frame)>,
    /// What the provider said the program declares, when it said anything.
    pub interface: Option<Interface>,
    /// The bound data: one value per `data` variable, in CmdStan's JSON
    /// convention (Stan TODO §10). An empty object for a program that declares
    /// no data.
    pub data: Json,
    /// Every dimension's keys and labels as the binder numbered them — what a
    /// provider writes beside a raw run (`coordinates.json`, Stan TODO §14).
    /// Empty when nothing was bound.
    pub coordinates: Coordinates,
}

impl PosteriorInput {
    /// The dataset called `name`.
    pub fn dataset(&self, name: &str) -> Option<&Frame> {
        self.datasets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, frame)| frame)
    }
}

/// One element of one variable in one chain: every iteration's value, in order.
///
/// The granularity `_fd_model_draws` stores (Stan TODO §14): "the chains for
/// this parameter" is one indexed read, and one element's chain is one value
/// that parses straight into a `Vec<f64>`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DrawSeries {
    /// The variable — `alpha`, `lp__`, `divergent__`.
    pub variable: String,
    /// The **1-based** index into it, outer to inner: `[]` for a scalar, `[2,
    /// 1]` for `Sigma[2, 1]`. From CmdStan's column names, never from column
    /// position, because CmdStan writes matrices column-major.
    pub element: Vec<usize>,
    /// The chain, from 1.
    pub chain: u32,
    /// Whether these are warmup iterations (stored only when `save_warmup` is
    /// on).
    pub warmup: bool,
    /// The values, one per iteration in order. NaN and the infinities are real
    /// values a generated quantity can take and are kept as such.
    pub draws: Vec<f64>,
}

impl DrawSeries {
    /// The post-warmup draws of `variable[element]` in `chain`.
    pub fn new(
        variable: impl Into<String>,
        element: Vec<usize>,
        chain: u32,
        draws: Vec<f64>,
    ) -> DrawSeries {
        DrawSeries {
            variable: variable.into(),
            element,
            chain,
            warmup: false,
            draws,
        }
    }

    /// The same series, marked as warmup.
    pub fn warmup(mut self) -> DrawSeries {
        self.warmup = true;
        self
    }

    /// `alpha[3]`, `Sigma[2,1]`, `lp__` — for sentences.
    pub fn label(&self) -> String {
        if self.element.is_empty() {
            self.variable.clone()
        } else {
            format!(
                "{}[{}]",
                self.variable,
                self.element
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

/// What a posterior fit produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PosteriorResult {
    /// The provider's own record of the fit — kept small (Stan TODO §14): the
    /// program snapshot and its hashes, the seed, the toolchain version.
    pub state: Json,
    /// The draws. Stored in `_fd_model_draws`, in the transaction that marks
    /// the instance fitted.
    pub draws: Vec<DrawSeries>,
    /// Anything the provider wants shown beyond the host's summary.
    pub parameters: Vec<ParameterBlock>,
}

/// Where a long fit has got to, as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitStage {
    /// Waiting for the process budget.
    Queued,
    /// Compiling the program.
    Compiling,
    /// Running the chains.
    Sampling,
    /// Reading the draws back and computing the summary.
    Summarising,
}

/// Which half of its run a chain is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainPhase {
    /// Adapting; these draws are discarded unless `save_warmup` is on.
    Warmup,
    /// Drawing the iterations that are kept.
    Sampling,
}

/// Where one chain has got to — CmdStan's `Iteration: 400 / 2000 [ 20%]
/// (Warmup)`, read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChainProgress {
    /// The chain, from 1.
    pub chain: u32,
    /// The iteration it has reached, counting warmup.
    pub iteration: u64,
    /// How many iterations it will run, counting warmup.
    pub total: u64,
    /// Which half those iterations are in.
    pub phase: ChainPhase,
}

/// A report on a running fit — what the host writes to the instance's
/// [`ATTR_PROGRESS`](crate::ATTR_PROGRESS) and the screen polls.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Progress {
    /// The stage.
    pub stage: FitStage,
    /// Each chain that has started, by chain number. Empty before sampling.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<ChainProgress>,
}

impl Progress {
    /// A report of `stage` with no chains yet.
    pub fn stage(stage: FitStage) -> Progress {
        Progress {
            stage,
            chains: Vec::new(),
        }
    }
}

/// Where a provider sends its progress. The host's implementation writes it to
/// the instance row (at most once a second); a test's collects it.
pub trait FitProgress: Send + Sync {
    /// Where the fit has got to. Called as often as the provider likes; the
    /// host decides how often that reaches the row.
    fn report(&self, progress: &Progress);
}

/// A sink that drops every report — for a fit nobody is watching.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl FitProgress for NoProgress {
    fn report(&self, _progress: &Progress) {}
}

/// What a long fit is run with: where its progress goes and whether it has
/// been asked to stop.
///
/// The cancel flag is the host's to set and the provider's to read. The host
/// sets it when it reads `cancel_requested` back off the instance row, which is
/// what makes a cancel work from any node: the row is still the registry.
#[derive(Clone, Copy)]
pub struct FitContext<'a> {
    /// Where progress goes.
    pub progress: &'a dyn FitProgress,
    cancel: Option<&'a AtomicBool>,
}

impl<'a> FitContext<'a> {
    /// A context reporting to `progress` and stopped by `cancel`.
    pub fn new(progress: &'a dyn FitProgress, cancel: &'a AtomicBool) -> FitContext<'a> {
        FitContext {
            progress,
            cancel: Some(cancel),
        }
    }

    /// A context nobody watches and nobody can cancel — what a fit that is not
    /// a posterior, and a test, is run with.
    pub fn detached() -> FitContext<'static> {
        FitContext {
            progress: &NoProgress,
            cancel: None,
        }
    }

    /// Report where the fit has got to.
    pub fn report(&self, progress: &Progress) {
        self.progress.report(progress);
    }

    /// Whether the fit has been asked to stop. A provider checks this between
    /// its steps and kills what it started when it is true.
    pub fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::SeqCst))
    }
}

impl std::fmt::Debug for FitContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FitContext")
            .field("cancelled", &self.cancelled())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detached_context_is_never_cancelled_and_a_flagged_one_is_once_set() {
        assert!(!FitContext::detached().cancelled());
        let flag = AtomicBool::new(false);
        let ctx = FitContext::new(&NoProgress, &flag);
        assert!(!ctx.cancelled());
        flag.store(true, Ordering::SeqCst);
        assert!(ctx.cancelled());
    }

    #[test]
    fn progress_is_the_json_the_screen_polls() {
        let progress = Progress {
            stage: FitStage::Sampling,
            chains: vec![ChainProgress {
                chain: 1,
                iteration: 400,
                total: 2000,
                phase: ChainPhase::Warmup,
            }],
        };
        assert_eq!(
            serde_json::to_value(&progress).expect("json"),
            serde_json::json!({
                "stage": "sampling",
                "chains": [{ "chain": 1, "iteration": 400, "total": 2000, "phase": "warmup" }],
            })
        );
        assert_eq!(
            serde_json::to_value(Progress::stage(FitStage::Compiling)).expect("json"),
            serde_json::json!({ "stage": "compiling" })
        );
    }

    #[test]
    fn a_series_names_its_element_the_way_stan_writes_it() {
        assert_eq!(DrawSeries::new("lp__", vec![], 1, vec![]).label(), "lp__");
        assert_eq!(
            DrawSeries::new("Sigma", vec![2, 1], 1, vec![]).label(),
            "Sigma[2,1]"
        );
    }
}

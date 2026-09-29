//! Bayesian models with Stan (layer 6, beside `sc-model`).
//!
//! The Stan-specific half of the Bayesian-models milestone (TODO §4): the
//! declaration parser, CmdStan discovery, the compile cache and the runner, the
//! CmdStan CSV reader and `StanProvider`. Everything that is *not* Stan-specific
//! — related datasets, the binder, the draws table, the posterior summary —
//! belongs to `sc-model`, so that a second Bayesian provider would not have to
//! reimplement it.
//!
//! **There is no Cargo feature.** CmdStan is a directory and a command line,
//! not a library this crate links, so the crate is always compiled and whether
//! Stan is available is a runtime fact that [`cmdstan::discover`] reports.
//!
//! What exists so far:
//!
//! - [`cmdstan`] (TODO Phase 0): finding a CmdStan on this machine and
//!   installing one. Built first, so that the machine this milestone is
//!   developed on had a CmdStan before any test needed one.
//! - [`program`] (Phase 2): a program read out of a file store with its
//!   `#include`s, and the declaration parser that turns it into the host's
//!   [`Interface`](sc_model::Interface).
//! - [`stanc`] (Phase 2): the Stan compiler as the authority on whether a
//!   program is valid, its diagnostics mapped back to store paths, and its
//!   `--info` compared with our parse.
//! - [`StanProvider`] (Phases 2 and 4): the `ModelProvider`, registered by the
//!   server beside the built-ins. It declares its configuration, answers the
//!   program's interface, and fits: compile, run, read the draws, publish the
//!   raw run.
//! - [`compile`] (Phase 4): the compile cache — one compile at a time per
//!   node, keyed by what the program is.
//! - [`run`] (Phase 4): one process per chain within the node's process
//!   budget, progress read from CmdStan's output, failures as sentences.
//! - [`run_dir`] (Phase 4): the raw run, in scratch while it runs and
//!   published to a file store when the model keeps one.
//! - [`output`]: CmdStan's CSV read into draws, elements by column name.

pub mod cmdstan;
pub mod compile;
pub mod output;
mod process;
pub mod program;
mod provider;
pub mod run;
pub mod run_dir;
pub mod stanc;

pub use process::{Stopped, Watch};
pub use provider::{CompileAnswer, STAN_PROVIDER, StanProvider, StoreLookup, config_keys};
pub use stanc::ProgramCheck;

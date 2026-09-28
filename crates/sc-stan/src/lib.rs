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
//! - [`StanProvider`] (Phase 2): the `ModelProvider`, registered by the server
//!   beside the built-ins. It declares its configuration and answers the
//!   program's interface; it samples nothing yet (Phase 4).

pub mod cmdstan;
pub mod program;
mod provider;
pub mod stanc;

pub use provider::{STAN_PROVIDER, StanProvider, StoreLookup, config_keys};

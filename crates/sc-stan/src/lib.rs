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
//! Only [`cmdstan`] exists yet (TODO Phase 0): it is built first so that the
//! machine this milestone is developed on has a CmdStan before any test needs
//! one.

pub mod cmdstan;

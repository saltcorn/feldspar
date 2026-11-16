//! Bayesian regression from a formula (layer 6; TODO-bayesian-regression,
//! TECHNICAL_DESIGN §14.2 "Bayesian regression from a formula").
//!
//! A model provider, `bayesian_regression`, whose configuration is a
//! one-line formula in lme4/brms syntax:
//!
//! ```text
//! price | weights(w) ~ area + region + (1 + area | county)
//! sigma ~ region
//! ```
//!
//! The provider **generates** a Stan program from the formula's structure and
//! fits it through `sc-stan`'s compile cache and runner, with the data bound by
//! the host's binder (`sc-model`). The pipeline is pure Rust with no I/O, so
//! every stage is tested without CmdStan:
//!
//! - [`parse`]: the text → a [`Formula`] — lines, terms expanded by R's
//!   formula rules, group-level terms, the response's addition terms, `bf()`
//!   unwrapped, pasted `prior(…)` calls lifted out. Every error is a sentence
//!   with a span.
//! - [`resolve`]: a formula against the dataset and the form → [`Resolved`] —
//!   the family and links, each linear predictor's terms as the binder's
//!   `terms` binding takes them, the group-level blocks, and the prior classes.
//! - [`family`]: the table of families (§3.6).
//! - [`prior`]: the prior menu, its constraints, the defaults and the
//!   resolution of the rows an admin sets (§3.7).
//!
//! - [`canon`]: the resolved formula → the [`Structure`] that alone decides the
//!   program's text, and the [`DataPlan`] binding each of its data variables
//!   (§5).
//! - [`generate`]: the structure → the Stan program (§4).
//! - [`lower`]: the configuration → the lowered configuration the host binds
//!   with: the program, the bindings and the prior data.
//! - [`check`]: what the model editor asks as the admin types.
//! - [`output`]: the brms-style parameter table of a fit (§6).
//! - [`BayesRegProvider`]: all of it as a `ModelProvider`, fitting through
//!   `sc_stan::StanProvider::run_program`.
//!
//! ## A clean-room implementation
//!
//! brms is GPL-2, and nothing here is derived from its source: neither its R
//! code nor its Stan templates were read, and no output of its
//! `make_stancode()` is copied into the generator or its tests (Part I §2.1 of
//! the TODO). What this crate follows is published **behaviour**:
//!
//! - brms's reference documentation and vignettes — `?brmsformula`,
//!   `?brmsfamily`, `?set_prior`, `?addition-terms`, `?gr` — and Bürkner (2017),
//!   *JSS* 80(1), and (2018), *The R Journal* 10(1), which describe the model
//!   classes mathematically;
//! - Bates, Mächler, Bolker & Walker (2015), *JSS* 67(1), for lme4's
//!   group-level syntax; R's `?formula` and `?model.matrix`, and Chambers &
//!   Hastie (1992), *Statistical Models in S*, for the population-level
//!   expansion and its coding (implemented in `sc_model::terms`);
//! - the Stan User's Guide and Functions Reference for the constructions the
//!   generator will emit (non-centred parameterisation, LKJ, Cholesky
//!   factors), Gelman & Hill (2007), and McElreath (2020).
//!
//! Each module names the source of what it implements, and the generated
//! program's variable names are this crate's own.

pub mod canon;
pub mod check;
pub mod family;
pub mod generate;
pub mod lower;
pub mod output;
pub mod parse;
pub mod prior;
mod provider;
pub mod resolve;

pub use canon::{Canonical, DataPlan, GENERATOR_VERSION, Structure, canonicalise, data_plan};
pub use check::{Check, check};
pub use family::{FAMILIES, Family, Link, Response, family};
pub use generate::{PROGRAM_PATH, generate};
pub use lower::{Built, build, keys};
pub use parse::{Formula, ParseError, ParseErrors, Span, parse_formula, strip_priors};
pub use prior::{
    ClassKind, ClassSlot, DataScale, Prior, PriorFamily, PriorRow, PriorTable, merge_rows,
};
pub use provider::{BayesRegProvider, GENERATED_STORE, lower_config, program_of};
pub use resolve::{Block, BlockPart, Data, Names, Predictor, Resolved, Settings, resolve};

/// The name the provider is registered and stored under.
pub const PROVIDER: &str = "bayesian_regression";

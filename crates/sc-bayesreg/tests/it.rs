//! Every integration test in this crate, in one binary (the workspace convention).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "formulas.rs"]
mod formulas;
#[path = "generate.rs"]
mod generate;
#[path = "provider.rs"]
mod provider;
#[path = "recovery.rs"]
mod recovery;

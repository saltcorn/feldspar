//! Every integration test in this crate, in one binary (the workspace's
//! convention; see `sc-model/tests/it.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "programs.rs"]
mod programs;
#[path = "provider.rs"]
mod provider;
#[path = "real_cmdstan.rs"]
mod real_cmdstan;
#[path = "runner.rs"]
mod runner;
#[path = "stanc.rs"]
mod stanc;

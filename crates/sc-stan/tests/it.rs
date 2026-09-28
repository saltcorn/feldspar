//! Every integration test in this crate, in one binary (the workspace's
//! convention; see `sc-model/tests/it.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "real_cmdstan.rs"]
mod real_cmdstan;

//! Every integration test in this crate, in one binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "store.rs"]
mod store;

#[path = "phases.rs"]
mod phases;

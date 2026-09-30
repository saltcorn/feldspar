//! Every integration test in this crate, in one binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "fixture.rs"]
mod fixture;

#[path = "operations.rs"]
mod operations;

#[path = "store.rs"]
mod store;

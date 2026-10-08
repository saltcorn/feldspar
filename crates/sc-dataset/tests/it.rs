//! Every integration test in this crate, in one binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "fixture.rs"]
mod fixture;

#[path = "geometry.rs"]
mod geometry;

#[path = "operations.rs"]
mod operations;

#[path = "spatial.rs"]
mod spatial;

#[path = "store.rs"]
mod store;

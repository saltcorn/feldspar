//! Every integration test in this crate, in one binary (the workspace links V8
//! into every test target).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "fixture.rs"]
mod fixture;

#[path = "export.rs"]
mod export;

#[path = "identity.rs"]
mod identity;

#[path = "stores.rs"]
mod stores;

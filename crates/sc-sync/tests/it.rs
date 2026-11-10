//! Every integration test in this crate, in one binary (the workspace links V8
//! into every test target).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "fixture.rs"]
mod fixture;

#[path = "base.rs"]
mod base;

#[path = "deploy.rs"]
mod deploy;

#[path = "export.rs"]
mod export;

#[path = "identity.rs"]
mod identity;

#[path = "plan.rs"]
mod plan;

#[path = "stores.rs"]
mod stores;

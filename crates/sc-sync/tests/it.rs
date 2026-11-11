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

#[path = "registry.rs"]
mod registry;

#[path = "stores.rs"]
mod stores;

#[path = "source.rs"]
mod source;

#[path = "structure_zip.rs"]
mod structure_zip;

#[path = "definition_of_done.rs"]
mod definition_of_done;

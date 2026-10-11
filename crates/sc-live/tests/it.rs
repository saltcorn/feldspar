//! Every integration test in this crate, in one binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "changes.rs"]
mod changes;
#[path = "filters.rs"]
mod filters;
#[path = "hub.rs"]
mod hub;
#[path = "publish.rs"]
mod publish;
#[path = "readers.rs"]
mod readers;

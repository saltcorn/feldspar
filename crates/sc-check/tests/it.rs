//! Every integration test in this crate, in one binary (the workspace links V8
//! into every test target).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "checklists.rs"]
mod checklists;

#[path = "framework_steps.rs"]
mod framework_steps;

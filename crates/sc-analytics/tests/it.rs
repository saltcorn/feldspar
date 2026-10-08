//! Every integration test in this crate, in one binary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "workspaces.rs"]
mod workspaces;

#[path = "plots.rs"]
mod plots;

#[path = "reshaped.rs"]
mod reshaped;

#[path = "hypothesis.rs"]
mod hypothesis;

#[path = "fit_outputs.rs"]
mod fit_outputs;

#[path = "layers.rs"]
mod layers;

#[path = "maps.rs"]
mod maps;

#[path = "map_workspace.rs"]
mod map_workspace;

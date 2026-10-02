//! The Analytics UI's server half (layer 6; analytics TODO A1.12).
//!
//! The Analytics UI is a set of **workspaces**, each of one of seven kinds,
//! each keeping its own state. This crate holds them, the plot spec and its
//! stat compiler ([`plot`], A2), and the hypothesis tests and map layers as the
//! later milestones bring them; and the demo data ([`demo`]). Datasets are not
//! here: they are `sc-dataset`'s, because models read them too.

pub mod demo;
pub mod model_outputs;
pub mod plot;
pub mod stats;
mod workspace;

pub use workspace::{
    WORKSPACES_TABLE, Workspace, WorkspaceId, WorkspaceKind, bootstrap_workspaces,
    create_workspace, delete_workspace, list_workspaces, load_workspace, rename_workspace,
    require_workspace, save_workspace_state,
};

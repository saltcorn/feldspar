//! The Analytics UI's server half (layer 6; analytics TODO A1.12).
//!
//! The Analytics UI is a set of **workspaces**, each of one of eight kinds,
//! each keeping its own state. This crate holds them now, and the plot spec,
//! its stat compiler, the hypothesis tests and the map layers as the later
//! milestones bring them; and the demo data ([`demo`]). Datasets are not here: they are `sc-dataset`'s,
//! because models read them too.

pub mod demo;
mod workspace;

pub use workspace::{
    WORKSPACES_TABLE, Workspace, WorkspaceId, WorkspaceKind, bootstrap_workspaces,
    create_workspace, delete_workspace, list_workspaces, load_workspace, rename_workspace,
    require_workspace, save_workspace_state,
};

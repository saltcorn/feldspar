//! The Analytics UI's server half (layer 6; analytics TODO A1.12).
//!
//! The Analytics UI is a set of **workspaces**, each of one of six kinds,
//! each keeping its own state. This crate holds them, the plot spec and its
//! stat compiler ([`plot`], A2), the hypothesis tests ([`stats`]), the panels
//! that are dragged between them and the usage index over them ([`panel`],
//! A4), the map layers' data for the browser ([`layer`], A5.5), the map spec
//! and the geometry source a map panel chooses ([`map`], A5.6–A5.7), the Map
//! workspace's classification ([`classify`], A5.9), attribute table and
//! selection ([`selection`], A5.10) and toolbox ([`tools`], A5.12); a
//! dashboard's stat cards ([`card`], A6.2) and cross-filtering
//! ([`crossfilter`], A6.3–A6.6); the Analytics UI as an application framework
//! ([`app`], A9); the A/B tests workspace over the Astro module's tables ([`ab`],
//! TODO-astro W3.6); and the demo data ([`demo`]). Datasets are not here: they are `sc-dataset`'s, because models
//! read them too.

/// This crate's own tables' access to the primary database
/// (TODO-code-quality Q3.5): the one `Unchecked::system` its internal stores
/// make, listed in docs/SECURITY_REVIEW_2026-10.md's system-authority
/// inventory. Crate-private, so nothing outside the crate borrows it.
pub(crate) const INTERNAL_TABLES: sc_catalog::Unchecked<'static> = sc_catalog::Unchecked::system(
    "sc-analytics: the workspace store and the demo dataset an admin installs",
);

pub mod ab;
pub mod app;
pub mod card;
pub mod classify;
pub mod crossfilter;
pub mod demo;
pub mod layer;
pub mod map;
pub mod model_outputs;
pub mod panel;
pub mod plot;
pub mod selection;
pub mod stats;
pub mod tools;
mod workspace;

pub use workspace::{
    WORKSPACES_TABLE, Workspace, WorkspaceId, WorkspaceKind, bootstrap_workspaces,
    create_workspace, delete_workspace, list_workspaces, load_workspace, rename_workspace,
    require_workspace, save_workspace_state, share_workspace,
};

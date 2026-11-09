//! Source control for an installation's structure (APP_SYNC_PROPOSAL,
//! TODO-app-sync).
//!
//! An installation's structure — its tables and fields, applications, views,
//! pages, triggers, roles, modules, file stores and the rest — is written as a
//! tree of files, one JSON file per entity, so it can live in a git repository
//! and move between instances by Commit, Pull and Deploy. The database stays the
//! source of truth for the running instance; the repository is the source of
//! truth for what the structure should be.
//!
//! This crate holds milestone S1, the tree and its export:
//!
//! - [`tree`]: [`StructureTree`], the files and their canonical rendering, and
//!   the tree's digest.
//! - [`format`](mod@format): `feldspar-structure.json`, and refusing a tree
//!   written in a newer format than this build reads.
//! - [`export`](mod@export): the live installation as a tree, through the admin
//!   API's own JSON with bookkeeping and instance-bound fields stripped.
//! - [`identity`]: every table and field given an id before it is exported.
//! - [`secrets`]: every secret written as the sentinel.
//! - [`stores`]: which of a local store's files are synced, and the Status
//!   warnings about them.
//!
//! The three-way diff, the planner and the executor (milestones S2 and S3) build
//! on these.

pub mod export;
pub mod format;
pub mod identity;
pub mod secrets;
pub mod stores;
pub mod tree;

pub use export::{Export, export};
pub use format::{FORMAT_FILE, FORMAT_VERSION, StructureFormat, check_format, read_format};
pub use identity::{ensure_overlays, is_structure_table};
pub use secrets::Registries;
pub use stores::{
    ContentClaim, ContentClaimer, LARGE_FILE_BYTES, StoreUses, StoreWarning, SyncedDirs, resolve,
    store_uses, walk_synced,
};
pub use tree::{StructureTree, TreeEntry, path_segment, render_json};

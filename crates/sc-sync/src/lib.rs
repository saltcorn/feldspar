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
//! Milestone S2, Status and the planner:
//!
//! - [`entity`]: a tree read as entities, each with what it is matched by.
//! - [`base`]: the base's store files kept in the sync mirror, and the target
//!   a Discard plans.
//! - [`diff`]: [`Status`], the three-way diff of the base, the tip and live.
//! - [`entity_diff`](mod@entity_diff): one entity's diff, field by field (or line by line).
//! - [`plan`](mod@plan): [`Plan`], the classified operations that turn live into a
//!   target tree, in deploy order, with their digest.
//! - [`validate`](mod@validate): the dry runs and other checks a plan needs the
//!   database for.
//!
//! The executor (milestone S3) applies a plan.

pub mod base;
pub mod diff;
pub mod entity;
pub mod entity_diff;
pub mod export;
pub mod format;
pub mod identity;
pub mod plan;
pub mod secrets;
pub mod stores;
pub mod tree;
pub mod validate;

pub use base::{BASE_REF, base_file_bytes, discard_target, keep_base_files};
pub use diff::{Class, Status, StatusEntry};
pub use entity::{Content, Entities, Entity, Kind, blob_hash};
pub use entity_diff::{EntityDiff, JsonChange, entity_diff};
pub use export::{Export, export};
pub use format::{FORMAT_FILE, FORMAT_VERSION, StructureFormat, check_format, read_format};
pub use identity::{ensure_overlays, is_structure_table};
pub use plan::{Blocker, OpBody, OpClass, Plan, PlannedOp, Step, plan, plan_digest};
pub use secrets::Registries;
pub use stores::{
    ContentClaim, ContentClaimer, LARGE_FILE_BYTES, StoreUses, StoreWarning, SyncedDirs, resolve,
    store_uses, walk_synced,
};
pub use tree::{StructureTree, TreeEntry, path_segment, render_json};
pub use validate::validate;

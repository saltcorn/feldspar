//! [`FileStoreDef`]: the stored *definition* of a file store, as distinct from
//! the connected [`FileStore`](crate::FileStore) instance it produces.
//!
//! The MVP had no such type, because a store existed only as a
//! `--file-store NAME=PATH` process argument: there was nothing to persist and
//! nothing to edit. Making stores admin-editable makes a store an object with a
//! lifecycle — created, edited, deleted, reconnected at boot — and that object
//! needs a value type before it can have a row.
//!
//! The split matters and is worth stating plainly:
//!
//! - A **definition** is inert data: a name, which backend, and that backend's
//!   settings. It is what the admin edits and what a `_fd_file_stores` row
//!   holds. It can describe a store that does not currently work — a path that
//!   has since been unmounted — and that is a state the admin UI must be able to
//!   show and let them fix, not an error to refuse to load.
//! - An **instance** is a live `Arc<dyn FileStore>` in the catalog's registry,
//!   which by existing asserts the store is reachable.
//!
//! Persisting a definition lives in `sc-catalog`, not here: this crate has no
//! `Catalog`, and cannot gain one, since `sc-catalog` already depends on it.
//! Turning a definition into an instance is the backend registry's job (TODO
//! §1.2).

use sc_types::Attrs;
use sc_types::Role;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The name of the sole backend the MVP registers: a store backed by a local
/// directory, implemented by [`LocalFileStore`](crate::LocalFileStore).
///
/// A constant rather than a bare `"local"` at each use because a backend name is
/// a stored value — it is written into a `_fd_file_stores` row and read back —
/// so it is part of the on-disk format, not an incidental string.
pub const LOCAL_BACKEND: &str = "local";

/// The `path` setting of the [`local`](LOCAL_BACKEND) backend: the directory the
/// store is rooted at.
pub const CFG_PATH: &str = "path";

/// The `create` setting of the [`local`](LOCAL_BACKEND) backend: whether to
/// create the directory when it does not exist, rather than refusing to connect.
pub const CFG_CREATE: &str = "create";

/// The backend backing a store with a **clone of a git repository**, implemented
/// by [`GitFileStore`](crate::GitFileStore).
///
/// The second registered backend, and the one that proves the registry was worth
/// building: adding it changed `sc-files` and nothing in the admin UI's settings
/// rendering, because a backend declares its settings and the form renders what
/// it is handed (§6.2).
pub const GIT_BACKEND: &str = "git";

/// The `url` setting of the [`git`](GIT_BACKEND) backend: the repository to
/// clone. Any URL git accepts — `git@host:owner/repo.git`, `https://…`, or a
/// local path.
pub const CFG_URL: &str = "url";

/// The `directory` setting of the [`git`](GIT_BACKEND) backend: where the
/// working tree lives, chosen by the admin **when the store is created** and
/// fixed thereafter.
///
/// Optional, and normally left blank — a clone Saltcorn makes is Saltcorn's to
/// place, in [`clone_dir`](crate::clone_dir). It is here for the two cases where
/// only the admin can know the answer: a checkout that must live somewhere
/// specific (a path another process serves from), and a working tree that
/// **already exists** on the machine — which Saltcorn then adopts as it stands
/// rather than cloning over, so a repository already checked out on the server
/// can be connected with no URL at all.
///
/// [`create_only`](sc_types::FormField::create_only), because the directory
/// names *where the working tree was put*: changing it later would not move the
/// tree, it would abandon it, along with anything uncommitted in it. That is
/// also why [`ATTR_CLONE_PATH`] still exists and still wins — the setting is
/// what the admin asked for, the attribute is what happened.
///
/// Left blank, it stays blank in the row — the location is then derived, and a
/// stored copy of a derived answer is a second answer that can disagree — but it
/// is **shown** as the directory the store actually got
/// ([`display_config`](crate::display_config)). A control that is read-only and
/// empty would otherwise tell an admin nothing about where their files are.
pub const CFG_DIR: &str = "directory";

/// The `branch` setting of the [`git`](GIT_BACKEND) backend: which branch to
/// check out. Empty means the repository's default branch.
pub const CFG_BRANCH: &str = "branch";

/// The `key_path` setting of the [`git`](GIT_BACKEND) backend: the SSH private
/// key git authenticates with.
///
/// Usually written by [`record_deploy_key`](crate::record_deploy_key) when the
/// admin generates a deploy key, but settable by hand — pointing a store at a
/// key that already exists on the machine is a legitimate configuration.
pub const CFG_KEY_PATH: &str = "key_path";

/// The `public_key` setting of the [`git`](GIT_BACKEND) backend: the public half
/// of [`CFG_KEY_PATH`], kept so the admin can read it again after generating it.
///
/// A public key, so storing and displaying it discloses nothing; what it saves
/// is an admin who generated a deploy key, navigated away, and now needs to
/// paste it into the repository's settings.
pub const CFG_PUBLIC_KEY: &str = "public_key";

/// Attribute (§9, sparse per-store values) recording **where** a git store was
/// cloned.
///
/// An attribute rather than a setting because it is not the admin's to choose:
/// the clone directory is picked by Saltcorn (see
/// [`clone_path`](crate::clone_path)) and recorded so that renaming the store
/// cannot orphan its working tree.
pub const ATTR_CLONE_PATH: &str = "clone_path";

/// Identifies a stored file-store definition. A UUID, per the §9 rule that every
/// system metadata table has a UUID primary key.
///
/// Distinct from `sc-catalog`'s `FileStoreId`, which is a store's *name* and is
/// what a `File` field or an application references. The name is the key
/// everything else in the system uses; this id is the row's identity, so a store
/// can be renamed without its row changing identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileStoreDefId(pub Uuid);

impl FileStoreDefId {
    /// A fresh random id, for a definition that has never been saved.
    pub fn new() -> FileStoreDefId {
        FileStoreDefId(Uuid::new_v4())
    }
}

impl Default for FileStoreDefId {
    fn default() -> FileStoreDefId {
        FileStoreDefId::new()
    }
}

/// The stored definition of one file store (technical design §14.1).
///
/// The fields mirror the `_fd_file_stores` columns, which follow §9's
/// column-vs-attributes rule: every store has an id, name, description, backend,
/// backend config and optional minimum role, so each of those gets a column, and
/// anything sparse goes in [`attributes`](FileStoreDef::attributes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStoreDef {
    /// The row's identity.
    pub id: FileStoreDefId,
    /// The store's name: the key it is connected and referenced under, unique
    /// across stores.
    pub name: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// Which backend serves this store — [`LOCAL_BACKEND`] for now.
    pub backend: String,
    /// The backend's settings, validated against that backend's declared
    /// `config_spec` on save (TODO §1.2).
    pub config: Attrs,
    /// Minimum role permitted to reach the store at all, if restricted. Follows
    /// the `sc-auth` convention (`1` = admin … `100` = public), so a lower number
    /// is more restrictive — the same scale as
    /// [`FileMeta::min_role`](crate::FileMeta), which this sits above: a store's
    /// floor applies before any per-file rule.
    pub min_role: Option<Role>,
    /// Sparse per-store values (§9).
    pub attributes: Attrs,
}

impl FileStoreDef {
    /// A definition named `name` served by `backend`, with no settings, no
    /// description and no role floor — the base to add settings to.
    pub fn new(name: impl Into<String>, backend: impl Into<String>) -> FileStoreDef {
        FileStoreDef {
            id: FileStoreDefId::new(),
            name: name.into(),
            description: String::new(),
            backend: backend.into(),
            config: Attrs::new(),
            min_role: None,
            attributes: Attrs::new(),
        }
    }

    /// A [`local`](LOCAL_BACKEND) store named `name` rooted at `path` — the
    /// common case, and the one `--file-store NAME=PATH` expresses.
    pub fn local(name: impl Into<String>, path: impl Into<String>) -> FileStoreDef {
        FileStoreDef::new(name, LOCAL_BACKEND).with(CFG_PATH, path.into())
    }

    /// A [`git`](GIT_BACKEND) store named `name` cloned from `url`, on the
    /// repository's default branch and with no key configured yet.
    pub fn git(name: impl Into<String>, url: impl Into<String>) -> FileStoreDef {
        FileStoreDef::new(name, GIT_BACKEND).with(CFG_URL, url.into())
    }

    /// Set a backend setting, returning `self` for chaining.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.config.insert(key.into(), value.into());
        self
    }

    /// Set the description, returning `self` for chaining.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Set the minimum role, returning `self` for chaining.
    pub fn min_role(mut self, role: Role) -> Self {
        self.min_role = Some(role);
        self
    }

    /// Set the id, returning `self` for chaining — for rebuilding a definition
    /// that already has a row.
    pub fn id(mut self, id: FileStoreDefId) -> Self {
        self.id = id;
        self
    }

    /// The value of a backend setting as text, if it is set and is a string.
    ///
    /// Settings are `Attrs` (arbitrary JSON) because a backend declares its own,
    /// so reading one is always "if it is there and is the right shape". The
    /// backend registry validates them against a spec on save (TODO §1.2); this
    /// is the convenience for the settings that are simple strings.
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(serde_json::Value::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_definition_carries_its_path_as_a_setting() {
        let def = FileStoreDef::local("apps", "/srv/apps");
        assert_eq!(def.name, "apps");
        assert_eq!(def.backend, LOCAL_BACKEND);
        assert_eq!(def.setting(CFG_PATH), Some("/srv/apps"));
        // Nothing is set that was not asked for.
        assert_eq!(def.description, "");
        assert_eq!(def.min_role, None);
        assert!(def.attributes.is_empty());
    }

    #[test]
    fn ids_are_distinct_per_definition() {
        // The id is the row identity, so two fresh definitions must not collide
        // — otherwise saving the second would silently update the first.
        let a = FileStoreDef::local("a", "/a");
        let b = FileStoreDef::local("b", "/b");
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn a_missing_or_non_string_setting_reads_as_none() {
        let def = FileStoreDef::new("s", LOCAL_BACKEND).with("depth", 3);
        assert_eq!(def.setting(CFG_PATH), None);
        // Present but not a string: still `None` rather than a stringified 3,
        // so a caller cannot mistake a wrong-typed setting for a valid one.
        assert_eq!(def.setting("depth"), None);
    }

    #[test]
    fn builders_chain() {
        let def = FileStoreDef::local("docs", "/srv/docs")
            .description("Shared documents")
            .min_role(Role::of(40));
        assert_eq!(def.description, "Shared documents");
        assert_eq!(def.min_role, Some(Role::of(40)));
        assert_eq!(def.setting(CFG_PATH), Some("/srv/docs"));
    }
}

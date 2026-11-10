//! A structure tree read as **entities** (APP_SYNC_PROPOSAL §3.1, §3.2, §4).
//!
//! Most files of a tree are one entity each, but not all: `roles.json` and
//! `modules.json` are lists, a settings section holds one entity per key, and a
//! store's synced files are entities of their own, keyed by store and path. The
//! three-way diff and the planner compare entities, not files, so a rename
//! (which moves the file) is still one entity, and one changed setting is not
//! a change to its whole section.
//!
//! Each entity has what it is matched by (§3.2): an **id** for the definition
//! kinds, and a **name** — the role level for a role, the package for a module,
//! `section.key` for a setting, the path for a store file. A child kind (a view,
//! a store file) also has a **scope**, the path of its parent in the same tree,
//! so a view is matched only among its own application's views.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value as Json;
use sha1::{Digest, Sha1};

use crate::format::FORMAT_FILE;
use crate::tree::{STORE_FILES_DIR, StructureTree, TreeEntry};

/// What kind of thing an entity is.
///
/// Declared **parents first**: matching walks the kinds in this order, so an
/// application's group is known before its views are matched within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A module (`modules.json`), by package name.
    Module,
    /// A role (`roles.json`), by its level.
    Role,
    /// One key of a portable settings section.
    Setting,
    /// The users table's admin-added fields (`users.json`).
    Users,
    /// A table.
    Table,
    /// A database connection.
    DbConnection,
    /// A file store's definition.
    FileStore,
    /// One file of a local store's synced directories.
    StoreFile,
    /// A code library.
    CodeLibrary,
    /// An LLM provider and its models.
    LlmProvider,
    /// An MCP server.
    McpServer,
    /// A trigger, with its workflow.
    Trigger,
    /// A stream.
    Stream,
    /// An agent.
    Agent,
    /// An analytics dataset.
    Dataset,
    /// A statistical model.
    Model,
    /// An analytics workspace.
    Workspace,
    /// An application.
    Application,
    /// An application's view.
    View,
    /// An application's page.
    Page,
    /// An application's library item.
    LibraryItem,
    /// An application's translations for one locale.
    Translation,
    /// A checklist.
    Checklist,
    /// A JSON file this build does not know the meaning of.
    Other,
}

impl Kind {
    /// Every kind, parents first.
    pub const ALL: [Kind; 24] = [
        Kind::Module,
        Kind::Role,
        Kind::Setting,
        Kind::Users,
        Kind::Table,
        Kind::DbConnection,
        Kind::FileStore,
        Kind::StoreFile,
        Kind::CodeLibrary,
        Kind::LlmProvider,
        Kind::McpServer,
        Kind::Trigger,
        Kind::Stream,
        Kind::Agent,
        Kind::Dataset,
        Kind::Model,
        Kind::Workspace,
        Kind::Application,
        Kind::View,
        Kind::Page,
        Kind::LibraryItem,
        Kind::Translation,
        Kind::Checklist,
        Kind::Other,
    ];

    /// The kind's name in a [`key`](Entity::key) and on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Module => "module",
            Kind::Role => "role",
            Kind::Setting => "setting",
            Kind::Users => "users",
            Kind::Table => "table",
            Kind::DbConnection => "db_connection",
            Kind::FileStore => "file_store",
            Kind::StoreFile => "store_file",
            Kind::CodeLibrary => "code_library",
            Kind::LlmProvider => "llm_provider",
            Kind::McpServer => "mcp_server",
            Kind::Trigger => "trigger",
            Kind::Stream => "stream",
            Kind::Agent => "agent",
            Kind::Dataset => "dataset",
            Kind::Model => "model",
            Kind::Workspace => "workspace",
            Kind::Application => "application",
            Kind::View => "view",
            Kind::Page => "page",
            Kind::LibraryItem => "library_item",
            Kind::Translation => "translation",
            Kind::Checklist => "checklist",
            Kind::Other => "other",
        }
    }

    /// The kind a key's prefix names.
    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == name)
    }

    /// Whether entities of this kind carry a UUID `id` they are matched by
    /// first (§3.2).
    pub fn has_id(self) -> bool {
        !matches!(
            self,
            Kind::Module
                | Kind::Role
                | Kind::Setting
                | Kind::Users
                | Kind::StoreFile
                | Kind::Translation
                | Kind::Other
        )
    }

    /// The kind an entity of this kind lives inside, if any.
    pub fn parent(self) -> Option<Kind> {
        match self {
            Kind::StoreFile => Some(Kind::FileStore),
            Kind::View | Kind::Page | Kind::LibraryItem | Kind::Translation => {
                Some(Kind::Application)
            }
            _ => None,
        }
    }
}

/// What an entity is: a JSON value, or a store file's bytes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// The entity's JSON.
    Json {
        /// The value.
        value: Json,
    },
    /// A store file, known by its git blob hash; its bytes when the tree it
    /// came from has them (a base keeps only the hash, §4).
    File {
        /// The git blob hash of the bytes.
        hash: String,
        /// The size in bytes, when known.
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        /// The bytes, when the tree has them.
        #[serde(skip)]
        bytes: Option<Vec<u8>>,
    },
}

impl Content {
    /// The JSON value, for a JSON entity.
    pub fn json(&self) -> Option<&Json> {
        match self {
            Content::Json { value } => Some(value),
            Content::File { .. } => None,
        }
    }

    /// A file's content from its bytes.
    pub fn file(bytes: &[u8]) -> Content {
        Content::File {
            hash: blob_hash(bytes),
            size: Some(bytes.len() as u64),
            bytes: Some(bytes.to_vec()),
        }
    }
}

/// The hash git gives a blob holding `bytes`: SHA-1 over `blob <len>\0` and the
/// bytes, as lowercase hex.
///
/// The one a store file is compared by, so the hash a base records for a file
/// is the object the sync mirror keeps reachable under its base ref (§4).
pub fn blob_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// One entity of a tree.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entity {
    /// What it is.
    pub kind: Kind,
    /// What it is matched by after its id: its name, role level, package,
    /// `section.key`, or path within its store.
    pub name: String,
    /// Its UUID, for the kinds that have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The path of the entity it lives inside, in the same tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The file it is in.
    pub path: String,
    /// What it is.
    pub content: Content,
}

impl Entity {
    /// The JSON value, for a JSON entity.
    pub fn json(&self) -> Option<&Json> {
        self.content.json()
    }

    /// A key naming the entity for a person and the CLI: `table:tickets`,
    /// `view:helpdesk/list_tickets`, `store_file:assets/img/logo.png`,
    /// `setting:localisation.default_locale`. A child is named inside the path
    /// segment of what it lives in.
    pub fn key(&self) -> String {
        match self.scope.as_deref().map(scope_segment) {
            Some(scope) => format!("{}:{scope}/{}", self.kind.as_str(), self.name),
            None => format!("{}:{}", self.kind.as_str(), self.name),
        }
    }

    /// The value it is compared by: its JSON without what matching already
    /// accounts for. A matched entity's id is not a difference — a name match
    /// with a different id is the same entity, and the instance's id is kept
    /// (§3.2) — and neither are the ids of a table's fields, which are matched
    /// the same way.
    pub fn comparable(&self) -> Comparable<'_> {
        match &self.content {
            Content::File { hash, .. } => Comparable::Hash(hash),
            Content::Json { value } => Comparable::Json(comparable_json(self.kind, value)),
        }
    }

    /// Whether two matched entities are the same.
    pub fn same_as(&self, other: &Entity) -> bool {
        self.comparable() == other.comparable()
    }
}

/// What an entity is compared by.
#[derive(Debug, PartialEq)]
pub enum Comparable<'a> {
    /// A JSON entity, normalised.
    Json(Json),
    /// A file, by its hash.
    Hash(&'a str),
}

/// `value` without the ids that matching accounts for.
pub fn comparable_json(kind: Kind, value: &Json) -> Json {
    let mut value = value.clone();
    if kind.has_id()
        && let Json::Object(map) = &mut value
    {
        map.remove("id");
    }
    if matches!(kind, Kind::Table | Kind::Users)
        && let Some(Json::Array(fields)) = value.get_mut("fields")
    {
        for field in fields {
            if let Json::Object(map) = field {
                map.remove("id");
            }
        }
    }
    value
}

/// Every entity of `tree`, in path order.
///
/// `feldspar-structure.json` is not an entity, and neither is a file that is
/// not JSON and not under a store's `files/`: a repository's subdirectory may
/// hold a README beside the structure.
pub fn entities(tree: &StructureTree) -> Vec<Entity> {
    let mut out = Vec::new();
    for (path, entry) in tree.iter() {
        if path == FORMAT_FILE {
            continue;
        }
        match entry {
            TreeEntry::File(bytes) => {
                if let Some((store, rest)) = store_file_parts(path) {
                    out.push(Entity {
                        kind: Kind::StoreFile,
                        name: rest.to_owned(),
                        id: None,
                        scope: Some(format!("file-stores/{store}.json")),
                        path: path.to_owned(),
                        content: Content::file(bytes),
                    });
                }
            }
            TreeEntry::Blob { hash, size } => {
                if let Some((store, rest)) = store_file_parts(path) {
                    out.push(Entity {
                        kind: Kind::StoreFile,
                        name: rest.to_owned(),
                        id: None,
                        scope: Some(format!("file-stores/{store}.json")),
                        path: path.to_owned(),
                        content: Content::File {
                            hash: hash.clone(),
                            size: *size,
                            bytes: None,
                        },
                    });
                }
            }
            TreeEntry::Json(value) => json_entities(path, value, &mut out),
        }
    }
    out
}

/// `(store segment, path in the store)` for a store file's tree path.
fn store_file_parts(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("file-stores/")?;
    let (store, rest) = rest.split_once('/')?;
    let rest = rest.strip_prefix(STORE_FILES_DIR)?.strip_prefix('/')?;
    (!store.is_empty() && !rest.is_empty()).then_some((store, rest))
}

fn json_entities(path: &str, value: &Json, out: &mut Vec<Entity>) {
    let one = |kind: Kind, name: String, scope: Option<String>| Entity {
        kind,
        id: if kind.has_id() {
            value.get("id").and_then(Json::as_str).map(str::to_owned)
        } else {
            None
        },
        name,
        scope,
        path: path.to_owned(),
        content: Content::Json {
            value: value.clone(),
        },
    };
    let named = |key: &str| {
        value
            .get(key)
            .and_then(Json::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| file_stem(path).to_owned())
    };

    match path {
        "roles.json" => {
            for role in value.as_array().into_iter().flatten() {
                let level = role.get("role").map(Json::to_string).unwrap_or_default();
                out.push(list_item(Kind::Role, level, path, role));
            }
            return;
        }
        "modules.json" => {
            for module in value.as_array().into_iter().flatten() {
                let name = module
                    .get("name")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned();
                out.push(list_item(Kind::Module, name, path, module));
            }
            return;
        }
        "users.json" => {
            out.push(one(Kind::Users, "users".to_owned(), None));
            return;
        }
        _ => {}
    }
    if let Some(section) = path
        .strip_prefix("settings/")
        .and_then(|p| p.strip_suffix(".json"))
        .filter(|s| !s.contains('/'))
    {
        if let Json::Object(map) = value {
            for (key, setting) in map {
                out.push(list_item(
                    Kind::Setting,
                    format!("{section}.{key}"),
                    path,
                    setting,
                ));
            }
        }
        return;
    }
    let parts: Vec<&str> = path.split('/').collect();
    let kind = match parts.as_slice() {
        ["tables", _] => Some(Kind::Table),
        ["db-connections", _] => Some(Kind::DbConnection),
        ["file-stores", _] => Some(Kind::FileStore),
        ["code-libraries", _] => Some(Kind::CodeLibrary),
        ["triggers", _] => Some(Kind::Trigger),
        ["streams", _] => Some(Kind::Stream),
        ["agents", _] => Some(Kind::Agent),
        ["llm-providers", _] => Some(Kind::LlmProvider),
        ["mcp-servers", _] => Some(Kind::McpServer),
        ["analytics", "datasets", _] => Some(Kind::Dataset),
        ["analytics", "models", _] => Some(Kind::Model),
        ["analytics", "workspaces", _] => Some(Kind::Workspace),
        ["checklists", _] => Some(Kind::Checklist),
        _ => None,
    };
    if let Some(kind) = kind {
        out.push(one(kind, named("name"), None));
        return;
    }
    if let ["applications", app, rest @ ..] = parts.as_slice() {
        let scope = Some(format!("applications/{app}/application.json"));
        match rest {
            ["application.json"] => {
                out.push(one(Kind::Application, named("subdomain"), None));
                return;
            }
            ["views", _] => return out.push(one(Kind::View, named("name"), scope)),
            ["pages", _] => return out.push(one(Kind::Page, named("name"), scope)),
            ["library", _] => return out.push(one(Kind::LibraryItem, named("name"), scope)),
            ["translations", file] => {
                let locale = file.strip_suffix(".json").unwrap_or(file).to_owned();
                return out.push(one(Kind::Translation, locale, scope));
            }
            _ => {}
        }
    }
    out.push(one(Kind::Other, path.to_owned(), None));
}

/// One item of a list file (`roles.json`, `modules.json`) or one key of a
/// settings section: an entity with no id of its own, sharing its file.
fn list_item(kind: Kind, name: String, path: &str, value: &Json) -> Entity {
    Entity {
        kind,
        name,
        id: None,
        scope: None,
        path: path.to_owned(),
        content: Content::Json {
            value: value.clone(),
        },
    }
}

/// The path segment naming a scope: `assets` for `file-stores/assets.json`,
/// `desk` for `applications/desk/application.json`.
fn scope_segment(scope: &str) -> &str {
    if let Some(rest) = scope.strip_prefix("applications/") {
        return rest.split('/').next().unwrap_or(rest);
    }
    file_stem(scope)
}

fn file_stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".json").unwrap_or(file)
}

/// The entities of `tree`, with each one's index by path, for looking up a
/// child's scope.
#[derive(Debug, Clone, Default)]
pub struct Entities {
    /// Every entity.
    pub list: Vec<Entity>,
    /// The index of the (first) entity in each file.
    by_path: BTreeMap<String, usize>,
}

impl Entities {
    /// Read `tree`'s entities.
    pub fn of(tree: &StructureTree) -> Entities {
        let list = entities(tree);
        let mut by_path = BTreeMap::new();
        for (i, e) in list.iter().enumerate() {
            by_path.entry(e.path.clone()).or_insert(i);
        }
        Entities { list, by_path }
    }

    /// The entity whose file is `path`.
    pub fn at(&self, path: &str) -> Option<usize> {
        self.by_path.get(path).copied()
    }
}

/// Write `entity` into `tree`, replacing what is at its place.
///
/// The inverse of [`entities`] for one entity: a list item or a setting goes
/// into its shared file, anything else is its file. Used to build a target
/// tree out of another one (a commit's selection, a discard, a registry save).
pub fn put_entity(tree: &mut StructureTree, entity: &Entity) {
    match entity.kind {
        Kind::Role | Kind::Module => {
            let key = if entity.kind == Kind::Role {
                "role"
            } else {
                "name"
            };
            let mut list = tree
                .json(&entity.path)
                .and_then(Json::as_array)
                .cloned()
                .unwrap_or_default();
            let value = entity.json().cloned().unwrap_or(Json::Null);
            let same = |item: &Json| list_key(entity.kind, item.get(key)) == entity.name;
            match list.iter_mut().find(|item| same(item)) {
                Some(item) => *item = value,
                None => list.push(value),
            }
            sort_list(entity.kind, &mut list);
            tree.insert_json(entity.path.clone(), Json::Array(list));
        }
        Kind::Setting => {
            let mut section = tree
                .json(&entity.path)
                .and_then(Json::as_object)
                .cloned()
                .unwrap_or_default();
            let key = setting_key(&entity.name);
            section.insert(key.to_owned(), entity.json().cloned().unwrap_or(Json::Null));
            tree.insert_json(entity.path.clone(), Json::Object(section));
        }
        _ => match &entity.content {
            Content::Json { value } => tree.insert_json(entity.path.clone(), value.clone()),
            Content::File {
                bytes: Some(bytes), ..
            } => tree.insert_file(entity.path.clone(), bytes.clone()),
            Content::File { hash, size, .. } => tree.insert_blob(entity.path.clone(), hash, *size),
        },
    }
}

/// Take `entity` out of `tree`.
pub fn remove_entity(tree: &mut StructureTree, entity: &Entity) {
    match entity.kind {
        Kind::Role | Kind::Module => {
            let key = if entity.kind == Kind::Role {
                "role"
            } else {
                "name"
            };
            if let Some(list) = tree.json(&entity.path).and_then(Json::as_array) {
                let kept: Vec<Json> = list
                    .iter()
                    .filter(|item| list_key(entity.kind, item.get(key)) != entity.name)
                    .cloned()
                    .collect();
                tree.insert_json(entity.path.clone(), Json::Array(kept));
            }
        }
        Kind::Setting => {
            if let Some(section) = tree.json(&entity.path).and_then(Json::as_object) {
                let mut section = section.clone();
                section.remove(setting_key(&entity.name));
                tree.insert_json(entity.path.clone(), Json::Object(section));
            }
        }
        _ => {
            tree.remove(&entity.path);
        }
    }
}

fn list_key(kind: Kind, value: Option<&Json>) -> String {
    match (kind, value) {
        (Kind::Role, Some(v)) => v.to_string(),
        (_, Some(Json::String(s))) => s.clone(),
        _ => String::new(),
    }
}

/// The key of a setting, out of its `section.key` name.
fn setting_key(name: &str) -> &str {
    name.split_once('.').map_or(name, |(_, key)| key)
}

/// A list file in the order the export writes it: roles by level, modules by
/// name.
fn sort_list(kind: Kind, list: &mut [Json]) {
    if kind == Kind::Role {
        list.sort_by_key(|r| r.get("role").and_then(Json::as_i64).unwrap_or(0));
    } else {
        list.sort_by(|a, b| {
            a.get("name")
                .and_then(Json::as_str)
                .cmp(&b.get("name").and_then(Json::as_str))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tree_is_read_as_entities() {
        let mut tree = StructureTree::new();
        tree.insert_json(FORMAT_FILE, json!({ "format": 1, "writer": "x" }));
        tree.insert_json(
            "roles.json",
            json!([{ "role": 1, "name": "Admin" }, { "role": 40, "name": "staff" }]),
        );
        tree.insert_json(
            "settings/localisation.json",
            json!({ "default_locale": "fr" }),
        );
        tree.insert_json(
            "tables/tickets.json",
            json!({ "id": "t1", "name": "tickets" }),
        );
        tree.insert_json(
            "applications/desk/application.json",
            json!({ "id": "a1", "name": "Desk", "subdomain": "desk" }),
        );
        tree.insert_json(
            "applications/desk/views/list.json",
            json!({ "id": "v1", "name": "list" }),
        );
        tree.insert_file("file-stores/assets/files/img/logo.png", vec![1, 2, 3]);
        tree.insert_file("README.md", b"hello".to_vec());

        let all = Entities::of(&tree);
        let keys: Vec<String> = all.list.iter().map(Entity::key).collect();
        assert_eq!(
            keys,
            [
                "application:desk",
                "view:desk/list",
                "store_file:assets/img/logo.png",
                "role:1",
                "role:40",
                "setting:localisation.default_locale",
                "table:tickets",
            ]
        );
        let view = &all.list[1];
        assert_eq!(view.id.as_deref(), Some("v1"));
        assert_eq!(
            view.scope.as_deref(),
            Some("applications/desk/application.json")
        );
        // A store file is known by git's own hash of its bytes.
        let file = &all.list[2];
        assert!(matches!(
            &file.content,
            Content::File { hash, size: Some(3), .. } if hash == &blob_hash(&[1, 2, 3])
        ));
    }

    #[test]
    fn the_blob_hash_is_gits() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(
            blob_hash(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn ids_are_not_differences() {
        let a = Entity {
            kind: Kind::Table,
            name: "t".into(),
            id: Some("1".into()),
            scope: None,
            path: "tables/t.json".into(),
            content: Content::Json {
                value: json!({ "id": "1", "name": "t", "fields": [{ "id": "f1", "name": "x" }] }),
            },
        };
        let mut b = a.clone();
        b.id = Some("2".into());
        b.content = Content::Json {
            value: json!({ "id": "2", "name": "t", "fields": [{ "id": "f2", "name": "x" }] }),
        };
        assert!(a.same_as(&b));
        b.content = Content::Json {
            value: json!({ "id": "2", "name": "t", "fields": [{ "id": "f2", "name": "y" }] }),
        };
        assert!(!a.same_as(&b));
    }

    #[test]
    fn an_entity_is_put_back_into_its_shared_file() {
        let mut tree = StructureTree::new();
        tree.insert_json("roles.json", json!([{ "role": 1, "name": "Admin" }]));
        tree.insert_json("settings/maps.json", json!({ "a": 1 }));
        let all = Entities::of(&tree);

        let mut role = all.list[0].clone();
        role.name = "40".into();
        role.content = Content::Json {
            value: json!({ "role": 40, "name": "staff" }),
        };
        put_entity(&mut tree, &role);
        assert_eq!(
            tree.json("roles.json").unwrap(),
            &json!([{ "role": 1, "name": "Admin" }, { "role": 40, "name": "staff" }])
        );
        remove_entity(&mut tree, &all.list[0]);
        assert_eq!(
            tree.json("roles.json").unwrap(),
            &json!([{ "role": 40, "name": "staff" }])
        );

        let mut setting = all.list[1].clone();
        setting.name = "maps.b".into();
        setting.content = Content::Json { value: json!(2) };
        put_entity(&mut tree, &setting);
        assert_eq!(
            tree.json("settings/maps.json").unwrap(),
            &json!({ "a": 1, "b": 2 })
        );
    }
}

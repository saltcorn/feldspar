//! The registry editor (APP_SYNC_PROPOSAL D7, §7.6): every configuration value
//! of the installation, one JSON file at a time, edited in the browser or at
//! the CLI.
//!
//! **What it covers is the instance tree** ([`Mode::Instance`]): the portable
//! tree plus what is this instance's own — every settings section, the build
//! hosts, a store's directory and keys — each marked
//! ([`RegistryEntry::instance_only`]). An entry is one file of that tree:
//! `applications/desk/views/list.json`, `tables/tickets.json`,
//! `settings/email.json`. Store files are not entries; they are edited in the
//! file manager. Secrets are the sentinel, as everywhere: handing the sentinel
//! back keeps the stored value, and typing a value replaces it.
//!
//! **A save is a one-entity deploy against live.** The target is the instance
//! tree with the one file replaced, added or removed ([`edit_target`]), and it
//! goes through the planner ([`plan_instance`]) and the executor exactly as a
//! deploy does: classified, dry-run, ticks for what loses data, the sync lock,
//! the stale-plan check, the safety backup and a ledger row (of kind `edit`,
//! with the file's JSON before and after). Two staleness checks guard it: the
//! **version** the editor opened ([`version`]) must still be live's, so an
//! edit over a change someone else saved since is refused; and the plan's
//! digest, as for any deploy.
//!
//! After a save the file is exported again, and what is stored is compared with
//! what was submitted ([`EditReceipt::changes`]): a value the save path
//! normalised or dropped is reported rather than silently differing.
//!
//! With a sync link, a save of a portable entity is an ordinary local change
//! (§4); an instance-only value is not in the portable tree, so editing it is
//! never one.

use serde::Serialize;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use sc_error::{Error, Result};
use sc_types::SECRET_SENTINEL;

use crate::deploy::{Confirm, Deployer, EditRun, Policy, Receipt, Request, Run, release};
use crate::entity::{Content, Entities, Entity, Kind};
use crate::entity_diff::{JsonChange, json_changes};
use crate::export::{Export, INTERNAL_SECTION, InstanceOnly, Mode};
use crate::format::FORMAT_FILE;
use crate::ledger::{LedgerKind, Source, bootstrap_ledger, take_lock};
use crate::plan::{Plan, plan_instance};
use crate::tree::{StructureTree, TreeEntry, render_json};
use crate::validate::validate;

/// One file of the instance tree, as the registry lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegistryEntry {
    /// Its path in the tree: what the registry calls it by.
    pub path: String,
    /// What kind of entity it is. A file holding a list (`roles.json`) or a
    /// settings section is listed as the kind of its items.
    pub kind: Kind,
    /// Its name: the entity's, or the section's, or `roles` / `modules`.
    pub name: String,
    /// For a view, page, library item or translation, the application it is
    /// in (its subdomain).
    pub application: Option<String>,
    /// What of it is this instance's alone; `None` when all of it is
    /// portable.
    pub instance_only: Option<InstanceOnly>,
    /// The version of what is there now ([`version`]).
    pub version: String,
}

/// One file of the instance tree, with its JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegistryEntity {
    /// Which file.
    #[serde(flatten)]
    pub entry: RegistryEntry,
    /// Its JSON, secrets as the sentinel.
    pub value: Json,
}

/// The version of a file's JSON: what a save names to say what it was edited
/// from. A digest of the canonical rendering, so key order does not count.
pub fn version(value: &Json) -> String {
    let rendered = render_json(&crate::tree::canonical(value.clone()));
    let digest = Sha256::digest(&rendered);
    format!("{digest:x}")[..16].to_owned()
}

/// Every entry of an instance export, in path order.
pub fn list(export: &Export) -> Vec<RegistryEntry> {
    let entities = Entities::of(&export.tree);
    export
        .tree
        .iter()
        .filter_map(|(path, entry)| {
            let TreeEntry::Json(value) = entry else {
                return None;
            };
            entry_of(export, &entities, path, value)
        })
        .collect()
}

/// The entry at `path`, with its JSON.
pub fn get(export: &Export, path: &str) -> Option<RegistryEntity> {
    let value = export.tree.json(path)?;
    let entry = entry_of(export, &Entities::of(&export.tree), path, value)?;
    Some(RegistryEntity {
        entry,
        value: value.clone(),
    })
}

fn entry_of(
    export: &Export,
    entities: &Entities,
    path: &str,
    value: &Json,
) -> Option<RegistryEntry> {
    if path == FORMAT_FILE {
        return None;
    }
    let (kind, name, scope) = match entities.at(path).map(|i| &entities.list[i]) {
        Some(e) => (e.kind, file_name(e), e.scope.clone()),
        // A list or a section with nothing in it yet has no entity to read
        // its kind off.
        None => (kind_of_path(path)?, file_stem(path).to_owned(), None),
    };
    if kind == Kind::Other || kind == Kind::StoreFile {
        return None;
    }
    let application = scope.as_deref().and_then(|s| {
        s.strip_prefix("applications/")
            .and_then(|rest| rest.split('/').next())
            .map(str::to_owned)
    });
    Some(RegistryEntry {
        path: path.to_owned(),
        kind,
        name,
        application,
        instance_only: export.instance_only.get(path).cloned(),
        version: version(value),
    })
}

/// The name an entry is listed by: the entity's, or the file's for a list or
/// a section, whose entities are its items.
fn file_name(e: &Entity) -> String {
    match e.kind {
        Kind::Role | Kind::Module | Kind::Setting => file_stem(&e.path).to_owned(),
        _ => e.name.clone(),
    }
}

fn file_stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".json").unwrap_or(file)
}

/// The kind a file at `path` holds, from where it is: what a new entry is
/// checked by.
pub fn kind_of_path(path: &str) -> Option<Kind> {
    if path == "roles.json" {
        return Some(Kind::Role);
    }
    if path == "modules.json" {
        return Some(Kind::Module);
    }
    if path == "users.json" {
        return Some(Kind::Users);
    }
    if !path.ends_with(".json") {
        return None;
    }
    let mut probe = StructureTree::new();
    probe.insert_json(path, serde_json::json!({}));
    if path.starts_with("settings/") && !path["settings/".len()..].contains('/') {
        return Some(Kind::Setting);
    }
    crate::entity::entities(&probe)
        .first()
        .map(|e| e.kind)
        .filter(|k| !matches!(k, Kind::Other | Kind::StoreFile))
}

/// The file an address names, for the CLI (§7.3): a tree path
/// (`tables/tickets.json`), or a kind and a name — `table/tickets`,
/// `setting/email`, and for a kind inside an application `view/desk/list`.
/// A path that is not there yet is answered as it is, for a new entry; a kind
/// and name that match nothing are refused.
pub fn resolve(entries: &[RegistryEntry], address: &str) -> Result<String> {
    let address = address.trim();
    if address.ends_with(".json") {
        return Ok(address.to_owned());
    }
    let (kind, rest) = address.split_once('/').ok_or_else(|| {
        Error::invalid(format!(
            "`{address}` names no registry entry: write KIND/NAME (`table/tickets`, \
             `view/APP/NAME`) or the entry's path (`tables/tickets.json`)"
        ))
    })?;
    let kind = Kind::parse(kind).ok_or_else(|| {
        Error::invalid(format!(
            "`{kind}` is not a kind of registry entry; `feldspar registry list` shows them"
        ))
    })?;
    let found: Vec<&RegistryEntry> = entries
        .iter()
        .filter(|e| e.kind == kind)
        .filter(|e| match &e.application {
            Some(app) => rest == format!("{app}/{}", e.name),
            None => rest == e.name,
        })
        .collect();
    match found.as_slice() {
        [one] => Ok(one.path.clone()),
        [] => Err(Error::not_found(format!(
            "there is no {} `{rest}`; to create one, name the path it goes at \
             (such as `{}`)",
            kind.as_str(),
            example_path(kind)
        ))),
        many => Err(Error::invalid(format!(
            "`{address}` names more than one entry ({}); name its path instead",
            many.iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn example_path(kind: Kind) -> &'static str {
    match kind {
        Kind::View => "applications/APP/views/NAME.json",
        Kind::Page => "applications/APP/pages/NAME.json",
        Kind::Application => "applications/NAME/application.json",
        Kind::Trigger => "triggers/NAME.json",
        _ => "tables/NAME.json",
    }
}

/// One registry save: the file, what it becomes, and what it was edited from.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    /// The file.
    pub path: String,
    /// Its new JSON; `None` deletes it.
    pub value: Option<Json>,
    /// The [`version`] the editor opened; `None` for a new file.
    pub version: Option<String>,
}

/// A save's plan, for the review.
#[derive(Debug, Clone, Serialize)]
pub struct EditPlan {
    /// The plan.
    pub plan: Plan,
    /// The file's JSON as it is now; `None` when it is new.
    pub before: Option<Json>,
    /// The JSON as planned: what was submitted, with an id given to a new
    /// entity that had none. A confirm submits this.
    pub value: Option<Json>,
}

/// What a save did.
#[derive(Debug, Clone, Serialize)]
pub struct EditReceipt {
    /// The run's receipt, as a deploy's.
    #[serde(flatten)]
    pub receipt: Receipt,
    /// Where the entity is now (a rename moves its file); `None` once deleted.
    pub path: Option<String>,
    /// What is stored, exported again.
    pub stored: Option<Json>,
    /// Its version.
    pub stored_version: Option<String>,
    /// Where what is stored differs from what was submitted: a value the save
    /// path normalised, or a key it does not keep. A secret, which is the
    /// sentinel again, is not a difference.
    pub changes: Vec<JsonChange>,
}

/// The target of `edit`: the instance tree `live` with the one file replaced,
/// added or removed, and the JSON as planned. Refuses an edit of a file that
/// changed since its version was read, a new file where one is, and a file the
/// registry does not hold.
pub fn edit_target(live: &StructureTree, edit: &Edit) -> Result<(StructureTree, Option<Json>)> {
    let path = edit.path.as_str();
    let kind = kind_of_path(path).ok_or_else(|| {
        Error::invalid(format!(
            "`{path}` is not something the registry holds; an entry is a JSON file such as \
             `tables/<name>.json` or `applications/<app>/views/<name>.json`"
        ))
    })?;
    let current = live.json(path);
    match (current, &edit.version) {
        (Some(current), Some(opened)) => {
            if &version(current) != opened {
                return Err(stale(path));
            }
        }
        (Some(_), None) => {
            return Err(Error::invalid(format!(
                "`{path}` is there already; open it to edit it"
            )));
        }
        (None, Some(_)) => {
            return Err(Error::invalid(format!(
                "`{path}` was deleted since it was opened; nothing was saved"
            )));
        }
        (None, None) if edit.value.is_none() => {
            return Err(Error::not_found(format!("there is no `{path}` to delete")));
        }
        (None, None) => {}
    }

    let mut target = live.clone();
    target.remove(path);
    let Some(mut value) = edit.value.clone() else {
        return Ok((target, None));
    };
    check_shape(kind, path, &value)?;
    if current.is_none()
        && kind.has_id()
        && let Json::Object(map) = &mut value
        && map.get("id").is_none_or(Json::is_null)
    {
        map.insert("id".to_owned(), Json::String(Uuid::new_v4().to_string()));
    }
    let value = crate::tree::canonical(value);
    target.insert_json(path, value.clone());
    Ok((target, Some(value)))
}

fn stale(path: &str) -> Error {
    Error::invalid(format!(
        "`{path}` was changed since it was opened, so this save is stale and nothing was \
         saved; open it again to see the change, then make yours"
    ))
}

/// What can be refused before planning: the wrong JSON shape for the file,
/// and a setting that is not in its section.
fn check_shape(kind: Kind, path: &str, value: &Json) -> Result<()> {
    match kind {
        Kind::Role | Kind::Module if !value.is_array() => Err(Error::invalid(format!(
            "`{path}` must be a JSON list, one item per {}",
            kind.as_str()
        ))),
        Kind::Role | Kind::Module => Ok(()),
        Kind::Setting => {
            let Json::Object(map) = value else {
                return Err(Error::invalid(format!(
                    "`{path}` must be a JSON object of settings"
                )));
            };
            let section = file_stem(path);
            let keys: Vec<&str> = crate::export::sections(Mode::Instance)
                .into_iter()
                .find(|(name, _, _)| *name == section)
                .map(|(_, fields, _)| fields.into_iter().map(|f| f.name()).collect())
                .ok_or_else(|| {
                    Error::invalid(format!("there is no settings section `{section}`"))
                })?;
            match map.keys().find(|k| !keys.contains(&k.as_str())) {
                Some(unknown) => Err(Error::invalid(format!(
                    "`{unknown}` is not a setting of the `{section}` section{}; its settings \
                     are {}",
                    if section == INTERNAL_SECTION {
                        " (or is one only the server writes)"
                    } else {
                        ""
                    },
                    keys.join(", ")
                ))),
                None => Ok(()),
            }
        }
        _ if !value.is_object() => Err(Error::invalid(format!("`{path}` must be a JSON object"))),
        _ => Ok(()),
    }
}

/// The entity saved at `path` in the target, as it is in `after`: where it is
/// now and its JSON. Found by id where it has one, since a rename moves its
/// file.
pub(crate) fn saved(
    after: &StructureTree,
    target: &StructureTree,
    path: &str,
) -> Option<(String, Json)> {
    let wanted = Entities::of(target);
    let one = wanted.at(path).map(|i| &wanted.list[i]);
    if let Some(e) = one
        && e.kind.has_id()
        && let Some(id) = &e.id
    {
        let found = Entities::of(after);
        if let Some(there) = found
            .list
            .iter()
            .find(|f| f.kind == e.kind && f.id.as_ref() == Some(id))
            && let Content::Json { value } = &there.content
        {
            return Some((there.path.clone(), value.clone()));
        }
    }
    after
        .json(path)
        .map(|value| (path.to_owned(), value.clone()))
}

/// What is stored that differs from what was submitted, leaving out a secret
/// that reads back as the sentinel.
fn differences(submitted: &Json, stored: &Json) -> Vec<JsonChange> {
    json_changes(Some(submitted), Some(stored))
        .into_iter()
        .filter(|c| c.after.as_ref().and_then(Json::as_str) != Some(SECRET_SENTINEL))
        .collect()
}

/// Who a save is by, and under what policy.
#[derive(Debug, Clone)]
pub struct EditBy {
    /// The admin's email, or the operator.
    pub actor: Option<String>,
    /// From where.
    pub source: Source,
    /// The instance's policy (§5): with `destructive: never`, a save that
    /// drops data is refused here as a deploy is.
    pub policy: Policy,
}

impl Deployer<'_> {
    /// The instance tree, exported, with its marks: what the registry lists.
    pub async fn instance_export(&self) -> Result<Export> {
        let registries = self.executor.host.registries()?;
        crate::export::export_mode(self.executor.catalog, &registries, Mode::Instance).await
    }

    /// The plan the save review shows.
    pub async fn plan_edit(&self, edit: &Edit) -> Result<EditPlan> {
        let live = self.live_in(Mode::Instance).await?;
        self.plan_edit_from(&live, edit).await
    }

    async fn plan_edit_from(&self, live: &StructureTree, edit: &Edit) -> Result<EditPlan> {
        let (target, value) = edit_target(live, edit)?;
        let mut plan = plan_instance(live, &target);
        validate(self.executor.catalog, &mut plan).await?;
        Ok(EditPlan {
            plan,
            before: live.json(&edit.path).cloned(),
            value,
        })
    }

    /// Apply the save `confirm` reviewed: under the sync lock, re-checked
    /// against live — the version, then the plan's digest — and recorded.
    /// `edit.value` is the plan's [`EditPlan::value`].
    pub async fn edit(&self, edit: &Edit, confirm: &Confirm, by: &EditBy) -> Result<EditReceipt> {
        let catalog = self.executor.catalog;
        bootstrap_ledger(catalog).await?;
        let lock = take_lock(catalog, "registry save").await?;
        let result = async {
            let live = self.live_in(Mode::Instance).await?;
            let (target, value) = edit_target(&live, edit)?;
            let mut plan = plan_instance(&live, &target);
            validate(catalog, &mut plan).await?;
            let empty = StructureTree::new();
            let request = Request {
                base: &empty,
                target: &target,
                commit: None,
                branch: None,
                actor: by.actor.clone(),
                source: by.source,
                policy: by.policy,
            };
            let run = Run {
                kind: LedgerKind::Edit,
                request: &request,
                live: &live,
                target: &target,
                edit: Some(EditRun {
                    path: edit.path.clone(),
                    before: live.json(&edit.path).cloned(),
                }),
            };
            let (receipt, after) = self.run(run, plan, confirm).await?;
            let saved = value
                .as_ref()
                .and_then(|_| saved(&after, &target, &edit.path));
            let changes = match (&value, &saved) {
                (Some(submitted), Some((_, stored))) => differences(submitted, stored),
                _ => Vec::new(),
            };
            Ok(EditReceipt {
                receipt,
                stored_version: saved.as_ref().map(|(_, v)| version(v)),
                path: saved.as_ref().map(|(p, _)| p.clone()),
                stored: saved.map(|(_, v)| v),
                changes,
            })
        }
        .await;
        release(catalog, lock, result).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> StructureTree {
        let mut t = StructureTree::new();
        t.insert_json(FORMAT_FILE, json!({ "format": 1 }));
        t.insert_json("roles.json", json!([{ "role": 1, "name": "admin" }]));
        t.insert_json("settings/email.json", json!({}));
        t.insert_json(
            "applications/desk/application.json",
            json!({ "id": "a1", "name": "Desk", "subdomain": "desk" }),
        );
        t.insert_json(
            "applications/desk/views/list.json",
            json!({ "id": "v1", "name": "list" }),
        );
        t.insert_file("file-stores/assets/files/logo.png", vec![1, 2, 3]);
        t
    }

    #[test]
    fn every_json_file_but_the_format_is_an_entry() {
        let export = Export {
            tree: tree(),
            ..Export::default()
        };
        let entries: Vec<(String, Kind, String, Option<String>)> = list(&export)
            .into_iter()
            .map(|e| (e.path, e.kind, e.name, e.application))
            .collect();
        assert_eq!(
            entries,
            [
                (
                    "applications/desk/application.json".into(),
                    Kind::Application,
                    "desk".into(),
                    None
                ),
                (
                    "applications/desk/views/list.json".into(),
                    Kind::View,
                    "list".into(),
                    Some("desk".into())
                ),
                ("roles.json".into(), Kind::Role, "roles".into(), None),
                // An empty section has no entity, and is listed all the same.
                (
                    "settings/email.json".into(),
                    Kind::Setting,
                    "email".into(),
                    None
                ),
            ]
        );
    }

    #[test]
    fn an_address_is_a_path_or_a_kind_and_a_name() {
        let export = Export {
            tree: tree(),
            ..Export::default()
        };
        let entries = list(&export);
        assert_eq!(
            resolve(&entries, "view/desk/list").unwrap(),
            "applications/desk/views/list.json"
        );
        assert_eq!(
            resolve(&entries, "setting/email").unwrap(),
            "settings/email.json"
        );
        assert_eq!(
            resolve(&entries, "tables/new.json").unwrap(),
            "tables/new.json"
        );
        assert!(resolve(&entries, "view/list").is_err());
        assert!(resolve(&entries, "nonsense/x").is_err());
        assert!(resolve(&entries, "tickets").is_err());
    }

    #[test]
    fn a_version_ignores_key_order() {
        assert_eq!(
            version(&json!({ "a": 1, "b": 2 })),
            version(&json!({ "b": 2, "a": 1 }))
        );
        assert_ne!(version(&json!({ "a": 1 })), version(&json!({ "a": 2 })));
    }

    #[test]
    fn a_target_is_live_with_one_file_changed() {
        let live = tree();
        let path = "applications/desk/views/list.json";
        let opened = version(live.json(path).unwrap());
        let edit = Edit {
            path: path.into(),
            value: Some(json!({ "id": "v1", "name": "list", "description": "All" })),
            version: Some(opened.clone()),
        };
        let (target, _) = edit_target(&live, &edit).unwrap();
        assert_eq!(target.json(path).unwrap()["description"], "All");
        assert_eq!(target.len(), live.len());

        // Opened from something else: stale.
        let stale = Edit {
            version: Some("0000".into()),
            ..edit.clone()
        };
        let e = edit_target(&live, &stale).unwrap_err().to_string();
        assert!(e.contains("stale"), "{e}");

        // A delete.
        let delete = Edit {
            value: None,
            ..edit
        };
        assert!(edit_target(&live, &delete).unwrap().0.json(path).is_none());
    }

    #[test]
    fn a_new_entity_is_given_an_id_and_cannot_overwrite() {
        let live = tree();
        let edit = Edit {
            path: "applications/desk/pages/home.json".into(),
            value: Some(json!({ "name": "home" })),
            version: None,
        };
        let (_, value) = edit_target(&live, &edit).unwrap();
        let id = value.unwrap()["id"].as_str().unwrap().to_owned();
        assert!(Uuid::parse_str(&id).is_ok());

        let over = Edit {
            path: "applications/desk/views/list.json".into(),
            ..edit
        };
        assert!(edit_target(&live, &over).is_err());
    }

    #[test]
    fn what_the_registry_does_not_hold_is_refused() {
        let live = tree();
        for (path, value) in [
            ("file-stores/assets/files/logo.png", json!({})),
            ("README.md", json!({})),
            ("somewhere/else.json", json!({})),
            ("roles.json", json!({ "not": "a list" })),
            ("settings/email.json", json!({ "no_such_setting": 1 })),
        ] {
            let version = live.json(path).map(version);
            let edit = Edit {
                path: path.into(),
                value: Some(value),
                version,
            };
            assert!(edit_target(&live, &edit).is_err(), "{path}");
        }
    }
}

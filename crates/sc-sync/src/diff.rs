//! Status: the three-way diff of the base, the branch tip and live
//! (APP_SYNC_PROPOSAL §4).
//!
//! **Matching** (§3.2) comes first: each entity of one tree is paired with the
//! same entity in the others, by id and then by name, within its kind and —
//! for a child kind — within its parent's match. So a renamed field's table is
//! one entity whatever its file is called, and a table built independently on
//! two instances under the same name is one entity with two ids (the
//! instance's is kept).
//!
//! **Classifying** each match then follows §4's table, one class per entity.
//! A git store's pinned commit is weighed apart from the rest of its definition
//! (§3.5): a moved pin is a local change that does not block Deploy, and a
//! conflict on the pin alone resolves to the tip.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value as Json;

use crate::entity::{Entities, Entity, Kind};
use crate::tree::StructureTree;

/// What Status says about one entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// The same in the base, the tip and live; Status hides it.
    InSync,
    /// Changed on the tip; Deploy applies it.
    Incoming,
    /// Deleted on the tip; Deploy deletes it (or defers the delete, §5).
    IncomingDelete,
    /// New on the tip; Deploy creates it.
    IncomingNew,
    /// Changed here; commit or discard it.
    LocalChange,
    /// Deleted here; commit or discard it.
    LocalDelete,
    /// New here; commit or discard it.
    LocalNew,
    /// A git store's checked-out commit moved here, and nothing else about it
    /// changed. Committed like a local change, but it does not block Deploy
    /// (§3.5).
    PinMoved,
    /// Changed on both sides, differently.
    Conflict,
}

impl Class {
    /// Whether the entity is something Status shows.
    pub fn shown(self) -> bool {
        self != Class::InSync
    }

    /// Whether Deploy applies it.
    pub fn is_incoming(self) -> bool {
        matches!(
            self,
            Class::Incoming | Class::IncomingDelete | Class::IncomingNew
        )
    }

    /// Whether it is this instance's to commit or discard.
    pub fn is_local(self) -> bool {
        matches!(
            self,
            Class::LocalChange | Class::LocalDelete | Class::LocalNew | Class::PinMoved
        )
    }

    /// Whether it stops Deploy (D5): every local class and a conflict, except a
    /// moved pin.
    pub fn blocks_deploy(self) -> bool {
        matches!(
            self,
            Class::LocalChange | Class::LocalDelete | Class::LocalNew | Class::Conflict
        )
    }

    /// The words Status shows it under.
    pub fn label(self) -> &'static str {
        match self {
            Class::InSync => "In sync",
            Class::Incoming | Class::IncomingNew => "Incoming",
            Class::IncomingDelete => "Incoming delete",
            Class::LocalChange | Class::LocalDelete | Class::LocalNew | Class::PinMoved => "Local",
            Class::Conflict => "Conflict",
        }
    }
}

/// One entity in Status.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusEntry {
    /// The entity's key (`table:tickets`), from live, else the tip, else the
    /// base.
    pub key: String,
    /// What it is.
    pub kind: Kind,
    /// Its name, from the same side as the key.
    pub name: String,
    /// What Status says about it.
    pub class: Class,
    /// Whether the base should take the tip's version: the sides agree, but
    /// the base does not (both made the same change, or the entity was built
    /// the same on both before they were linked).
    pub adopt: bool,
    /// The entity in the base.
    pub base: Option<Entity>,
    /// The entity at the branch tip.
    pub tip: Option<Entity>,
    /// The entity in live.
    pub live: Option<Entity>,
}

impl StatusEntry {
    /// The side the entry is named from.
    fn named(&self) -> Option<&Entity> {
        self.live
            .as_ref()
            .or(self.tip.as_ref())
            .or(self.base.as_ref())
    }

    /// Whether it is a git store whose pinned commit differs between the tip
    /// and live.
    pub fn pin_differs(&self) -> bool {
        self.kind == Kind::FileStore && pin(self.tip.as_ref()) != pin(self.live.as_ref())
    }
}

/// The three-way comparison of every entity.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Status {
    /// Every entity of the three trees, in sync or not, ordered by kind and
    /// key.
    pub entries: Vec<StatusEntry>,
}

impl Status {
    /// Compare `base`, the branch `tip` and `live`.
    pub fn of(base: &StructureTree, tip: &StructureTree, live: &StructureTree) -> Status {
        // Live first: a name match keeps the instance's id, so live's entities
        // are what the others are matched onto.
        let sides = [Entities::of(live), Entities::of(tip), Entities::of(base)];
        let groups = match_sides(&sides);
        let mut entries: Vec<StatusEntry> = groups
            .into_iter()
            .map(|g| {
                let pick = |side: usize| g.members[side].map(|i| sides[side].list[i].clone());
                let (live, tip, base) = (pick(0), pick(1), pick(2));
                let (class, adopt) = classify(base.as_ref(), tip.as_ref(), live.as_ref());
                let mut entry = StatusEntry {
                    key: String::new(),
                    kind: g.kind,
                    name: String::new(),
                    class,
                    adopt,
                    base,
                    tip,
                    live,
                };
                let named = entry.named().cloned();
                if let Some(named) = named {
                    entry.key = named.key();
                    entry.name = named.name;
                }
                entry
            })
            .collect();
        entries.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
        Status { entries }
    }

    /// The entries Status shows: everything not in sync.
    pub fn shown(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.class.shown())
    }

    /// The entry with this key.
    pub fn entry(&self, key: &str) -> Option<&StatusEntry> {
        self.entries.iter().find(|e| {
            e.key == key
                || [&e.base, &e.tip, &e.live]
                    .into_iter()
                    .flatten()
                    .any(|side| side.key() == key)
        })
    }

    /// What stops Deploy (D5): each entity in a local class or a conflict.
    pub fn deploy_blockers(&self) -> Vec<&StatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.class.blocks_deploy())
            .collect()
    }

    /// How many entities are in each class Status shows.
    pub fn counts(&self) -> Vec<(Class, usize)> {
        let mut counts: Vec<(Class, usize)> = Vec::new();
        for entry in self.shown() {
            match counts.iter_mut().find(|(c, _)| *c == entry.class) {
                Some((_, n)) => *n += 1,
                None => counts.push((entry.class, 1)),
            }
        }
        counts.sort();
        counts
    }
}

/// §4's table, for one entity: its class, and whether the base should take the
/// tip's version.
pub fn classify(
    base: Option<&Entity>,
    tip: Option<&Entity>,
    live: Option<&Entity>,
) -> (Class, bool) {
    let is_git = |e: Option<&Entity>| e.is_some_and(is_git_store);
    if [base, tip, live].iter().all(|e| e.is_none() || is_git(*e)) && (is_git(tip) || is_git(live))
    {
        return classify_git_store(base, tip, live);
    }
    classify_by(base, tip, live, |a, b| a.same_as(b))
}

/// The table itself, with `same` saying whether two versions are equal.
fn classify_by(
    base: Option<&Entity>,
    tip: Option<&Entity>,
    live: Option<&Entity>,
    same: impl Fn(&Entity, &Entity) -> bool,
) -> (Class, bool) {
    let agree = |a: Option<&Entity>, b: Option<&Entity>| match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same(a, b),
        _ => false,
    };
    match base {
        None => match (tip, live) {
            (Some(_), None) => (Class::IncomingNew, false),
            (None, Some(_)) => (Class::LocalNew, false),
            (Some(t), Some(l)) if same(t, l) => (Class::InSync, true),
            (Some(_), Some(_)) => (Class::Conflict, false),
            // Nothing anywhere: not an entity any tree has.
            (None, None) => (Class::InSync, false),
        },
        Some(b) => {
            let tip_same = agree(Some(b), tip);
            let live_same = agree(Some(b), live);
            match (tip_same, live_same) {
                (true, true) => (Class::InSync, false),
                (false, true) if tip.is_none() => (Class::IncomingDelete, false),
                (false, true) => (Class::Incoming, false),
                (true, false) if live.is_none() => (Class::LocalDelete, false),
                (true, false) => (Class::LocalChange, false),
                (false, false) if agree(tip, live) => (Class::InSync, true),
                (false, false) => (Class::Conflict, false),
            }
        }
    }
}

/// A git store: its definition by the table, and its pin weighed apart.
fn classify_git_store(
    base: Option<&Entity>,
    tip: Option<&Entity>,
    live: Option<&Entity>,
) -> (Class, bool) {
    let (rest, rest_adopt) = classify_by(base, tip, live, |a, b| {
        without_pin(a.json()) == without_pin(b.json())
    });
    // A store new or deleted on one side is that, whatever its pin.
    if base.is_none() || tip.is_none() || live.is_none() {
        return classify_by(base, tip, live, |a, b| a.same_as(b));
    }
    let (pin_class, pin_adopt) = classify_by(base, tip, live, |a, b| pin(Some(a)) == pin(Some(b)));
    if rest.blocks_deploy() {
        return (rest, false);
    }
    let class = match pin_class {
        Class::InSync => rest,
        Class::Incoming if rest == Class::InSync => Class::Incoming,
        // Only here, and nothing else about the store changed.
        Class::LocalChange if rest == Class::InSync => Class::PinMoved,
        // A conflict on the pin alone resolves to the tip.
        Class::Conflict => Class::Incoming,
        _ => rest,
    };
    let adopt = class == Class::InSync && (rest_adopt || pin_adopt);
    (class, adopt)
}

/// Whether `entity` is a git store's definition.
pub fn is_git_store(entity: &Entity) -> bool {
    entity.kind == Kind::FileStore
        && entity
            .json()
            .and_then(|v| v.get("backend"))
            .and_then(Json::as_str)
            == Some(sc_files::GIT_BACKEND)
}

/// A git store's pinned commit.
pub fn pin(entity: Option<&Entity>) -> Option<&str> {
    entity?.json()?.get("commit")?.as_str()
}

fn without_pin(value: Option<&Json>) -> Option<Json> {
    let mut value = crate::entity::comparable_json(Kind::FileStore, value?);
    if let Json::Object(map) = &mut value {
        map.remove("commit");
    }
    Some(value)
}

// --- matching -----------------------------------------------------------------

/// One entity across the sides: which entity of each side it is.
#[derive(Debug, Clone)]
pub struct Group {
    /// The kind.
    pub kind: Kind,
    /// The group of the entity it lives inside.
    pub scope: Option<usize>,
    /// The index of its entity on each side, if that side has it.
    pub members: Vec<Option<usize>>,
}

/// Match the entities of every side (§3.2): within a kind, and within the
/// match of their parent, first by id and then by name.
///
/// Side 0's entities are taken as they are; each later side's are matched onto
/// the groups so far that have no entity of that side yet, all by id before any
/// by name, so an id match is never taken by a name match first. What matches
/// nothing starts a group of its own.
pub fn match_sides(sides: &[Entities]) -> Vec<Group> {
    let n = sides.len();
    let mut groups: Vec<Group> = Vec::new();
    // Where each side's entity went.
    let mut placed: Vec<Vec<Option<usize>>> =
        sides.iter().map(|s| vec![None; s.list.len()]).collect();
    // The groups each (kind, scope, id) and (kind, scope, name) is in.
    type Key = (Kind, Option<usize>, String);
    let mut by_id: HashMap<Key, Vec<usize>> = HashMap::new();
    let mut by_name: HashMap<Key, Vec<usize>> = HashMap::new();

    for kind in Kind::ALL {
        for side in 0..n {
            let entities = &sides[side];
            let of_kind: Vec<usize> = (0..entities.list.len())
                .filter(|&i| entities.list[i].kind == kind)
                .collect();
            // The parent's group, through this side's own parent entity.
            let scope_of = |i: usize, placed: &Vec<Vec<Option<usize>>>| {
                let scope = entities.list[i].scope.as_deref()?;
                let parent = entities.at(scope)?;
                placed[side][parent]
            };
            let join = |i: usize,
                        g: usize,
                        groups: &mut Vec<Group>,
                        placed: &mut Vec<Vec<Option<usize>>>,
                        by_id: &mut HashMap<Key, Vec<usize>>,
                        by_name: &mut HashMap<Key, Vec<usize>>| {
                let e = &entities.list[i];
                groups[g].members[side] = Some(i);
                placed[side][i] = Some(g);
                let scope = groups[g].scope;
                if let Some(id) = &e.id {
                    by_id.entry((kind, scope, id.clone())).or_default().push(g);
                }
                by_name
                    .entry((kind, scope, e.name.clone()))
                    .or_default()
                    .push(g);
            };
            // By id.
            for &i in &of_kind {
                let Some(id) = &entities.list[i].id else {
                    continue;
                };
                let scope = scope_of(i, &placed);
                let found = by_id.get(&(kind, scope, id.clone())).and_then(|gs| {
                    gs.iter()
                        .copied()
                        .find(|&g| groups[g].members[side].is_none())
                });
                if let Some(g) = found {
                    join(i, g, &mut groups, &mut placed, &mut by_id, &mut by_name);
                }
            }
            // By name, then on its own.
            for &i in &of_kind {
                if placed[side][i].is_some() {
                    continue;
                }
                let scope = scope_of(i, &placed);
                let name = entities.list[i].name.clone();
                let found = by_name.get(&(kind, scope, name)).and_then(|gs| {
                    gs.iter()
                        .copied()
                        .find(|&g| groups[g].members[side].is_none())
                });
                let g = match found {
                    Some(g) => g,
                    None => {
                        groups.push(Group {
                            kind,
                            scope,
                            members: vec![None; n],
                        });
                        groups.len() - 1
                    }
                };
                join(i, g, &mut groups, &mut placed, &mut by_id, &mut by_name);
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree(entries: &[(&str, Json)]) -> StructureTree {
        let mut tree = StructureTree::new();
        for (path, value) in entries {
            tree.insert_json(*path, value.clone());
        }
        tree
    }

    fn view(id: &str, name: &str, label: &str) -> (String, Json) {
        (
            format!("applications/desk/views/{name}.json"),
            json!({ "id": id, "name": name, "label": label }),
        )
    }

    fn app() -> (&'static str, Json) {
        (
            "applications/desk/application.json",
            json!({ "id": "a1", "name": "desk", "subdomain": "desk" }),
        )
    }

    /// A tree holding the application and the given views.
    fn with(views: &[(String, Json)]) -> StructureTree {
        let mut entries: Vec<(&str, Json)> = vec![app()];
        for (path, value) in views {
            entries.push((path.as_str(), value.clone()));
        }
        tree(&entries)
    }

    /// The class of the one view in the three trees.
    fn class_of(base: &StructureTree, tip: &StructureTree, live: &StructureTree) -> (Class, bool) {
        let status = Status::of(base, tip, live);
        let views: Vec<&StatusEntry> = status
            .entries
            .iter()
            .filter(|e| e.kind == Kind::View)
            .collect();
        assert_eq!(views.len(), 1, "{views:#?}");
        (views[0].class, views[0].adopt)
    }

    #[test]
    fn one_class_per_row_of_the_table() {
        let v = view("v1", "list", "List");
        let changed = view("v1", "list", "All tickets");
        let other = view("v1", "list", "Every ticket");
        let none: Vec<(String, Json)> = Vec::new();
        let (b, c, o, empty) = (
            with(std::slice::from_ref(&v)),
            with(std::slice::from_ref(&changed)),
            with(std::slice::from_ref(&other)),
            with(&none),
        );

        // B → R same, B → L same.
        assert_eq!(class_of(&b, &b, &b), (Class::InSync, false));
        // changed, same.
        assert_eq!(class_of(&b, &c, &b), (Class::Incoming, false));
        // deleted, same.
        assert_eq!(class_of(&b, &empty, &b), (Class::IncomingDelete, false));
        // not in B, absent from live.
        assert_eq!(class_of(&empty, &b, &empty), (Class::IncomingNew, false));
        // same, changed.
        assert_eq!(class_of(&b, &b, &c), (Class::LocalChange, false));
        // same, deleted.
        assert_eq!(class_of(&b, &b, &empty), (Class::LocalDelete, false));
        // not in B or R, present.
        assert_eq!(class_of(&empty, &empty, &b), (Class::LocalNew, false));
        // changed on both, differently; changed on one and deleted on the other.
        assert_eq!(class_of(&b, &c, &o), (Class::Conflict, false));
        assert_eq!(class_of(&b, &c, &empty), (Class::Conflict, false));
        assert_eq!(class_of(&b, &empty, &c), (Class::Conflict, false));
        // changed on both, the same way: in sync, and the base takes it.
        assert_eq!(class_of(&b, &c, &c), (Class::InSync, true));
        // not in B, present on both and equal: in sync, the base takes it.
        assert_eq!(class_of(&empty, &b, &b), (Class::InSync, true));
        // not in B, present on both and different.
        assert_eq!(class_of(&empty, &b, &c), (Class::Conflict, false));
    }

    #[test]
    fn deleted_on_both_sides_is_in_sync() {
        let b = with(&[view("v1", "list", "List")]);
        let empty = with(&[]);
        let status = Status::of(&b, &empty, &empty);
        let entry = status
            .entries
            .iter()
            .find(|e| e.kind == Kind::View)
            .unwrap();
        assert_eq!((entry.class, entry.adopt), (Class::InSync, true));
    }

    #[test]
    fn a_rename_by_id_is_one_entity() {
        let base = with(&[view("v1", "list", "List")]);
        let tip = with(&[view("v1", "all_tickets", "List")]);
        let status = Status::of(&base, &tip, &base);
        let views: Vec<&StatusEntry> = status.shown().collect();
        assert_eq!(views.len(), 1, "{views:#?}");
        assert_eq!(views[0].class, Class::Incoming);
        assert_eq!(views[0].tip.as_ref().unwrap().name, "all_tickets");
        assert_eq!(views[0].live.as_ref().unwrap().name, "list");
        // Named from live.
        assert_eq!(views[0].key, "view:desk/list");
    }

    #[test]
    fn a_name_match_with_a_different_id_is_the_same_entity() {
        // Built the same on both instances before they were linked: the
        // repository's id is not this instance's.
        let empty = with(&[]);
        let tip = with(&[view("repo-id", "list", "List")]);
        let live = with(&[view("live-id", "list", "List")]);
        let status = Status::of(&empty, &tip, &live);
        let views: Vec<&StatusEntry> = status
            .entries
            .iter()
            .filter(|e| e.kind == Kind::View)
            .collect();
        assert_eq!(views.len(), 1);
        assert_eq!((views[0].class, views[0].adopt), (Class::InSync, true));

        // And an id match is taken before a name match: a view renamed on the
        // tip to the name another live view has is still the renamed one.
        let base = with(&[view("v1", "a", "A"), view("v2", "b", "B")]);
        let tip = with(&[view("v1", "b", "A"), view("v2", "c", "B")]);
        let status = Status::of(&base, &tip, &base);
        for entry in status.entries.iter().filter(|e| e.kind == Kind::View) {
            assert_eq!(
                entry.tip.as_ref().unwrap().id,
                entry.live.as_ref().unwrap().id,
                "{entry:#?}"
            );
        }
    }

    #[test]
    fn a_child_is_matched_within_its_parent() {
        // Two applications with a view of the same name each.
        let mut entries = [
            (
                "applications/a/application.json".to_owned(),
                json!({ "id": "a", "subdomain": "a" }),
            ),
            (
                "applications/b/application.json".to_owned(),
                json!({ "id": "b", "subdomain": "b" }),
            ),
            (
                "applications/a/views/list.json".to_owned(),
                json!({ "id": "va", "name": "list" }),
            ),
            (
                "applications/b/views/list.json".to_owned(),
                json!({ "id": "vb", "name": "list" }),
            ),
        ];
        let live = tree(
            &entries
                .iter()
                .map(|(p, v)| (p.as_str(), v.clone()))
                .collect::<Vec<_>>(),
        );
        // On the tip, b's view has another id; it is still b's, not a's.
        entries[3].1 = json!({ "id": "other", "name": "list", "label": "x" });
        let tip = tree(
            &entries
                .iter()
                .map(|(p, v)| (p.as_str(), v.clone()))
                .collect::<Vec<_>>(),
        );
        let status = Status::of(&live, &tip, &live);
        let shown: Vec<&StatusEntry> = status.shown().collect();
        assert_eq!(shown.len(), 1, "{shown:#?}");
        assert_eq!(shown[0].key, "view:b/list");
        assert_eq!(shown[0].class, Class::Incoming);
    }

    fn store(commit: &str, branch: &str) -> StructureTree {
        tree(&[(
            "file-stores/web.json",
            json!({
                "id": "s1", "name": "web", "backend": "git",
                "config": { "url": "git@github.com:o/web.git", "branch": branch },
                "commit": commit,
            }),
        )])
    }

    fn store_class(base: &StructureTree, tip: &StructureTree, live: &StructureTree) -> Class {
        Status::of(base, tip, live).entries[0].class
    }

    #[test]
    fn a_moved_pin_is_local_but_does_not_block_deploy() {
        let base = store("aaa", "main");
        let live = store("bbb", "main");
        let class = store_class(&base, &base, &live);
        assert_eq!(class, Class::PinMoved);
        assert!(class.is_local());
        assert!(!class.blocks_deploy());
        // A pin moved on the tip is incoming.
        assert_eq!(store_class(&base, &live, &base), Class::Incoming);
        // Any other local change to the store is an ordinary one.
        assert_eq!(
            store_class(&base, &base, &store("aaa", "dev")),
            Class::LocalChange
        );
    }

    #[test]
    fn a_conflict_on_the_pin_alone_resolves_to_the_tip() {
        let base = store("aaa", "main");
        let tip = store("ccc", "main");
        let live = store("bbb", "main");
        assert_eq!(store_class(&base, &tip, &live), Class::Incoming);
        // A pin moved here and the branch changed on the tip: incoming, the
        // planner keeps the live pin.
        assert_eq!(
            store_class(&base, &store("aaa", "dev"), &live),
            Class::Incoming
        );
        // But a conflict on the rest is a conflict.
        assert_eq!(
            store_class(&base, &store("aaa", "dev"), &store("aaa", "rel")),
            Class::Conflict
        );
    }
}

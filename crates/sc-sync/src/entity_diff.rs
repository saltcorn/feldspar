//! One entity's diff, field by field: what `getSyncEntityDiff` returns
//! (APP_SYNC_PROPOSAL §4, §7.2).
//!
//! Status says *that* an entity differs; expanding its row says *how*: each
//! changed field of its JSON, at a path a person can read (`fields[status].label`
//! rather than `/fields/2/label`), from the base to the tip (what is incoming)
//! and from the base to live (what is local). A store file that is text is a
//! line diff instead (§3.5).
//!
//! The unit is still the entity. These are for reading; merging is the
//! registry editor's (§7.6) or git's.

use serde::Serialize;
use serde_json::{Map, Value as Json};

use crate::diff::{Class, StatusEntry};
use crate::entity::{Content, Entity, Kind};

/// One changed value.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JsonChange {
    /// Where: object keys joined by `.`, an array element as `[i]`, or as
    /// `[name]` when the elements are objects named by an `id` or `name`.
    pub path: String,
    /// The value before, absent when it was added.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Json>,
    /// The value after, absent when it was removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Json>,
}

/// A text file's change, as a unified diff.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextDiff {
    /// The unified diff, with three lines of context.
    pub unified: String,
}

/// One side's change to an entity, from the base.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SideDiff {
    /// Each changed field of a JSON entity.
    pub changes: Vec<JsonChange>,
    /// A text file's line diff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextDiff>,
    /// Whether a file changed but is not text, so there is no line diff.
    pub binary: bool,
}

/// What `getSyncEntityDiff` returns for one entity.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityDiff {
    /// The entity's key.
    pub key: String,
    /// What it is.
    pub kind: Kind,
    /// Its name.
    pub name: String,
    /// Its Status class.
    pub class: Class,
    /// The base's version (its JSON, or a file's hash and size).
    pub base: Option<Content>,
    /// The tip's version.
    pub tip: Option<Content>,
    /// Live's version.
    pub live: Option<Content>,
    /// Base → tip.
    pub incoming: SideDiff,
    /// Base → live.
    pub local: SideDiff,
}

/// The diff of one Status entry.
///
/// `bytes` finds a store file's bytes by its blob hash when the side's tree
/// holds only the hash, as a base does (the sync mirror keeps them, §4); a file
/// whose bytes cannot be found has no line diff.
pub fn entity_diff(entry: &StatusEntry, bytes: &dyn Fn(&str) -> Option<Vec<u8>>) -> EntityDiff {
    let content = |e: &Option<Entity>| e.as_ref().map(|e| e.content.clone());
    EntityDiff {
        key: entry.key.clone(),
        kind: entry.kind,
        name: entry.name.clone(),
        class: entry.class,
        base: content(&entry.base),
        tip: content(&entry.tip),
        live: content(&entry.live),
        incoming: side_diff(entry.base.as_ref(), entry.tip.as_ref(), bytes),
        local: side_diff(entry.base.as_ref(), entry.live.as_ref(), bytes),
    }
}

/// One side's change from `before` to `after`.
pub fn side_diff(
    before: Option<&Entity>,
    after: Option<&Entity>,
    bytes: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> SideDiff {
    let is_file = |e: Option<&Entity>| e.is_some_and(|e| matches!(e.content, Content::File { .. }));
    if is_file(before) || is_file(after) {
        return file_diff(before, after, bytes);
    }
    let value = |e: Option<&Entity>| e.and_then(Entity::json).cloned();
    SideDiff {
        changes: json_changes(value(before).as_ref(), value(after).as_ref()),
        ..SideDiff::default()
    }
}

fn file_diff(
    before: Option<&Entity>,
    after: Option<&Entity>,
    bytes: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> SideDiff {
    let file = |e: Option<&Entity>| -> Option<(String, Option<Vec<u8>>)> {
        match &e?.content {
            Content::File {
                hash, bytes: own, ..
            } => Some((hash.clone(), own.clone().or_else(|| bytes(hash)))),
            Content::Json { .. } => None,
        }
    };
    let (a, b) = (file(before), file(after));
    if a.as_ref().map(|f| &f.0) == b.as_ref().map(|f| &f.0) {
        return SideDiff::default();
    }
    let text = |f: &Option<(String, Option<Vec<u8>>)>| -> Option<Option<String>> {
        match f {
            None => Some(None),
            Some((_, Some(bytes))) => String::from_utf8(bytes.clone()).ok().map(Some),
            Some((_, None)) => None,
        }
    };
    match (text(&a), text(&b)) {
        (Some(old), Some(new)) => {
            let (old, new) = (old.unwrap_or_default(), new.unwrap_or_default());
            let unified = similar::TextDiff::from_lines(&old, &new)
                .unified_diff()
                .context_radius(3)
                .header("before", "after")
                .to_string();
            SideDiff {
                text: Some(TextDiff { unified }),
                ..SideDiff::default()
            }
        }
        _ => SideDiff {
            binary: true,
            ..SideDiff::default()
        },
    }
}

/// Every changed value between two JSON values, in path order.
pub fn json_changes(before: Option<&Json>, after: Option<&Json>) -> Vec<JsonChange> {
    let mut out = Vec::new();
    walk("", before, after, &mut out);
    out
}

fn walk(path: &str, before: Option<&Json>, after: Option<&Json>, out: &mut Vec<JsonChange>) {
    if before == after {
        return;
    }
    match (before, after) {
        (Some(Json::Object(a)), Some(Json::Object(b))) => {
            let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                walk(&join_key(path, key), a.get(key), b.get(key), out);
            }
        }
        (Some(Json::Array(a)), Some(Json::Array(b))) => match (element_keys(a), element_keys(b)) {
            (Some(ka), Some(kb)) => {
                // Matched by id, then name, as entities are.
                let mut used = vec![false; b.len()];
                for (i, item) in a.iter().enumerate() {
                    let found = kb
                        .iter()
                        .enumerate()
                        .find(|(j, k)| !used[*j] && k.0.is_some() && k.0 == ka[i].0)
                        .or_else(|| {
                            kb.iter()
                                .enumerate()
                                .find(|(j, k)| !used[*j] && k.1 == ka[i].1)
                        })
                        .map(|(j, _)| j);
                    let label = format!("{path}[{}]", ka[i].1);
                    match found {
                        Some(j) => {
                            used[j] = true;
                            walk(&label, Some(item), Some(&b[j]), out);
                        }
                        None => walk(&label, Some(item), None, out),
                    }
                }
                for (j, item) in b.iter().enumerate() {
                    if !used[j] {
                        walk(&format!("{path}[{}]", kb[j].1), None, Some(item), out);
                    }
                }
            }
            _ if a.iter().all(is_scalar) && b.iter().all(is_scalar) => {
                out.push(change(path, before, after))
            }
            _ => {
                for i in 0..a.len().max(b.len()) {
                    walk(&format!("{path}[{i}]"), a.get(i), b.get(i), out);
                }
            }
        },
        _ => out.push(change(path, before, after)),
    }
}

fn change(path: &str, before: Option<&Json>, after: Option<&Json>) -> JsonChange {
    JsonChange {
        path: path.to_owned(),
        before: before.cloned(),
        after: after.cloned(),
    }
}

fn join_key(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn is_scalar(value: &Json) -> bool {
    !matches!(value, Json::Object(_) | Json::Array(_))
}

/// Each element's `(id, name)`, when every element is an object with a name
/// (or an id) and no two share a name — the elements can then be told apart by
/// it.
fn element_keys(items: &[Json]) -> Option<Vec<(Option<String>, String)>> {
    let keys: Vec<(Option<String>, String)> = items
        .iter()
        .map(|item| {
            let obj: &Map<String, Json> = item.as_object()?;
            let text = |k: &str| obj.get(k).and_then(Json::as_str).map(str::to_owned);
            let id = text("id");
            let name = text("name").or_else(|| id.clone())?;
            Some((id, name))
        })
        .collect::<Option<_>>()?;
    let mut names: Vec<&String> = keys.iter().map(|k| &k.1).collect();
    names.sort();
    let unique = names.windows(2).all(|w| w[0] != w[1]);
    (unique && !keys.is_empty()).then_some(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Status;
    use crate::tree::StructureTree;
    use serde_json::json;

    fn paths(changes: &[JsonChange]) -> Vec<&str> {
        changes.iter().map(|c| c.path.as_str()).collect()
    }

    #[test]
    fn a_table_diff_names_fields_by_name() {
        let before = json!({
            "name": "tickets",
            "label": "Tickets",
            "fields": [
                { "id": "f1", "name": "title", "label": "Title" },
                { "id": "f2", "name": "status", "label": "Status" },
            ],
        });
        let after = json!({
            "name": "tickets",
            "label": "All tickets",
            "fields": [
                { "id": "f1", "name": "title", "label": "Title" },
                // Renamed by id: one element, its name changed.
                { "id": "f2", "name": "state", "label": "Status" },
                { "id": "f3", "name": "priority", "label": "Priority" },
            ],
        });
        let changes = json_changes(Some(&before), Some(&after));
        assert_eq!(
            paths(&changes),
            ["fields[status].name", "fields[priority]", "label"]
        );
        assert_eq!(changes[0].before, Some(json!("status")));
        assert_eq!(changes[0].after, Some(json!("state")));
        assert_eq!(changes[1].before, None);
        assert_eq!(changes[2].after, Some(json!("All tickets")));

        // A list of plain values is one change.
        let changes = json_changes(
            Some(&json!({ "tables": ["a", "b"] })),
            Some(&json!({ "tables": ["a", "c"] })),
        );
        assert_eq!(paths(&changes), ["tables"]);
        // Nothing changed, nothing listed.
        assert!(json_changes(Some(&before), Some(&before)).is_empty());
    }

    #[test]
    fn the_payload_carries_both_sides_from_the_base() {
        let mut base = StructureTree::new();
        base.insert_json(
            "triggers/notify.json",
            json!({ "id": "t1", "name": "notify", "when": "insert", "description": "" }),
        );
        let mut tip = base.clone();
        tip.insert_json(
            "triggers/notify.json",
            json!({ "id": "t1", "name": "notify", "when": "update", "description": "" }),
        );
        let mut live = base.clone();
        live.insert_json(
            "triggers/notify.json",
            json!({ "id": "t1", "name": "notify", "when": "insert", "description": "mine" }),
        );
        let status = Status::of(&base, &tip, &live);
        let entry = status.entry("trigger:notify").unwrap();
        assert_eq!(entry.class, Class::Conflict);
        let diff = entity_diff(entry, &|_| None);
        assert_eq!(paths(&diff.incoming.changes), ["when"]);
        assert_eq!(paths(&diff.local.changes), ["description"]);
        assert!(matches!(diff.base, Some(Content::Json { .. })));
        // It is what the endpoint sends.
        let wire = serde_json::to_value(&diff).unwrap();
        assert_eq!(wire["class"], "conflict");
        assert_eq!(wire["local"]["changes"][0]["after"], "mine");
    }

    #[test]
    fn a_text_file_is_a_line_diff_and_a_binary_one_is_not() {
        let path = "file-stores/assets/files/app.js";
        let old = b"let a = 1;\nlet b = 2;\n".to_vec();
        let mut base = StructureTree::new();
        base.insert_file(path, old.clone());
        // The base keeps only the hash; the bytes come from the mirror.
        let base = base.with_blob_hashes();
        let mut live = StructureTree::new();
        live.insert_file(path, b"let a = 1;\nlet b = 3;\n".to_vec());
        let status = Status::of(&base, &base, &live);
        let entry = status.entry("store_file:assets/app.js").unwrap();
        assert_eq!(entry.class, Class::LocalChange);

        let hash = crate::entity::blob_hash(&old);
        let mirror = |h: &str| (h == hash).then(|| old.clone());
        let diff = entity_diff(entry, &mirror);
        let text = diff.local.text.unwrap().unified;
        assert!(text.contains("-let b = 2;\n+let b = 3;"), "{text}");
        assert!(diff.incoming.text.is_none() && !diff.incoming.binary);

        // Without the base's bytes there is no line diff, and no claim of one.
        let diff = entity_diff(entry, &|_| None);
        assert!(diff.local.text.is_none());
        assert!(diff.local.binary);

        let mut png = StructureTree::new();
        png.insert_file(path, vec![0x89, b'P', 0xff, 0]);
        let status = Status::of(&base, &base, &png);
        let diff = entity_diff(status.entry("store_file:assets/app.js").unwrap(), &mirror);
        assert!(diff.local.binary && diff.local.text.is_none());
    }
}

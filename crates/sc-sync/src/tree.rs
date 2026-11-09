//! The structure tree: an installation's structure as the files a repository
//! holds (APP_SYNC_PROPOSAL §3.1).
//!
//! A [`StructureTree`] is an ordered map from a repository path
//! (`tables/tickets.json`) to what is at it. Almost everything is a JSON value,
//! one entity per file; a local store's synced files
//! (`file-stores/<name>/files/…`) are bytes, kept as they are.
//!
//! **Rendering is canonical**, because git diffs and merges are line-based and
//! two exports of the same structure must be the same bytes: object keys
//! sorted at every depth, two-space indentation, and a trailing newline. A file
//! that is already canonical parses and renders back to exactly itself, which
//! is what lets a tree read from a commit be compared with one exported from
//! live by their bytes, and what the [digest](StructureTree::digest) is taken
//! over.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};
use sha2::{Digest, Sha256};

use sc_error::{Error, Result};

/// The directory a store's synced files live under, inside
/// `file-stores/<name>/`. Everything below it is a file's bytes, not JSON.
pub const STORE_FILES_DIR: &str = "files";

/// What is at one path of a [`StructureTree`].
#[derive(Debug, Clone, PartialEq)]
pub enum TreeEntry {
    /// An entity, or a list of them: a `.json` file.
    Json(Json),
    /// A file's bytes: a local store's synced file, or anything else in the
    /// repository that is not one of the tree's JSON files.
    File(Vec<u8>),
}

impl TreeEntry {
    /// The bytes the entry is written as.
    pub fn render(&self) -> Vec<u8> {
        match self {
            TreeEntry::Json(value) => render_json(value),
            TreeEntry::File(bytes) => bytes.clone(),
        }
    }

    /// The JSON value, for an entity.
    pub fn as_json(&self) -> Option<&Json> {
        match self {
            TreeEntry::Json(value) => Some(value),
            TreeEntry::File(_) => None,
        }
    }
}

/// An installation's structure as repository files, ordered by path.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StructureTree {
    entries: BTreeMap<String, TreeEntry>,
}

impl StructureTree {
    /// An empty tree.
    pub fn new() -> StructureTree {
        StructureTree::default()
    }

    /// Put `value` at `path`, replacing whatever was there.
    ///
    /// The value is stored canonically (its object keys sorted), so a tree built
    /// in memory compares equal to the same tree read back from its files.
    pub fn insert_json(&mut self, path: impl Into<String>, value: Json) {
        self.entries
            .insert(path.into(), TreeEntry::Json(canonical(value)));
    }

    /// Put a file's bytes at `path`, replacing whatever was there.
    pub fn insert_file(&mut self, path: impl Into<String>, bytes: Vec<u8>) {
        self.entries.insert(path.into(), TreeEntry::File(bytes));
    }

    /// Remove what is at `path`, returning it.
    pub fn remove(&mut self, path: &str) -> Option<TreeEntry> {
        self.entries.remove(path)
    }

    /// What is at `path`.
    pub fn get(&self, path: &str) -> Option<&TreeEntry> {
        self.entries.get(path)
    }

    /// The JSON value at `path`, if it is a JSON entry.
    pub fn json(&self, path: &str) -> Option<&Json> {
        self.get(path).and_then(TreeEntry::as_json)
    }

    /// Every path, in order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Every entry, in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &TreeEntry)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// How many paths the tree has.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the tree has no paths at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The tree as the files a repository holds: each path with its bytes,
    /// rendered canonically.
    pub fn to_files(&self) -> BTreeMap<String, Vec<u8>> {
        self.entries
            .iter()
            .map(|(path, entry)| (path.clone(), entry.render()))
            .collect()
    }

    /// Read a tree back from repository files.
    ///
    /// A `.json` file is parsed; a file under a store's `files/` directory, and
    /// any other file, is kept as its bytes. A `.json` file that does not parse
    /// is refused with its path, since a tree that silently lost an entity
    /// would deploy as that entity's deletion. A tree whose
    /// [`feldspar-structure.json`](crate::format::FORMAT_FILE) says it was
    /// written in a newer format than this build reads is refused too
    /// ([`crate::format::check_format`]).
    pub fn from_files(files: BTreeMap<String, Vec<u8>>) -> Result<StructureTree> {
        let mut tree = StructureTree::new();
        for (path, bytes) in files {
            check_path(&path)?;
            if is_store_file(&path) || !path.ends_with(".json") {
                tree.entries.insert(path, TreeEntry::File(bytes));
                continue;
            }
            let value: Json = serde_json::from_slice(&bytes)
                .map_err(|e| Error::invalid(format!("`{path}` is not valid JSON: {e}")))?;
            tree.entries.insert(path, TreeEntry::Json(canonical(value)));
        }
        crate::format::check_format(&tree)?;
        Ok(tree)
    }

    /// A digest of the whole tree: SHA-256 over every path and its rendered
    /// bytes, in order, as lowercase hex.
    ///
    /// Two trees have the same digest exactly when [`to_files`](Self::to_files)
    /// gives the same files. Each path and each file is length-prefixed, so no
    /// two different trees run together into the same input.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        for (path, entry) in &self.entries {
            let bytes = entry.render();
            hasher.update((path.len() as u64).to_be_bytes());
            hasher.update(path.as_bytes());
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(&bytes);
        }
        format!("{:x}", hasher.finalize())
    }
}

/// Whether `path` is one of a store's synced files
/// (`file-stores/<store>/files/…`).
pub fn is_store_file(path: &str) -> bool {
    let mut parts = path.splitn(4, '/');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("file-stores"), Some(store), Some(STORE_FILES_DIR), Some(rest))
            if !store.is_empty() && !rest.is_empty()
    )
}

/// A repository path the tree will hold: relative, `/`-separated, with no empty,
/// `.` or `..` segment. Anything else could name a file outside the tree when
/// it is written out.
fn check_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..");
    if bad {
        return Err(Error::invalid(format!(
            "`{path}` is not a path inside the structure tree"
        )));
    }
    Ok(())
}

/// `value` rendered canonically: keys sorted at every depth, two-space
/// indentation, and a trailing newline.
pub fn render_json(value: &Json) -> Vec<u8> {
    // `canonical` sorts whatever order the map was built in, so the rendering
    // does not depend on whether `serde_json` preserves insertion order.
    let sorted = canonical(value.clone());
    // Infallible: a `Value` always serialises, and into a `Vec` nothing fails.
    let mut out = serde_json::to_vec_pretty(&sorted).unwrap_or_default();
    out.push(b'\n');
    out
}

/// `value` with its object keys sorted, at every depth.
pub fn canonical(value: Json) -> Json {
    match value {
        Json::Object(map) => {
            let mut entries: Vec<(String, Json)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key, canonical(value));
            }
            Json::Object(sorted)
        }
        Json::Array(items) => Json::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

/// An entity's name as one segment of a repository path.
///
/// Names are the admin's, so they may hold anything; a path segment may not
/// hold a `/`, and a few other characters make a file awkward or dangerous to
/// check out (a leading `.`, a `\`, a control character). Those, and `%`
/// itself, are written as `%XX`; everything else, non-ASCII letters included,
/// is kept, so `tables/tickets.json` reads as what it is. The name itself is in
/// the entity's JSON, so the segment never has to be decoded.
pub fn path_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, ch) in name.chars().enumerate() {
        let escape = matches!(
            ch,
            '/' | '\\' | '%' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        ) || ch.is_control()
            || (i == 0 && ch == '.');
        if escape {
            let mut buf = [0u8; 4];
            for byte in ch.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        // An entity with no name still needs a file to be in.
        out.push_str("%00");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rendering_sorts_keys_indents_by_two_and_ends_with_a_newline() {
        let value = json!({ "z": 1, "a": { "y": [ { "b": 2, "a": 1 } ], "x": null } });
        let text = String::from_utf8(render_json(&value)).unwrap();
        assert_eq!(
            text,
            "{\n  \"a\": {\n    \"x\": null,\n    \"y\": [\n      {\n        \"a\": 1,\n        \
             \"b\": 2\n      }\n    ]\n  },\n  \"z\": 1\n}\n"
        );
    }

    #[test]
    fn rendering_is_stable_whatever_order_the_value_was_built_in() {
        let mut one = Map::new();
        one.insert("name".into(), json!("tickets"));
        one.insert(
            "fields".into(),
            json!([{ "name": "title", "type": "String" }]),
        );
        let mut two = Map::new();
        two.insert(
            "fields".into(),
            json!([{ "type": "String", "name": "title" }]),
        );
        two.insert("name".into(), json!("tickets"));
        assert_eq!(
            render_json(&Json::Object(one.clone())),
            render_json(&Json::Object(two.clone()))
        );

        let mut a = StructureTree::new();
        a.insert_json("tables/tickets.json", Json::Object(one));
        let mut b = StructureTree::new();
        b.insert_json("tables/tickets.json", Json::Object(two));
        assert_eq!(a, b);
        assert_eq!(a.to_files(), b.to_files());
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn files_round_trip_byte_for_byte() {
        let mut tree = StructureTree::new();
        tree.insert_json(
            "feldspar-structure.json",
            json!({ "format": crate::format::FORMAT_VERSION, "writer": "0.1.0" }),
        );
        tree.insert_json(
            "tables/tickets.json",
            json!({
                "id": "6c1d3c1e-0d55-4c1e-9a7a-6a3f0a4b8f01",
                "name": "tickets",
                "fields": [{ "name": "title", "label": "Titel — überall", "precision": 0.1 }],
                "constraints": [],
            }),
        );
        tree.insert_json("roles.json", json!([{ "role": 1, "name": "admin" }]));
        // Bytes that are not JSON and not UTF-8, under a store's files.
        tree.insert_file(
            "file-stores/assets/files/logo.png",
            vec![0x89, b'P', 0, 0xff],
        );
        // A store file named `.json` is still a file, not an entity: its bytes
        // are its own, however they are formatted.
        tree.insert_file(
            "file-stores/assets/files/data.json",
            b"{\"b\":1,   \"a\":2}".to_vec(),
        );

        let files = tree.to_files();
        let back = StructureTree::from_files(files.clone()).unwrap();
        assert_eq!(back, tree);
        assert_eq!(back.to_files(), files);
        assert_eq!(back.digest(), tree.digest());
        assert_eq!(
            files["file-stores/assets/files/data.json"],
            b"{\"b\":1,   \"a\":2}"
        );
    }

    #[test]
    fn a_canonical_file_parses_and_renders_back_to_itself() {
        let text = "{\n  \"a\": [\n    1,\n    2.5,\n    \"x\"\n  ],\n  \"b\": {}\n}\n";
        let mut files = BTreeMap::new();
        files.insert("roles.json".to_owned(), text.as_bytes().to_vec());
        let tree = StructureTree::from_files(files).unwrap();
        assert_eq!(tree.to_files()["roles.json"], text.as_bytes());
    }

    #[test]
    fn the_digest_changes_with_any_byte_and_any_path() {
        let mut tree = StructureTree::new();
        tree.insert_json("roles.json", json!([{ "role": 1 }]));
        let before = tree.digest();
        assert_eq!(before.len(), 64);

        let mut changed = tree.clone();
        changed.insert_json("roles.json", json!([{ "role": 2 }]));
        assert_ne!(changed.digest(), before);

        let mut moved = StructureTree::new();
        moved.insert_json("roles2.json", json!([{ "role": 1 }]));
        assert_ne!(moved.digest(), before);

        assert_eq!(
            tree.digest(),
            before,
            "the digest is a function of the tree"
        );
    }

    #[test]
    fn a_broken_json_file_or_an_escaping_path_is_refused() {
        let mut files = BTreeMap::new();
        files.insert("tables/t.json".to_owned(), b"{ not json".to_vec());
        let err = StructureTree::from_files(files).unwrap_err().to_string();
        assert!(err.contains("tables/t.json"), "{err}");

        for path in ["../x.json", "/etc/passwd", "a//b.json", "a/./b", "a\\b"] {
            let mut files = BTreeMap::new();
            files.insert(path.to_owned(), b"{}".to_vec());
            let err = StructureTree::from_files(files).unwrap_err().to_string();
            assert!(err.contains("not a path"), "{path}: {err}");
        }
    }

    #[test]
    fn store_files_are_recognised_by_their_place() {
        assert!(is_store_file("file-stores/assets/files/logo.png"));
        assert!(is_store_file("file-stores/assets/files/img/a.json"));
        assert!(!is_store_file("file-stores/assets.json"));
        assert!(!is_store_file("file-stores/assets/files/"));
        assert!(!is_store_file("tables/files/x.json"));
    }

    #[test]
    fn a_name_becomes_a_safe_path_segment() {
        assert_eq!(path_segment("tickets"), "tickets");
        assert_eq!(path_segment("List tickets"), "List tickets");
        assert_eq!(path_segment("Überblick"), "Überblick");
        assert_eq!(path_segment("a/b"), "a%2Fb");
        assert_eq!(path_segment("50%"), "50%25");
        assert_eq!(path_segment(".hidden"), "%2Ehidden");
        assert_eq!(path_segment("a.b"), "a.b");
        assert_eq!(path_segment("tab\there"), "tab%09here");
        assert_eq!(path_segment(""), "%00");
    }
}

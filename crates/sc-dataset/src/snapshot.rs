//! What a model fit records about its dataset (analytics TODO A1.8): the
//! resolved definition, and a hash of what it means.
//!
//! A dataset is shared and edited, so a fit that only named it would silently
//! change meaning the next time somebody added a filter. The fit keeps the
//! definitions it read — the dataset and every dataset it reaches — and so
//! predicts the way it was fitted, whatever happens to the dataset since. The
//! hash is how the fit notices: the model's current dataset hashes differently.
//!
//! The hash is of the **meaning**: each dataset's base and its enabled
//! operations' kinds and parameters. A rename, a new description, a disabled
//! operation or a reordered id is not a different dataset, and a fit is not
//! stale for it.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use crate::compile::Library;
use crate::def::{DatasetDef, DatasetId};

/// A dataset and every dataset it reaches, as a fit read them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The dataset itself.
    pub root: DatasetId,
    /// It, then the datasets it reaches, by id.
    pub datasets: Vec<DatasetDef>,
}

impl Snapshot {
    /// The snapshot of `root` in `library`, or `None` when it is not there.
    pub fn of(library: &Library, root: DatasetId) -> Option<Snapshot> {
        library.get(root)?;
        Some(Snapshot {
            root,
            datasets: library.closure(root),
        })
    }

    /// The definitions as a library, to compile from.
    pub fn library(&self) -> Library {
        Library::new(self.datasets.iter().cloned())
    }

    /// The dataset itself.
    pub fn def(&self) -> &DatasetDef {
        self.datasets
            .iter()
            .find(|d| d.id == self.root)
            .unwrap_or(&self.datasets[0])
    }

    /// The hash of what the definitions mean (see the module docs), as
    /// lowercase hex.
    pub fn hash(&self) -> String {
        let meaning: Vec<Json> = self
            .datasets
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "base": d.base,
                    "operations": d
                        .operations
                        .iter()
                        .filter(|o| o.enabled)
                        .map(|o| serde_json::to_value(&o.op).unwrap_or(Json::Null))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut text = String::new();
        canonical(&Json::Array(meaning), &mut text);
        Snapshot::hash_text(&text)
    }

    /// The SHA-256 of `text`, as lowercase hex — for a caller combining
    /// several snapshots' hashes into one.
    pub fn hash_text(text: &str) -> String {
        let digest = Sha256::digest(text.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// JSON with every object's keys sorted, so the text — and the hash — does not
/// depend on how a map happened to be ordered.
fn canonical(value: &Json, out: &mut String) {
    match value {
        Json::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Json::String((*k).clone()).to_string());
                out.push(':');
                canonical(&map[*k], out);
            }
            out.push('}');
        }
        Json::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(v, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::Op;

    #[test]
    fn the_hash_follows_the_meaning_and_not_the_name() {
        let def = DatasetDef::over_table("prices", "houses").then(Op::filter("price > 1"));
        let library = Library::new([def.clone()]);
        let snap = Snapshot::of(&library, def.id).expect("snapshot");
        let hash = snap.hash();
        assert_eq!(hash.len(), 64);

        // Renamed: the same dataset.
        let renamed = DatasetDef {
            name: "other name".into(),
            ..def.clone()
        };
        let again = Snapshot::of(&Library::new([renamed]), def.id).expect("snapshot");
        assert_eq!(again.hash(), hash);

        // A new operation: a different one.
        let edited = def.clone().then(Op::filter("price < 10"));
        let edited = Snapshot::of(&Library::new([edited]), def.id).expect("snapshot");
        assert_ne!(edited.hash(), hash);

        // …unless it is disabled.
        let mut off = def.clone().then(Op::filter("price < 10"));
        off.operations[1].enabled = false;
        let off = Snapshot::of(&Library::new([off]), def.id).expect("snapshot");
        assert_eq!(off.hash(), hash);
    }

    #[test]
    fn a_snapshot_carries_the_datasets_it_reaches() {
        let base = DatasetDef::over_table("base", "houses");
        let top = DatasetDef::new("top", crate::def::Base::dataset(base.id));
        let library = Library::new([base.clone(), top.clone()]);
        let snap = Snapshot::of(&library, top.id).expect("snapshot");
        assert_eq!(snap.datasets.len(), 2);
        assert_eq!(snap.def().id, top.id);
        assert!(Snapshot::of(&library, DatasetId::new()).is_none());
    }
}

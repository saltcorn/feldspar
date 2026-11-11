//! The base: each entity as it was last synced here, by a commit, a deploy or
//! a discard (APP_SYNC_PROPOSAL §4).
//!
//! The base is a tree rather than a commit, so it survives force-pushes and
//! branch switches and can hold deferred operations (§5). It is kept in the
//! database, and so it holds a store file only as its git blob hash
//! ([`StructureTree::with_blob_hashes`]); the bytes stay in the sync mirror,
//! where [`BASE_REF`] keeps them reachable whatever happens to the remote's
//! history, so a Discard can still put a file back.

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use sc_files::GitRepo;

use crate::diff::Status;
use crate::entity::{Content, put_entity, remove_entity};
use crate::tree::{StructureTree, TreeEntry, is_store_file};

/// The mirror's local ref holding the base's store files. Never pushed.
pub const BASE_REF: &str = "refs/feldspar/base";

/// Keep `tree`'s store files reachable in `mirror` under [`BASE_REF`], and
/// return the base to store for it: `tree` with each file's bytes replaced by
/// its blob hash.
///
/// A file `tree` holds only as a hash (it came from the base before) stays
/// reachable too, when the mirror still has it: a base that advanced for some
/// entities keeps the old version of the rest.
pub async fn keep_base_files(mirror: &GitRepo, tree: &StructureTree) -> Result<StructureTree> {
    let mut files: Vec<(String, Vec<u8>)> = tree
        .iter()
        .filter_map(|(path, entry)| match entry {
            TreeEntry::File(bytes) if is_store_file(path) => Some((path.to_owned(), bytes.clone())),
            _ => None,
        })
        .collect();
    let hashed: Vec<(&str, &str)> = tree
        .iter()
        .filter_map(|(path, entry)| match entry {
            TreeEntry::Blob { hash, .. } => Some((path, hash.as_str())),
            _ => None,
        })
        .collect();
    let hashes: Vec<&str> = hashed.iter().map(|(_, h)| *h).collect();
    for ((path, _), bytes) in hashed.iter().zip(mirror.read_blobs(&hashes).await?) {
        if let Some(bytes) = bytes {
            files.push(((*path).to_owned(), bytes));
        }
    }
    mirror.record_blobs(BASE_REF, &files).await?;
    Ok(tree.with_blob_hashes())
}

/// The bytes of every base store file that `keys` name, read from `mirror`:
/// what [`discard_target`] needs to put a file back.
pub async fn base_file_bytes(
    mirror: &GitRepo,
    status: &Status,
    keys: &[&str],
) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for key in keys {
        let Some(entry) = status.entry(key) else {
            continue;
        };
        if let Some(Content::File {
            hash, bytes: None, ..
        }) = entry.base.as_ref().map(|b| &b.content)
            && let Some(bytes) = mirror.read_blob(hash).await?
        {
            out.insert(hash.clone(), bytes);
        }
    }
    Ok(out)
}

/// The tree a Discard of `keys` plans (§2): live, with each chosen entity back
/// at its base version — put back when the base has it, taken out when it does
/// not. Planned like any other target, so discarding something that holds data
/// is classified as §5 says.
///
/// `bytes` gives a store file's bytes by blob hash when the base holds only
/// the hash ([`base_file_bytes`]).
pub fn discard_target(
    live: &StructureTree,
    status: &Status,
    keys: &[&str],
    bytes: &BTreeMap<String, Vec<u8>>,
) -> Result<StructureTree> {
    let mut target = live.clone();
    for key in keys {
        let entry = status
            .entry(key)
            .ok_or_else(|| Error::not_found(format!("Status has nothing called `{key}`")))?;
        if let Some(live) = &entry.live {
            remove_entity(&mut target, live);
        }
        if let Some(base) = &entry.base {
            let mut base = base.clone();
            if let Content::File {
                hash, bytes: None, ..
            } = &base.content
            {
                let found = bytes.get(hash).ok_or_else(|| {
                    Error::not_found(format!(
                        "the base version of `{key}` (blob {hash}) is no longer in the sync \
                         mirror, so it cannot be put back"
                    ))
                })?;
                base.content = Content::file(found);
            }
            put_entity(&mut target, &base);
        }
    }
    Ok(target)
}

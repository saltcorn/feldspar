//! `feldspar-structure.json`: which format a structure tree is written in, and
//! which build wrote it (APP_SYNC_PROPOSAL §3.1, "Versions").
//!
//! The format version says how to read the tree; the writer says which
//! Feldspar produced it, which is the question asked of a commit that turns up
//! with something unexpected in it. A tree in a format newer than this build
//! knows is refused, with a sentence naming both versions and telling the
//! operator to upgrade: deploying it would mean reading fields this build does
//! not know are there, and dropping them.

use serde_json::{Value as Json, json};

use sc_error::{Error, Result};

use crate::tree::StructureTree;

/// The file at the root of every tree.
pub const FORMAT_FILE: &str = "feldspar-structure.json";

/// The format this build writes, and the newest it reads.
pub const FORMAT_VERSION: u64 = 1;

/// The Feldspar version this build is.
pub const WRITER_VERSION: &str = sc_backup::PRODUCT_VERSION;

/// What a tree's [`FORMAT_FILE`] says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureFormat {
    /// The format version.
    pub format: u64,
    /// The Feldspar version that wrote it.
    pub writer: String,
}

impl StructureFormat {
    /// The format this build writes.
    pub fn current() -> StructureFormat {
        StructureFormat {
            format: FORMAT_VERSION,
            writer: WRITER_VERSION.to_owned(),
        }
    }

    /// The file's JSON.
    pub fn to_json(&self) -> Json {
        json!({ "format": self.format, "writer": self.writer })
    }

    /// Read the file's JSON.
    pub fn from_json(value: &Json) -> Result<StructureFormat> {
        let format = value
            .get("format")
            .and_then(Json::as_u64)
            .filter(|v| *v >= 1)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "`{FORMAT_FILE}` has no format version; it should hold \
                     {{ \"format\": <number>, \"writer\": <version> }}"
                ))
            })?;
        let writer = value
            .get("writer")
            .and_then(Json::as_str)
            .unwrap_or("an unknown version")
            .to_owned();
        Ok(StructureFormat { format, writer })
    }
}

/// What `tree` says about its format: `None` for a tree with no
/// [`FORMAT_FILE`] (an empty repository, before the first commit).
pub fn read_format(tree: &StructureTree) -> Result<Option<StructureFormat>> {
    match tree.get(FORMAT_FILE) {
        None => Ok(None),
        Some(entry) => {
            let value = entry
                .as_json()
                .ok_or_else(|| Error::invalid(format!("`{FORMAT_FILE}` is not a JSON file")))?;
            StructureFormat::from_json(value).map(Some)
        }
    }
}

/// Refuse a tree written in a format newer than this build reads.
///
/// An older format, and a tree with no format file, are accepted: the reader
/// reads older formats as the backup reader reads older backups.
pub fn check_format(tree: &StructureTree) -> Result<()> {
    let Some(found) = read_format(tree)? else {
        return Ok(());
    };
    if found.format > FORMAT_VERSION {
        return Err(Error::invalid(format!(
            "This structure was written by Feldspar {} in structure format {}, and this \
             instance runs Feldspar {WRITER_VERSION}, which reads structure format \
             {FORMAT_VERSION} and older. Upgrade this instance to Feldspar {} or later \
             before deploying it.",
            found.writer, found.format, found.writer
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn files_with(format: &Json) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        files.insert(FORMAT_FILE.to_owned(), crate::tree::render_json(format));
        files
    }

    #[test]
    fn the_current_format_reads_back() {
        let mut tree = StructureTree::new();
        tree.insert_json(FORMAT_FILE, StructureFormat::current().to_json());
        let back = StructureTree::from_files(tree.to_files()).unwrap();
        assert_eq!(
            read_format(&back).unwrap(),
            Some(StructureFormat::current())
        );
        // No format file is an empty repository's state, and is read.
        assert_eq!(read_format(&StructureTree::new()).unwrap(), None);
    }

    #[test]
    fn a_newer_format_is_refused_naming_both_versions() {
        let newer = json!({ "format": FORMAT_VERSION + 1, "writer": "9.4.0" });
        let err = StructureTree::from_files(files_with(&newer))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Feldspar 9.4.0"), "{err}");
        assert!(
            err.contains(&format!("format {}", FORMAT_VERSION + 1)),
            "{err}"
        );
        assert!(err.contains(&format!("Feldspar {WRITER_VERSION}")), "{err}");
        assert!(
            err.contains(&format!("format {FORMAT_VERSION} and older")),
            "{err}"
        );
        assert!(err.contains("Upgrade this instance"), "{err}");
    }

    #[test]
    fn an_older_format_is_read_and_a_missing_version_is_not() {
        // Format 1 is the oldest there is, so "older" is the current one here;
        // what is checked is that the bound is inclusive.
        let same = json!({ "format": FORMAT_VERSION, "writer": "0.0.1" });
        StructureTree::from_files(files_with(&same)).unwrap();

        let err = StructureTree::from_files(files_with(&json!({ "writer": "0.1.0" })))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no format version"), "{err}");
    }
}

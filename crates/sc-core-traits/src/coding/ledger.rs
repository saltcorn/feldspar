//! The change ledger and the run's diff (TODO §6).
//!
//! **The model edits, and the harness produces the diff** (R§3.1). The first
//! time a run touches a path, the ledger records what was there before: the
//! text, or that nothing was. Deletes and moves are touches like any other, and
//! a move is also recorded as a move. The run's diff is then the ledger's
//! pre-images against what the store holds now, computed here with `similar`,
//! so every store backend gets one, S3 included, and no git is needed.
//!
//! Later phases read the ledger too: the test ratchet (a deleted test file, a
//! test block removed) and the per-feature commit.
//!
//! **Large files.** A pre-image over [`MAX_PRE_IMAGE_BYTES`], or one that is not
//! text, is kept as a hash and a size only. The run's state is saved with the
//! run after every step, and a megabyte of bundle copied into it each time costs
//! more than a line-level diff of that file is worth. Such a file still shows up
//! as changed, without its lines.

use std::collections::BTreeMap;

use sc_catalog::Catalog;
use sc_error::Result;
use sc_files::FileStore;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use similar::{ChangeTag, TextDiff};

use super::state::{CodingState, content_hash};
use crate::files::FileScope;

/// The largest pre-image kept whole.
pub const MAX_PRE_IMAGE_BYTES: usize = 1_000_000;

/// What each touched path held before the run first touched it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    /// Pre-images by store path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    files: BTreeMap<String, PreImage>,
    /// Every move, in order, by store path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    moves: Vec<Move>,
}

/// One path's content before the run touched it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreImage {
    /// Nothing was there.
    Absent,
    /// This text was there.
    Text {
        /// The text.
        text: String,
    },
    /// A file too large, or not text, kept by hash.
    Opaque {
        /// Its content hash.
        hash: String,
        /// Its size in bytes.
        size: u64,
    },
}

impl PreImage {
    /// The pre-image of `bytes`, or of nothing.
    pub(crate) fn of(bytes: Option<&[u8]>) -> PreImage {
        match bytes {
            None => PreImage::Absent,
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) if bytes.len() <= MAX_PRE_IMAGE_BYTES => PreImage::Text {
                    text: text.to_owned(),
                },
                _ => PreImage::Opaque {
                    hash: content_hash(bytes),
                    size: bytes.len() as u64,
                },
            },
        }
    }
}

/// One move, by store path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    /// Where it was.
    pub from: String,
    /// Where it went.
    pub to: String,
}

impl Ledger {
    /// Whether the run has touched nothing.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.moves.is_empty()
    }

    /// Record `path`'s content before a change, if this is the run's first
    /// touch of it. `before` is `None` when nothing is there.
    pub fn touch(&mut self, path: &str, before: Option<&[u8]>) {
        self.files
            .entry(path.to_owned())
            .or_insert_with(|| PreImage::of(before));
    }

    /// Record `path`'s pre-image as already worked out, if this is the run's
    /// first touch of it: how a change the shell made is entered (TODO 6a.6).
    pub fn touch_with(&mut self, path: &str, before: PreImage) {
        self.files.entry(path.to_owned()).or_insert(before);
    }

    /// Record a move. Both paths must already have been touched.
    pub fn moved(&mut self, from: &str, to: &str) {
        self.moves.push(Move {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }

    /// The touched paths, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// What `path` held before the run touched it, if the run did.
    pub fn pre_image(&self, path: &str) -> Option<&PreImage> {
        self.files.get(path)
    }

    /// Every move, in order.
    pub fn moves(&self) -> &[Move] {
        &self.moves
    }

    /// Take in a later run's ledger over the same scope: its pre-images for
    /// paths this one has not touched, and its moves after this one's. What a
    /// planner run's diff is made of, with its sessions' (TODO 10.5).
    pub fn absorb(&mut self, later: &Ledger) {
        for (path, before) in &later.files {
            self.touch_with(path, before.clone());
        }
        self.moves.extend(later.moves.iter().cloned());
    }
}

/// How one path changed over the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    /// It did not exist before.
    Added,
    /// It existed and still does, with different content.
    Modified,
    /// It existed and does not now.
    Deleted,
}

impl ChangeStatus {
    /// The one-letter code `git status --short` uses.
    pub fn code(self) -> char {
        match self {
            ChangeStatus::Added => 'A',
            ChangeStatus::Modified => 'M',
            ChangeStatus::Deleted => 'D',
        }
    }
}

/// One changed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// The path, relative to the scope.
    pub path: String,
    /// How it changed.
    pub status: ChangeStatus,
    /// Lines added, or `None` for a file kept by hash.
    pub added: Option<usize>,
    /// Lines removed, or `None` for a file kept by hash.
    pub removed: Option<usize>,
}

/// The run's changes, as a diffstat and a unified diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunDiff {
    /// Every path whose content differs from its pre-image, sorted.
    pub files: Vec<FileChange>,
    /// Every move, relative to the scope, in order.
    pub moves: Vec<(String, String)>,
    /// The unified diff of every text change.
    pub unified: String,
}

impl RunDiff {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// A diffstat: one line per file, then the totals.
    pub fn stat(&self) -> String {
        let mut out = String::new();
        for (from, to) in &self.moves {
            out.push_str(&format!("R {from} → {to}\n"));
        }
        let (mut added, mut removed) = (0, 0);
        for file in &self.files {
            match (file.added, file.removed) {
                (Some(a), Some(r)) => {
                    added += a;
                    removed += r;
                    out.push_str(&format!(
                        "{} {} | +{a} -{r}\n",
                        file.status.code(),
                        file.path
                    ));
                }
                _ => out.push_str(&format!(
                    "{} {} | not text, or too large to diff\n",
                    file.status.code(),
                    file.path
                )),
            }
        }
        out.push_str(&format!(
            "{} file{} changed, {added} insertion{}(+), {removed} deletion{}(-)",
            self.files.len(),
            plural(self.files.len()),
            plural(added),
            plural(removed),
        ));
        out
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The diff of everything this `coding` instance's run changed, from its state.
pub async fn run_diff(scope: &FileScope, catalog: &Catalog, state: &Json) -> Result<RunDiff> {
    let state = CodingState::load(state);
    let (store, _) = scope.connect(catalog).await?;
    diff_ledger(scope, store.as_ref(), &state.ledger).await
}

/// The diff of `ledger` against what `store` holds now.
pub async fn diff_ledger(
    scope: &FileScope,
    store: &dyn FileStore,
    ledger: &Ledger,
) -> Result<RunDiff> {
    let mut diff = RunDiff {
        moves: ledger
            .moves()
            .iter()
            .map(|m| (scope.relative(&m.from), scope.relative(&m.to)))
            .collect(),
        ..RunDiff::default()
    };
    for (path, before) in &ledger.files {
        let at = sc_files::StorePath::parse(path)?;
        let now = match store.stat(&at).await? {
            Some(stat) if !stat.is_dir => Some(store.read(&at).await?),
            _ => None,
        };
        let rel = scope.relative(path);
        let status = match (before, &now) {
            (PreImage::Absent, None) => continue,
            (PreImage::Absent, Some(_)) => ChangeStatus::Added,
            (_, None) => ChangeStatus::Deleted,
            (_, Some(_)) => ChangeStatus::Modified,
        };
        let old_text = match before {
            PreImage::Absent => Some(""),
            PreImage::Text { text } => Some(text.as_str()),
            PreImage::Opaque { .. } => None,
        };
        let new_text = match &now {
            None => Some(""),
            Some(bytes) if bytes.len() <= MAX_PRE_IMAGE_BYTES => std::str::from_utf8(bytes).ok(),
            Some(_) => None,
        };
        let (Some(old_text), Some(new_text)) = (old_text, new_text) else {
            let same = match (before, &now) {
                (PreImage::Opaque { hash, .. }, Some(bytes)) => *hash == content_hash(bytes),
                _ => false,
            };
            if !same {
                diff.files.push(FileChange {
                    path: rel,
                    status,
                    added: None,
                    removed: None,
                });
            }
            continue;
        };
        if status == ChangeStatus::Modified && old_text == new_text {
            continue;
        }
        let lines = TextDiff::from_lines(old_text, new_text);
        let (mut added, mut removed) = (0, 0);
        for change in lines.iter_all_changes() {
            match change.tag() {
                ChangeTag::Insert => added += 1,
                ChangeTag::Delete => removed += 1,
                ChangeTag::Equal => {}
            }
        }
        let (from, to) = match status {
            ChangeStatus::Added => ("/dev/null".to_owned(), format!("b/{rel}")),
            ChangeStatus::Deleted => (format!("a/{rel}"), "/dev/null".to_owned()),
            ChangeStatus::Modified => (format!("a/{rel}"), format!("b/{rel}")),
        };
        diff.unified.push_str(
            &lines
                .unified_diff()
                .context_radius(3)
                .header(&from, &to)
                .to_string(),
        );
        diff.files.push(FileChange {
            path: rel,
            status,
            added: Some(added),
            removed: Some(removed),
        });
    }
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_touch_records_a_pre_image() {
        let mut ledger = Ledger::default();
        ledger.touch("a.ts", Some(b"one\n"));
        ledger.touch("a.ts", Some(b"two\n"));
        ledger.touch("b.ts", None);
        assert_eq!(
            ledger.pre_image("a.ts"),
            Some(&PreImage::Text {
                text: "one\n".to_owned()
            })
        );
        assert_eq!(ledger.pre_image("b.ts"), Some(&PreImage::Absent));
        assert_eq!(ledger.paths().collect::<Vec<_>>(), ["a.ts", "b.ts"]);
    }

    #[test]
    fn absorbing_a_later_ledger_keeps_the_earlier_pre_images() {
        let mut first = Ledger::default();
        first.touch("a.ts", Some(b"original\n"));
        let mut later = Ledger::default();
        later.touch("a.ts", Some(b"after the first session\n"));
        later.touch("b.ts", None);
        later.moved("c.ts", "d.ts");
        first.absorb(&later);
        assert_eq!(
            first.pre_image("a.ts"),
            Some(&PreImage::Text {
                text: "original\n".to_owned()
            })
        );
        assert_eq!(first.pre_image("b.ts"), Some(&PreImage::Absent));
        assert_eq!(first.moves().len(), 1);
    }

    #[test]
    fn a_large_or_binary_pre_image_is_kept_by_hash() {
        let big = vec![b'x'; MAX_PRE_IMAGE_BYTES + 1];
        assert!(matches!(
            PreImage::of(Some(&big)),
            PreImage::Opaque { size, .. } if size == big.len() as u64
        ));
        assert!(matches!(
            PreImage::of(Some(&[0xff, 0xfe])),
            PreImage::Opaque { .. }
        ));
    }

    #[test]
    fn a_diffstat_counts_lines_and_files() {
        let diff = RunDiff {
            files: vec![
                FileChange {
                    path: "src/a.ts".to_owned(),
                    status: ChangeStatus::Modified,
                    added: Some(2),
                    removed: Some(1),
                },
                FileChange {
                    path: "logo.png".to_owned(),
                    status: ChangeStatus::Added,
                    added: None,
                    removed: None,
                },
            ],
            moves: vec![("old.ts".to_owned(), "new.ts".to_owned())],
            unified: String::new(),
        };
        assert_eq!(
            diff.stat(),
            "R old.ts → new.ts\nM src/a.ts | +2 -1\nA logo.png | not text, or too large to diff\n\
             2 files changed, 2 insertions(+), 1 deletion(-)"
        );
    }
}

//! The scope before and after a shell command, so the change ledger sees what
//! the shell did (TODO §7a, 6a.6).
//!
//! The file tools write through the ledger. A shell writes behind its back, so
//! each shell call is bracketed: the scope is walked before it (skipping the
//! directories every coding tool skips) and hashed, and the files are copied
//! so their pre-images are at hand. After the call it is walked and hashed again,
//! and each path whose hash changed, appeared or vanished is entered in the
//! ledger with its pre-image. The model's record of having read it is
//! forgotten, so its next edit asks for a re-read.
//!
//! **Before every call, not once per session.** The spec asked for one snapshot
//! before the first shell call, compared after each. Taking it before each call
//! instead costs a walk per call and buys two things: nothing is held in memory
//! between calls (so nothing is lost on a resume), and a change the file tools
//! or a person made between two shell calls is never mistaken for the shell's.
//! The ledger keeps only a path's first pre-image, so the run's diff is the same.
//!
//! **In a git repository** only the files that differ from `HEAD`, or that git
//! does not track, are copied: every other pre-image is `HEAD`'s blob, read (by
//! the commit hash taken before the call, in case the command committed) only for
//! a path that actually changed.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::ledger::{MAX_PRE_IMAGE_BYTES, PreImage};
use super::state::{CodingState, content_hash};
use crate::files::FileScope;

/// The most bytes of file content one snapshot copies. Past it, files are kept
/// by hash, and their pre-images are opaque.
pub const MAX_COPY_BYTES: usize = 64 * 1024 * 1024;

/// The most files one snapshot looks at.
pub const MAX_FILES: usize = 50_000;

/// One file as the snapshot saw it.
#[derive(Debug, Clone)]
struct Entry {
    hash: String,
    size: u64,
    copy: Option<Vec<u8>>,
}

/// A scope's files, by path relative to the scope.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    files: BTreeMap<String, Entry>,
    git: Option<GitBase>,
}

/// The commit the clean tracked files are read back from.
#[derive(Debug, Clone)]
struct GitBase {
    head: String,
    clean: HashSet<String>,
}

impl Snapshot {
    /// Walk `dir` on a blocking thread. `copy` keeps the contents too.
    pub async fn take_async(dir: PathBuf, copy: bool) -> Snapshot {
        tokio::task::spawn_blocking(move || Snapshot::take(&dir, copy))
            .await
            .unwrap_or_default()
    }

    /// Walk `dir`: every file's hash, and with `copy` its content, unless git
    /// holds it.
    pub fn take(dir: &Path, copy: bool) -> Snapshot {
        let git = match copy {
            true => git_base(dir),
            false => None,
        };
        let mut snapshot = Snapshot {
            files: BTreeMap::new(),
            git,
        };
        let mut copied = 0usize;
        let mut pending = vec![(dir.to_path_buf(), String::new())];
        while let Some((abs, rel)) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&abs) else {
                continue;
            };
            for entry in entries.flatten() {
                if snapshot.files.len() >= MAX_FILES {
                    return snapshot;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = match rel.is_empty() {
                    true => name.clone(),
                    false => format!("{rel}/{name}"),
                };
                // Not followed: a link out of the scope is not the scope's.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    if !sc_files::DEFAULT_EXCLUDED_DIRS.contains(&name.as_str()) {
                        pending.push((entry.path(), path));
                    }
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let Ok(bytes) = std::fs::read(entry.path()) else {
                    continue;
                };
                let in_git = snapshot
                    .git
                    .as_ref()
                    .is_some_and(|git| git.clean.contains(&path));
                let keep = copy
                    && !in_git
                    && bytes.len() <= MAX_PRE_IMAGE_BYTES
                    && copied + bytes.len() <= MAX_COPY_BYTES;
                if keep {
                    copied += bytes.len();
                }
                snapshot.files.insert(
                    path,
                    Entry {
                        hash: content_hash(&bytes),
                        size: bytes.len() as u64,
                        copy: keep.then_some(bytes),
                    },
                );
            }
        }
        snapshot
    }

    /// What `rel` held when this snapshot was taken.
    fn pre_image(&self, dir: &Path, rel: &str) -> PreImage {
        let Some(entry) = self.files.get(rel) else {
            return PreImage::Absent;
        };
        if let Some(copy) = &entry.copy {
            return PreImage::of(Some(copy));
        }
        if let Some(git) = &self.git
            && git.clean.contains(rel)
            && let Some(bytes) = git_show(dir, &git.head, rel)
            && content_hash(&bytes) == entry.hash
        {
            return PreImage::of(Some(&bytes));
        }
        PreImage::Opaque {
            hash: entry.hash.clone(),
            size: entry.size,
        }
    }
}

/// The paths that differ between two snapshots, sorted, each with the status
/// letter `git status --short` would give it.
pub fn changes(before: &Snapshot, after: &Snapshot) -> Vec<(char, String)> {
    let mut changed: Vec<(char, String)> = Vec::new();
    for (path, entry) in &after.files {
        match before.files.get(path) {
            None => changed.push(('A', path.clone())),
            Some(old) if old.hash != entry.hash => changed.push(('M', path.clone())),
            Some(_) => {}
        }
    }
    for path in before.files.keys() {
        if !after.files.contains_key(path) {
            changed.push(('D', path.clone()));
        }
    }
    changed.sort_by(|a, b| a.1.cmp(&b.1));
    changed
}

/// Enter every change between two snapshots of `scope` in the run's ledger,
/// forget the model's reads of those paths, and describe them, one line each.
pub async fn record_changes(
    scope: &FileScope,
    dir: &Path,
    before: &Snapshot,
    after: &Snapshot,
    state: &mut CodingState,
) -> Vec<String> {
    let changed = changes(before, after);
    if changed.is_empty() {
        return Vec::new();
    }
    let paths: Vec<String> = changed.iter().map(|(_, p)| p.clone()).collect();
    let (dir_owned, before_owned) = (dir.to_path_buf(), before.clone());
    // Reading pre-images back from git runs a process per path.
    let pre_images = tokio::task::spawn_blocking(move || {
        paths
            .iter()
            .map(|rel| before_owned.pre_image(&dir_owned, rel))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let mut lines = Vec::with_capacity(changed.len());
    for ((status, rel), pre_image) in changed.iter().zip(pre_images) {
        let Ok(store_path) = scope.resolve(rel) else {
            continue;
        };
        state.ledger.touch_with(&store_path, pre_image);
        state.forget(&store_path);
        lines.push(format!("{status} {rel}"));
    }
    lines
}

/// `HEAD` and the tracked files that do not differ from it, when `dir` is in a
/// git work tree with a commit.
fn git_base(dir: &Path) -> Option<GitBase> {
    let head = String::from_utf8(git(dir, &["rev-parse", "--verify", "HEAD"])?)
        .ok()?
        .trim()
        .to_owned();
    let tracked = git(dir, &["ls-files", "-z"])?;
    let dirty = git(dir, &["diff", "--name-only", "--relative", "-z", "HEAD"])?;
    let split = |bytes: &[u8]| -> HashSet<String> {
        bytes
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect()
    };
    let dirty = split(&dirty);
    let clean = split(&tracked)
        .into_iter()
        .filter(|p| !dirty.contains(p))
        .collect();
    Some(GitBase { head, clean })
}

/// `rel` as committed at `head`.
fn git_show(dir: &Path, head: &str, rel: &str) -> Option<Vec<u8>> {
    git(dir, &["show", &format!("{head}:./{rel}")])
}

/// A git command's stdout, when it succeeds.
fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sc-snapshot-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap_or_default();
        dir
    }

    #[test]
    fn a_change_is_an_add_a_modify_or_a_delete_and_excluded_dirs_are_skipped() {
        let dir = temp("changes");
        let write = |rel: &str, text: &str| {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap_or_default();
            }
            std::fs::write(path, text).unwrap_or_default();
        };
        write("a.txt", "one");
        write("gone.txt", "bye");
        write("node_modules/x.js", "ignored");
        let before = Snapshot::take(&dir, true);
        write("a.txt", "two");
        write("src/new.txt", "hi");
        write("node_modules/x.js", "still ignored");
        let _ = std::fs::remove_file(dir.join("gone.txt"));
        let after = Snapshot::take(&dir, false);
        assert_eq!(
            changes(&before, &after),
            vec![
                ('M', "a.txt".to_owned()),
                ('D', "gone.txt".to_owned()),
                ('A', "src/new.txt".to_owned()),
            ]
        );
        assert_eq!(
            before.pre_image(&dir, "a.txt"),
            PreImage::Text {
                text: "one".to_owned()
            }
        );
        assert_eq!(before.pre_image(&dir, "src/new.txt"), PreImage::Absent);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

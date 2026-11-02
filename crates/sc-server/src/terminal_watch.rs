//! What the IDE terminal's program changed on disk (design §12.1, "The
//! workbench must be told about changed files").
//!
//! The IDE's filesystem provider has no watcher: it reads the store over HTTP,
//! and an open editor shows what it read. The chat relay knows from its tool
//! calls which files an agent wrote; a terminal carries only bytes. So while a
//! terminal is attached, its directory is watched here, and the bridge sends
//! what changed to the browser ([`crate::terminal`]'s `{"changed": [...]}`).
//!
//! ## What is watched
//!
//! - The session's directory, **one directory at a time** rather than
//!   recursively, so that `node_modules`, `.git` and build output are never
//!   watched at all: a recursive watch would add an inotify watch for every
//!   directory under `node_modules` and then throw their events away. A
//!   directory that appears later is added when it does.
//! - The top of the store's `.git`, without its sub-directories. A commit, a
//!   checkout or a staged file rewrites `index`, `HEAD` or `COMMIT_EDITMSG`
//!   there, which is all the IDE needs to know to rescan source control; the
//!   object store's churn is not watched. Its changes are reported as the one
//!   path [`GIT`].
//!
//! ## What is reported
//!
//! Paths relative to the **store's root**, `/`-separated, so the browser needs
//! nothing but the store to find them, wherever in it the session started:
//! those still there, and those gone. Which is which is decided when they are
//! reported, not from the events, because an editor's save is often a
//! removal and a rename onto the same name.
//! Changes are gathered until the directory has been quiet for [`QUIET`], and
//! never for longer than [`LONGEST_WAIT`], so an install that writes for a
//! minute is still reported while it runs.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// How long the directory must be quiet before what changed is reported.
pub(crate) const QUIET: Duration = Duration::from_millis(300);

/// The longest changes are held back while more keep arriving.
pub(crate) const LONGEST_WAIT: Duration = Duration::from_secs(2);

/// The path a change to the store's git state is reported as.
pub(crate) const GIT: &str = ".git";

/// Directories never watched, wherever they are: dependencies, git's
/// internals and build output. What happens inside them is nothing an open
/// editor shows.
const SKIPPED: [&str; 7] = [
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    ".expo",
    ".cache",
];

/// The most directories one session watches. Past it, a new directory's
/// changes go unreported rather than using up the user's inotify watches,
/// which the language server and every other session share.
const MAX_WATCHED_DIRS: usize = 10_000;

/// What changed, as store paths.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Report {
    /// Written, created, or [`GIT`].
    pub(crate) changed: Vec<String>,
    /// Gone when the report was made.
    pub(crate) removed: Vec<String>,
}

/// A watch on one session's directory, and the changes not yet reported.
pub(crate) struct Changes {
    watcher: RecommendedWatcher,
    events: mpsc::UnboundedReceiver<notify::Result<Event>>,
    /// The store's root, which reported paths are relative to.
    root: PathBuf,
    /// The session's directory.
    dir: PathBuf,
    /// The store's `.git`.
    git: PathBuf,
    watched: usize,
    pending: BTreeSet<String>,
    /// When the first and the latest of the pending changes arrived.
    first: Option<Instant>,
    latest: Option<Instant>,
}

impl Changes {
    /// Watch `dir`, which is in the store whose root is `root`.
    pub(crate) fn watch(root: &Path, dir: &Path) -> notify::Result<Changes> {
        // The kernel reports the paths it was given; resolved, so a store
        // reached through a symlink still has its paths inside the root.
        let root = root.canonicalize()?;
        let dir = dir.canonicalize()?;
        let (sender, events) = mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |event| {
            // The receiver goes with the session; nothing is left to tell.
            let _ = sender.send(event);
        })?;
        let git = root.join(GIT);
        let mut changes = Changes {
            watcher,
            events,
            root,
            dir: dir.clone(),
            git: git.clone(),
            watched: 0,
            pending: BTreeSet::new(),
            first: None,
            latest: None,
        };
        changes.watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        changes.watched += 1;
        changes.add_below(&dir, None);
        if git.is_dir() {
            changes.watcher.watch(&git, RecursiveMode::NonRecursive)?;
        }
        Ok(changes)
    }

    /// The next changes, once the directory has been quiet; `None` once the
    /// watch has ended.
    ///
    /// Cancel-safe: what has been gathered is kept in `self`, so a `select!`
    /// that drops this future loses nothing.
    pub(crate) async fn next(&mut self) -> Option<Report> {
        loop {
            let due = self.due();
            tokio::select! {
                event = self.events.recv() => self.take(event?),
                () = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                    self.first = None;
                    self.latest = None;
                    return Some(self.report());
                }
            }
        }
    }

    /// The pending changes, split into what is there and what is gone.
    fn report(&mut self) -> Report {
        let (changed, removed) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|path| path == GIT || self.root.join(path).symlink_metadata().is_ok());
        Report { changed, removed }
    }

    /// When the pending changes are to be reported, if there are any.
    fn due(&self) -> Option<Instant> {
        Some((self.latest? + QUIET).min(self.first? + LONGEST_WAIT))
    }

    /// Note what `event` changed.
    fn take(&mut self, event: notify::Result<Event>) {
        // An error is a watch the kernel dropped (a directory removed under
        // it); the paths that matter arrive as events of their own.
        let Ok(event) = event else { return };
        // Opening and reading a file changes nothing.
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let appeared = matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        let mut noted = false;
        for path in &event.paths {
            if path.starts_with(&self.git) {
                self.pending.insert(GIT.to_owned());
                noted = true;
                continue;
            }
            let Some(reported) = self.reported(path) else {
                continue;
            };
            if appeared && is_watched_dir(path) {
                // What was written into it before its watch was added is
                // reported with it, or a `mkdir -p` and a file written at
                // once would be the directory alone.
                let mut found = BTreeSet::new();
                self.add_below_new(path, &mut found);
                self.pending.extend(found);
            }
            self.pending.insert(reported);
            noted = true;
        }
        if noted {
            let now = Instant::now();
            self.first.get_or_insert(now);
            self.latest = Some(now);
        }
    }

    /// The store path `path` is reported as, or `None` when it is not
    /// reported: outside the session's directory, or inside a skipped one.
    fn reported(&self, path: &Path) -> Option<String> {
        let in_dir = path.strip_prefix(&self.dir).ok()?;
        // A skipped directory itself appearing is reported; what is in it is not.
        if in_dir.parent().is_some_and(|parent| {
            parent
                .components()
                .any(|c| matches!(c, Component::Normal(name) if is_skipped(name)))
        }) {
            return None;
        }
        store_path(path.strip_prefix(&self.root).ok()?)
    }

    /// Watch a directory that has just appeared, and everything under it;
    /// the files found go into `found`.
    fn add_below_new(&mut self, dir: &Path, found: &mut BTreeSet<String>) {
        if self.watched >= MAX_WATCHED_DIRS
            || self
                .watcher
                .watch(dir, RecursiveMode::NonRecursive)
                .is_err()
        {
            return;
        }
        self.watched += 1;
        self.add_below(dir, Some(found));
    }

    /// Watch every directory under `dir` that is not skipped, putting the files
    /// found into `found` when one is given.
    fn add_below(&mut self, dir: &Path, mut found: Option<&mut BTreeSet<String>>) {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&next) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                // Not followed through a symlink: what it points at may be
                // outside the store, or the directory it is in.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    if is_skipped(&entry.file_name()) || self.watched >= MAX_WATCHED_DIRS {
                        continue;
                    }
                    if self
                        .watcher
                        .watch(&path, RecursiveMode::NonRecursive)
                        .is_ok()
                    {
                        self.watched += 1;
                        stack.push(path);
                    }
                } else if let Some(found) = found.as_deref_mut()
                    && let Some(reported) = self.reported(&path)
                {
                    found.insert(reported);
                }
            }
        }
    }
}

/// Whether `name` is a directory never watched.
fn is_skipped(name: &std::ffi::OsStr) -> bool {
    SKIPPED.iter().any(|skipped| name == *skipped)
}

/// Whether `path` is a directory to watch: a real one, not a symlink, and not
/// skipped.
fn is_watched_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|meta| meta.is_dir())
        && path.file_name().is_some_and(|name| !is_skipped(name))
}

/// `relative` as a store path, or `None` for the root itself or a name that is
/// not UTF-8 (which the store's API could not name either).
fn store_path(relative: &Path) -> Option<String> {
    let parts = relative
        .components()
        .map(|c| match c {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, unique temp directory, created on disk.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sc-terminal-watch-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The paths the next report names as changed, or `None` if there is no
    /// report within a few seconds.
    async fn next(changes: &mut Changes) -> Option<Vec<String>> {
        next_report(changes).await.map(|report| report.changed)
    }

    async fn next_report(changes: &mut Changes) -> Option<Report> {
        tokio::time::timeout(Duration::from_secs(5), changes.next())
            .await
            .ok()
            .flatten()
    }

    /// Nothing is reported for a while longer than a report would take.
    async fn quiet(changes: &mut Changes) -> Option<Report> {
        tokio::time::timeout(LONGEST_WAIT + QUIET * 2, changes.next())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn several_writes_arrive_as_one_report_of_store_paths() {
        let root = temp_dir("writes");
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        let mut changes = Changes::watch(&root, &root.join("app")).unwrap();

        std::fs::write(root.join("app/src/a.ts"), "a").unwrap();
        std::fs::write(root.join("app/src/a.ts"), "aa").unwrap();
        std::fs::write(root.join("app/b.ts"), "b").unwrap();
        assert_eq!(
            next(&mut changes).await,
            Some(vec!["app/b.ts".to_owned(), "app/src/a.ts".to_owned()])
        );
        assert_eq!(quiet(&mut changes).await, None);
    }

    #[tokio::test]
    async fn skipped_directories_report_nothing() {
        let root = temp_dir("skipped");
        for dir in ["node_modules/pkg", "dist", "src/build"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let mut changes = Changes::watch(&root, &root).unwrap();

        std::fs::write(root.join("node_modules/pkg/index.js"), "x").unwrap();
        std::fs::write(root.join("dist/bundle.js"), "x").unwrap();
        std::fs::write(root.join("src/build/out.js"), "x").unwrap();
        assert_eq!(quiet(&mut changes).await, None);
    }

    #[tokio::test]
    async fn a_new_directory_is_watched_and_reported_with_its_files() {
        let root = temp_dir("new-dir");
        let mut changes = Changes::watch(&root, &root).unwrap();

        std::fs::create_dir_all(root.join("src/screens")).unwrap();
        std::fs::write(root.join("src/screens/Home.tsx"), "x").unwrap();
        let first = next(&mut changes).await.expect("a report");
        assert!(first.contains(&"src".to_owned()), "{first:?}");

        // Watched from now on: a later write is reported on its own.
        std::fs::write(root.join("src/screens/Home.tsx"), "y").unwrap();
        assert_eq!(
            next(&mut changes).await,
            Some(vec!["src/screens/Home.tsx".to_owned()])
        );
    }

    #[tokio::test]
    async fn the_store_git_state_is_reported_as_one_path() {
        let root = temp_dir("git");
        std::fs::create_dir_all(root.join(".git/objects/ab")).unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        // From an application's directory, the store's `.git` is above it.
        let mut changes = Changes::watch(&root, &root.join("app")).unwrap();

        std::fs::write(root.join(".git/index"), "x").unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        assert_eq!(next(&mut changes).await, Some(vec![GIT.to_owned()]));

        // The object store is not watched.
        std::fs::write(root.join(".git/objects/ab/cdef"), "x").unwrap();
        assert_eq!(quiet(&mut changes).await, None);
    }

    #[tokio::test]
    async fn a_file_gone_by_the_report_is_reported_as_removed() {
        let root = temp_dir("removed");
        std::fs::write(root.join("old.ts"), "x").unwrap();
        std::fs::write(root.join("kept.ts"), "x").unwrap();
        let mut changes = Changes::watch(&root, &root).unwrap();

        std::fs::remove_file(root.join("old.ts")).unwrap();
        // An editor's save: the file goes, and comes back under its name.
        std::fs::write(root.join("kept.ts.tmp"), "y").unwrap();
        std::fs::remove_file(root.join("kept.ts")).unwrap();
        std::fs::rename(root.join("kept.ts.tmp"), root.join("kept.ts")).unwrap();
        assert_eq!(
            next_report(&mut changes).await,
            Some(Report {
                changed: vec!["kept.ts".to_owned()],
                removed: vec!["kept.ts.tmp".to_owned(), "old.ts".to_owned()],
            })
        );
    }

    #[test]
    fn a_store_path_is_slash_separated_and_never_the_root() {
        assert_eq!(store_path(Path::new("a/b.ts")), Some("a/b.ts".to_owned()));
        assert_eq!(store_path(Path::new("")), None);
    }
}

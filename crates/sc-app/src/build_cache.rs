//! Knowing when an application's build would produce what it produced last
//! time, so the boot path can skip it (design §13.2).
//!
//! Every boot used to run every application's bundler, and a bundler run is
//! seconds to minutes, so start-up time grew with the number of applications
//! even when nothing in any of them had changed. The fix is a **key**: a hash of
//! everything the build reads, written beside the output after a successful
//! build and compared before the next one.
//!
//! ## The source tree is hashed by git
//!
//! The commit hash would do for a clean checkout, but an application under
//! development is rarely clean, and a key that ignored edits would serve a stale
//! bundle. What is hashed instead is the **tree** git would record if everything
//! were committed now: the working directory is staged into a *throwaway* index
//! (a copy of the real one, so git's stat cache means only changed files are
//! re-read) and written with `git write-tree`. That hash
//!
//! - covers staged, unstaged and untracked-but-not-ignored files alike, while
//!   `.gitignore` keeps `node_modules` and the like out;
//! - equals `HEAD^{tree}` on a clean checkout, so there is one path, not two;
//! - depends only on content, so an edit that is undone, or a rebase or amend
//!   that lands on the same content, gets its old key back.
//!
//! The real index, the refs and the working tree are never touched; the only
//! trace is blob objects for changed files, which `git gc` collects.
//!
//! Only the **source directory's** subtree is hashed (`write-tree --prefix`), so
//! editing one application in a repository holding several does not rebuild the
//! others, and the output directory and the install marker are left out even
//! where nothing ignores them, since the build itself writes them.
//!
//! ## What else is in the key
//!
//! The build spec (a changed command is a different build), the Feldspar version,
//! and the bytes of every **generated** file emitted just before the build. The
//! generated client and runtime normally sit inside the source tree and are
//! hashed there too, but a client path may point outside it, and a schema change
//! must rebuild either way.
//!
//! ## Where the stamp lives, and when there is none
//!
//! In the repository's git dir (`.git/feldspar-builds/`), named for the output
//! directory, not in the output directory: everything in there is served, and a
//! file beside it in the source tree would show up in `git status`. A missing or
//! empty output directory still forces a build, because reusing means loading
//! the bundle and that load fails.
//!
//! A source directory that is not in a git repository, or a repository git
//! cannot write a tree for (an unresolved merge), has **no key**, and is built
//! every time as before. So does any failure computing one: a cache that guesses
//! would sometimes serve the wrong bundle, and one that builds loses only time.
//!
//! **Known gap:** an ignored file that the bundler reads — `.env.local` for Vite
//! — is not in the key. An explicit build (the Build button, the tool) always
//! runs the bundler, which is the way to pick such a change up.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::framework::BuildSpec;

/// Bumped when what goes into the key changes, so a stamp written by an older
/// scheme never matches.
const KEY_SCHEME: &str = "feldspar-build/1";

/// The directory under the git dir that holds one stamp per output directory.
const STAMP_DIR: &str = "feldspar-builds";

/// A computed build key and where its stamp is kept.
#[derive(Debug, Clone)]
pub(crate) struct BuildKey {
    /// The hex digest of everything the build reads.
    pub key: String,
    /// The file the key of the last successful build is written to.
    stamp: PathBuf,
}

impl BuildKey {
    /// Whether the last successful build of this output directory had this key.
    pub fn matches_stamp(&self) -> bool {
        std::fs::read_to_string(&self.stamp).is_ok_and(|stamp| stamp.trim() == self.key)
    }

    /// Forget the last build, before a new one starts writing over its output: a
    /// build that fails half way leaves an output directory that no key
    /// describes, and a stamp still naming the old key would let a later boot on
    /// the old source serve it.
    pub fn clear_stamp(&self) {
        std::fs::remove_file(&self.stamp).ok();
    }

    /// Record that a build with this key succeeded. Failing to is not an error —
    /// the next boot just builds again.
    pub fn write_stamp(&self) {
        if let Some(dir) = self.stamp.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(&self.stamp, &self.key).ok();
    }
}

/// The key for building `spec` over `source_dir` into `output_dir`, with
/// `generated` the files emitted for this build (path and bytes), or `None`
/// when there is no trustworthy one (see the module docs).
pub(crate) async fn build_key(
    spec: &BuildSpec,
    source_dir: &Path,
    output_dir: &Path,
    generated: &[(String, Vec<u8>)],
) -> Option<BuildKey> {
    let (tree, stamp_dir) = source_tree(spec, source_dir, output_dir).await?;

    let mut hasher = Sha256::new();
    for part in [
        KEY_SCHEME,
        env!("CARGO_PKG_VERSION"),
        &format!("{spec:?}"),
        &tree,
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    for (path, bytes) in generated {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    let key = hex(&hasher.finalize());

    // Named for the output directory, since that is what the stamp vouches for.
    let name = hex(&Sha256::digest(output_dir.to_string_lossy().as_bytes()));
    Some(BuildKey {
        key,
        stamp: stamp_dir.join(&name[..32]),
    })
}

/// The tree hash of `source_dir`'s working state and the stamp directory, or
/// `None` when it is not in a repository or git fails.
async fn source_tree(
    spec: &BuildSpec,
    source_dir: &Path,
    output_dir: &Path,
) -> Option<(String, PathBuf)> {
    // One call for all three: where `source_dir` sits in its repository (empty at
    // the top), the real index, and the stamp directory. `--git-path` answers
    // relative to the working directory, and follows a worktree's `.git` file.
    let out = git(
        source_dir,
        None,
        &[
            "rev-parse",
            "--show-prefix",
            "--git-path",
            "index",
            "--git-path",
            STAMP_DIR,
        ],
    )
    .await?;
    let mut lines = out.split('\n');
    let prefix = lines.next()?.to_owned();
    let index = source_dir.join(lines.next()?);
    let stamp_dir = source_dir.join(lines.next()?);

    let scratch = ScratchIndex::new(&index)?;

    // Paths the build writes itself are no part of its input. Both are relative
    // to the source directory, which is where git runs.
    let mut excluded = Vec::new();
    if let Ok(out) = output_dir.strip_prefix(source_dir) {
        excluded.push(out.to_string_lossy().into_owned());
    }
    if let Some(install) = &spec.install {
        excluded.push(install.marker.clone());
    }
    excluded.retain(|p| !p.is_empty());

    // Drop them where the index already tracks them (a committed `dist`), then
    // stage everything else.
    if !excluded.is_empty() {
        let mut rm = vec!["rm", "-r", "-q", "--cached", "--ignore-unmatch", "--"];
        rm.extend(excluded.iter().map(String::as_str));
        git(source_dir, Some(scratch.path()), &rm).await?;
    }
    // `.gitignore` usually covers them already, and an exclude pathspec naming
    // an ignored path makes `git add` fail ("paths are ignored"), so only the
    // ones it does not cover are excluded here. `check-ignore` prints the ignored
    // ones and exits 1 when there are none, which is not a failure.
    // `--no-index` because a committed `dist` counts as not ignored otherwise,
    // and by now it is out of the scratch index anyway.
    let ignored = if excluded.is_empty() {
        Vec::new()
    } else {
        let mut check = vec!["check-ignore", "--no-index", "--"];
        check.extend(excluded.iter().map(String::as_str));
        let out = git_status_ok(source_dir, &check, &[0, 1]).await?;
        out.lines().map(str::to_owned).collect()
    };
    let pathspecs: Vec<String> = excluded
        .iter()
        .filter(|p| !ignored.contains(p))
        .map(|p| format!(":(exclude,literal){p}"))
        .collect();
    let mut add = vec!["add", "-A", "--", "."];
    add.extend(pathspecs.iter().map(String::as_str));
    git(source_dir, Some(scratch.path()), &add).await?;

    let prefix_arg = format!("--prefix={prefix}");
    let mut write = vec!["write-tree"];
    if !prefix.is_empty() {
        write.push(&prefix_arg);
    }
    let tree = git(source_dir, Some(scratch.path()), &write).await?;
    let tree = tree.trim();
    (!tree.is_empty()).then(|| (tree.to_owned(), stamp_dir))
}

/// A copy of the repository's index, removed when dropped.
struct ScratchIndex(PathBuf);

impl ScratchIndex {
    /// Copy `index`, or start empty in a repository that has none yet. The copy
    /// sits beside the real one, so it is on the same filesystem and inside a
    /// directory git already owns.
    ///
    /// The copy keeps the original's mtime. Git trusts an entry whose stat data
    /// matches the file only when the file is older than the index; a file
    /// written in the same second as the index is "racily clean" and re-read.
    /// A copy stamped *now* looks newer than such a file, so a same-size edit
    /// made in the second the index was written would keep its old blob and
    /// its old key.
    fn new(index: &Path) -> Option<ScratchIndex> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = index.with_file_name(format!(
            "feldspar-build-index-{}-{nanos}",
            std::process::id()
        ));
        let scratch = ScratchIndex(path);
        if index.is_file() {
            let modified = std::fs::metadata(index).ok()?.modified().ok()?;
            std::fs::copy(index, &scratch.0).ok()?;
            std::fs::File::options()
                .write(true)
                .open(&scratch.0)
                .ok()?
                .set_modified(modified)
                .ok()?;
        }
        Some(scratch)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// Run git in `dir`, optionally against another index, returning stdout on
/// success and `None` on any failure — including git not being installed, and
/// `dir` being in no work tree.
async fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Option<String> {
    let output = git_command(dir, index, args)?.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// [`git`] for a command whose exit status carries an answer: stdout when it
/// exits with one of `ok`, `None` otherwise.
async fn git_status_ok(dir: &Path, args: &[&str], ok: &[i32]) -> Option<String> {
    let output = git_command(dir, None, args)?.output().await.ok()?;
    if !output.status.code().is_some_and(|code| ok.contains(&code)) {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `git args` in `dir`, against the repository of the work tree `dir` is in,
/// named explicitly rather than discovered: `dir` is an app's source directory,
/// which users below admin write, and a planted bare repository layout there
/// would otherwise be the repository git uses, `core.fsmonitor` and all
/// (security review 2026-11, #1).
fn git_command(dir: &Path, index: Option<&Path>, args: &[&str]) -> Option<Command> {
    let work_tree = sc_files::git_command::work_tree_of(dir)?;
    let mut cmd = sc_files::git_command::command(&work_tree);
    cmd.args(args).current_dir(dir);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    Some(cmd)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::InstallSpec;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "sc-app-build-cache-{}-{tag}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn sh(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn spec() -> BuildSpec {
        BuildSpec {
            command: "sh".to_owned(),
            args: vec!["build.sh".to_owned()],
            source_dir: "web".to_owned(),
            output_dir: "web/dist".to_owned(),
            install: Some(InstallSpec {
                command: "npm".to_owned(),
                args: vec!["install".to_owned()],
                marker: "node_modules".to_owned(),
            }),
            server_entry: None,
        }
    }

    /// A repository with `web/` (the app) and `other/` (another app) committed.
    fn repo(tag: &str) -> Option<TempDir> {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: git is not installed");
            return None;
        }
        let tmp = TempDir::new(tag);
        let root = &tmp.0;
        std::fs::create_dir_all(root.join("web/src")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("web/src/main.ts"), "one").unwrap();
        std::fs::write(root.join("other/main.ts"), "x").unwrap();
        sh(root, &["init", "-q"]);
        sh(root, &["add", "-A"]);
        sh(root, &["commit", "-q", "-m", "init"]);
        Some(tmp)
    }

    async fn key(root: &Path) -> String {
        let web = root.join("web");
        build_key(&spec(), &web, &web.join("dist"), &[])
            .await
            .expect("a key in a git repository")
            .key
    }

    #[tokio::test]
    async fn the_key_follows_the_source_directorys_content_in_any_git_state() {
        let Some(tmp) = repo("states") else { return };
        let root = &tmp.0;
        let clean = key(root).await;
        assert_eq!(clean, key(root).await, "the same tree, the same key");

        // An unstaged edit changes it; staging the same edit does not change it
        // again, and neither does committing it.
        std::fs::write(root.join("web/src/main.ts"), "two").unwrap();
        let edited = key(root).await;
        assert_ne!(edited, clean);
        sh(root, &["add", "-A"]);
        assert_eq!(key(root).await, edited);
        sh(root, &["commit", "-q", "-m", "two"]);
        assert_eq!(key(root).await, edited);

        // Undoing the edit gets the first key back, commit or no commit.
        std::fs::write(root.join("web/src/main.ts"), "one").unwrap();
        assert_eq!(key(root).await, clean);

        // An untracked file is part of the source; an ignored one is not.
        std::fs::write(root.join("web/src/new.ts"), "new").unwrap();
        assert_ne!(key(root).await, clean);
        std::fs::write(root.join(".gitignore"), "new.ts\n").unwrap();
        assert_eq!(key(root).await, clean);

        // The real index was left as it was: nothing is staged.
        let staged = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(staged.stdout.is_empty(), "{staged:?}");
    }

    /// A user below admin can write an app's source directory, so a bare
    /// repository layout planted there — `HEAD`, `objects/`, `refs/` and a
    /// `config` naming an fsmonitor command — must not become the repository a
    /// build's git runs against (security review 2026-11, #1).
    #[tokio::test]
    async fn a_planted_bare_repository_in_the_source_dir_is_not_run_by_a_build() {
        let Some(tmp) = repo("planted") else { return };
        let root = &tmp.0;
        let clean = key(root).await;
        let web = root.join("web");
        std::fs::create_dir_all(web.join("objects/k")).unwrap();
        std::fs::create_dir_all(web.join("refs/k")).unwrap();
        std::fs::write(web.join("HEAD"), "ref: refs/heads/x\n").unwrap();
        let marker = root.join("fsmonitor-ran");
        std::fs::write(
            web.join("config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tworktree = .\n\
                 \tfsmonitor = \"touch '{}'\"\n",
                marker.display()
            ),
        )
        .unwrap();
        let planted = key(root).await;
        assert!(!marker.exists(), "the planted fsmonitor command ran");
        // The planted files are source like any other: the key changed.
        assert_ne!(planted, clean);
    }

    #[tokio::test]
    async fn a_same_size_edit_in_the_second_of_the_commit_changes_the_key() {
        // Git's stat check is to the second, so an edit that keeps the size and
        // lands in the second the index was written is caught only by its
        // racy-clean rule — which a scratch index stamped later would defeat.
        let unix_now = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
        };
        let start = unix_now();
        tokio::time::sleep(std::time::Duration::from_nanos(
            1_000_000_000 - u64::from(start.subsec_nanos()),
        ))
        .await;
        let Some(tmp) = repo("racy") else { return };
        let root = &tmp.0;
        let clean = key(root).await;
        std::fs::write(root.join("web/src/main.ts"), "two").unwrap();
        let second = unix_now().as_secs();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        if second != start.as_secs() + 1 {
            eprintln!("skipping: the setup did not fit in one second");
            return;
        }
        assert_ne!(key(root).await, clean);
    }

    #[tokio::test]
    async fn what_the_build_writes_and_other_apps_are_not_in_the_key() {
        let Some(tmp) = repo("outside") else { return };
        let root = &tmp.0;
        let clean = key(root).await;

        // The output directory and the install marker — not ignored here, and
        // `dist` even committed — are the build's own writes.
        std::fs::create_dir_all(root.join("web/dist")).unwrap();
        std::fs::write(root.join("web/dist/index.html"), "a").unwrap();
        sh(root, &["add", "-A"]);
        sh(root, &["commit", "-q", "-m", "dist"]);
        std::fs::write(root.join("web/dist/index.html"), "b").unwrap();
        std::fs::create_dir_all(root.join("web/node_modules/x")).unwrap();
        std::fs::write(root.join("web/node_modules/x/i.js"), "y").unwrap();
        assert_eq!(key(root).await, clean);

        // The usual case, where `.gitignore` already covers both.
        std::fs::write(root.join("web/.gitignore"), "dist\nnode_modules\n").unwrap();
        let ignoring = key(root).await;
        std::fs::write(root.join("web/dist/index.html"), "c").unwrap();
        std::fs::write(root.join("web/node_modules/x/i.js"), "z").unwrap();
        assert_eq!(key(root).await, ignoring);
        std::fs::remove_file(root.join("web/.gitignore")).unwrap();

        // Another application in the same repository is not this one's input.
        std::fs::write(root.join("other/main.ts"), "changed").unwrap();
        assert_eq!(key(root).await, clean);
    }

    #[tokio::test]
    async fn the_spec_and_the_generated_files_are_in_the_key() {
        let Some(tmp) = repo("inputs") else { return };
        let web = tmp.0.join("web");
        let out = web.join("dist");
        let base = build_key(&spec(), &web, &out, &[]).await.unwrap();

        let mut other = spec();
        other.args.push("--prod".to_owned());
        assert_ne!(
            build_key(&other, &web, &out, &[]).await.unwrap().key,
            base.key
        );

        let client = vec![("../client.ts".to_owned(), b"export {}".to_vec())];
        let with = build_key(&spec(), &web, &out, &client).await.unwrap();
        assert_ne!(with.key, base.key);
        assert_eq!(with.stamp, base.stamp, "one stamp per output directory");

        // The stamp round-trips, and is cleared.
        assert!(!base.matches_stamp());
        base.write_stamp();
        assert!(base.matches_stamp());
        assert!(!with.matches_stamp());
        base.clear_stamp();
        assert!(!base.matches_stamp());
    }

    #[tokio::test]
    async fn a_directory_outside_any_repository_has_no_key() {
        let tmp = TempDir::new("plain");
        // `GIT_CEILING_DIRECTORIES` would be the tidy way to stop git finding
        // an enclosing repository, but the temp dir is not in one.
        let web = tmp.0.join("web");
        std::fs::create_dir_all(&web).unwrap();
        if git(&web, None, &["rev-parse", "--git-dir"]).await.is_some() {
            eprintln!("skipping: the temp directory is inside a git repository");
            return;
        }
        assert!(
            build_key(&spec(), &web, &web.join("dist"), &[])
                .await
                .is_none()
        );
    }
}

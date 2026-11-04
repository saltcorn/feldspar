//! A commit per feature, for a store whose scope is a git work tree (TODO 9.4).
//!
//! `implement_feature` commits a feature once its independent check is green,
//! with a message the cheap role writes. **Only the feature's own paths** are
//! committed — the paths its session's ledger changed, including deletions and
//! both ends of a move — so what someone else left uncommitted in the working
//! tree stays theirs.
//!
//! Which trees count is the session header's rule ([`super::header`]): the
//! scope's git work tree must lie inside the store's own directory. That covers
//! a git store, and a local store that is a repository, such as the one the
//! React scaffold initialises. A store inside somebody else's repository — a
//! checkout of the server, say — gets no commit, and neither does a store with
//! no local directory: both get the diff only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::Attrs;

use super::ledger::RunDiff;
use crate::files::FileScope;

/// Commit each green feature. On by default; it applies only to a git work
/// tree.
pub const CFG_COMMIT: &str = "commit";

/// How long one git command may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The `-c` setting every agent `add` and `commit` carries: hooks off.
const NO_HOOKS: &str = "core.hooksPath=/dev/null";

/// The most characters of the diff the commit message's writer is shown.
const MAX_MESSAGE_DIFF_CHARS: usize = 6_000;

/// Whether the configuration commits green features.
pub fn enabled(config: &Attrs) -> bool {
    config
        .get(CFG_COMMIT)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// The work tree a feature is committed to: its top level, when the scope is
/// inside a git work tree that lies within the store's own directory.
pub async fn work_tree(scope: &FileScope, catalog: &Catalog) -> Option<PathBuf> {
    let (store, _) = scope.connect(catalog).await.ok()?;
    let store_dir = store
        .local_path(&sc_files::StorePath::root())
        .ok()??
        .canonicalize()
        .ok()?;
    let dir = store.local_path(&scope.resolve_path("").ok()?).ok()??;
    if !dir.is_dir() {
        return None;
    }
    let top = git(&dir, &["rev-parse", "--show-toplevel"]).await.ok()?;
    let top = Path::new(top.trim()).canonicalize().ok()?;
    top.starts_with(store_dir).then_some(top)
}

/// The instruction the cheap role writes a commit message from.
pub const MESSAGE_SYSTEM: &str = "Write a git commit message for the change below: a subject \
     line of at most 72 characters in the imperative mood, then, only if it helps, a blank line \
     and a body of at most three short lines. Reply with the message and nothing else.";

/// What the cheap role is shown: the feature and the capped diff.
pub fn message_prompt(title: &str, description: &str, diff: &RunDiff) -> String {
    let mut unified: String = diff.unified.chars().take(MAX_MESSAGE_DIFF_CHARS).collect();
    if unified.len() < diff.unified.len() {
        unified.push_str("\n[… diff cut]");
    }
    format!(
        "Feature: {title}\n{description}\n\nDiffstat:\n{}\n\nDiff:\n{unified}",
        diff.stat()
    )
}

/// A message as a commit wants it: code fences and blank edges removed, and
/// `fallback` when nothing is left.
pub fn clean_message(message: &str, fallback: &str) -> String {
    let lines: Vec<&str> = message
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();
    let text = lines.join("\n").trim().to_owned();
    match text.is_empty() {
        true => fallback.to_owned(),
        false => text,
    }
}

/// Stage and commit the paths `diff` changed under the scope, in the work
/// tree at `top`. Returns the new commit's short hash, or `None` when git had
/// nothing to commit.
pub async fn commit(
    top: &Path,
    scope: &FileScope,
    catalog: &Catalog,
    diff: &RunDiff,
    message: &str,
) -> Result<Option<String>> {
    let (store, _) = scope.connect(catalog).await?;
    let mut rels: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
    for (from, to) in &diff.moves {
        rels.push(from);
        rels.push(to);
    }
    let mut paths: Vec<String> = Vec::new();
    for rel in rels {
        let absolute = store
            .local_path(&scope.resolve_path(rel)?)?
            .ok_or_else(|| Error::invalid("the store has no local directory to commit in"))?;
        let path = absolute.to_string_lossy().into_owned();
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return Ok(None);
    }

    // No hooks: a repository whose `core.hooksPath` is inside the working tree
    // (husky's `.husky/`) has hooks the agent may have just written, and they
    // would run as the server (security review #16).
    let mut add = vec!["-c", NO_HOOKS, "add", "--all", "--"];
    add.extend(paths.iter().map(String::as_str));
    git(top, &add).await?;
    // Asked of git directly rather than read off a failed commit's wording:
    // the paths may be unchanged (the feature's edits undone, or already
    // committed), and then there is nothing to commit.
    if !has_staged_changes(top, &paths).await? {
        return Ok(None);
    }
    // Only these paths: whatever else is staged stays staged, uncommitted.
    let mut args: Vec<&str> = vec!["-c", NO_HOOKS];
    let identity = git(top, &["config", "user.email"]).await.is_ok();
    if !identity {
        args.extend([
            "-c",
            "user.name=Saltcorn",
            "-c",
            "user.email=saltcorn@localhost",
        ]);
    }
    args.extend(["commit", "--quiet", "--no-verify", "-m", message, "--"]);
    args.extend(paths.iter().map(String::as_str));
    git(top, &args).await?;
    let hash = git(top, &["rev-parse", "--short", "HEAD"]).await?;
    Ok(Some(hash.trim().to_owned()))
}

/// Whether the index differs from `HEAD` at any of `paths` — what a commit of
/// those paths would record. In a repository with no commit yet, the index is
/// compared with the empty tree, so anything staged counts.
async fn has_staged_changes(dir: &Path, paths: &[String]) -> Result<bool> {
    let mut args = vec!["diff", "--cached", "--quiet", "--"];
    args.extend(paths.iter().map(String::as_str));
    let output = run_git(dir, &args).await?;
    // `--quiet` exits 1 when there are differences and 0 when there are none;
    // anything else is git failing.
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(git_failure(&args, &output)),
    }
}

/// A git command's stdout in `dir`, or its failure with git's own words.
async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = run_git(dir, args).await?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(git_failure(args, &output))
}

/// Run git in `dir` with no prompt and a time limit, whatever its exit status.
async fn run_git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    // Against the work tree's own repository, never one git discovers from
    // `dir`, which a user below admin may write (security review 2026-11, #1).
    let work_tree = sc_files::git_command::work_tree_of(dir)
        .ok_or_else(|| Error::invalid(format!("{} is not in a git work tree", dir.display())))?;
    let run = sc_files::git_command::command(&work_tree)
        .current_dir(dir)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(GIT_TIMEOUT, run)
        .await
        .map_err(|_| Error::invalid(format!("`git {}` timed out", args.join(" "))))?
        .map_err(|e| Error::invalid(format!("git could not be run: {e}")))
}

/// A git command that exited unsuccessfully, with git's own words.
fn git_failure(args: &[&str], output: &std::process::Output) -> Error {
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The sub-command, past any `-c key=value` settings.
    let mut words = args.iter().copied();
    let command = std::iter::from_fn(|| match words.next()? {
        "-c" => words.next().map(|_| ""),
        word => Some(word),
    })
    .find(|w| !w.is_empty())
    .unwrap_or_default();
    Error::invalid(format!(
        "`git {command}` failed: {} {}",
        stdout.trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_loses_its_fences_and_falls_back_when_empty() {
        assert_eq!(
            clean_message("```\nAdd the tasks page\n```\n", "x"),
            "Add the tasks page"
        );
        assert_eq!(clean_message("  \n", "Tasks page"), "Tasks page");
    }

    #[tokio::test]
    async fn staged_changes_are_asked_of_git_not_read_off_a_message() {
        let dir = std::env::temp_dir().join(format!("sc-commit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // `git` runs against an existing work tree only, so the repository is
        // made directly.
        let init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(init.success());
        let ident = [
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@localhost",
            "-c",
            NO_HOOKS,
        ];
        let a = dir.join("a.txt").to_string_lossy().into_owned();
        let paths = vec![a.clone()];

        // No commit yet and nothing staged; then a new file staged.
        assert!(!has_staged_changes(&dir, &paths).await.unwrap());
        std::fs::write(&a, "one").unwrap();
        git(&dir, &["add", "--all", "--", &a]).await.unwrap();
        assert!(has_staged_changes(&dir, &paths).await.unwrap());

        let mut commit = ident.to_vec();
        commit.extend(["commit", "--quiet", "-m", "first"]);
        git(&dir, &commit).await.unwrap();
        // Committed: nothing left. Rewritten to the same text: still nothing.
        assert!(!has_staged_changes(&dir, &paths).await.unwrap());
        std::fs::write(&a, "one").unwrap();
        git(&dir, &["add", "--all", "--", &a]).await.unwrap();
        assert!(!has_staged_changes(&dir, &paths).await.unwrap());
        // A deletion staged is a change.
        std::fs::remove_file(&a).unwrap();
        git(&dir, &["add", "--all", "--", &a]).await.unwrap();
        assert!(has_staged_changes(&dir, &paths).await.unwrap());

        // git failing (here: no such directory) is an error, not "no changes".
        let missing = dir.join("missing");
        assert!(has_staged_changes(&missing, &paths).await.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}

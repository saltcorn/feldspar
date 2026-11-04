//! The one way this system runs `git` over a working tree (security review
//! 2026-11, #1 / C1).
//!
//! Git finds its repository by walking up from the working directory, and at
//! each directory it accepts not only a `.git` but a *bare repository layout*
//! — a `HEAD` file, an `objects/` and a `refs/` directory, and a `config` —
//! which are ordinary files a user below admin can write into an app's source
//! directory. A planted `config` can name `core.fsmonitor`, a shell command
//! git runs on the next `git add` or `git status`. So git is never left to
//! discover anything here:
//!
//! - [`work_tree_of`] finds the work tree by looking for `.git` only (a
//!   directory, or a worktree's `.git` file — a path segment no store write
//!   may name), never for a bare layout;
//! - [`command`] / [`std_command`] name it explicitly: `GIT_DIR=<root>/.git`
//!   and `GIT_WORK_TREE=<root>`, and pass `-c safe.bareRepository=explicit
//!   -c core.fsmonitor=false -c core.hooksPath=/dev/null`, so neither a planted
//!   layout nor a hook or fsmonitor in the repository's own configuration runs.
//!
//! The commands start in the work tree; a caller that needs pathspecs relative
//! to a sub-directory sets `current_dir` to it (inside the work tree).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The settings every git run gets, before its sub-command.
pub const HARDENING_FLAGS: [&str; 6] = [
    "-c",
    "safe.bareRepository=explicit",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

/// The work tree `dir` is in: `dir` or its nearest ancestor that has a `.git`
/// (a directory, or the file a linked worktree or submodule has). `None` when
/// there is none — a bare repository layout does not count.
pub fn work_tree_of(dir: &Path) -> Option<PathBuf> {
    let dir = std::path::absolute(dir).ok()?;
    dir.ancestors()
        .find(|d| d.join(".git").symlink_metadata().is_ok())
        .map(Path::to_path_buf)
}

/// The environment and arguments [`command`] and [`std_command`] share.
fn harden<C>(
    cmd: &mut C,
    work_tree: &Path,
    env: fn(&mut C, &OsStr, &OsStr),
    arg: fn(&mut C, &OsStr),
) {
    let git_dir = work_tree.join(".git");
    env(cmd, "GIT_DIR".as_ref(), git_dir.as_os_str());
    env(cmd, "GIT_WORK_TREE".as_ref(), work_tree.as_os_str());
    // Nothing here should ever ask a question, and a server has nobody to
    // answer one.
    env(cmd, "GIT_TERMINAL_PROMPT".as_ref(), "0".as_ref());
    for flag in HARDENING_FLAGS {
        arg(cmd, flag.as_ref());
    }
}

/// `git` for the repository whose work tree is `work_tree` (from
/// [`work_tree_of`] or a store's configured root), hardened as the module docs
/// say, started in `work_tree` with no stdin. Sub-command and arguments follow.
pub fn command(work_tree: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    harden(
        &mut cmd,
        work_tree,
        |c, k, v| {
            c.env(k, v);
        },
        |c, a| {
            c.arg(a);
        },
    );
    cmd.current_dir(work_tree).stdin(Stdio::null());
    cmd
}

/// [`command`] as a blocking `std::process::Command`.
pub fn std_command(work_tree: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    harden(
        &mut cmd,
        work_tree,
        |c, k, v| {
            c.env(k, v);
        },
        |c, a| {
            c.arg(a);
        },
    );
    cmd.current_dir(work_tree).stdin(Stdio::null());
    cmd
}

/// `git` with the hardening settings but no repository, for a command that
/// makes one (`clone`, `init`) in `dir`.
pub fn command_without_repository(dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(HARDENING_FLAGS)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(dir)
        .stdin(Stdio::null());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sc-git-command-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A planted bare repository layout (HEAD, objects/, refs/, a config
    /// naming an fsmonitor command) in a sub-directory of a work tree is
    /// neither found as the repository nor run.
    #[test]
    fn a_planted_bare_layout_is_not_the_repository_and_its_fsmonitor_never_runs() {
        if !git_available() {
            eprintln!("skipping: git is not on the PATH");
            return;
        }
        let root = temp("planted");
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(init.success());
        let web = root.join("web");
        std::fs::create_dir_all(web.join("objects")).unwrap();
        std::fs::create_dir_all(web.join("refs")).unwrap();
        std::fs::write(web.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let marker = root.join("pwned");
        std::fs::write(
            web.join("config"),
            format!(
                "[core]\n\tbare = false\n\tworktree = .\n\tfsmonitor = \"touch {}\"\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::write(web.join("index.html"), "<p>hi</p>").unwrap();

        assert_eq!(work_tree_of(&web), Some(root.clone()));
        let mut cmd = std_command(&root);
        cmd.current_dir(&web).args(["add", "-A", "--", "."]);
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut status = std_command(&root);
        status.current_dir(&web).args(["status", "--porcelain"]);
        let out = status.output().unwrap();
        assert!(out.status.success());
        // The add went to the real repository at the root …
        let listed = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(listed.contains("web/index.html"), "{listed}");
        // … and the planted fsmonitor never ran.
        assert!(!marker.exists(), "the planted fsmonitor command ran");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Without a `.git` anywhere above, there is no work tree — a bare layout
    /// alone is not one.
    #[test]
    fn a_bare_layout_alone_is_no_work_tree() {
        let dir = temp("bare-only");
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::fs::create_dir_all(dir.join("refs")).unwrap();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        // The temp dir itself may sit inside some checkout; only check that
        // `dir` is not taken for one.
        assert_ne!(work_tree_of(&dir), Some(dir.clone()));
        std::fs::remove_dir_all(&dir).ok();
    }
}

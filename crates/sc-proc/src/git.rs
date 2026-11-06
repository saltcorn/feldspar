//! [`Git`]: `git` over a [`RepoRoot`], never over a repository git discovers
//! (security review 2026-11, #1 / C1).
//!
//! Git finds its repository by walking up from the working directory, and at
//! each directory it accepts not only a `.git` but a *bare repository layout*
//! — a `HEAD` file, an `objects/` and a `refs/` directory, and a `config` —
//! which are ordinary files a user below admin could write into an app's
//! source directory. A planted `config` can name `core.fsmonitor`, a shell
//! command git runs on the next `git add` or `git status`. So git is never
//! left to discover anything here:
//!
//! - a [`RepoRoot`] is found by looking for `.git` only (a directory, or a
//!   linked worktree's `.git` file — a path segment no store write below admin
//!   may name), never for a bare layout;
//! - [`Git::new`] names it: `GIT_DIR=<root>/.git` and `GIT_WORK_TREE=<root>`,
//!   with [`HARDENING_FLAGS`], so neither a planted layout nor a hook or
//!   fsmonitor in the repository's own configuration runs.

use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};

use crate::authority::SpawnAuthority;
use crate::command::{HostCommand, ProcError};
use crate::words::{EnvName, Timeout};

/// The settings every git run gets, before its sub-command: no bare
/// repository is discovered, no fsmonitor command or hook runs, and the `ext::`
/// transport (a URL that is a shell command) is refused.
pub const HARDENING_FLAGS: [&str; 8] = [
    "-c",
    "safe.bareRepository=explicit",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "protocol.ext.allow=never",
];

/// The top of a git work tree: a directory with a `.git` in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRoot(PathBuf);

impl RepoRoot {
    /// `dir`, when it has a `.git` of its own (a store's configured root).
    pub fn at(dir: &Path) -> Option<RepoRoot> {
        let dir = std::path::absolute(dir).ok()?;
        dir.join(".git")
            .symlink_metadata()
            .is_ok()
            .then_some(RepoRoot(dir))
    }

    /// The work tree `dir` is in: `dir` or its nearest ancestor with a `.git`.
    /// `None` when there is none — a bare repository layout does not count.
    pub fn enclosing(dir: &Path) -> Option<RepoRoot> {
        let dir = std::path::absolute(dir).ok()?;
        dir.ancestors()
            .find(|d| d.join(".git").symlink_metadata().is_ok())
            .map(|d| RepoRoot(d.to_path_buf()))
    }

    /// The work tree's directory.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// `git`, hardened as the module docs say, with its repository named. A
/// [`HostCommand`] underneath (through `Deref`), so sub-commands, flags and
/// operands are added the same way.
#[derive(Debug)]
pub struct Git(HostCommand);

const GIT_DIR: EnvName = EnvName::git("GIT_DIR");
const GIT_WORK_TREE: EnvName = EnvName::git("GIT_WORK_TREE");
const GIT_INDEX_FILE: EnvName = EnvName::git("GIT_INDEX_FILE");
const GIT_SSH_COMMAND: EnvName = EnvName::git("GIT_SSH_COMMAND");
const GIT_TERMINAL_PROMPT: EnvName = EnvName::git("GIT_TERMINAL_PROMPT");

impl Git {
    /// `git` against `repo`, started in its root (set [`cwd`](HostCommand::cwd)
    /// to a directory inside it for pathspecs relative to that).
    pub fn new(repo: &RepoRoot, who: &impl SpawnAuthority, timeout: Timeout) -> Git {
        let mut git = Git::bare(who, timeout);
        git.0
            .env(GIT_DIR, repo.0.join(".git"))
            .env(GIT_WORK_TREE, &repo.0)
            .cwd(&repo.0);
        git
    }

    /// `git` with no repository, for a command that makes one (`clone`,
    /// `init`), started in `dir`.
    pub fn without_repository(dir: &Path, who: &impl SpawnAuthority, timeout: Timeout) -> Git {
        let mut git = Git::bare(who, timeout);
        git.0.cwd(dir);
        git
    }

    fn bare(who: &impl SpawnAuthority, timeout: Timeout) -> Git {
        let mut cmd = HostCommand::new("git", who, timeout);
        // Nothing here should ever ask a question, and a server has nobody to
        // answer one.
        cmd.env(GIT_TERMINAL_PROMPT, "0").args(HARDENING_FLAGS);
        Git(cmd)
    }

    /// Use `index` instead of the repository's own index.
    pub fn index_file(&mut self, index: &Path) -> &mut Self {
        self.0.env(GIT_INDEX_FILE, index);
        self
    }

    /// Reach the remote over ssh with `key` and nothing else, asking nothing:
    /// `IdentitiesOnly`, `BatchMode`, and `StrictHostKeyChecking=accept-new`
    /// (a first clone from a new host is normal; a changed key is refused).
    pub fn ssh_key(&mut self, key: &Path) -> &mut Self {
        let quoted = format!("'{}'", key.to_string_lossy().replace('\'', r"'\''"));
        self.0.env(
            GIT_SSH_COMMAND,
            format!(
                "ssh -i {quoted} -o IdentitiesOnly=yes -o BatchMode=yes \
                 -o StrictHostKeyChecking=accept-new"
            ),
        );
        self
    }

    /// The command underneath.
    pub fn into_command(self) -> HostCommand {
        self.0
    }

    /// [`HostCommand::output`].
    pub async fn output(self) -> Result<Output, ProcError> {
        self.0.output().await
    }

    /// [`HostCommand::status`].
    pub async fn status(self) -> Result<ExitStatus, ProcError> {
        self.0.status().await
    }

    /// [`HostCommand::output_blocking`].
    pub fn output_blocking(self) -> Result<Output, ProcError> {
        self.0.output_blocking()
    }
}

impl Deref for Git {
    type Target = HostCommand;

    fn deref(&self) -> &HostCommand {
        &self.0
    }
}

impl DerefMut for Git {
    fn deref_mut(&mut self) -> &mut HostCommand {
        &mut self.0
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::authority::SystemTask;
    use crate::words::Operand;

    const TEST: SystemTask = SystemTask::new("sc-proc git test");

    fn temp(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("sc-proc-git-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_available() -> bool {
        let mut probe = HostCommand::new("git", &TEST, Timeout::secs(30));
        probe.arg("--version");
        probe.output_blocking().is_ok_and(|o| o.status.success())
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
        let mut init = Git::without_repository(&root, &TEST, Timeout::secs(30));
        init.args(["init", "-q"]);
        assert!(init.output_blocking().unwrap().status.success());
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

        let repo = RepoRoot::enclosing(&web).unwrap();
        assert_eq!(repo.path(), root.as_path());
        let mut add = Git::new(&repo, &TEST, Timeout::secs(30));
        add.cwd(&web)
            .args(["add", "-A"])
            .operand(Operand::path("."));
        let out = add.output_blocking().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut status = Git::new(&repo, &TEST, Timeout::secs(30));
        status.cwd(&web).args(["status", "--porcelain"]);
        let out = status.output_blocking().unwrap();
        assert!(out.status.success());
        let listed = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(listed.contains("web/index.html"), "{listed}");
        assert!(!marker.exists(), "the planted fsmonitor command ran");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_ssh_command_pins_one_key_and_never_prompts() {
        let mut git = Git::without_repository(Path::new("/"), &TEST, Timeout::secs(1));
        git.ssh_key(Path::new("/keys/my key.key"));
        let cmd = format!("{git:?}");
        assert!(cmd.contains("GIT_SSH_COMMAND"), "{cmd}");
        assert!(cmd.contains("'/keys/my key.key'"), "{cmd}");
        assert!(cmd.contains("IdentitiesOnly=yes"), "{cmd}");
        assert!(cmd.contains("BatchMode=yes"), "{cmd}");
        assert!(cmd.contains("StrictHostKeyChecking=accept-new"), "{cmd}");
    }

    #[test]
    fn a_bare_layout_alone_is_no_work_tree() {
        let dir = temp("bare-only");
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::fs::create_dir_all(dir.join("refs")).unwrap();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(RepoRoot::at(&dir).is_none());
        assert_ne!(
            RepoRoot::enclosing(&dir).map(|r| r.path().to_path_buf()),
            Some(dir.clone())
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

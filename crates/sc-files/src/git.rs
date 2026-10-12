//! Git-repository [`FileStore`] driver: a store whose contents are a **working
//! tree** cloned from a remote, with the repository operations an admin needs to
//! move changes in and out of it.
//!
//! ## What is new here, and what is not
//!
//! The bytes are on local disk, so everything [`LocalFileStore`] does — reading,
//! writing, listing, the traversal sandbox, xattr metadata — is exactly right
//! and is delegated to it unchanged. What a git store adds is *where the
//! directory comes from* and *what else can be done to it*:
//!
//! - **Where.** The admin supplies a URL, not a path, and the clone lives in an
//!   OS-appropriate data directory ([`data_dir`]) that Saltcorn owns, because a
//!   directory Saltcorn created by cloning is Saltcorn's to place — unlike a
//!   `local` store's directory, which is the admin's own and must be named by
//!   them. An admin who *does* have a place in mind names it in
//!   [`CFG_DIR`](crate::CFG_DIR) when creating the store, and only then: the
//!   setting says where the working tree was put, so changing it afterwards
//!   would abandon that tree rather than move it.
//!
//!   A directory that already holds a checkout is **adopted, not cloned over**
//!   (see [`GitRepo::ensure_cloned`]). That is what makes the URL optional: a
//!   repository someone has already cloned onto the server is connected by
//!   naming its directory and leaving the URL blank, and the remote — if it has
//!   one — is the one the checkout already points at.
//! - **What else.** [`GitRepo`] carries `pull`, `push`, `stage`, `unstage`,
//!   `discard`, `commit` and `status`. These are not [`FileStore`] methods and deliberately
//!   so: the
//!   trait is the contract every backend answers, and three quarters of its
//!   implementations will never have a remote to push to. A caller that wants
//!   git operations asks for a git repository.
//!
//! ## Authentication: a generated deploy key
//!
//! Cloning a private repository over SSH needs a key the remote will accept, and
//! the flow that works for a hosted repository is the *deploy key*: generate a
//! keypair here, hand the admin the public half to paste into the repository's
//! settings, keep the private half. So [`generate_deploy_key`] exists and is
//! called **before** the store is saved — the admin must be able to install the
//! key at the remote before anything tries to clone, or the first clone fails
//! for a reason that has nothing to do with their configuration.
//!
//! The private key is written to [`key_dir`] with `0600` permissions on Unix and
//! is never returned by the API; only the public half is, which is the half that
//! is meant to be copied around.
//!
//! ## Every git invocation is non-interactive
//!
//! A server has no terminal, so a git subprocess that asks a question does not
//! get an answer — it hangs, holding a request open until something times out.
//! Every invocation therefore runs with `GIT_TERMINAL_PROMPT=0`, and every
//! invocation using a deploy key runs ssh with `BatchMode=yes`, so a missing or
//! rejected credential is an immediate error carrying git's own message rather
//! than a stall.

use async_trait::async_trait;
use bytes::Bytes;
use sc_error::{Context, Error, Result};
use sc_types::{Attrs, BasicType, FormField, Operation, OperationScope};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::backend::OperationOutcome;
use crate::def::{
    ATTR_CLONE_PATH, CFG_BRANCH, CFG_DIR, CFG_KEY_PATH, CFG_PUBLIC_KEY, CFG_URL, FileStoreDef,
    GIT_BACKEND,
};
use crate::local::LocalFileStore;
use crate::paths::{data_dir, path_safe};
use crate::store::{Entry, FileMeta, FileStat, FileStore};

/// Where clones live: `<data dir>/git-stores`.
pub fn clone_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("git-stores"))
}

/// Where deploy keys live: `<data dir>/keys`.
pub fn key_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("keys"))
}

/// The directory a store's working tree lives in, in order of authority: its
/// recorded [`ATTR_CLONE_PATH`](crate::ATTR_CLONE_PATH), then the
/// [`CFG_DIR`](crate::CFG_DIR) the admin named when creating it, then
/// `<clone dir>/<name>`.
///
/// The recorded attribute wins over the setting because the two answer different
/// questions: the setting is where the admin *asked* for the checkout, the
/// attribute is where one *is*. They agree in every ordinary case — the
/// attribute is written from this function's own answer on the first clone — and
/// where they could not, following the attribute is what keeps an existing
/// working tree reachable.
///
/// The path is recorded on first clone rather than derived every time, so that
/// renaming a store does not orphan its working tree: a rename would otherwise
/// silently point the store at an empty directory and lose whatever was
/// uncommitted in the old one.
pub fn clone_path(def: &FileStoreDef) -> Result<PathBuf> {
    if let Some(recorded) = def
        .attributes
        .get(ATTR_CLONE_PATH)
        .and_then(serde_json::Value::as_str)
        && !recorded.trim().is_empty()
    {
        return Ok(PathBuf::from(recorded));
    }
    if let Some(dir) = configured_dir(def) {
        return Ok(dir);
    }
    Ok(clone_dir()?.join(path_safe(&def.name)))
}

/// The [`CFG_DIR`](crate::CFG_DIR) setting as a path, if the admin gave one.
///
/// Whitespace is not a directory, for the same reason whitespace is not a URL:
/// catching it here means the store falls back to the Saltcorn-owned location
/// rather than cloning into a directory named `" "`.
fn configured_dir(def: &FileStoreDef) -> Option<PathBuf> {
    def.setting(CFG_DIR)
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// Fill in the working-copy directory for a store that never named one — the
/// git half of [`display_config`](crate::display_config).
///
/// Leaving the admin's blank is what a store that let Saltcorn choose looks like
/// in the database, and it is the right thing to *store*: the directory is
/// derived, and writing the derived answer back into the settings would be a
/// second copy of it that could disagree with [`clone_path`]. But it is the
/// wrong thing to *show*. The setting is read-only on an edit, so an empty
/// control says "no working copy" where the truth is "one Saltcorn placed", and
/// the admin has nowhere else to learn where their files are.
///
/// So the answer is filled in on the way out and never on the way in — which is
/// also why a save cannot pin it: the form hands this value straight back, and
/// [`preserve_create_only`](sc_types::preserve_create_only) drops it again
/// because nothing is stored for it.
///
/// A definition that already names a directory is left alone, and so is one
/// whose path cannot be worked out — a daemon with no `HOME` and no
/// [`DATA_DIR_ENV`](crate::DATA_DIR_ENV). Showing nothing is right there: there is genuinely no
/// answer, and inventing one would name a directory nothing will use.
pub(crate) fn fill_display_config(def: &FileStoreDef, config: &mut Attrs) {
    if configured_dir(def).is_some() {
        return;
    }
    if let Ok(path) = clone_path(def) {
        config.insert(
            CFG_DIR.to_owned(),
            serde_json::Value::String(path.to_string_lossy().into_owned()),
        );
    }
}

/// The git backend's own save-time check, on top of the generic one every
/// backend's settings go through (see
/// [`validate_file_store_config`](crate::validate_file_store_config)).
///
/// There is exactly one thing to say that a per-field spec cannot: a git store
/// needs **either** a URL to clone from **or** a directory that already holds a
/// checkout. Neither is required on its own — that is the whole point of the
/// pairing — so the requirement is a relation between two settings, and
/// [`validate_attrs`](sc_types::validate_attrs) checks fields one at a time.
///
/// Structural, in the sense the backend module docs give the word: it asks
/// whether the admin has said enough to describe a store, not whether the
/// directory is there. A directory that was named but has no checkout in it yet
/// is a perfectly savable definition — cloning is what fills it, and reporting
/// its absence is [`GitRepo::ensure_cloned`]'s job.
pub(crate) fn validate_git_config(def: &FileStoreDef) -> Result<()> {
    let has_url = def
        .setting(CFG_URL)
        .map(str::trim)
        .is_some_and(|u| !u.is_empty());
    // A recorded clone path counts as a directory: the store has a working tree
    // and Saltcorn put it there, so a store created from a URL whose URL is
    // later cleared stays valid rather than becoming uneditable.
    let has_recorded_path = def
        .attributes
        .get(ATTR_CLONE_PATH)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|p| !p.trim().is_empty());
    if has_url || has_recorded_path || configured_dir(def).is_some() {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "git file store `{}` needs a `{CFG_URL}` to clone from, or a `{CFG_DIR}` that \
         already holds a checkout",
        def.name
    )))
}

/// A generated SSH deploy key: the private half's location, and the public half
/// itself.
///
/// The private key is a *path* and the public key is its *contents*, which is
/// the asymmetry the two halves deserve. The public half is meant to be copied
/// into a repository's settings, so it travels; the private half must not leave
/// the machine, so what travels is only where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployKey {
    /// Absolute path of the private key file.
    pub private_key_path: PathBuf,
    /// The public key, in `authorized_keys` one-line form — what is pasted into
    /// the repository's deploy-key settings.
    pub public_key: String,
}

/// Generate an ed25519 SSH keypair for the store named `name`, returning the
/// public half.
///
/// Runs `ssh-keygen`, rather than generating the key in-process. The key must be
/// in OpenSSH's own on-disk format for `ssh` to use it, and `ssh-keygen` is
/// where that format is defined and is already present on any machine that can
/// clone over SSH — a machine that cannot run it could not use the key it
/// produced. An absent `ssh-keygen` is reported as such.
///
/// ed25519 with no passphrase: a passphrase-protected key is unusable by a
/// non-interactive server (see the module docs), and ed25519 is what every
/// hosted forge accepts and the shortest key to paste.
///
/// **An existing key of the same name is replaced.** Generating a deploy key is
/// an explicit act, and the alternative — refusing, or silently reusing the old
/// key — would leave the admin looking at a public key the remote has already
/// rejected with no way to get a new one.
pub async fn generate_deploy_key(name: &str) -> Result<DeployKey> {
    let dir = key_dir()?;
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating deploy-key directory {}", dir.display()))?;

    let private = dir.join(format!("{}.key", path_safe(name)));
    let public = private.with_extension("key.pub");
    // `ssh-keygen` refuses to overwrite without asking, and asking is a hang
    // (module docs), so clear the way first.
    let _ = tokio::fs::remove_file(&private).await;
    let _ = tokio::fs::remove_file(&public).await;

    let output = Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-C")
        .arg(format!("saltcorn:{name}"))
        .arg("-f")
        .arg(&private)
        .output()
        .await
        .map_err(|e| {
            Error::config(format!(
                "could not run `ssh-keygen` to generate a deploy key ({e}); \
                 it is part of OpenSSH and must be installed to use the git backend"
            ))
        })?;
    if !output.status.success() {
        return Err(Error::invalid(format!(
            "`ssh-keygen` failed: {}",
            combined(&output.stdout, &output.stderr)
        )));
    }

    // 0600: an SSH private key readable by other users on the box is one `ssh`
    // will itself refuse to use, and rightly.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600))
            .await
            .with_context(|| format!("restricting permissions on {}", private.display()))?;
    }

    let public_key = tokio::fs::read_to_string(&public)
        .await
        .with_context(|| format!("reading generated public key {}", public.display()))?;

    Ok(DeployKey {
        private_key_path: private,
        public_key: public_key.trim().to_owned(),
    })
}

/// The outcome of one git invocation: whether it succeeded, and everything it
/// said.
///
/// stdout and stderr are kept together because git splits them along lines that
/// are of no use to a reader — `git push` reports success on stderr — and the
/// admin UI shows this verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    /// Whether git exited zero.
    pub success: bool,
    /// stdout and stderr, interleaved in that order and trimmed.
    pub output: String,
}

/// What a working tree currently is: which branch, what is uncommitted, and how
/// it stands against its upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatus {
    /// Whether the directory holds a clone at all. Everything below is empty
    /// when it does not.
    pub cloned: bool,
    /// The checked-out branch, or empty on a detached head or an empty
    /// repository.
    pub branch: String,
    /// One `git status --porcelain` line per changed path.
    pub changes: Vec<String>,
    /// Every branch that can be checked out: the local ones, plus the names of
    /// remote-tracking branches with no local counterpart — which `git checkout`
    /// creates a tracking branch for, so they are switchable in one step.
    pub branches: Vec<String>,
    /// The last commit, as `<short hash> <subject> (<author>, <when>)`; empty in
    /// a repository with no commits yet.
    pub last_commit: String,
    /// Commits the working tree has that its upstream does not.
    pub ahead: u32,
    /// Commits the upstream has that the working tree does not.
    pub behind: u32,
    /// Whether the checked-out branch has an upstream to push to and count
    /// against. A branch created here has none until its first push, which is
    /// what a source-control view offers to *publish* rather than push.
    pub upstream: bool,
    /// Whether cloning would put a working copy here: there is no clone, and the
    /// directory is missing or empty. A directory holding someone else's files
    /// is not one a clone may write into (see [`GitRepo::ensure_cloned`]), so a
    /// view offers the Clone button only when this is set.
    pub can_clone: bool,
}

impl GitStatus {
    /// Whether there is nothing to commit.
    pub fn clean(&self) -> bool {
        self.changes.is_empty()
    }
}

/// The outcome of a commit: whether one was actually made, and git's output.
///
/// "Nothing to commit" is **not** an error and is reported as `committed:
/// false`. Pressing Commit on an unchanged tree is a question ("is there
/// anything to save?"), and answering it with a failure would make the admin
/// hunt for a mistake they did not make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    /// Whether a commit was created.
    pub committed: bool,
    /// git's own output.
    pub output: String,
}

/// A git working tree Saltcorn manages: where it is, where it came from, and
/// which key reaches the remote.
///
/// Separate from [`GitFileStore`] because the operations are needed when there
/// is **no** connected store: a store that has not been cloned yet has nothing
/// to connect, and cloning it is exactly what fixes that. So this is built from
/// a [`FileStoreDef`] — inert data — and never requires an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepo {
    root: PathBuf,
    url: Option<String>,
    branch: Option<String>,
    key_path: Option<PathBuf>,
}

impl GitRepo {
    /// Build the repository a definition describes.
    ///
    /// Fails only on a definition that is not a git store, which
    /// [`validate_file_store_config`](crate::validate_file_store_config) rejects
    /// on save, so reaching it here means a caller went around the registry.
    ///
    /// A **missing URL is not a failure**: a store whose directory already holds
    /// a checkout has nothing to clone, and the remote it pushes to is the one
    /// that checkout already has. Cloning is where a URL becomes necessary, and
    /// [`ensure_cloned`](GitRepo::ensure_cloned) is where its absence is
    /// reported — with the directory that was looked in, which is the fact that
    /// makes the message actionable.
    pub fn from_def(def: &FileStoreDef) -> Result<GitRepo> {
        if def.backend != GIT_BACKEND {
            return Err(Error::invalid(format!(
                "file store `{}` is a `{}` store, not a git repository",
                def.name, def.backend
            )));
        }
        Ok(GitRepo {
            root: clone_path(def)?,
            url: def
                .setting(CFG_URL)
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_owned),
            branch: def
                .setting(CFG_BRANCH)
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(str::to_owned),
            key_path: def
                .setting(CFG_KEY_PATH)
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
        })
    }

    /// The working tree's directory — where the clone is or will be.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The remote URL, or `None` for a store that adopted a directory someone
    /// else cloned.
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// Whether the directory holds a clone. A `.git` *file* counts as well as a
    /// directory, since that is what a worktree or submodule link looks like.
    pub fn is_cloned(&self) -> bool {
        self.root.join(".git").exists()
    }

    /// Clone the remote into the working-tree directory, unless the directory
    /// **already holds a checkout** — in which case that checkout is adopted as
    /// it stands and this only says so.
    ///
    /// Idempotent on purpose: it runs on every save of a git store, and a save
    /// that re-cloned would throw away uncommitted work. Changing the URL of an
    /// already-cloned store therefore does *not* re-clone it; the admin who
    /// wants a different repository makes a different store, which is also the
    /// only reading under which their existing working tree is safe.
    ///
    /// Adoption is the same rule, reached from the other side, and it is what a
    /// store created against an existing checkout relies on: whether the working
    /// tree got there by this store's own first clone or by someone typing `git
    /// clone` on the server a year ago, a directory that is a repository is one
    /// nothing here should overwrite.
    ///
    /// Two ways to fail, both of them the right answer:
    ///
    /// - **No URL and no checkout.** There is nothing to clone from and nothing
    ///   to adopt. The message names the directory that was looked in, since the
    ///   fix is either to supply a URL or to point the store at the checkout the
    ///   admin meant.
    /// - **A directory that exists, is not a repository, and is not empty.**
    ///   git's own refusal, passed through: the contents are someone's, and they
    ///   were not put there by this store.
    pub async fn ensure_cloned(&self) -> Result<GitOutput> {
        if self.is_cloned() {
            return Ok(GitOutput {
                success: true,
                output: format!("using the working copy already in {}", self.root.display()),
            });
        }
        let Some(url) = &self.url else {
            return Err(Error::invalid(format!(
                "git file store has no `{CFG_URL}` to clone from, and {} is not a git \
                 working copy; give a repository URL, or point the store at a directory \
                 that already holds a checkout",
                self.root.display()
            )));
        };
        let parent = self.root.parent().unwrap_or(&self.root).to_owned();
        tokio::fs::create_dir_all(&parent)
            .await
            .with_context(|| format!("creating clone directory {}", parent.display()))?;

        let mut args: Vec<String> = vec!["clone".to_owned()];
        if let Some(branch) = &self.branch {
            args.push("--branch".to_owned());
            args.push(branch.clone());
        }
        args.push(url.clone());
        args.push(self.root.to_string_lossy().into_owned());

        // Run from the parent: the target does not exist yet, so it cannot be
        // the working directory.
        let out = self.run(&parent, &args).await?;
        if !out.success {
            return Err(Error::invalid(format!(
                "cloning {url} failed: {}",
                out.output
            )));
        }
        Ok(out)
    }

    /// `git pull` — fetch the remote and merge it into the working tree.
    ///
    /// `--no-edit` because a merge commit would otherwise open an editor, and an
    /// editor on a server is the hang the module docs describe.
    pub async fn pull(&self) -> Result<GitOutput> {
        self.checked(&["pull", "--no-edit"], "pull").await
    }

    /// `git push` the checked-out branch to `origin`.
    ///
    /// `--set-upstream` so that the first push of a branch created locally
    /// works rather than failing with "no upstream", which is a state the admin
    /// cannot fix from this UI.
    pub async fn push(&self) -> Result<GitOutput> {
        let branch = self.current_branch().await?;
        if branch.is_empty() {
            return Err(Error::invalid(
                "cannot push: the working tree is not on a branch",
            ));
        }
        self.checked(&["push", "--set-upstream", "origin", &branch], "push")
            .await
    }

    /// Add `paths` to the index, or **everything** when `paths` is empty.
    ///
    /// This is `git add`, including deletions (`git add` has staged a removed
    /// file since 2.0) and files created outside the editor, so one operation
    /// covers every row a source-control view can show a `+` against.
    ///
    /// A path is a **pathspec**, checked by [`pathspecs`] before it reaches git:
    /// what arrives is a client's idea of a file in this working tree, and a
    /// working tree is not the place to discover that `../../etc` was one.
    pub async fn stage(&self, paths: &[String]) -> Result<GitOutput> {
        let paths = pathspecs(paths)?;
        let mut args = vec!["add".to_owned()];
        if paths.is_empty() {
            args.push("--all".to_owned());
        }
        args.push("--".to_owned());
        args.extend(paths);
        self.checked_owned(&args, "add").await
    }

    /// Take `paths` out of the index, or **everything** when `paths` is empty,
    /// leaving the working tree alone.
    ///
    /// `git reset -q` rather than `git restore --staged`, for one reason worth
    /// writing down: in a repository with no commits there is no `HEAD` for
    /// `restore` to resolve and it fails, while `reset` empties the index entry
    /// and the first file ever added becomes untracked again — which is what
    /// pressing `−` on it means.
    pub async fn unstage(&self, paths: &[String]) -> Result<GitOutput> {
        let paths = pathspecs(paths)?;
        let mut args = vec!["reset".to_owned(), "-q".to_owned(), "--".to_owned()];
        args.extend(paths);
        self.checked_owned(&args, "reset").await
    }

    /// Throw away the **unstaged** changes to `paths`: an edited or deleted file
    /// goes back to what the index holds (its staged or committed contents), and
    /// an untracked file is deleted. What is staged is left alone — that is
    /// VS Code's Discard, which sits on the Changes group only.
    ///
    /// Irreversible, so narrower than its siblings in three ways:
    ///
    /// - `paths` is **required**. An empty list is refused rather than read as
    ///   *everything*; a client discarding every change names every change.
    /// - Each path must be one `status` currently lists with an unstaged change.
    ///   A path that is clean, only staged, or conflicted is refused by name —
    ///   a conflict is resolved by staging the chosen version, not discarded.
    /// - The pathspecs are **literal** (`--literal-pathspecs`), so a file named
    ///   `*.md` discards that file and not every Markdown file in the tree.
    pub async fn discard(&self, paths: &[String]) -> Result<GitOutput> {
        let paths = pathspecs(paths)?;
        if paths.is_empty() {
            return Err(Error::invalid("name the paths whose changes to discard"));
        }
        self.require_cloned()?;
        let status = self.status().await?;
        let changes: Vec<GitChange> = status
            .changes
            .iter()
            .filter_map(|line| parse_change(line))
            .collect();

        let mut tracked = Vec::new();
        let mut untracked = Vec::new();
        for path in &paths {
            let Some(change) = changes.iter().find(|c| &c.path == path) else {
                return Err(Error::invalid(format!("{path} has no changes to discard")));
            };
            let mut code = change.status.chars();
            let index = code.next().unwrap_or(' ');
            let worktree = code.next().unwrap_or(' ');
            if change.status == "??" {
                untracked.push(path.clone());
            } else if index == 'U'
                || worktree == 'U'
                || matches!(change.status.as_str(), "AA" | "DD")
            {
                return Err(Error::invalid(format!(
                    "{path} has a merge conflict; resolve it and stage the result instead"
                )));
            } else if worktree == ' ' {
                return Err(Error::invalid(format!(
                    "{path} has only staged changes; unstage them first"
                )));
            } else {
                tracked.push(path.clone());
            }
        }

        let mut said = Vec::new();
        for (files, command) in [(&tracked, "checkout"), (&untracked, "clean")] {
            if files.is_empty() {
                continue;
            }
            let mut args = vec!["--literal-pathspecs".to_owned(), command.to_owned()];
            args.push(if command == "clean" { "-fq" } else { "-q" }.to_owned());
            args.push("--".to_owned());
            args.extend(files.iter().cloned());
            let out = self.checked_owned(&args, command).await?;
            if !out.output.is_empty() {
                said.push(out.output);
            }
        }
        Ok(GitOutput {
            success: true,
            output: said.join("\n"),
        })
    }

    /// Commit with `message`: the whole working tree when `stage_all`, and
    /// otherwise **only what is in the index**.
    ///
    /// Both are real. A Source Control view (the IDE's, or the admin screen's
    /// panel) with something staged commits only that — sweeping up the files
    /// the admin deliberately left unstaged would make staging a lie — and with
    /// nothing staged commits everything, VS Code's "smart commit".
    ///
    /// An identity is supplied for the commit only when the repository has none
    /// configured, so an admin's own `user.name`/`user.email` wins where they
    /// have set one and a bare machine can still commit where they have not.
    pub async fn commit(&self, message: &str, stage_all: bool) -> Result<CommitOutcome> {
        let message = message.trim();
        if message.is_empty() {
            return Err(Error::invalid("a commit needs a message"));
        }
        self.require_cloned()?;

        if stage_all {
            let staged = self.run(&self.root, &["add", "-A"]).await?;
            if !staged.success {
                return Err(Error::invalid(format!(
                    "staging changes failed: {}",
                    staged.output
                )));
            }
        }

        let mut args: Vec<String> = Vec::new();
        if !self.has_identity().await? {
            args.extend(
                [
                    "-c",
                    "user.name=Saltcorn",
                    "-c",
                    "user.email=saltcorn@localhost",
                ]
                .map(str::to_owned),
            );
        }
        args.extend(["commit".to_owned(), "-m".to_owned(), message.to_owned()]);

        let out = self.run(&self.root, &args).await?;
        if out.success {
            return Ok(CommitOutcome {
                committed: true,
                output: out.output,
            });
        }
        // A commit with nothing staged exits non-zero and says so. That is a
        // no-op, not a failure — see `CommitOutcome`.
        if out.output.contains("nothing to commit")
            || out.output.contains("no changes added to commit")
        {
            return Ok(CommitOutcome {
                committed: false,
                output: out.output,
            });
        }
        Err(Error::invalid(format!("commit failed: {}", out.output)))
    }

    /// What the working tree currently is (see [`GitStatus`]).
    ///
    /// Every sub-command here is allowed to fail: an empty repository has no
    /// `HEAD` to name, a branch with no upstream has nothing to count against,
    /// and neither is an error — they are states this reports rather than
    /// refuses. Only a directory that is not a clone at all short-circuits.
    pub async fn status(&self) -> Result<GitStatus> {
        if !self.is_cloned() {
            return Ok(GitStatus {
                can_clone: self.root_is_empty(),
                ..GitStatus::default()
            });
        }
        let branch = self.current_branch().await?;
        let changes = self
            // Two flags, both so that a change can be *addressed* rather than
            // only counted. `core.quotepath=false` keeps a non-ASCII path as
            // itself instead of C-style `\303\251` escapes; `--untracked-files=all`
            // names each new file rather than collapsing a new directory to
            // `notes/`, which is not a path anything can open.
            .run(
                &self.root,
                &[
                    "-c",
                    "core.quotepath=false",
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                ],
            )
            .await?
            .output
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect();
        let last_commit = self
            .run(&self.root, &["log", "-1", "--pretty=%h %s (%an, %ar)"])
            .await?;
        let (ahead, behind) = self.tracking().await?;
        let upstream = self
            .run(
                &self.root,
                &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
            )
            .await?
            .success;
        let branches = self.branches().await?;
        Ok(GitStatus {
            cloned: true,
            branch,
            changes,
            branches,
            last_commit: if last_commit.success {
                last_commit.output
            } else {
                String::new()
            },
            ahead,
            behind,
            upstream,
            can_clone: false,
        })
    }

    /// Whether the working-tree directory is missing or has nothing in it.
    fn root_is_empty(&self) -> bool {
        match std::fs::read_dir(&self.root) {
            Ok(mut entries) => entries.next().is_none(),
            Err(_) => !self.root.exists(),
        }
    }

    /// The checked-out branch, or empty when there is none (detached head, or a
    /// repository with no commits).
    async fn current_branch(&self) -> Result<String> {
        self.require_cloned()?;
        let out = self
            .run(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await?;
        if !out.success || out.output == "HEAD" {
            return Ok(String::new());
        }
        Ok(out.output)
    }

    /// How far the branch is ahead of and behind its upstream, or `(0, 0)` when
    /// it has no upstream to compare against.
    async fn tracking(&self) -> Result<(u32, u32)> {
        let out = self
            .run(
                &self.root,
                &["rev-list", "--left-right", "--count", "HEAD...@{u}"],
            )
            .await?;
        if !out.success {
            return Ok((0, 0));
        }
        let mut parts = out.output.split_whitespace();
        let ahead = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let behind = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        Ok((ahead, behind))
    }

    /// Every branch that can be checked out (see [`GitStatus::branches`]), in
    /// the order git lists them: local branches first, then remote-only names.
    ///
    /// A remote-tracking branch is offered under its **short** name (`origin/`
    /// stripped) because that is the name `git checkout` wants: given one that
    /// matches exactly one remote, git creates the local tracking branch itself.
    /// `HEAD` is dropped — `origin/HEAD` is a symbolic ref, not a branch someone
    /// means to switch to.
    ///
    /// A repository with no commits has no branches to list and says so with an
    /// empty list rather than an error, exactly as [`status`](GitRepo::status)
    /// does for the other things such a repository does not have.
    pub async fn branches(&self) -> Result<Vec<String>> {
        self.require_cloned()?;
        let local = self
            .run(&self.root, &["branch", "--format=%(refname:short)"])
            .await?;
        let mut names: Vec<String> = if local.success {
            local
                .output
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        } else {
            Vec::new()
        };

        let remote = self
            .run(
                &self.root,
                &["branch", "--remotes", "--format=%(refname:short)"],
            )
            .await?;
        if remote.success {
            for line in remote.output.lines() {
                let full = line.trim();
                // `origin/main` → `main`; anything without a remote prefix is
                // not a name checkout would understand and is left alone.
                let Some((_, short)) = full.split_once('/') else {
                    continue;
                };
                if short.is_empty() || short == "HEAD" || full.contains(" -> ") {
                    continue;
                }
                if !names.iter().any(|known| known == short) {
                    names.push(short.to_owned());
                }
            }
        }
        Ok(names)
    }

    /// Switch to `branch`, creating it from the current head when `create`.
    ///
    /// Plainly `git checkout`: **no `--force`, no stash**. A switch that would
    /// discard uncommitted work fails with git's own refusal, which names the
    /// files in the way and is the answer an admin needs — the alternative is an
    /// editor that silently eats the change someone just made.
    pub async fn checkout(&self, branch: &str, create: bool) -> Result<GitOutput> {
        let branch = branch.trim();
        if branch.is_empty() {
            return Err(Error::invalid("a checkout needs a branch name"));
        }
        let mut args = vec!["checkout"];
        if create {
            args.push("-b");
        }
        args.push(branch);
        self.checked(&args, "checkout").await
    }

    /// Whether the repository resolves a committer identity (from its own
    /// config, the user's, or the environment).
    async fn has_identity(&self) -> Result<bool> {
        Ok(self
            .run(&self.root, &["config", "--get", "user.email"])
            .await?
            .success)
    }

    /// Run a sub-command in the working tree and turn a non-zero exit into an
    /// error naming the operation.
    async fn checked(&self, args: &[&str], what: &str) -> Result<GitOutput> {
        self.require_cloned()?;
        let out = self.run(&self.root, args).await?;
        if !out.success {
            return Err(Error::invalid(format!("git {what} failed: {}", out.output)));
        }
        Ok(out)
    }

    /// [`checked`](GitRepo::checked) for an argument list built at runtime — a
    /// pathspec list is as many arguments as the admin selected files.
    async fn checked_owned(&self, args: &[String], what: &str) -> Result<GitOutput> {
        self.require_cloned()?;
        let out = self.run(&self.root, args).await?;
        if !out.success {
            return Err(Error::invalid(format!("git {what} failed: {}", out.output)));
        }
        Ok(out)
    }

    /// The error for an operation on a store that has never been cloned, which
    /// says what to do about it rather than letting git report a directory that
    /// is not a repository.
    fn require_cloned(&self) -> Result<()> {
        if self.is_cloned() {
            return Ok(());
        }
        Err(Error::invalid(format!(
            "{} is not a git clone yet; clone the repository first",
            self.root.display()
        )))
    }

    /// Run `git` with this repository's credentials, non-interactively.
    async fn run<S: AsRef<std::ffi::OsStr>>(&self, dir: &Path, args: &[S]) -> Result<GitOutput> {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir).args(args);
        // No terminal, so no question git asks can be answered (module docs).
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        if let Some(key) = &self.key_path {
            cmd.env("GIT_SSH_COMMAND", ssh_command(key));
        }
        let output = cmd.output().await.map_err(|e| {
            Error::config(format!(
                "could not run `git` ({e}); it must be installed to use the git backend"
            ))
        })?;
        Ok(GitOutput {
            success: output.status.success(),
            output: combined(&output.stdout, &output.stderr),
        })
    }
}

/// Check client-supplied paths before they become git pathspecs, dropping the
/// blank ones.
///
/// Four rejections, each for something a pathspec can do that a path in a
/// working tree cannot. An **absolute** path (including a `C:\` drive) and a
/// `..` **segment** both name somewhere else — git would usually refuse them
/// itself ("is outside repository"), but a store whose working tree contains a
/// symlinked directory is exactly the case where it would not, and the traversal
/// rule here is the same one [`LocalFileStore`] applies to every read. A
/// **leading `-`** would be read as an option: `--` is passed before the list for
/// that reason, and this rejects it anyway, because a defence that only works
/// when the caller remembers the separator is not one. A **leading `:`** is
/// git's pathspec magic (`:(exclude)`, `:/`), which is a small query language
/// where a file name is wanted.
///
/// Blank entries are dropped rather than refused: the list arrives as lines of
/// text (see [`ARG_PATHS`]), and a trailing newline is not an error. An all-blank
/// list therefore means *everything*, which is what each caller of this already
/// means by an empty one.
fn pathspecs(paths: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        if path.starts_with('/') || path.starts_with('\\') || drive_prefixed(path) {
            return Err(Error::invalid(format!(
                "{path:?} is not a path inside the working copy"
            )));
        }
        if path.split(['/', '\\']).any(|segment| segment == "..") {
            return Err(Error::invalid(format!(
                "path {path:?} escapes the file store root"
            )));
        }
        if path.starts_with('-') {
            return Err(Error::invalid(format!(
                "{path:?} would be read as a git option, not a path"
            )));
        }
        if path.starts_with(':') {
            return Err(Error::invalid(format!(
                "{path:?} would be read as a git pathspec pattern, not a path"
            )));
        }
        out.push(path.to_owned());
    }
    Ok(out)
}

/// Whether a path begins with a Windows drive letter, which makes it absolute
/// there and is not something a store-relative path ever is.
fn drive_prefixed(path: &str) -> bool {
    let mut chars = path.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic()
    )
}

/// Drop the blank lines at either end of some git output, and **only** those.
///
/// A plain `trim` is what this used to be, and it was wrong in one specific
/// place: `git status --porcelain` writes the index column first, so a file that
/// is modified but unstaged is ` M README.md` — and trimming the whole output
/// eats that leading space on the *first* line only, turning it into `M ` and
/// telling a reader the file is staged. A code that means the opposite of the
/// truth is worse than no code, and it was invisible while there was one group
/// to put every change in.
fn trim_blank_lines(text: &str) -> &str {
    text.trim_start_matches(['\n', '\r']).trim_end()
}

/// The `GIT_SSH_COMMAND` that makes git use one specific key and ask nothing.
///
/// - `IdentitiesOnly=yes` — otherwise ssh offers every key an agent holds
///   before the one we named, and a forge that accepts one of those would use
///   the wrong identity while a forge that rejects them may hit its auth-attempt
///   limit before reaching ours.
/// - `BatchMode=yes` — never prompt (module docs).
/// - `StrictHostKeyChecking=accept-new` — a first clone from a host that is not
///   in `known_hosts` is the normal case here, and with `BatchMode` the strict
///   default would fail it; `accept-new` still refuses a host whose key has
///   *changed*, which is the case that matters.
fn ssh_command(key: &Path) -> String {
    format!(
        "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
        shell_quote(&key.to_string_lossy())
    )
}

/// Single-quote a path for the shell-like splitting git applies to
/// `GIT_SSH_COMMAND`, so a key path containing a space still names one argument.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// stdout then stderr, lossily decoded and trimmed.
fn combined(stdout: &[u8], stderr: &[u8]) -> String {
    let out = String::from_utf8_lossy(stdout);
    let err = String::from_utf8_lossy(stderr);
    let mut joined = String::new();
    for part in [trim_blank_lines(&out), trim_blank_lines(&err)] {
        if part.is_empty() {
            continue;
        }
        if !joined.is_empty() {
            joined.push('\n');
        }
        joined.push_str(part);
    }
    joined
}

/// A [`FileStore`] whose directory is a git working tree.
///
/// Every byte-level operation is [`LocalFileStore`]'s — the files are on local
/// disk and the traversal sandbox, the xattr metadata and the listing behaviour
/// should be identical, not merely similar. What this adds is the [`GitRepo`]
/// behind it and an unconditional [`is_git_repo`](FileStore::is_git_repo), so a
/// caller that needs to know (an app scaffold deciding whether to `git init`,
/// design §13.3) gets the right answer without inspecting the disk.
#[derive(Debug, Clone)]
pub struct GitFileStore {
    inner: LocalFileStore,
    repo: GitRepo,
}

impl GitFileStore {
    /// Connect a git store named `name` over an existing clone.
    ///
    /// The directory must already be a clone: connecting is not the place to
    /// reach the network. [`GitRepo::ensure_cloned`] is, and the server calls it
    /// when the admin saves the store.
    pub fn new(name: impl Into<String>, repo: GitRepo) -> Result<GitFileStore> {
        let name = name.into();
        if !repo.is_cloned() {
            return Err(Error::not_found(format!(
                "git file store `{name}` has not been cloned into {} yet",
                repo.root().display()
            )));
        }
        let inner = LocalFileStore::new(name, repo.root())?;
        Ok(GitFileStore { inner, repo })
    }

    /// The working tree behind the store, for the repository operations.
    pub fn repo(&self) -> &GitRepo {
        &self.repo
    }
}

#[async_trait]
impl FileStore for GitFileStore {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn read(&self, path: &str) -> Result<Bytes> {
        self.inner.read(path).await
    }

    async fn write(&self, path: &str, data: Bytes) -> Result<()> {
        self.inner.write(path, data).await
    }

    async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        self.inner.list(dir).await
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        self.inner.mkdir(path).await
    }

    async fn delete(&self, path: &str) -> Result<bool> {
        self.inner.delete(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.inner.rename(from, to).await
    }

    /// Always true, by construction — this store *is* a clone.
    fn is_git_repo(&self) -> bool {
        true
    }

    fn local_path(&self, rel: &str) -> Result<Option<PathBuf>> {
        self.inner.local_path(rel)
    }

    async fn stat(&self, path: &str) -> Result<Option<FileStat>> {
        self.inner.stat(path).await
    }

    async fn open_read(
        &self,
        path: &str,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<crate::BoxAsyncRead> {
        self.inner.open_read(path, range).await
    }

    async fn open_append<'a>(&'a self, path: &str) -> Result<crate::BoxAsyncWrite<'a>> {
        self.inner.open_append(path).await
    }

    async fn concat(&self, parts: &[String], to: &str) -> Result<u64> {
        self.inner.concat(parts, to).await
    }

    async fn get_meta(&self, path: &str) -> Result<FileMeta> {
        self.inner.get_meta(path).await
    }

    async fn set_meta(&self, path: &str, meta: &FileMeta) -> Result<()> {
        self.inner.set_meta(path, meta).await
    }
}

/// The name of the operation that generates a deploy key — the one that runs
/// *before* the store is saved.
pub const OP_GENERATE_KEY: &str = "generate_deploy_key";
/// The name of the operation reporting the working tree's state.
pub const OP_STATUS: &str = "status";
/// The name of the operation that clones the remote.
pub const OP_CLONE: &str = "clone";
/// The name of the operation that fetches and merges the remote.
pub const OP_PULL: &str = "pull";
/// The name of the operation that pushes the branch.
pub const OP_PUSH: &str = "push";
/// The name of the operation that adds paths to the index.
pub const OP_STAGE: &str = "stage";
/// The name of the operation that takes paths back out of the index.
pub const OP_UNSTAGE: &str = "unstage";
/// The name of the operation that throws away unstaged changes to paths.
pub const OP_DISCARD: &str = "discard";
/// The `paths` argument of [`OP_STAGE`], [`OP_UNSTAGE`] and [`OP_DISCARD`]: one
/// path per line, relative to the store root. Empty means *everything* to the
/// first two; [`OP_DISCARD`] requires it, because an empty box there would be a
/// destructive default.
///
/// Lines of text rather than a JSON array because an operation's arguments are
/// [`FormField`]s and the admin screen renders one control per field (§6.2): a
/// textarea of paths is something an admin can fill in, and a JSON array is
/// something they would have to hand-write brackets for. The IDE joins the
/// files it selected with newlines and is no worse off.
pub const ARG_PATHS: &str = "paths";
/// The name of the operation that commits.
pub const OP_COMMIT: &str = "commit";
/// The `message` argument of [`OP_COMMIT`].
pub const ARG_MESSAGE: &str = "message";
/// The `staged_only` argument of [`OP_COMMIT`]: commit what is in the index
/// rather than staging the working copy first.
///
/// Absent means *no*: commit everything. The Source Control views send `true`
/// whenever something is staged.
pub const ARG_STAGED_ONLY: &str = "staged_only";
/// The name of the operation that switches branch.
pub const OP_CHECKOUT: &str = "checkout";
/// The `branch` argument of [`OP_CHECKOUT`].
pub const ARG_BRANCH: &str = "branch";
/// The `create` argument of [`OP_CHECKOUT`]: make the branch rather than
/// expecting it to exist.
pub const ARG_CREATE: &str = "create";

/// What the git backend offers beyond its settings (§6.2's [`Operation`]).
///
/// Declared as data for the same reason the settings are: the admin UI renders a
/// button per operation and a form per argument, and knows nothing about git. A
/// backend supplied by a plugin declares its own the same way and gets the same
/// buttons.
///
/// Note the scopes, which are the substance rather than bookkeeping. Generating
/// a deploy key is [`Configure`](OperationScope::Configure) — it must run before
/// the store is saved, because saving clones and cloning needs a key the remote
/// already accepts. Everything else is
/// [`Instance`](OperationScope::Instance): there is no working tree to pull
/// until there is a saved store.
pub fn git_operations() -> Vec<Operation> {
    vec![
        Operation::new(OP_GENERATE_KEY, OperationScope::Configure)
            .label("Generate deploy key")
            .description(
                "Creates an SSH key for this repository and fills in the settings below. \
                 Add the public key to the repository's deploy keys — with write access if \
                 you want to push — before saving, because saving clones the repository.",
            ),
        Operation::new(OP_STATUS, OperationScope::Instance)
            .label("Refresh status")
            .description("What the working copy looks like right now.")
            // The one an admin needs in front of them before deciding what to
            // do, so it runs on opening the store rather than on a press.
            .automatic(),
        Operation::new(OP_CLONE, OperationScope::Instance)
            .label("Clone")
            .description(
                "Clones the repository if it has not been cloned yet. \
                 An existing working copy is left untouched.",
            )
            // The clone *is* the creation of a git store: until it succeeds
            // there is no working copy, and therefore no store. Saying so is
            // what makes creating one transactional — a failed clone leaves no
            // row behind for the admin's corrected second attempt to collide
            // with. It remains a button too, since a working copy can be lost
            // (deleted, or a database restored onto a fresh machine) and
            // re-running it is the repair.
            .on_create(),
        Operation::new(OP_PULL, OperationScope::Instance)
            .label("Pull")
            .description("Fetches the remote and merges it into the working copy."),
        Operation::new(OP_PUSH, OperationScope::Instance)
            .label("Push")
            .description("Sends committed changes to the remote."),
        Operation::new(OP_STAGE, OperationScope::Instance)
            .label("Stage changes")
            .description(
                "Adds changes to the index, ready to be committed. \
                 Leave the paths empty to stage everything.",
            )
            .input([FormField::new(ARG_PATHS, BasicType::Text)
                .label("Paths, one per line")
                .multiline()]),
        Operation::new(OP_UNSTAGE, OperationScope::Instance)
            .label("Unstage changes")
            .description(
                "Takes changes back out of the index, leaving the files themselves alone. \
                 Leave the paths empty to unstage everything.",
            )
            .input([FormField::new(ARG_PATHS, BasicType::Text)
                .label("Paths, one per line")
                .multiline()]),
        Operation::new(OP_DISCARD, OperationScope::Instance)
            .label("Discard changes")
            .description(
                "Throws away unstaged changes: edited files go back to their staged or \
                 committed contents and untracked files are deleted. This cannot be undone.",
            )
            .input([FormField::new(ARG_PATHS, BasicType::Text)
                .label("Paths, one per line")
                .multiline()
                .required()]),
        Operation::new(OP_COMMIT, OperationScope::Instance)
            .label("Commit all changes")
            .description(
                "Stages everything in the working copy and commits it — or, with \
                 `staged_only`, commits just what has been staged.",
            )
            .input([
                FormField::new(ARG_MESSAGE, BasicType::Text)
                    .label("Commit message")
                    .required(),
                FormField::new(ARG_STAGED_ONLY, BasicType::Bool)
                    .label("Commit only what is staged"),
            ]),
        Operation::new(OP_CHECKOUT, OperationScope::Instance)
            .label("Switch branch")
            .description(
                "Checks out another branch. Uncommitted changes that the switch would \
                 overwrite stop it — commit or pull them first.",
            )
            .input([
                FormField::new(ARG_BRANCH, BasicType::Text)
                    .label("Branch")
                    .required(),
                FormField::new(ARG_CREATE, BasicType::Bool)
                    .label("Create the branch from the current one"),
            ]),
    ]
}

/// Run one of [`git_operations`] against `def`, which may be an unsaved
/// definition the admin is still editing.
///
/// `def` is mutable because two of these configure as well as act: the key
/// generator writes the key path and public key into the settings, and a clone
/// records where it cloned to. The caller persists what changed.
///
/// Called through [`run_backend_operation`](crate::run_backend_operation), which
/// has already checked the operation exists and that its arguments match what it
/// declared — so a missing commit message never reaches here.
pub(crate) async fn run_git_operation(
    def: &mut FileStoreDef,
    operation: &str,
    input: &Attrs,
) -> Result<OperationOutcome> {
    if operation == OP_GENERATE_KEY {
        // The one operation that must work with no repository and no clone: it
        // is what makes reaching the repository possible in the first place.
        let key = generate_deploy_key(&def.name).await?;
        record_deploy_key(def, &key);
        return Ok(OperationOutcome::text(format!(
            "Generated a deploy key. Add this to the repository's deploy keys:\n\n{}",
            key.public_key
        )));
    }

    let repo = GitRepo::from_def(def)?;
    let text = match operation {
        OP_STATUS => describe(&repo.status().await?, &repo),
        OP_CLONE => {
            // Recorded before the clone runs, so the location survives a later
            // rename — which would otherwise re-derive a fresh directory and
            // abandon this working copy.
            record_clone_path(def, repo.root());
            let out = repo.ensure_cloned().await?;
            format!(
                "{}\n\n{}",
                out.output,
                describe(&repo.status().await?, &repo)
            )
        }
        OP_PULL => repo.pull().await?.output,
        OP_PUSH => repo.push().await?.output,
        OP_DISCARD => {
            let paths = lines(input.get(ARG_PATHS));
            let out = repo.discard(&paths).await?;
            if out.output.trim().is_empty() {
                match paths.len() {
                    1 => format!("Discarded the changes to {}.", paths[0]),
                    n => format!("Discarded the changes to {n} paths."),
                }
            } else {
                out.output
            }
        }
        OP_STAGE | OP_UNSTAGE => {
            let paths = lines(input.get(ARG_PATHS));
            let out = if operation == OP_STAGE {
                repo.stage(&paths).await?
            } else {
                repo.unstage(&paths).await?
            };
            // `git add` and `git reset` say nothing when they work, and silence
            // is the one answer an operation whose whole output is git's cannot
            // pass on. The status rides back alongside regardless (below), so
            // this line is for the admin screen that shows only `output`.
            if out.output.trim().is_empty() {
                let what = if operation == OP_STAGE {
                    "Staged"
                } else {
                    "Unstaged"
                };
                match paths.len() {
                    0 => format!("{what} every change."),
                    1 => format!("{what} {}.", paths[0]),
                    n => format!("{what} {n} paths."),
                }
            } else {
                out.output
            }
        }
        OP_CHECKOUT => {
            let branch = input
                .get(ARG_BRANCH)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let create = input
                .get(ARG_CREATE)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            repo.checkout(branch, create).await?.output
        }
        OP_COMMIT => {
            let message = input
                .get(ARG_MESSAGE)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let staged_only = input
                .get(ARG_STAGED_ONLY)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let outcome = repo.commit(message, !staged_only).await?;
            if outcome.committed {
                outcome.output
            } else {
                // Not a failure — see `CommitOutcome`. Said plainly, because
                // git's own "nothing to commit, working tree clean" arrives
                // through an error path and would read as one.
                if staged_only {
                    "Nothing to commit — nothing is staged.".to_owned()
                } else {
                    "Nothing to commit — the working copy has no changes.".to_owned()
                }
            }
        }
        other => return Err(Error::invalid(format!("unknown git operation `{other}`"))),
    };

    // Every one of these leaves the working tree in a state a source-control
    // view wants to redraw from, and the operation has just proved the
    // repository is reachable — so the state rides back with the outcome rather
    // than costing the caller a second request. Best-effort: the operation
    // succeeded, and a status that will not compute is not a reason to report it
    // as having failed.
    let outcome = OperationOutcome::text(text);
    Ok(match repo.status().await {
        Ok(status) => outcome.with_data(status_payload(&status)),
        Err(_) => outcome,
    })
}

/// The [`ARG_PATHS`] argument as the list of paths it stands for.
///
/// Anything that is not text is no paths at all, which means *everything* to both
/// operations that take it — the same answer a missing argument gives, and the
/// only one available, since the alternative is to refuse a request whose
/// declared validation has already passed.
fn lines(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(serde_json::Value::as_str)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A [`GitStatus`] as the data an SCM view can draw: the same facts as
/// [`describe`]'s prose, plus the porcelain lines split into a status code and a
/// path, because a view has to address each changed file by name.
///
/// The **shape is git's**, and deliberately so: this is the payload of the git
/// backend's operations, not a vocabulary every backend has to fit into. A
/// backend with a different idea of "changed" describes it its own way, and a
/// client that does not recognise the shape has `output` — which is why `data`
/// is optional and why nothing but the IDE's git view reads it.
fn status_payload(status: &GitStatus) -> serde_json::Value {
    serde_json::json!({
        "cloned": status.cloned,
        "branch": status.branch,
        "branches": status.branches,
        "ahead": status.ahead,
        "behind": status.behind,
        "upstream": status.upstream,
        "can_clone": status.can_clone,
        "last_commit": status.last_commit,
        "changes": status
            .changes
            .iter()
            .filter_map(|line| parse_change(line))
            .map(|change| serde_json::json!({ "status": change.status, "path": change.path }))
            .collect::<Vec<_>>(),
    })
}

/// One changed path, as `git status --porcelain` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitChange {
    /// The two-character `XY` code: index status, then working-tree status —
    /// `??` for untracked, ` M` for modified but unstaged, and so on. Passed
    /// through as git wrote it, since a view showing "M" is showing git's letter.
    pub status: String,
    /// The path, relative to the repository root.
    pub path: String,
}

/// Split one porcelain line into its code and its path, or `None` for a line too
/// short to be one.
///
/// A rename reads `R  old -> new`, and the path that matters is the **new** one:
/// it is what the file is called now and therefore what an editor can open. The
/// quotes git puts round a path needing them are stripped, so callers see the
/// name rather than the encoding (status is read with `core.quotepath=false`, so
/// what is inside them is already the path itself).
pub fn parse_change(line: &str) -> Option<GitChange> {
    if line.len() < 4 {
        return None;
    }
    let (code, rest) = line.split_at(2);
    let path = rest.trim_start();
    let path = match path.split_once(" -> ") {
        Some((_, after)) => after,
        None => path,
    };
    let path = path.trim();
    let path = path
        .strip_prefix('"')
        .and_then(|p| p.strip_suffix('"'))
        .unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    Some(GitChange {
        status: code.to_owned(),
        path: path.to_owned(),
    })
}

/// A working tree's state as a few lines of prose for the admin.
///
/// Text rather than a structure, which is what keeps the admin UI free of git:
/// a screen that rendered branches and ahead/behind counts would be a screen
/// that knows what those are, and could not render a plugin backend's status at
/// all. The porcelain lines are git's own and are passed through unchanged.
fn describe(status: &GitStatus, repo: &GitRepo) -> String {
    if !status.cloned {
        return match repo.url() {
            Some(_) => format!(
                "Not cloned yet. Cloning will put the working copy in {}.",
                repo.root().display()
            ),
            // Nothing to clone from: this store was pointed at a directory, and
            // the directory does not hold a checkout. Saying so names the two
            // ways out rather than reporting a clone that cannot happen.
            None => format!(
                "{} does not hold a git working copy, and no repository URL is set.",
                repo.root().display()
            ),
        };
    }
    let mut lines = Vec::new();
    lines.push(match status.branch.as_str() {
        "" => "Not on a branch.".to_owned(),
        branch => format!("On branch {branch}."),
    });
    lines.push(match (status.ahead, status.behind) {
        (0, 0) => "Up to date with the remote.".to_owned(),
        (a, 0) => format!("{a} commit(s) to push."),
        (0, b) => format!("{b} commit(s) to pull."),
        (a, b) => format!("{a} commit(s) to push, {b} to pull."),
    });
    if status.clean() {
        lines.push("No uncommitted changes.".to_owned());
    } else {
        lines.push(format!("{} uncommitted change(s):", status.changes.len()));
        lines.extend(status.changes.iter().cloned());
    }
    if !status.last_commit.is_empty() {
        lines.push(format!("Last commit: {}", status.last_commit));
    }
    lines.push(format!("Working copy: {}", repo.root().display()));
    lines.join("\n")
}

/// Record where a store was cloned, so a later rename cannot orphan the working
/// tree (see [`clone_path`]).
pub fn record_clone_path(def: &mut FileStoreDef, path: &Path) {
    def.attributes.insert(
        ATTR_CLONE_PATH.to_owned(),
        serde_json::Value::String(path.to_string_lossy().into_owned()),
    );
}

/// Put a generated key into a definition's settings: the private key's path,
/// which is how git reaches the remote, and the public key, which the admin
/// pastes into the repository's deploy keys.
///
/// Both live in `config` rather than `attributes` because both are things the
/// admin can legitimately supply by hand — pointing a store at a key that
/// already exists on the machine is a perfectly good configuration, and one an
/// admin managing several stores against one repository will want.
pub fn record_deploy_key(def: &mut FileStoreDef, key: &DeployKey) {
    def.config.insert(
        CFG_KEY_PATH.to_owned(),
        serde_json::Value::String(key.private_key_path.to_string_lossy().into_owned()),
    );
    def.config.insert(
        CFG_PUBLIC_KEY.to_owned(),
        serde_json::Value::String(key.public_key.clone()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::DATA_DIR_ENV;
    use crate::paths::testing::temp_env;

    #[test]
    fn the_data_directory_honours_the_override() {
        // A test must never clone into the developer's real data directory.
        temp_env(|| {
            unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/sc-data") };
            assert_eq!(data_dir().unwrap(), PathBuf::from("/tmp/sc-data"));
            assert_eq!(
                clone_dir().unwrap(),
                PathBuf::from("/tmp/sc-data/git-stores")
            );
            assert_eq!(key_dir().unwrap(), PathBuf::from("/tmp/sc-data/keys"));
        });
    }

    #[test]
    fn a_clone_path_is_derived_from_the_name_until_one_is_recorded() {
        temp_env(|| {
            unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/sc-data") };
            let mut def = FileStoreDef::git("web app", "git@example.com:me/app.git");
            assert_eq!(
                clone_path(&def).unwrap(),
                PathBuf::from("/tmp/sc-data/git-stores/web_app")
            );

            // Once recorded, the recorded path wins — renaming the store must
            // not point it at a different, empty directory.
            record_clone_path(&mut def, Path::new("/srv/checkout"));
            def.name = "renamed".to_owned();
            assert_eq!(clone_path(&def).unwrap(), PathBuf::from("/srv/checkout"));
        });
    }

    #[test]
    fn a_repo_is_built_from_the_definitions_settings() {
        temp_env(|| {
            unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/sc-data") };
            let def = FileStoreDef::git("app", "git@example.com:me/app.git")
                .with(CFG_BRANCH, "main")
                .with(CFG_KEY_PATH, "/keys/app.key");
            let repo = GitRepo::from_def(&def).unwrap();
            assert_eq!(repo.url(), Some("git@example.com:me/app.git"));
            assert_eq!(repo.branch.as_deref(), Some("main"));
            assert_eq!(repo.key_path, Some(PathBuf::from("/keys/app.key")));
            assert!(!repo.is_cloned());
        });
    }

    #[test]
    fn a_non_git_definition_is_not_a_repository() {
        let def = FileStoreDef::local("docs", "/srv/docs");
        let err = GitRepo::from_def(&def).unwrap_err().to_string();
        assert!(err.contains("docs"), "{err}");
        assert!(err.contains("local"), "{err}");
    }

    #[test]
    fn a_blank_url_is_no_url() {
        temp_env(|| {
            unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/sc-data") };
            // Whitespace is not a URL. The repository is still buildable — a
            // store may have a directory instead — but it has nothing to clone
            // from, which is what stops it being git's "repository '   ' does
            // not exist".
            let def = FileStoreDef::git("app", "   ");
            assert_eq!(GitRepo::from_def(&def).unwrap().url(), None);
        });
    }

    #[test]
    fn a_configured_directory_is_where_the_working_copy_goes() {
        temp_env(|| {
            unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/sc-data") };
            let def = FileStoreDef::git("app", "git@example.com:me/app.git")
                .with(CFG_DIR, "/srv/checkout");
            assert_eq!(clone_path(&def).unwrap(), PathBuf::from("/srv/checkout"));
            assert_eq!(
                GitRepo::from_def(&def).unwrap().root(),
                Path::new("/srv/checkout")
            );

            // Blank is not a directory: the store falls back to the location
            // Saltcorn picks rather than a directory named " ".
            let blank = FileStoreDef::git("app", "u").with(CFG_DIR, "  ");
            assert_eq!(
                clone_path(&blank).unwrap(),
                PathBuf::from("/tmp/sc-data/git-stores/app")
            );
        });
    }

    #[test]
    fn a_store_may_have_a_directory_and_no_url_at_all() {
        temp_env(|| {
            unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/sc-data") };
            let def = FileStoreDef::new("app", GIT_BACKEND).with(CFG_DIR, "/srv/checkout");
            let repo = GitRepo::from_def(&def).unwrap();
            assert_eq!(repo.url(), None);
            assert_eq!(repo.root(), Path::new("/srv/checkout"));
        });
    }

    #[tokio::test]
    async fn cloning_with_no_url_and_no_checkout_says_what_is_missing() {
        // No data-directory override needed: the store names its own directory,
        // so nothing here consults the one Saltcorn would have picked.
        let def = FileStoreDef::new("app", GIT_BACKEND).with(CFG_DIR, "/definitely/not/here");
        let err = GitRepo::from_def(&def)
            .unwrap()
            .ensure_cloned()
            .await
            .unwrap_err()
            .to_string();
        // Both halves of the fix: supply a URL, or point at a real checkout.
        assert!(err.contains(CFG_URL), "{err}");
        assert!(err.contains("/definitely/not/here"), "{err}");
    }

    #[test]
    fn a_recorded_clone_path_still_wins_over_the_configured_directory() {
        // They agree in every ordinary case — the attribute is written from
        // `clone_path`'s own answer. Where they could not, the attribute is
        // where a working tree *is*, and following it is what keeps that tree
        // reachable.
        temp_env(|| {
            unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/sc-data") };
            let mut def = FileStoreDef::git("app", "git@example.com:me/app.git")
                .with(CFG_DIR, "/srv/checkout");
            record_clone_path(&mut def, Path::new("/srv/original"));
            assert_eq!(clone_path(&def).unwrap(), PathBuf::from("/srv/original"));
        });
    }

    #[test]
    fn the_ssh_command_pins_one_key_and_never_prompts() {
        let cmd = ssh_command(Path::new("/keys/my key.key"));
        assert!(cmd.contains("'/keys/my key.key'"), "{cmd}");
        assert!(cmd.contains("IdentitiesOnly=yes"), "{cmd}");
        assert!(cmd.contains("BatchMode=yes"), "{cmd}");
        assert!(cmd.contains("StrictHostKeyChecking=accept-new"), "{cmd}");
    }

    #[test]
    fn a_porcelain_line_keeps_its_index_column() {
        // ` M` is "modified, not staged" and `M ` is "staged"; the first line's
        // leading space is the difference, and a trim would have eaten it.
        let out = combined(b" M README.md\n?? notes/today.md\n", b"");
        assert_eq!(out, " M README.md\n?? notes/today.md");
        assert_eq!(
            parse_change(out.lines().next().unwrap()).unwrap().status,
            " M"
        );
    }

    #[test]
    fn a_path_is_checked_before_it_becomes_a_pathspec() {
        assert_eq!(
            pathspecs(&["src/App.tsx".to_owned(), "  ".to_owned()]).unwrap(),
            ["src/App.tsx"]
        );
        assert!(pathspecs(&["../etc/passwd".to_owned()]).is_err());
        assert!(pathspecs(&["C:\\Windows".to_owned()]).is_err());
        assert!(pathspecs(&["-n".to_owned()]).is_err());
        assert!(pathspecs(&[":/".to_owned()]).is_err());
    }

    #[test]
    fn output_keeps_both_streams() {
        assert_eq!(combined(b"out\n", b"err\n"), "out\nerr");
        assert_eq!(combined(b"", b"err"), "err");
        assert_eq!(combined(b"", b""), "");
    }

    #[test]
    fn a_status_with_no_changes_is_clean() {
        assert!(GitStatus::default().clean());
        let dirty = GitStatus {
            changes: vec![" M src/main.rs".to_owned()],
            ..GitStatus::default()
        };
        assert!(!dirty.clean());
    }
}

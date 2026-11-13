//! The sync mirror: this instance's clone of the linked repository
//! (APP_SYNC_PROPOSAL §2).
//!
//! **The mirror is never a workspace.** It lives at `<data dir>/sync-repo`,
//! nothing is checked out or edited in it, and it never holds a commit the
//! remote does not have:
//!
//! - [`Mirror::pull`] fetches and moves the branch to the remote's tip: a
//!   fast-forward, or a reset when the remote was force-pushed. Because of
//!   the next rule there is never local work for a reset to lose.
//! - [`Mirror::push_tree`] writes a commit on top of the tip, from the tree
//!   alone, and pushes it. Only a push that succeeded moves the branch. A
//!   refused push (the remote moved in between, or the key is read-only)
//!   resets the branch to the remote's tip and changes nothing else, so the
//!   edits are still in live and pressing Commit again redoes it on top.
//!
//! Trees are read from the commit object (§6 step 2), not from files on disk.

use std::path::{Path, PathBuf};

use serde::Serialize;

use sc_catalog::Catalog;
use sc_error::{Context, Error, Result};
use sc_files::GitRepo;

use crate::link::{Credential, SyncLink, open_token};
use crate::tree::{StructureTree, TreeEntry};

/// The mirror's directory inside a data directory.
pub fn mirror_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sync-repo")
}

/// What a pull did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pulled {
    /// The branch's commit before.
    pub before: Option<String>,
    /// The branch's commit now: the remote's tip, or `None` when the remote
    /// has no such branch yet.
    pub tip: Option<String>,
    /// Whether the branch was reset rather than fast-forwarded: the remote's
    /// history was rewritten (or the branch deleted) since the last pull.
    pub forced: bool,
}

/// The linked repository's clone.
pub struct Mirror {
    repo: GitRepo,
    url: String,
    branch: String,
    subdirectory: Option<String>,
}

impl Mirror {
    /// The mirror of `link` in `data_dir`, reaching the remote with the
    /// link's credential.
    pub fn open(catalog: &Catalog, data_dir: &Path, link: &SyncLink) -> Result<Mirror> {
        let key = match &link.credential {
            Credential::DeployKey { key_path, .. } => Some(key_path.clone()),
            _ => None,
        };
        let mut repo = GitRepo::new(
            mirror_dir(data_dir),
            Some(link.url.clone()),
            Some(link.branch.clone()),
            key,
        );
        if let Some(token) = open_token(catalog, &link.credential)? {
            repo = repo.with_https_token(token);
        }
        Ok(Mirror {
            repo,
            url: link.url.clone(),
            branch: link.branch.clone(),
            subdirectory: link.subdirectory.clone(),
        })
    }

    /// The repository underneath, which keeps the base's store files.
    pub fn repo(&self) -> &GitRepo {
        &self.repo
    }

    /// The branch.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    fn local_ref(&self) -> String {
        format!("refs/heads/{}", self.branch)
    }

    fn remote_ref(&self) -> String {
        format!("refs/remotes/origin/{}", self.branch)
    }

    /// The branch's commit as of the last pull, without asking the remote.
    pub async fn tip(&self) -> Result<Option<String>> {
        if !self.repo.is_cloned() {
            return Ok(None);
        }
        self.repo.ref_commit(&self.local_ref()).await
    }

    /// Fetch, then move the branch to the remote's tip.
    pub async fn pull(&self) -> Result<Pulled> {
        self.repo.init_mirror().await?;
        self.repo
            .fetch()
            .await
            .map_err(|e| reach_error(&self.url, &e))?;
        let before = self.repo.ref_commit(&self.local_ref()).await?;
        let tip = self.repo.ref_commit(&self.remote_ref()).await?;
        let forced = match (&before, &tip) {
            (Some(b), Some(t)) if b == t => {
                return Ok(Pulled {
                    before,
                    tip,
                    forced: false,
                });
            }
            (Some(b), Some(t)) => !self.repo.is_ancestor(b, t).await?,
            (Some(_), None) => true,
            (None, _) => false,
        };
        self.repo.set_ref(&self.local_ref(), tip.as_deref()).await?;
        Ok(Pulled {
            before,
            tip,
            forced,
        })
    }

    /// The structure tree at `commit` — the link's subdirectory of it — or
    /// an empty tree for no commit.
    pub async fn tree_at(&self, commit: Option<&str>) -> Result<StructureTree> {
        let Some(commit) = commit else {
            return Ok(StructureTree::new());
        };
        let files = self
            .repo
            .read_files_at(commit, self.subdirectory.as_deref())
            .await?;
        StructureTree::from_files(files)
            .with_context(|| format!("reading commit {}", short(commit)))
    }

    /// Commit `tree` on top of `parent` (the tip it was built from) with
    /// `message`, and push it. Answers the new commit, or `None` when `tree`
    /// is what `parent` already has.
    ///
    /// A refused push resets the branch to the remote's tip and fails with a
    /// sentence saying what to do; nothing else changes.
    pub async fn push_tree(
        &self,
        parent: Option<&str>,
        tree: &StructureTree,
        message: &str,
        author: Option<(&str, &str)>,
    ) -> Result<Option<String>> {
        if let Some((path, _)) = tree
            .iter()
            .find(|(_, e)| matches!(e, TreeEntry::Blob { .. }))
        {
            return Err(Error::msg(format!(
                "`{path}` has no bytes to commit, only a hash: a base was committed in \
                 place of a tree"
            )));
        }
        let Some(commit) = self
            .repo
            .commit_files(
                parent,
                self.subdirectory.as_deref(),
                &tree.to_files(),
                message,
                author,
            )
            .await?
        else {
            return Ok(None);
        };
        let pushed = self.repo.push_commit(&commit, &self.branch).await?;
        if !pushed.success {
            // Back to whatever the remote has: the mirror never holds a
            // commit the remote does not.
            let _ = self.pull().await;
            return Err(push_refused(&self.url, &self.branch, &pushed.output));
        }
        self.repo.set_ref(&self.local_ref(), Some(&commit)).await?;
        self.repo.set_ref(&self.remote_ref(), Some(&commit)).await?;
        Ok(Some(commit))
    }

    /// Put the tip commit on the remote as a new branch `name`.
    pub async fn push_branch(&self, commit: &str, name: &str) -> Result<()> {
        let pushed = self.repo.push_commit(commit, name).await?;
        if !pushed.success {
            return Err(push_refused(&self.url, name, &pushed.output));
        }
        Ok(())
    }
}

/// A commit's short form, for a sentence.
pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(10)]
}

fn reach_error(url: &str, e: &Error) -> Error {
    Error::invalid(format!(
        "cannot reach the repository {url}: {}. Check the URL and that this instance's \
         credentials are accepted there",
        e.causes()
    ))
}

fn push_refused(url: &str, branch: &str, said: &str) -> Error {
    let lower = said.to_lowercase();
    let why = if lower.contains("fetch first")
        || lower.contains("non-fast-forward")
        || lower.contains("stale info")
        || lower.contains("failed to update ref")
        || lower.contains("cannot lock ref")
    {
        format!(
            "`{branch}` moved on the remote since this instance last pulled. Nothing was \
             committed and the changes are still here; commit again to put them on top of \
             the new tip"
        )
    } else if lower.contains("permission")
        || lower.contains("denied")
        || lower.contains("403")
        || lower.contains("read only")
        || lower.contains("read-only")
    {
        "the remote does not let this instance push; its key or token is probably \
         read-only, which is enough to deploy but not to commit"
            .to_owned()
    } else {
        "nothing was committed".to_owned()
    };
    Error::invalid(format!(
        "the push to {url} was refused: {why} (git said: {})",
        said.trim()
    ))
}

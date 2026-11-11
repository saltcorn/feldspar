//! Source control for a linked installation (APP_SYNC_PROPOSAL §2, §4, §6):
//! the actions of the Source control screen, over the link, the mirror and the
//! base.
//!
//! - **Link** ([`SourceControl::set_link`]): store the URL, branch and
//!   credential, pull, and set the base to the tip's tree — less each entity
//!   live does not have, which would otherwise be a "local delete" ([`link_base`]).
//!   One rule covers every starting point (§4): against an empty repository
//!   everything in live is "local new", so the first Commit exports the
//!   installation; on an empty instance everything is incoming, so Deploy
//!   builds it; where both have content, what is identical is in sync, what
//!   only the tip has is incoming, and what only live has or has differently
//!   is local, reviewed before the first deploy.
//! - **Status** ([`SourceControl::status`]): base, the branch's tip as of the
//!   last pull, and live, compared three ways. It does not fetch.
//! - **Pull**: fetch and move the branch (a fast-forward, or a reset after a
//!   force-push). Live does not change.
//! - **Commit** ([`SourceControl::commit`]): pull, then commit the tip **plus
//!   the chosen local changes** and push. Only after the push succeeded does
//!   the base advance, and only for what was committed. A conflict is
//!   committed only when the admin chose *keep mine* for it; *discard mine* is
//!   a [discard](SourceControl::discard) of it, after which it is incoming; and
//!   *merge in editor* is a registry save of the merged version, then keep mine.
//!   A git store's pinned commit must be one its remote has (§3.5).
//! - **Branches**: list the remote's, switch (only the tip Status and Deploy
//!   compare against changes; local changes stay local), create one from the
//!   current tip.
//! - **Deploy** and **Discard**: the [`Deployer`]'s, with the base and the tip
//!   read from the link, and the next base stored after a deploy.
//!
//! Every action that writes takes the sync lock, as a deploy does.

use std::collections::BTreeSet;
use std::path::PathBuf;

use chrono::Utc;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use sc_catalog::{Catalog, load_file_store_by_name};
use sc_error::{Error, Result};
use sc_files::{GIT_BACKEND, GitRepo, generate_deploy_key_in};

use crate::base::{base_file_bytes, keep_base_files};
use crate::deploy::{Confirm, Deployer, Receipt, Request, release};
use crate::diff::{Class, Status, StatusEntry, pin};
use crate::entity::{Kind, put_entity, remove_entity};
use crate::entity_diff::{EntityDiff, entity_diff};
use crate::export::{Mode, export_mode};
use crate::format::{FORMAT_FILE, StructureFormat};
use crate::ledger::{Deployment, LedgerKind, Outcome, Source, record, take_lock};
use crate::link::{
    Base, Credential, SyncLink, check_branch, load_base, load_credential, load_link,
    save_credential, save_link, store_base,
};
use crate::mirror::{Mirror, Pulled, short};
use crate::plan::Plan;
use crate::stores::StoreWarning;
use crate::structure_zip::additive_target;
use crate::tree::StructureTree;

/// The deploy key's name in `<data dir>/keys`.
pub const SYNC_KEY_NAME: &str = "feldspar-sync";

/// Status of a linked installation.
#[derive(Debug, Clone, Serialize)]
pub struct LinkedStatus {
    /// The link.
    pub link: SyncLink,
    /// The branch's tip as of the last pull; `None` before the branch has a
    /// commit.
    pub tip: Option<String>,
    /// The commit the base was last taken from.
    pub base_commit: Option<String>,
    /// The three-way comparison.
    pub status: Status,
    /// What Status warns about the file stores (§3.5): a store nothing
    /// classifies, user content over a synced directory, a large file.
    pub warnings: Vec<String>,
}

/// A registry file's other sides (§7.6): what the base and the branch tip
/// hold for the entity live has at that path, for the editor's diff tabs and
/// its three-way view when the entity is in conflict.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Sides {
    /// What Status says about the entity, when it is one entity.
    pub class: Option<Class>,
    /// Its Status key.
    pub key: Option<String>,
    /// The file in the base.
    pub base: Option<serde_json::Value>,
    /// The file at the branch tip.
    pub tip: Option<serde_json::Value>,
}

/// What linking did.
#[derive(Debug, Clone, Serialize)]
pub struct Linked {
    /// The branch's tip, which the base now is.
    pub tip: Option<String>,
    /// Whether the base was taken again: a new link, or a new URL or
    /// subdirectory. A change of policy or credential keeps it.
    pub base_taken: bool,
}

/// What to commit.
#[derive(Debug, Clone, Default)]
pub struct CommitRequest {
    /// The commit message.
    pub message: String,
    /// The entities to commit, by Status key; `None` for every local change.
    pub keys: Option<Vec<String>>,
    /// The conflicts to commit anyway, keeping live's version (*keep mine*).
    pub keep_mine: BTreeSet<String>,
    /// Who: the admin's email, or the operator.
    pub actor: Option<String>,
    /// From where.
    pub source: Option<Source>,
}

/// What a commit did.
#[derive(Debug, Clone, Serialize)]
pub struct CommitReceipt {
    /// The ledger row's id.
    pub id: Uuid,
    /// The commit pushed.
    pub commit: String,
    /// The branch it is on.
    pub branch: String,
    /// The entities committed, by key.
    pub entities: Vec<String>,
    /// What the admin should know: a git store with uncommitted work, which
    /// its pin does not include.
    pub warnings: Vec<String>,
}

/// A deploy plan, with the commit it is of.
#[derive(Debug, Clone, Serialize)]
pub struct DeployPlan {
    /// The tip commit the plan deploys.
    pub commit: Option<String>,
    /// The plan.
    pub plan: Plan,
}

/// A plan of deploying a structure zip (§7.4).
#[derive(Debug, Clone, Serialize)]
pub struct ZipPlan {
    /// Whether the installation is linked, so the zip takes the branch tip's
    /// place and the base rules apply; otherwise it is applied additively.
    pub linked: bool,
    /// The plan.
    pub plan: Plan,
}

/// Source control on one installation.
pub struct SourceControl<'a> {
    /// The deployer, whose executor says where and as whom.
    pub deployer: Deployer<'a>,
    before_push: Option<Box<dyn Fn() + Send + Sync + 'a>>,
}

impl<'a> SourceControl<'a> {
    /// Source control over `deployer`'s installation.
    pub fn new(deployer: Deployer<'a>) -> SourceControl<'a> {
        SourceControl {
            deployer,
            before_push: None,
        }
    }

    /// Run `f` after a commit is written and before it is pushed: a test's
    /// stand-in for another instance pushing at that moment.
    #[doc(hidden)]
    pub fn before_push(mut self, f: impl Fn() + Send + Sync + 'a) -> SourceControl<'a> {
        self.before_push = Some(Box::new(f));
        self
    }

    fn catalog(&self) -> &Catalog {
        self.deployer.executor.catalog
    }

    fn data_dir(&self) -> &PathBuf {
        &self.deployer.executor.data_dir
    }

    /// The link, or `None` when the installation is not linked.
    pub async fn link(&self) -> Result<Option<SyncLink>> {
        load_link(self.catalog()).await
    }

    async fn require_link(&self) -> Result<SyncLink> {
        self.link().await?.ok_or_else(|| {
            Error::invalid(
                "this installation is not linked to a git repository; set up the link under \
                 Source control first",
            )
        })
    }

    fn mirror(&self, link: &SyncLink) -> Result<Mirror> {
        Mirror::open(self.catalog(), self.data_dir(), link)
    }

    async fn base(&self) -> Result<Base> {
        Ok(load_base(self.catalog()).await?.unwrap_or(Base {
            tree: StructureTree::new(),
            commit: None,
        }))
    }

    /// Generate a deploy key for the link and store it as the link's
    /// credential (before linking, when there is no link yet). Answers the
    /// credential, whose public half the remote must be given.
    pub async fn generate_key(&self) -> Result<Credential> {
        let key = generate_deploy_key_in(&self.data_dir().join("keys"), SYNC_KEY_NAME).await?;
        let credential = Credential::DeployKey {
            key_path: key.private_key_path,
            public_key: key.public_key,
        };
        match self.link().await? {
            Some(mut link) => {
                link.credential = credential.clone();
                save_link(self.catalog(), &link).await?;
            }
            None => save_credential(self.catalog(), &credential).await?,
        }
        Ok(credential)
    }

    /// The credential stored, linked or not.
    pub async fn credential(&self) -> Result<Credential> {
        load_credential(self.catalog()).await
    }

    /// Whether `link` reaches its remote: the branches it has, or why not.
    /// Nothing is stored.
    pub async fn test_link(&self, link: &SyncLink) -> Result<Vec<String>> {
        let link = link.clone().checked()?;
        let scratch = self
            .data_dir()
            .join(format!("sync-test-{}", Uuid::new_v4().simple()));
        let key = match &link.credential {
            Credential::DeployKey { key_path, .. } => Some(key_path.clone()),
            _ => None,
        };
        let mut repo = GitRepo::new(&scratch, Some(link.url.clone()), None, key);
        if let Some(token) = crate::link::open_token(self.catalog(), &link.credential)? {
            repo = repo.with_https_token(token);
        }
        let result = async {
            repo.init_mirror().await?;
            repo.remote_branches().await
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&scratch).await;
        result.map_err(|e| Error::invalid(format!("cannot reach {}: {}", link.url, e.causes())))
    }

    /// Link the installation (§4 "Linking"): store `link`, pull, and set the
    /// base to the tip's tree ([`link_base`]). Changing only the policy, the credential or the
    /// branch keeps the base, so it is not a new link (a branch change is a
    /// [switch](Self::switch_branch)).
    pub async fn set_link(&self, link: SyncLink) -> Result<Linked> {
        let link = link.checked()?;
        let catalog = self.catalog();
        let lock = take_lock(catalog, "link").await?;
        let result = async {
            let previous = load_link(catalog).await?;
            let has_base = load_base(catalog).await?.is_some();
            let relink = !has_base
                || previous
                    .as_ref()
                    .is_none_or(|p| p.url != link.url || p.subdirectory != link.subdirectory);
            let mirror = self.mirror(&link)?;
            // Reach the remote before storing anything, so a link that does not
            // work is refused rather than kept.
            let pulled = mirror.pull().await?;
            save_link(catalog, &link).await?;
            if relink {
                let tip = mirror.tree_at(pulled.tip.as_deref()).await?;
                let live = self.deployer.live_in(Mode::Portable).await?;
                let tree = keep_base_files(mirror.repo(), &link_base(&tip, &live)).await?;
                store_base(
                    catalog,
                    &Base {
                        tree,
                        commit: pulled.tip.clone(),
                    },
                )
                .await?;
            }
            Ok(Linked {
                tip: pulled.tip,
                base_taken: relink,
            })
        }
        .await;
        release(catalog, lock, result).await
    }

    /// Forget the link and the base; the credential is kept.
    pub async fn unlink(&self) -> Result<()> {
        crate::link::unlink(self.catalog()).await
    }

    /// Status (§4), against the tip as of the last pull.
    pub async fn status(&self) -> Result<LinkedStatus> {
        let link = self.require_link().await?;
        let mirror = self.mirror(&link)?;
        let tip_commit = mirror.tip().await?;
        let tip = mirror.tree_at(tip_commit.as_deref()).await?;
        let base = self.base().await?;
        let registries = self.deployer.executor.host.registries()?;
        let live = export_mode(self.catalog(), &registries, Mode::Portable).await?;
        Ok(LinkedStatus {
            status: Status::of(&base.tree, &tip, &live.tree),
            warnings: live.warnings.iter().map(StoreWarning::message).collect(),
            link,
            tip: tip_commit,
            base_commit: base.commit,
        })
    }

    /// One Status entry's diff, field by field, or line by line for a text
    /// file (`getSyncEntityDiff`). A base file's bytes come from the mirror.
    pub async fn entity_diff(&self, key: &str) -> Result<EntityDiff> {
        let link = self.require_link().await?;
        let mirror = self.mirror(&link)?;
        let tip = mirror.tree_at(mirror.tip().await?.as_deref()).await?;
        let base = self.base().await?;
        let live = self.deployer.live_in(Mode::Portable).await?;
        let status = Status::of(&base.tree, &tip, &live);
        let entry = status
            .entry(key)
            .ok_or_else(|| Error::not_found(format!("Status has nothing called `{key}`")))?;
        let bytes = base_file_bytes(mirror.repo(), &status, &[key]).await?;
        Ok(entity_diff(entry, &|hash| bytes.get(hash).cloned()))
    }

    /// What the base and the tip hold for the registry file at `path` of the
    /// instance tree; `None` when the installation is not linked. A file
    /// holding one entity is followed by that entity, so a rename shows its
    /// old file; anything else (a list, a settings section) is the same path.
    pub async fn sides(&self, path: &str) -> Result<Option<Sides>> {
        let Some(link) = self.link().await? else {
            return Ok(None);
        };
        let mirror = self.mirror(&link)?;
        let tip = mirror.tree_at(mirror.tip().await?.as_deref()).await?;
        let base = self.base().await?;
        let live = self.deployer.live_in(Mode::Portable).await?;
        let status = Status::of(&base.tree, &tip, &live);
        let at_path: Vec<&StatusEntry> = status
            .entries
            .iter()
            .filter(|e| e.live.as_ref().is_some_and(|l| l.path == path))
            .collect();
        let file = |tree: &StructureTree, side: Option<&crate::entity::Entity>| {
            tree.json(side.map_or(path, |e| e.path.as_str())).cloned()
        };
        Ok(Some(match at_path.as_slice() {
            [one] if one.live.as_ref().is_some_and(|l| l.id.is_some()) => Sides {
                class: Some(one.class),
                key: Some(one.key.clone()),
                base: one.base.as_ref().and_then(|b| file(&base.tree, Some(b))),
                tip: one.tip.as_ref().and_then(|t| file(&tip, Some(t))),
            },
            entries => Sides {
                class: entries
                    .iter()
                    .map(|e| e.class)
                    .find(|c| c.shown())
                    .or(entries.first().map(|e| e.class)),
                key: (entries.len() == 1).then(|| entries[0].key.clone()),
                base: file(&base.tree, None),
                tip: file(&tip, None),
            },
        }))
    }

    /// Pull: fetch and move the branch to the remote's tip. Live does not
    /// change.
    pub async fn pull(&self) -> Result<Pulled> {
        let link = self.require_link().await?;
        self.mirror(&link)?.pull().await
    }

    /// The remote's branches.
    pub async fn branches(&self) -> Result<Vec<String>> {
        let link = self.require_link().await?;
        let mirror = self.mirror(&link)?;
        mirror.repo().init_mirror().await?;
        mirror.repo().remote_branches().await
    }

    /// Switch to the remote's branch `name`: Status and Deploy compare against
    /// its tip from now on. Live and the base stay as they are, so a local
    /// change is still a local change, to be committed onto `name`.
    pub async fn switch_branch(&self, name: &str) -> Result<Pulled> {
        check_branch(name)?;
        let mut link = self.require_link().await?;
        if !self.branches().await?.iter().any(|b| b == name) {
            return Err(Error::not_found(format!(
                "the repository has no branch `{name}`; create it instead"
            )));
        }
        link.branch = name.to_owned();
        let pulled = self.mirror(&link)?.pull().await?;
        save_link(self.catalog(), &link).await?;
        Ok(pulled)
    }

    /// Create the branch `name` from the current tip, push it, and switch to
    /// it. On a branch with no commit yet there is nothing to push, and the
    /// first Commit creates it.
    pub async fn create_branch(&self, name: &str) -> Result<Pulled> {
        check_branch(name)?;
        let link = self.require_link().await?;
        if self.branches().await?.iter().any(|b| b == name) {
            return Err(Error::invalid(format!(
                "the repository already has a branch `{name}`; switch to it instead"
            )));
        }
        let mirror = self.mirror(&link)?;
        let pulled = mirror.pull().await?;
        if let Some(tip) = &pulled.tip {
            mirror.push_branch(tip, name).await?;
        }
        let mut link = link;
        link.branch = name.to_owned();
        let pulled = self.mirror(&link)?.pull().await?;
        save_link(self.catalog(), &link).await?;
        Ok(pulled)
    }

    /// Commit and push the chosen local changes (§2, §4).
    pub async fn commit(&self, request: &CommitRequest) -> Result<CommitReceipt> {
        let catalog = self.catalog();
        let link = self.require_link().await?;
        let lock = take_lock(catalog, "commit").await?;
        let result = self.commit_locked(&link, request).await;
        release(catalog, lock, result).await
    }

    async fn commit_locked(
        &self,
        link: &SyncLink,
        request: &CommitRequest,
    ) -> Result<CommitReceipt> {
        let started_at = Utc::now();
        let catalog = self.catalog();
        let message = request.message.trim();
        if message.is_empty() {
            return Err(Error::invalid("a commit needs a message"));
        }
        let mirror = self.mirror(link)?;
        let pulled = mirror.pull().await?;
        let tip = mirror.tree_at(pulled.tip.as_deref()).await?;
        let base = self.base().await?;
        let live = self.deployer.live_in(Mode::Portable).await?;
        let status = Status::of(&base.tree, &tip, &live);

        let selected = select(&status, request)?;
        let mut warnings = Vec::new();
        for entry in &selected {
            if entry.kind == Kind::FileStore && entry.pin_differs() {
                warnings.extend(check_pin(catalog, entry).await?);
            }
        }

        let mut committed = tip.clone();
        let mut next = base.tree.clone();
        for entry in &selected {
            if let Some(t) = &entry.tip {
                remove_entity(&mut committed, t);
            }
            if let Some(b) = &entry.base {
                remove_entity(&mut next, b);
            }
            if let Some(l) = &entry.live {
                put_entity(&mut committed, l);
                put_entity(&mut next, l);
            }
        }
        // Where both sides already agree, the base takes the tip's version.
        for entry in status.entries.iter().filter(|e| e.adopt) {
            if let Some(b) = &entry.base {
                remove_entity(&mut next, b);
            }
            if let Some(t) = &entry.tip {
                put_entity(&mut next, t);
            }
        }
        committed.insert_json(FORMAT_FILE, StructureFormat::current().to_json());
        // A file live has with nothing in it (no modules, a settings section
        // left at its defaults) is no entity to choose, and is written as live
        // has it, so a commit of everything is the export byte for byte.
        for (path, entry) in live.iter() {
            if committed.get(path).is_none() && holds_no_entity(path, entry) {
                committed.insert_entry(path, entry.clone());
            }
        }

        if let Some(hook) = &self.before_push {
            hook();
        }
        let author = request
            .actor
            .as_deref()
            .filter(|a| a.contains('@'))
            .map(|a| (a, a));
        let commit = mirror
            .push_tree(pulled.tip.as_deref(), &committed, message, author)
            .await?
            .ok_or_else(|| {
                Error::invalid("nothing to commit: the repository already has these changes")
            })?;

        let tree = keep_base_files(mirror.repo(), &next).await?;
        store_base(
            catalog,
            &Base {
                tree,
                commit: Some(commit.clone()),
            },
        )
        .await?;
        let entities: Vec<String> = selected.iter().map(|e| e.key.clone()).collect();
        let id = Uuid::new_v4();
        record(
            catalog,
            &Deployment {
                id,
                kind: LedgerKind::Commit,
                result: Outcome::Converged,
                commit: Some(commit.clone()),
                branch: Some(link.branch.clone()),
                actor: request.actor.clone(),
                source: request.source.unwrap_or(Source::Ui),
                summary: json!({
                    "message": message,
                    "entities": entities,
                    "warnings": warnings,
                }),
                deferred: Vec::new(),
                backups: Vec::new(),
                before: None,
                after: None,
                started_at,
                finished_at: Utc::now(),
            },
        )
        .await?;
        Ok(CommitReceipt {
            id,
            commit,
            branch: link.branch.clone(),
            entities,
            warnings,
        })
    }

    /// Pull, then plan deploying the tip (§6 steps 1–4).
    pub async fn plan_deploy(&self, actor: Option<String>, source: Source) -> Result<DeployPlan> {
        let link = self.require_link().await?;
        let mirror = self.mirror(&link)?;
        let pulled = mirror.pull().await?;
        let tip = mirror.tree_at(pulled.tip.as_deref()).await?;
        let base = self.base().await?;
        let request = deploy_request(&link, &base, &tip, pulled.tip.clone(), actor, source);
        Ok(DeployPlan {
            plan: self.deployer.plan_deploy(&request).await?,
            commit: pulled.tip,
        })
    }

    /// Deploy the tip the plan `confirm` reviewed was of, and store the next
    /// base (§6 steps 6–10). A tip that moved since is a stale plan.
    pub async fn deploy(
        &self,
        commit: Option<&str>,
        confirm: &Confirm,
        actor: Option<String>,
        source: Source,
    ) -> Result<Receipt> {
        let link = self.require_link().await?;
        let mirror = self.mirror(&link)?;
        let pulled = mirror.pull().await?;
        if pulled.tip.as_deref() != commit {
            return Err(Error::invalid(format!(
                "the plan is stale: `{}` is now at {}, and the plan was of {}. Nothing was \
                 applied; review the new plan and confirm it",
                link.branch,
                pulled.tip.as_deref().map_or("no commit", short),
                commit.map_or("no commit", short),
            )));
        }
        let tip = mirror.tree_at(pulled.tip.as_deref()).await?;
        let base = self.base().await?;
        let request = deploy_request(&link, &base, &tip, pulled.tip.clone(), actor, source);
        let receipt = self.deployer.deploy(&request, confirm).await?;
        let tree = keep_base_files(mirror.repo(), &receipt.base).await?;
        store_base(
            self.catalog(),
            &Base {
                tree,
                commit: pulled.tip,
            },
        )
        .await?;
        Ok(receipt)
    }

    /// Plan deploying the structure zip `zip` (§7.4). With a link, the zip
    /// takes the branch tip's place: the base rules apply, so local changes
    /// block it (D5) and what the zip lacks is deleted. Without one, it is
    /// applied additively ([`additive_target`]): created and updated, never
    /// deleted.
    pub async fn plan_zip(
        &self,
        zip: &StructureTree,
        actor: Option<String>,
        source: Source,
    ) -> Result<ZipPlan> {
        crate::format::check_format(zip)?;
        Ok(match self.link().await? {
            Some(link) => {
                let base = self.base().await?;
                let request = zip_request(Some(&link), &base.tree, zip, actor, source);
                ZipPlan {
                    linked: true,
                    plan: self.deployer.plan_deploy(&request).await?,
                }
            }
            None => {
                let live = self.deployer.live_in(Mode::Portable).await?;
                let target = additive_target(&live, zip);
                let request = zip_request(None, &live, &target, actor, source);
                ZipPlan {
                    linked: false,
                    plan: self.deployer.plan_deploy(&request).await?,
                }
            }
        })
    }

    /// Deploy the structure zip `zip` as the plan `confirm` reviewed
    /// ([`plan_zip`](Self::plan_zip)). With a link, the next base is stored, as
    /// after a deploy of the tip; it was taken from no commit.
    pub async fn deploy_zip(
        &self,
        zip: &StructureTree,
        confirm: &Confirm,
        actor: Option<String>,
        source: Source,
    ) -> Result<Receipt> {
        crate::format::check_format(zip)?;
        match self.link().await? {
            Some(link) => {
                let mirror = self.mirror(&link)?;
                let base = self.base().await?;
                let request = zip_request(Some(&link), &base.tree, zip, actor, source);
                let receipt = self.deployer.deploy(&request, confirm).await?;
                let tree = keep_base_files(mirror.repo(), &receipt.base).await?;
                store_base(self.catalog(), &Base { tree, commit: None }).await?;
                Ok(receipt)
            }
            None => {
                // The target is made from live as it is now; a live that
                // moves before the lock is taken changes the plan's digest,
                // so the plan is stale, as any other would be.
                let live = self.deployer.live_in(Mode::Portable).await?;
                let target = additive_target(&live, zip);
                let request = zip_request(None, &live, &target, actor, source);
                self.deployer.deploy(&request, confirm).await
            }
        }
    }

    /// Plan discarding `keys`: each back at its base version (§2, §5).
    pub async fn plan_discard(&self, keys: &[&str]) -> Result<Plan> {
        let link = self.require_link().await?;
        let mirror = self.mirror(&link)?;
        let tip_commit = mirror.tip().await?;
        let tip = mirror.tree_at(tip_commit.as_deref()).await?;
        let base = self.base().await?;
        let request = deploy_request(&link, &base, &tip, tip_commit, None, Source::Ui);
        self.deployer
            .plan_discard(&request, keys, Some(mirror.repo()))
            .await
    }

    /// Discard `keys` as the plan `confirm` reviewed. The base does not move:
    /// live moves to it. *Discard mine* on a conflict is this, after which the
    /// entity is incoming.
    pub async fn discard(
        &self,
        keys: &[&str],
        confirm: &Confirm,
        actor: Option<String>,
        source: Source,
    ) -> Result<Receipt> {
        let link = self.require_link().await?;
        let mirror = self.mirror(&link)?;
        let tip_commit = mirror.tip().await?;
        let tip = mirror.tree_at(tip_commit.as_deref()).await?;
        let base = self.base().await?;
        let request = deploy_request(&link, &base, &tip, tip_commit, actor, source);
        self.deployer
            .discard(&request, keys, Some(mirror.repo()), confirm)
            .await
    }
}

/// The base a new link starts from (§4 "Linking"): the tip's tree, except
/// for each entity live does not have. Against the tip, such an entity would be
/// a "local delete" — on an empty instance, everything — and a Commit would
/// delete it from the repository; left out, it is "incoming new", which Deploy
/// creates. Everything both have keeps the tip's version, so what is identical
/// is in sync and what differs is a local change, to commit or discard.
pub fn link_base(tip: &StructureTree, live: &StructureTree) -> StructureTree {
    let mut base = tip.clone();
    for entry in Status::of(tip, tip, live).entries {
        if let (Some(t), None) = (&entry.tip, &entry.live) {
            remove_entity(&mut base, t);
        }
    }
    base
}

/// Whether the file at `path` is a container with no entity in it.
fn holds_no_entity(path: &str, entry: &crate::tree::TreeEntry) -> bool {
    let mut one = StructureTree::new();
    one.insert_entry(path, entry.clone());
    path != FORMAT_FILE && crate::entity::entities(&one).is_empty()
}

fn deploy_request<'r>(
    link: &SyncLink,
    base: &'r Base,
    tip: &'r StructureTree,
    commit: Option<String>,
    actor: Option<String>,
    source: Source,
) -> Request<'r> {
    Request {
        base: &base.tree,
        target: tip,
        commit,
        branch: Some(link.branch.clone()),
        actor,
        source,
        policy: link.policy,
    }
}

/// A deploy of a structure zip: over the link's base and policy, or, with no
/// link, over live itself — so nothing is a local change — and the default
/// policy.
fn zip_request<'r>(
    link: Option<&SyncLink>,
    base: &'r StructureTree,
    target: &'r StructureTree,
    actor: Option<String>,
    source: Source,
) -> Request<'r> {
    Request {
        base,
        target,
        commit: None,
        branch: link.map(|l| l.branch.clone()),
        actor,
        source,
        policy: link.map(|l| l.policy).unwrap_or_default(),
    }
}

/// The entries a commit takes (§4): the chosen ones, or every local change.
/// A conflict is taken only with *keep mine*; with none chosen for it, the
/// commit is refused, naming it.
fn select<'s>(status: &'s Status, request: &CommitRequest) -> Result<Vec<&'s StatusEntry>> {
    let mut selected = Vec::new();
    let mut unresolved = Vec::new();
    match &request.keys {
        None => {
            for entry in &status.entries {
                if entry.class.is_local() {
                    selected.push(entry);
                } else if entry.class == Class::Conflict {
                    match keep_mine(request, entry) {
                        true => selected.push(entry),
                        false => unresolved.push(entry.key.clone()),
                    }
                }
            }
        }
        Some(keys) => {
            for key in keys {
                let entry = status.entry(key).ok_or_else(|| {
                    Error::not_found(format!("Status has nothing called `{key}`"))
                })?;
                match entry.class {
                    c if c.is_local() => selected.push(entry),
                    Class::Conflict if keep_mine(request, entry) => selected.push(entry),
                    Class::Conflict => unresolved.push(entry.key.clone()),
                    _ => {
                        return Err(Error::invalid(format!(
                            "`{key}` has no local change to commit ({})",
                            entry.class.label().to_lowercase()
                        )));
                    }
                }
            }
        }
    }
    if !unresolved.is_empty() {
        return Err(Error::invalid(format!(
            "{} changed both here and on the branch: {}. For each, keep mine (commit this \
             instance's version over the incoming one), discard mine (return it to the base, \
             so it is incoming), or merge it in the registry editor and then keep mine",
            if unresolved.len() == 1 {
                "an entity"
            } else {
                "some entities"
            },
            unresolved.join(", ")
        )));
    }
    if selected.is_empty() {
        return Err(Error::invalid(
            "nothing to commit: this instance has no local changes",
        ));
    }
    Ok(selected)
}

fn keep_mine(request: &CommitRequest, entry: &StatusEntry) -> bool {
    request.keep_mine.contains(&entry.key)
        || [&entry.base, &entry.tip, &entry.live]
            .into_iter()
            .flatten()
            .any(|e| request.keep_mine.contains(&e.key()))
}

/// Refuse to commit a git store pinned at a commit its remote does not have
/// (§3.5): another instance could never check it out. Answers the warnings —
/// uncommitted work in the store, which the pin does not include.
async fn check_pin(catalog: &Catalog, entry: &StatusEntry) -> Result<Vec<String>> {
    let (Some(live), Some(commit)) = (&entry.live, pin(entry.live.as_ref())) else {
        return Ok(Vec::new());
    };
    let Some(def) = load_file_store_by_name(catalog, &live.name).await? else {
        return Ok(Vec::new());
    };
    if def.backend != GIT_BACKEND {
        return Ok(Vec::new());
    }
    let repo = GitRepo::from_def(&def)?;
    let reached = repo.fetch().await;
    if reached.is_err() || !repo.on_remote(commit).await? {
        let why = match reached {
            Err(e) => format!(" (its remote could not be reached: {})", e.causes()),
            Ok(_) => String::new(),
        };
        return Err(Error::invalid(format!(
            "the git store `{}` is at commit {}, which its remote does not have{why}; push it \
             from the store first, so other instances can check it out, then commit again",
            live.name,
            short(commit),
        )));
    }
    let status = repo.status().await?;
    let mut warnings = Vec::new();
    if !status.changes.is_empty() {
        let n = status.changes.len();
        warnings.push(format!(
            "the git store `{}` has {n} uncommitted change{}, which the pinned commit {} does \
             not include",
            live.name,
            if n == 1 { "" } else { "s" },
            short(commit),
        ));
    }
    Ok(warnings)
}

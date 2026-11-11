//! Deploy and Discard (APP_SYNC_PROPOSAL §5, §6): planning a target against
//! live, and applying a confirmed plan under the sync lock.
//!
//! A deploy is two calls, as the review screen is two steps.
//! [`Deployer::plan_deploy`] exports live, compares it with the base and the
//! target, plans and validates — the plan the review screen shows, with its
//! digest. [`Deployer::deploy`] takes that digest back with the operator's
//! ticks and then, holding the sync lock:
//!
//! 1. **Plans again from scratch** (§6 step 6). If live, the target or the base
//!    moved since the review, the digest differs and nothing is applied: the
//!    plan is stale, and the operator reviews the new one.
//! 2. Refuses a plan with blockers, a tick on an operation that cannot be
//!    ticked, and a ticked data-destructive operation the instance's
//!    [`Policy`] forbids (§5).
//! 3. Writes the **safety backup** when a data-destructive operation is
//!    ticked: the structure plus the rows of every table or column about to be
//!    dropped, with the existing backup writer. A full backup to the backup
//!    schedules' destinations too, when the policy asks for one.
//! 4. Applies the ticked operations ([`Executor::apply`]).
//! 5. Reloads everything (`reload_all`), exports live again and **re-diffs it
//!    against the target** (§6 step 9). A difference the plan did not account
//!    for marks the receipt `drifted`, with the differences listed.
//! 6. Records the outcome in `_fd_deployments`, and hands back the **next
//!    base**: the target, except that an entity with an operation that was not
//!    applied — deferred, or after a failure — keeps the version live has now,
//!    so it shows as incoming again and not as a local change (§5).
//!
//! A discard is the same run over a different target: live with the chosen
//! entities back at their base version (`base::discard_target`). The base does
//! not move, since live is moving to it. So is a registry save
//! ([`crate::registry`]), over instance trees.

use std::collections::BTreeSet;
use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use sc_error::{Error, Result};
use sc_files::GitRepo;

use crate::apply::{Applied, Executor};
use crate::base::{base_file_bytes, discard_target};
use crate::diff::{Status, match_sides};
use crate::entity::{Entities, put_entity, remove_entity};
use crate::export::{Mode, export_mode};
use crate::ledger::{
    Deployment, LedgerKind, Outcome, Source, SyncLock, bootstrap_ledger, record, take_lock,
};
use crate::plan::{Blocker, OpClass, Plan, plan, plan_instance};
use crate::tree::StructureTree;
use crate::validate::validate;

/// Whether data-destructive operations may be ticked (§5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destructive {
    /// Ticked one at a time, with a typed confirmation.
    #[default]
    Confirm,
    /// Never from the UI or the API; only `feldspar sync deploy
    /// --allow-destructive` on the server.
    Never,
}

/// Whether a full backup is taken before every deploy (§5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullBackup {
    /// No.
    #[default]
    Off,
    /// When the operator asks for one on the review screen.
    Optional,
    /// Always; a deploy with no backup schedule to send it to is refused.
    Required,
}

/// An instance's policy, stored with its link, so instances can differ.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Whether data-destructive operations may be ticked.
    #[serde(default)]
    pub destructive: Destructive,
    /// Whether a full backup is taken first.
    #[serde(default)]
    pub full_backup: FullBackup,
}

/// What the operator confirmed on the review screen (§6 step 6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirm {
    /// The digest of the plan reviewed.
    pub plan_digest: String,
    /// Every operation to apply, by id. The rest are deferred.
    pub ticked: BTreeSet<String>,
    /// `--allow-destructive`: data-destructive operations even under
    /// [`Destructive::Never`]. Honoured only from the CLI.
    #[serde(default)]
    pub allow_destructive: bool,
    /// A full backup first, under [`FullBackup::Optional`].
    #[serde(default)]
    pub full_backup: bool,
}

impl Confirm {
    /// A confirmation of `plan` as it was offered: each operation ticked or not
    /// as its class defaults to.
    pub fn defaults(plan: &Plan) -> Confirm {
        Confirm {
            plan_digest: plan.digest.clone(),
            ticked: plan
                .operations
                .iter()
                .filter(|op| op.ticked)
                .map(|op| op.id.clone())
                .collect(),
            allow_destructive: false,
            full_backup: false,
        }
    }

    /// Tick the operation `id` too.
    pub fn tick(mut self, id: &str) -> Confirm {
        self.ticked.insert(id.to_owned());
        self
    }

    /// Untick the operation `id`.
    pub fn untick(mut self, id: &str) -> Confirm {
        self.ticked.remove(id);
        self
    }
}

/// What a deploy is of, and on whose behalf.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    /// The base: each entity as last synced here (§4). Its store files may be
    /// blob hashes only.
    pub base: &'a StructureTree,
    /// The target: the branch tip's tree, or a structure zip's.
    pub target: &'a StructureTree,
    /// The commit the target was read from, if one.
    pub commit: Option<String>,
    /// The branch, if one.
    pub branch: Option<String>,
    /// Who: the admin's email, or the operator.
    pub actor: Option<String>,
    /// From where.
    pub source: Source,
    /// The instance's policy.
    pub policy: Policy,
}

/// What a deploy or discard did: the receipt (§6 step 10).
#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    /// The ledger row's id.
    pub id: Uuid,
    /// How it ended.
    pub result: Outcome,
    /// The operations applied, deferred, not reached, and the one that failed.
    #[serde(flatten)]
    pub applied: Applied,
    /// Where live still differs from the target, for a `drifted` result.
    pub drift: Vec<String>,
    /// The backups written first: the safety backup's file, and where each
    /// full backup went.
    pub backups: Vec<String>,
    /// Entities created without a secret this instance has not got (§3.3).
    pub needs_secrets: Vec<String>,
    /// New git stores this instance could not clone anonymously (§3.5).
    pub needs_credentials: Vec<String>,
    /// The base to store from now on. Its store files are bytes; the caller
    /// records them in the mirror (`base::keep_base_files`) before storing it.
    #[serde(skip)]
    pub base: StructureTree,
}

/// Runs deploys and discards on one installation.
pub struct Deployer<'a> {
    /// Where, and as whom.
    pub executor: Executor<'a>,
}

impl<'a> Deployer<'a> {
    /// A deployer over `executor`.
    pub fn new(executor: Executor<'a>) -> Deployer<'a> {
        Deployer { executor }
    }

    /// Live, exported.
    async fn live(&self) -> Result<StructureTree> {
        self.live_in(Mode::Portable).await
    }

    /// Live, exported in `mode`.
    pub(crate) async fn live_in(&self, mode: Mode) -> Result<StructureTree> {
        let registries = self.executor.host.registries()?;
        Ok(export_mode(self.executor.catalog, &registries, mode)
            .await?
            .tree)
    }

    /// The plan the review screen shows: live → `request.target`, classified
    /// and validated, with a blocker for each local change or conflict (D5)
    /// and for anything the policy forbids outright.
    pub async fn plan_deploy(&self, request: &Request<'_>) -> Result<Plan> {
        let live = self.live().await?;
        self.plan_deploy_from(&live, request).await
    }

    async fn plan_deploy_from(&self, live: &StructureTree, request: &Request<'_>) -> Result<Plan> {
        let mut plan = plan(live, request.target, Some(request.base));
        validate(self.executor.catalog, &mut plan).await?;
        let status = Status::of(request.base, request.target, live);
        let local: Vec<&str> = status
            .deploy_blockers()
            .iter()
            .map(|e| e.key.as_str())
            .collect();
        if !local.is_empty() {
            plan.blockers.push(Blocker {
                op: None,
                entity: None,
                message: format!(
                    "this instance has {} uncommitted change{} ({}); commit or discard {} \
                     before deploying",
                    local.len(),
                    if local.len() == 1 { "" } else { "s" },
                    local.join(", "),
                    if local.len() == 1 { "it" } else { "them" },
                ),
            });
        }
        Ok(plan)
    }

    /// Apply the plan `confirm` reviewed (§6 steps 6–10).
    pub async fn deploy(&self, request: &Request<'_>, confirm: &Confirm) -> Result<Receipt> {
        let catalog = self.executor.catalog;
        bootstrap_ledger(catalog).await?;
        let lock = take_lock(catalog, "deploy").await?;
        let result = async {
            let live = self.live().await?;
            let plan = self.plan_deploy_from(&live, request).await?;
            let run = Run {
                kind: LedgerKind::Deploy,
                request,
                live: &live,
                target: request.target,
                edit: None,
            };
            self.run(run, plan, confirm)
                .await
                .map(|(receipt, _)| receipt)
        }
        .await;
        release(catalog, lock, result).await
    }

    /// The plan a discard of `keys` (Status's entity keys) shows: live with
    /// each of them back at its base version, classified the same way as a
    /// deploy, so discarding a new table that holds rows is a data-destructive
    /// drop (§5). `mirror` is the sync mirror, which holds the base's store
    /// files.
    pub async fn plan_discard(
        &self,
        request: &Request<'_>,
        keys: &[&str],
        mirror: Option<&GitRepo>,
    ) -> Result<Plan> {
        let live = self.live().await?;
        let (_, plan) = self.plan_discard_from(&live, request, keys, mirror).await?;
        Ok(plan)
    }

    async fn plan_discard_from(
        &self,
        live: &StructureTree,
        request: &Request<'_>,
        keys: &[&str],
        mirror: Option<&GitRepo>,
    ) -> Result<(StructureTree, Plan)> {
        let status = Status::of(request.base, request.target, live);
        let bytes = match mirror {
            Some(mirror) => base_file_bytes(mirror, &status, keys).await?,
            None => Default::default(),
        };
        let target = discard_target(live, &status, keys, &bytes)?;
        let mut plan = plan(live, &target, Some(request.base));
        validate(self.executor.catalog, &mut plan).await?;
        Ok((target, plan))
    }

    /// Apply the discard `confirm` reviewed.
    pub async fn discard(
        &self,
        request: &Request<'_>,
        keys: &[&str],
        mirror: Option<&GitRepo>,
        confirm: &Confirm,
    ) -> Result<Receipt> {
        let catalog = self.executor.catalog;
        bootstrap_ledger(catalog).await?;
        let lock = take_lock(catalog, "discard").await?;
        let result = async {
            let live = self.live().await?;
            let (target, plan) = self.plan_discard_from(&live, request, keys, mirror).await?;
            let run = Run {
                kind: LedgerKind::Discard,
                request,
                live: &live,
                target: &target,
                edit: None,
            };
            self.run(run, plan, confirm)
                .await
                .map(|(receipt, _)| receipt)
        }
        .await;
        release(catalog, lock, result).await
    }

    /// Steps 6 to 10 of a confirmed plan, made a moment ago under the lock.
    /// Answers the receipt and live as exported after it.
    pub(crate) async fn run(
        &self,
        run: Run<'_>,
        mut plan: Plan,
        confirm: &Confirm,
    ) -> Result<(Receipt, StructureTree)> {
        let started_at = Utc::now();
        let mode = if run.edit.is_some() {
            Mode::Instance
        } else {
            Mode::Portable
        };
        let catalog = self.executor.catalog;
        let request = run.request;
        if plan.digest != confirm.plan_digest {
            return Err(Error::invalid(
                "the plan is stale: the structure here, the target or the base changed since \
                 it was reviewed. Nothing was applied; review the new plan and confirm it",
            ));
        }
        if !plan.blockers.is_empty() {
            let reasons: Vec<&str> = plan.blockers.iter().map(|b| b.message.as_str()).collect();
            return Err(Error::invalid(format!(
                "the plan cannot be applied: {}",
                reasons.join("; ")
            )));
        }
        if let Some(unknown) = confirm.ticked.iter().find(|id| plan.op(id).is_none()) {
            return Err(Error::invalid(format!(
                "the plan has no operation `{unknown}` to tick"
            )));
        }
        for op in &mut plan.operations {
            op.ticked = confirm.ticked.contains(&op.id);
        }
        let destructive: Vec<&str> = plan
            .operations
            .iter()
            .filter(|op| op.ticked && op.class == OpClass::DataDestructive)
            .map(|op| op.summary.as_str())
            .collect();
        let allowed = request.source == Source::Cli && confirm.allow_destructive;
        if !destructive.is_empty() && request.policy.destructive == Destructive::Never && !allowed {
            return Err(Error::auth(format!(
                "this instance's policy does not allow data-destructive operations from here \
                 ({}); untick {}, or run `feldspar sync deploy --allow-destructive` on the server",
                destructive.join("; "),
                if destructive.len() == 1 { "it" } else { "them" },
            )));
        }

        let mut backups = Vec::new();
        if !destructive.is_empty() {
            backups.push(self.safety_backup(&plan).await?);
        }
        let full = match request.policy.full_backup {
            FullBackup::Off => false,
            FullBackup::Optional => confirm.full_backup,
            FullBackup::Required => true,
        };
        if full {
            backups.extend(full_backup(catalog).await?);
        }

        let mut applied = match mode {
            Mode::Portable => self.executor.apply(&plan, run.live, run.target).await,
            Mode::Instance => {
                self.executor
                    .apply_instance(&plan, run.live, run.target)
                    .await
            }
        };
        applied
            .notes
            .extend(self.executor.host.reload_all(catalog).await);

        let after = self.live_in(mode).await?;
        let held = applied.held();
        let base = (mode == Mode::Portable).then_some(request.base);
        let drift = drift(&after, run.target, base, &plan, &held);
        let result = if applied.failed.is_some() {
            Outcome::Failed
        } else if drift.is_empty() {
            Outcome::Converged
        } else {
            Outcome::Drifted
        };
        let base = match run.kind {
            LedgerKind::Deploy => next_base(run.live, run.target, &after, &plan, &held),
            _ => request.base.clone(),
        };
        let deferred: Vec<String> = applied.deferred.clone();
        let receipt = Receipt {
            id: Uuid::new_v4(),
            result,
            applied,
            drift,
            backups,
            needs_secrets: plan.needs_secrets.clone(),
            needs_credentials: plan.needs_credentials.clone(),
            base,
        };
        let (before, saved) = match &run.edit {
            Some(edit) => (
                Some(edit.before.clone().unwrap_or(Json::Null)),
                Some(
                    crate::registry::saved(&after, run.target, &edit.path)
                        .map_or(Json::Null, |(_, value)| value),
                ),
            ),
            None => (None, None),
        };
        let counts: serde_json::Map<String, Json> = plan
            .counts()
            .into_iter()
            .map(|(class, n)| (class_name(class).to_owned(), json!(n)))
            .collect();
        record(
            catalog,
            &Deployment {
                id: receipt.id,
                kind: run.kind,
                result,
                commit: request.commit.clone(),
                branch: request.branch.clone(),
                actor: request.actor.clone(),
                source: request.source,
                summary: json!({
                    "path": run.edit.as_ref().map(|e| e.path.clone()),
                    "plan_digest": plan.digest,
                    "counts": counts,
                    "applied": receipt.applied.applied,
                    "not_reached": receipt.applied.not_reached,
                    "failed": receipt.applied.failed,
                    "notes": receipt.applied.notes,
                    "drift": receipt.drift,
                    "needs_secrets": receipt.needs_secrets,
                    "needs_credentials": receipt.needs_credentials,
                }),
                deferred,
                backups: receipt.backups.clone(),
                before,
                after: saved,
                started_at,
                finished_at: Utc::now(),
            },
        )
        .await?;
        Ok((receipt, after))
    }

    /// The safety backup (§5): every table's structure, and the rows of each
    /// table the plan drops a table or a field of. Returns its file.
    async fn safety_backup(&self, plan: &Plan) -> Result<String> {
        let catalog = self.executor.catalog;
        let mut dropping: BTreeSet<String> = BTreeSet::new();
        for op in plan.operations.iter().filter(|op| op.ticked) {
            if op.class != OpClass::DataDestructive {
                continue;
            }
            if let crate::plan::OpBody::Schema { batch, .. } = &op.body {
                for operation in batch {
                    match operation {
                        sc_api::schema_edit::Operation::DropTable { table }
                        | sc_api::schema_edit::Operation::DropField { table, .. } => {
                            dropping.insert(table.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        let available = sc_backup::available(catalog).await?;
        let mut selection = sc_backup::Selection::everything(&available);
        selection.table_data.retain(|t| dropping.contains(t));
        // Structure and the rows about to go, not the installation: no
        // accounts, no store files, no fits.
        selection.users = false;
        selection.file_stores.clear();
        selection.fits = false;
        let dir = self.backup_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::file(format!("creating {}: {e}", dir.display())))?;
        let name = format!(
            "safety-{}-{}.zip",
            Utc::now().format("%Y%m%dT%H%M%SZ"),
            &Uuid::new_v4().simple().to_string()[..8]
        );
        let path = dir.join(&name);
        let file = std::fs::File::create(&path)
            .map_err(|e| Error::file(format!("creating {}: {e}", path.display())))?;
        sc_backup::write_backup_to(catalog, &selection, None, file).await?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// Where safety backups go: `<data dir>/sync-backups`.
    pub fn backup_dir(&self) -> PathBuf {
        self.executor.data_dir.join("sync-backups")
    }
}

/// What one run is of.
pub(crate) struct Run<'r> {
    pub(crate) kind: LedgerKind,
    pub(crate) request: &'r Request<'r>,
    pub(crate) live: &'r StructureTree,
    pub(crate) target: &'r StructureTree,
    /// For a registry save, which runs over instance trees: the file saved and
    /// what it was before, for the ledger.
    pub(crate) edit: Option<EditRun>,
}

/// The registry save a run is.
pub(crate) struct EditRun {
    /// The file saved.
    pub(crate) path: String,
    /// Its JSON before, if it was there.
    pub(crate) before: Option<Json>,
}

/// Give the lock back whatever happened, keeping the run's own error first.
pub(crate) async fn release<T>(
    catalog: &sc_catalog::Catalog,
    lock: SyncLock,
    result: Result<T>,
) -> Result<T> {
    let released = lock.release(catalog).await;
    let receipt = result?;
    released?;
    Ok(receipt)
}

/// A full backup through every backup schedule (§5), each to its
/// destination. Returns where each one went.
async fn full_backup(catalog: &sc_catalog::Catalog) -> Result<Vec<String>> {
    let schedules = sc_backup::schedule::load_schedules(catalog).await?;
    if schedules.is_empty() {
        return Err(Error::invalid(
            "this instance's policy asks for a full backup before a deploy, and there is no \
             backup schedule to send it to; add one under Backups, then deploy again",
        ));
    }
    let mut out = Vec::new();
    for schedule in &schedules {
        out.push(sc_backup::schedule::run_schedule(catalog, schedule, Utc::now()).await?);
    }
    Ok(out)
}

/// Where live still differs from `target` after a run, leaving out the
/// entities whose operations were not applied (§6 step 9): the operations of
/// a fresh plan, which normalises exactly as the first one did. With no base,
/// the trees are instance trees (a registry save).
fn drift(
    after: &StructureTree,
    target: &StructureTree,
    base: Option<&StructureTree>,
    plan: &Plan,
    held: &BTreeSet<String>,
) -> Vec<String> {
    let held_entities: BTreeSet<&str> = plan
        .operations
        .iter()
        .filter(|op| held.contains(&op.id))
        .map(|op| op.entity.as_str())
        .collect();
    let again = match base {
        Some(base) => crate::plan::plan(after, target, Some(base)),
        None => plan_instance(after, target),
    };
    again
        .operations
        .iter()
        .filter(|op| !held_entities.contains(op.entity.as_str()))
        .map(|op| op.summary.clone())
        .collect()
}

/// The base after a deploy: the target, except that each entity with an
/// operation that was not applied takes the version live has now — the old
/// one, for an entity nothing was applied to — so a deferred delete is still
/// "incoming delete" and not a local change (§5).
pub fn next_base(
    before: &StructureTree,
    target: &StructureTree,
    after: &StructureTree,
    plan: &Plan,
    held: &BTreeSet<String>,
) -> StructureTree {
    let held_entities: BTreeSet<&str> = plan
        .operations
        .iter()
        .filter(|op| held.contains(&op.id))
        .map(|op| op.entity.as_str())
        .collect();
    let mut base = target.clone();
    if held_entities.is_empty() {
        return base;
    }
    let sides = [
        Entities::of(before),
        Entities::of(target),
        Entities::of(after),
    ];
    for group in match_sides(&sides) {
        let member = |side: usize| group.members[side].map(|i| &sides[side].list[i]);
        let is_held = [member(0), member(1)]
            .into_iter()
            .flatten()
            .any(|e| held_entities.contains(e.key().as_str()));
        if !is_held {
            continue;
        }
        if let Some(t) = member(1) {
            remove_entity(&mut base, t);
        }
        if let Some(a) = member(2) {
            put_entity(&mut base, a);
        }
    }
    base
}

fn class_name(class: OpClass) -> &'static str {
    match class {
        OpClass::Safe => "safe",
        OpClass::MetadataDelete => "metadata_delete",
        OpClass::CanFail => "can_fail",
        OpClass::DataDestructive => "data_destructive",
        OpClass::Unsupported => "unsupported",
    }
}

//! What a plan needs the database to know (APP_SYNC_PROPOSAL §5, §6 step 3).
//!
//! The planner reads only trees. Three questions it cannot answer are asked
//! here, against live, and each one that fails becomes a [`Blocker`] carrying
//! the reason — or, for a role, a change of class:
//!
//! - **Can-fail operations** are run through `schema_edit::apply` with
//!   `dry_run: true`: the same refusals with the same messages and nothing
//!   written, so "312 rows have no value in `status`" reaches the review screen
//!   before anything is applied.
//! - **A role users still hold** is data-destructive to delete, not a metadata
//!   delete, and is unticked.
//! - **A git store whose pin changes** while its working tree has uncommitted
//!   changes or commits its remote does not have blocks, naming the store:
//!   resetting it would lose that work (§3.5).
//!
//! Missing secrets, unsupported changes and uncloneable stores are found by the
//! planner itself, since the trees say all there is to know about them.

use serde_json::Value as Json;

use sc_api::schema_edit::{self, ApplyOptions};
use sc_catalog::{Catalog, load_file_store_by_name};
use sc_error::Result;
use sc_files::{GIT_BACKEND, GitRepo};
use sc_types::Role;

use crate::diff::pin;
use crate::entity::Kind;
use crate::plan::{Blocker, OpBody, OpClass, Plan};

/// Check `plan` against live, adding what blocks it and reclassifying what
/// live shows to be more dangerous.
pub async fn validate(catalog: &Catalog, plan: &mut Plan) -> Result<()> {
    let mut blockers = Vec::new();
    for op in &mut plan.operations {
        match &op.body {
            OpBody::Schema { check, .. } if op.class == OpClass::CanFail && !check.is_empty() => {
                let options = ApplyOptions {
                    dry_run: true,
                    ..ApplyOptions::default()
                };
                if let Err(e) = schema_edit::apply(catalog, check, &options).await {
                    blockers.push(Blocker {
                        op: Some(op.id.clone()),
                        entity: Some(op.entity.clone()),
                        message: format!("cannot {}: {}", op.summary, e.causes()),
                    });
                }
            }
            OpBody::Delete { live } if op.kind == Kind::Role => {
                let Some(level) = live
                    .json()
                    .and_then(|v| v.get("role"))
                    .and_then(Json::as_i64)
                    .and_then(|l| u8::try_from(l).ok())
                else {
                    continue;
                };
                let holders = sc_auth::users_with_role(catalog, Role::of(level)).await?;
                if holders > 0 {
                    op.class = OpClass::DataDestructive;
                    op.ticked = false;
                    op.summary = format!(
                        "{}, which {holders} user{} hold{}",
                        op.summary,
                        if holders == 1 { "" } else { "s" },
                        if holders == 1 { "s" } else { "" },
                    );
                }
            }
            OpBody::Put {
                live: Some(live),
                target,
            } if op.kind == Kind::FileStore && pin(Some(live)) != pin(Some(target)) => {
                if let Some(message) = unsaved_work(catalog, &live.name).await? {
                    blockers.push(Blocker {
                        op: Some(op.id.clone()),
                        entity: Some(op.entity.clone()),
                        message,
                    });
                }
            }
            _ => {}
        }
    }
    plan.blockers.extend(blockers);
    Ok(())
}

/// Why resetting the git store `name` to another commit would lose work, if it
/// would.
async fn unsaved_work(catalog: &Catalog, name: &str) -> Result<Option<String>> {
    let Some(def) = load_file_store_by_name(catalog, name).await? else {
        return Ok(None);
    };
    if def.backend != GIT_BACKEND {
        return Ok(None);
    }
    let status = GitRepo::from_def(&def)?.status().await?;
    if !status.cloned {
        return Ok(None);
    }
    let mut what = Vec::new();
    if !status.changes.is_empty() {
        let n = status.changes.len();
        what.push(format!(
            "{n} uncommitted change{}",
            if n == 1 { "" } else { "s" }
        ));
    }
    if status.ahead > 0 {
        let n = status.ahead;
        what.push(format!(
            "{n} commit{} not pushed to its remote",
            if n == 1 { "" } else { "s" }
        ));
    } else if !status.upstream && !status.last_commit.is_empty() {
        what.push(format!(
            "commits on `{}` that no remote branch has",
            if status.branch.is_empty() {
                "a detached head"
            } else {
                &status.branch
            }
        ));
    }
    if what.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "the git store `{name}` has {}; checking out the pinned commit would lose them. \
         Commit and push them in the store, or discard them, then deploy again",
        what.join(" and ")
    )))
}

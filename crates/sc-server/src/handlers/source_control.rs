//! Source control (APP_SYNC_PROPOSAL §7.2): the link, Status, Pull, Commit,
//! Discard, branches, Deploy and the ledger, over `sc_sync::SourceControl`.

use std::collections::BTreeSet;

use serde::Deserialize;

use super::*;

/// Register this domain's admin handlers on `reg`.
pub(super) fn register(reg: &mut HandlerRegistry, state: &HandlerState) {
    use sc_api::handler_ids as ids;
    reg.handle(ids::GET_SYNC_LINK, typed(state, get_link));
    reg.handle(ids::SET_SYNC_LINK, typed(state, set_link));
    reg.handle(ids::DELETE_SYNC_LINK, typed(state, delete_link));
    reg.handle(ids::GENERATE_SYNC_KEY, typed(state, generate_key));
    reg.handle(ids::TEST_SYNC_LINK, typed(state, test_link));
    reg.handle(ids::GET_SYNC_STATUS, typed(state, status));
    reg.handle(ids::GET_SYNC_ENTITY_DIFF, typed(state, entity_diff));
    reg.handle(ids::SYNC_PULL, typed(state, pull));
    reg.handle(ids::SYNC_COMMIT, typed(state, commit));
    reg.handle(ids::PLAN_SYNC_DISCARD, typed(state, plan_discard));
    reg.handle(ids::SYNC_DISCARD, typed(state, discard));
    reg.handle(ids::LIST_SYNC_BRANCHES, typed(state, branches));
    reg.handle(ids::SWITCH_SYNC_BRANCH, typed(state, switch_branch));
    reg.handle(ids::PLAN_SYNC_DEPLOY, typed(state, plan_deploy));
    reg.handle(ids::SYNC_DEPLOY, typed(state, deploy));
    reg.handle(ids::LIST_DEPLOYMENTS, typed(state, deployments));

    // A structure zip (§7.4) waits in `pending` between its upload and its
    // deploy, as an uploaded backup does between its upload and its restore.
    let pending = sc_backup::PendingUploads::new();
    reg.handle(ids::UPLOAD_STRUCTURE_ZIP, {
        let pending = pending.clone();
        typed(state, move |_state, ctx: HandlerCtx<Admin>, (): ()| {
            let pending = pending.clone();
            async move {
                let file = ctx
                    .raw_file
                    .clone()
                    .ok_or_else(|| Error::invalid("this handler requires an uploaded file"))?;
                // Read once here, so a file that is not a structure zip is
                // refused on the upload rather than at the review.
                let tree = read_upload(&file).await?;
                let id = pending.keep(file)?;
                Ok(json!({ "id": id.to_string(), "files": tree.len() }))
            }
        })
    });
    reg.handle(ids::PLAN_SYNC_ZIP_DEPLOY, {
        let pending = pending.clone();
        typed(
            state,
            move |state, ctx: HandlerCtx<Admin>, body: UploadBody| {
                let pending = pending.clone();
                async move {
                    let zip =
                        read_upload(held(&pending, upload_id(&body.upload)?)?.as_ref()).await?;
                    let sc = source(&state, None)?;
                    let planned = sc.plan_zip(&zip, actor(&ctx), sc_sync::Source::Ui).await?;
                    let policy = sc.link().await?.map(|l| l.policy).unwrap_or_default();
                    let mut out = to_json(&planned.plan)?;
                    out["linked"] = json!(planned.linked);
                    out["policy"] = json!(policy);
                    Ok(out)
                }
            },
        )
    });
    reg.handle(ids::SYNC_ZIP_DEPLOY, {
        let pending = pending.clone();
        typed(
            state,
            move |state, ctx: HandlerCtx<Admin>, body: ZipDeployBody| {
                let pending = pending.clone();
                async move {
                    let id = upload_id(&body.upload)?;
                    let zip = read_upload(held(&pending, id)?.as_ref()).await?;
                    let confirm = sc_sync::Confirm {
                        plan_digest: body.plan_digest,
                        ticked: body.ticked,
                        allow_destructive: false,
                        full_backup: body.full_backup.unwrap_or(false),
                    };
                    let receipt = source(&state, Some(ctx.who.editor()))?
                        .deploy_zip(&zip, &confirm, actor(&ctx), sc_sync::Source::Ui)
                        .await?;
                    // Deployed: the upload is done with. A refused deploy (a stale
                    // plan) keeps it, for the review of the new plan.
                    let _ = pending.take(id);
                    reproject_apps(&state.apps);
                    to_json(&receipt)
                }
            },
        )
    });
}

#[derive(Deserialize)]
struct UploadBody {
    upload: String,
}

#[derive(Deserialize)]
struct ZipDeployBody {
    upload: String,
    plan_digest: String,
    ticked: BTreeSet<String>,
    #[serde(default)]
    full_backup: Option<bool>,
}

fn upload_id(raw: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(raw.trim())
        .map_err(|_| Error::invalid("`upload` must be the id the upload answered"))
}

/// The upload `id` names, still held.
fn held(pending: &sc_backup::PendingUploads, id: uuid::Uuid) -> Result<Arc<sc_backup::SpoolFile>> {
    pending.file(id).map_err(|_| {
        Error::not_found("that uploaded structure zip is no longer held — upload the file again")
    })
}

/// The structure zip an upload holds, read off the async threads.
async fn read_upload(file: &sc_backup::SpoolFile) -> Result<sc_sync::StructureTree> {
    let path = file.path().to_owned();
    // Not `read_zip`, which names the file: this one is a spool file, and
    // its path means nothing to the admin who uploaded it.
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)
            .map_err(|e| Error::file(format!("reading the uploaded zip: {e}")))?;
        sc_sync::read_zip_from(std::io::BufReader::new(file))
    })
    .await
    .map_err(|e| Error::msg(format!("reading the structure zip: {e}")))?
}

/// Source control over the running server, as `editor` (or as an admin read,
/// for what changes no entity).
pub(super) fn source<'a>(
    state: &'a HandlerState,
    editor: Option<sc_catalog::Editor>,
) -> Result<sc_sync::SourceControl<'a>> {
    Ok(sc_sync::SourceControl::new(sc_sync::Deployer::new(
        sc_sync::Executor {
            catalog: &state.catalog,
            host: state.apps.as_ref(),
            editor: editor.unwrap_or(sc_catalog::Editor::Delegated("source control read")),
            data_dir: sc_files::data_dir()?,
        },
    )))
}

/// The signed-in admin's email: who the ledger and a commit's author say.
fn actor(ctx: &HandlerCtx<Admin>) -> Option<String> {
    ctx.who
        .user()
        .and_then(|u| u.get(COL_EMAIL))
        .and_then(sc_query::Value::as_text)
        .map(str::to_owned)
}

/// What a link body is: `{ url, branch, subdirectory, credential, policy }`.
#[derive(Deserialize)]
struct LinkBody {
    url: String,
    branch: String,
    #[serde(default)]
    subdirectory: Option<String>,
    #[serde(default)]
    credential: Option<CredentialBody>,
    #[serde(default)]
    policy: Option<PolicyBody>,
}

#[derive(Deserialize)]
struct CredentialBody {
    kind: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Deserialize)]
struct PolicyBody {
    #[serde(default)]
    destructive: Option<sc_sync::Destructive>,
    #[serde(default)]
    full_backup: Option<sc_sync::FullBackup>,
}

/// The link a body asks for, from the stored link and credential where it
/// leaves something out.
async fn link_from(sc: &sc_sync::SourceControl<'_>, body: LinkBody) -> Result<sc_sync::SyncLink> {
    let catalog = sc.deployer.executor.catalog;
    let stored = sc.credential().await?;
    let credential = match body.credential {
        None => stored,
        Some(c) => match c.kind.as_str() {
            "none" => sc_sync::Credential::None,
            "deploy_key" => match stored {
                key @ sc_sync::Credential::DeployKey { .. } => key,
                _ => {
                    return Err(Error::invalid(
                        "there is no deploy key yet: generate one, give the remote its public \
                         key, then link",
                    ));
                }
            },
            "https_token" => match c.token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                Some(token) => sc_sync::Credential::https_token(
                    catalog,
                    c.username.as_deref().filter(|u| !u.trim().is_empty()),
                    token,
                )?,
                None => match stored {
                    token @ sc_sync::Credential::HttpsToken { .. } => token,
                    _ => return Err(Error::invalid("an HTTPS token link needs the token")),
                },
            },
            other => {
                return Err(Error::invalid(format!(
                    "`{other}` is not a kind of credential: `none`, `deploy_key` or `https_token`"
                )));
            }
        },
    };
    let mut policy = sc.link().await?.map(|l| l.policy).unwrap_or_default();
    if let Some(p) = body.policy {
        if let Some(d) = p.destructive {
            policy.destructive = d;
        }
        if let Some(f) = p.full_backup {
            policy.full_backup = f;
        }
    }
    let mut link = sc_sync::SyncLink::new(body.url, body.branch);
    link.subdirectory = body.subdirectory;
    link.credential = credential;
    link.policy = policy;
    Ok(link)
}

async fn get_link(state: HandlerState, _ctx: HandlerCtx<Admin>, _: ()) -> Result<Json> {
    let sc = source(&state, None)?;
    Ok(json!({
        "link": sc.link().await?,
        "credential": sc.credential().await?,
    }))
}

async fn set_link(state: HandlerState, ctx: HandlerCtx<Admin>, body: LinkBody) -> Result<Json> {
    let sc = source(&state, Some(ctx.who.editor()))?;
    let link = link_from(&sc, body).await?;
    to_json(&sc.set_link(link).await?)
}

async fn delete_link(state: HandlerState, _ctx: HandlerCtx<Admin>, _: ()) -> Result<Json> {
    source(&state, None)?.unlink().await?;
    Ok(json!({ "ok": true }))
}

async fn generate_key(state: HandlerState, _ctx: HandlerCtx<Admin>, _: ()) -> Result<Json> {
    to_json(&source(&state, None)?.generate_key().await?)
}

async fn test_link(state: HandlerState, _ctx: HandlerCtx<Admin>, body: LinkBody) -> Result<Json> {
    let sc = source(&state, None)?;
    let link = link_from(&sc, body).await?;
    Ok(json!({ "branches": sc.test_link(&link).await? }))
}

async fn status(state: HandlerState, _ctx: HandlerCtx<Admin>, _: ()) -> Result<Json> {
    let sc = source(&state, None)?;
    if sc.link().await?.is_none() {
        return Ok(json!({
            "linked": false,
            "link": null,
            "tip": null,
            "base_commit": null,
            "entries": [],
            "deploy_blockers": [],
            "warnings": [],
        }));
    }
    let linked = sc.status().await?;
    let entries: Vec<Json> = linked.status.shown().map(status_entry).collect();
    let blockers: Vec<&str> = linked
        .status
        .deploy_blockers()
        .iter()
        .map(|e| e.key.as_str())
        .collect();
    Ok(json!({
        "linked": true,
        "link": linked.link,
        "tip": linked.tip,
        "base_commit": linked.base_commit,
        "entries": entries,
        "deploy_blockers": blockers,
        "warnings": linked.warnings,
    }))
}

/// One Status row: what it is, its class, and the file it is in.
fn status_entry(entry: &sc_sync::StatusEntry) -> Json {
    let side = entry
        .live
        .as_ref()
        .or(entry.tip.as_ref())
        .or(entry.base.as_ref());
    let application = side.and_then(|e| e.scope.as_deref()).and_then(|s| {
        s.strip_prefix("applications/")
            .and_then(|rest| rest.split('/').next())
    });
    let path = side.map_or("", |e| e.path.as_str());
    json!({
        "key": entry.key,
        "kind": entry.kind,
        "name": entry.name,
        "class": entry.class,
        "application": application,
        "path": path,
        "in_registry": entry.kind != sc_sync::Kind::StoreFile
            && entry.kind != sc_sync::Kind::Other
            && path.ends_with(".json"),
    })
}

async fn entity_diff(state: HandlerState, ctx: HandlerCtx<Admin>, _: ()) -> Result<Json> {
    let key = ctx
        .query_get("key")
        .filter(|k| !k.is_empty())
        .ok_or_else(|| Error::invalid("`key` names the Status entry to compare"))?;
    to_json(&source(&state, None)?.entity_diff(key).await?)
}

async fn pull(state: HandlerState, _ctx: HandlerCtx<Admin>, _: ()) -> Result<Json> {
    to_json(&source(&state, None)?.pull().await?)
}

#[derive(Deserialize)]
struct CommitBody {
    message: String,
    #[serde(default)]
    keys: Option<Vec<String>>,
    #[serde(default)]
    keep_mine: Option<Vec<String>>,
}

async fn commit(state: HandlerState, ctx: HandlerCtx<Admin>, body: CommitBody) -> Result<Json> {
    let request = sc_sync::CommitRequest {
        message: body.message,
        keys: body.keys,
        keep_mine: body.keep_mine.unwrap_or_default().into_iter().collect(),
        actor: actor(&ctx),
        source: Some(sc_sync::Source::Ui),
    };
    to_json(
        &source(&state, Some(ctx.who.editor()))?
            .commit(&request)
            .await?,
    )
}

#[derive(Deserialize)]
struct KeysBody {
    keys: Vec<String>,
}

async fn plan_discard(
    state: HandlerState,
    _ctx: HandlerCtx<Admin>,
    body: KeysBody,
) -> Result<Json> {
    let keys: Vec<&str> = body.keys.iter().map(String::as_str).collect();
    to_json(&source(&state, None)?.plan_discard(&keys).await?)
}

#[derive(Deserialize)]
struct DiscardBody {
    keys: Vec<String>,
    plan_digest: String,
    ticked: BTreeSet<String>,
}

async fn discard(state: HandlerState, ctx: HandlerCtx<Admin>, body: DiscardBody) -> Result<Json> {
    let keys: Vec<&str> = body.keys.iter().map(String::as_str).collect();
    let confirm = sc_sync::Confirm {
        plan_digest: body.plan_digest,
        ticked: body.ticked,
        allow_destructive: false,
        full_backup: false,
    };
    let receipt = source(&state, Some(ctx.who.editor()))?
        .discard(&keys, &confirm, actor(&ctx), sc_sync::Source::Ui)
        .await?;
    reproject_apps(&state.apps);
    to_json(&receipt)
}

async fn branches(state: HandlerState, _ctx: HandlerCtx<Admin>, _: ()) -> Result<Json> {
    let sc = source(&state, None)?;
    let current = sc
        .link()
        .await?
        .map(|l| l.branch)
        .ok_or_else(|| Error::invalid("this installation is not linked to a git repository"))?;
    Ok(json!({ "current": current, "branches": sc.branches().await? }))
}

#[derive(Deserialize)]
struct BranchBody {
    name: String,
    #[serde(default)]
    create: Option<bool>,
}

async fn switch_branch(
    state: HandlerState,
    _ctx: HandlerCtx<Admin>,
    body: BranchBody,
) -> Result<Json> {
    let sc = source(&state, None)?;
    let name = body.name.trim();
    let pulled = match body.create.unwrap_or(false) {
        true => sc.create_branch(name).await?,
        false => sc.switch_branch(name).await?,
    };
    to_json(&pulled)
}

async fn plan_deploy(state: HandlerState, ctx: HandlerCtx<Admin>, _: ()) -> Result<Json> {
    let sc = source(&state, None)?;
    let planned = sc.plan_deploy(actor(&ctx), sc_sync::Source::Ui).await?;
    let policy = sc.link().await?.map(|l| l.policy).unwrap_or_default();
    let mut out = to_json(&planned.plan)?;
    out["commit"] = json!(planned.commit);
    out["policy"] = json!(policy);
    Ok(out)
}

#[derive(Deserialize)]
struct DeployBody {
    #[serde(default)]
    commit: Option<String>,
    plan_digest: String,
    ticked: BTreeSet<String>,
    #[serde(default)]
    full_backup: Option<bool>,
}

async fn deploy(state: HandlerState, ctx: HandlerCtx<Admin>, body: DeployBody) -> Result<Json> {
    let confirm = sc_sync::Confirm {
        plan_digest: body.plan_digest,
        ticked: body.ticked,
        allow_destructive: false,
        full_backup: body.full_backup.unwrap_or(false),
    };
    let receipt = source(&state, Some(ctx.who.editor()))?
        .deploy(
            body.commit.as_deref(),
            &confirm,
            actor(&ctx),
            sc_sync::Source::Ui,
        )
        .await?;
    // A deploy changes what applications' APIs are projected from.
    reproject_apps(&state.apps);
    to_json(&receipt)
}

async fn deployments(state: HandlerState, ctx: HandlerCtx<Admin>, _: ()) -> Result<Json> {
    let limit = match ctx.query_get("limit") {
        None | Some("") => 50,
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|_| Error::invalid("`limit` must be a whole number"))?
            .clamp(1, 500),
    };
    Ok(json!({
        "deployments": sc_sync::list_deployments(&state.catalog, limit).await?,
    }))
}

fn to_json(value: &impl serde::Serialize) -> Result<Json> {
    serde_json::to_value(value).map_err(|e| Error::msg(format!("the answer: {e}")))
}

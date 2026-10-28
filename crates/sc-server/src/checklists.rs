//! The checklists admin API (TODO-checklists C1.11): checklists, their steps,
//! their runs, the agents' proposals, and check users.
//!
//! A person may do anything here: every change goes through `sc_check::edit`
//! as [`Editor::Human`], which is where the agent policy of §4 lives too, so
//! the screen and the agent tools cannot disagree about what a change is. A
//! run started here is triggered `manual`, against the live mounts, with this
//! server's application router for the `http.*` steps.

use std::sync::Arc;

use sc_auth::{COL_EMAIL, USERS_TABLE, User};
use sc_catalog::Catalog;
use sc_check::{
    CheckEnv, ChecklistInput, ChecklistScope, Editor, ProposalChange, StepDraft, StepRegistry,
    edit, store,
};
use sc_error::{Error, Result};
use sc_query::{Expr, OrderBy, Projection, Select, Source, Statement};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::apps::AppMounts;
use crate::handler::{HandlerCtx, HandlerRegistry, HandlerResponse};

/// How many runs a checklist's detail carries.
const DETAIL_RUNS: u64 = 10;
/// How many runs `listChecklistRuns` returns when it names no limit.
const DEFAULT_RUNS: u64 = 50;

pub(crate) fn register(reg: &mut HandlerRegistry, catalog: Arc<Catalog>, apps: Arc<AppMounts>) {
    let registry = Arc::new(StepRegistry::builtin());

    macro_rules! handler {
        ($name:literal, |$ctx:ident, $catalog:ident, $registry:ident, $apps:ident| $body:expr) => {
            reg.register($name, {
                let catalog = catalog.clone();
                let registry = registry.clone();
                let apps = apps.clone();
                move |$ctx: HandlerCtx| {
                    let $catalog = catalog.clone();
                    let $registry = registry.clone();
                    let $apps = apps.clone();
                    async move {
                        let _ = (&$catalog, &$registry, &$apps, &$ctx);
                        let value: Json = $body;
                        Ok(HandlerResponse::ok(value))
                    }
                }
            });
        };
    }

    handler!("listChecklists", |ctx, catalog, registry, apps| {
        let scope = scope(ctx.query_get("application"))?;
        let mut out = Vec::new();
        for checklist in store::list_checklists(&catalog, scope).await? {
            let steps = store::current_steps(&catalog, checklist.id).await?;
            let unaccepted = steps
                .iter()
                .filter(|s| s.author.is_agent() && !s.accepted)
                .count();
            let proposals = store::proposals_of(&catalog, checklist.id).await?.len();
            let last = store::last_run(&catalog, checklist.id).await?;
            let mut item = checklist.to_json();
            item["steps"] = json!(steps.len());
            item["pending"] = json!(unaccepted + proposals);
            item["last_run"] = last.map_or(Json::Null, |run| {
                json!({
                    "id": run.id.to_string(),
                    "status": run.status.as_str(),
                    "finished_at": run.finished_at.to_rfc3339(),
                })
            });
            out.push(item);
        }
        Json::Array(out)
    });

    handler!("createChecklist", |ctx, catalog, registry, apps| {
        let input = checklist_input(&ctx.body)?;
        let steps = match ctx.body.get("steps") {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Array(items)) => items
                .iter()
                .map(StepDraft::from_json)
                .collect::<Result<_>>()?,
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`steps` should be a list, got {other}"
                )));
            }
        };
        let checklist =
            edit::create_checklist(&catalog, &registry, &editor(&ctx)?, input, steps).await?;
        detail(&catalog, checklist.id).await?
    });

    handler!("getChecklist", |ctx, catalog, registry, apps| {
        detail(&catalog, id(&ctx, "id")?).await?
    });

    handler!("updateChecklist", |ctx, catalog, registry, apps| {
        let id = id(&ctx, "id")?;
        let input = checklist_input(&ctx.body)?;
        edit::update_checklist(&catalog, &registry, &editor(&ctx)?, id, input).await?;
        detail(&catalog, id).await?
    });

    handler!("deleteChecklist", |ctx, catalog, registry, apps| {
        edit::delete_checklist(&catalog, &editor(&ctx)?, id(&ctx, "id")?).await?;
        json!({ "deleted": true })
    });

    handler!("addChecklistStep", |ctx, catalog, registry, apps| {
        let checklist = id(&ctx, "id")?;
        let mut body = ctx.body.clone();
        let before = match body.as_object_mut().and_then(|o| o.remove("before")) {
            None | Some(Json::Null) => None,
            Some(Json::String(s)) => Some(parse_uuid(&s, "before")?),
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`before` should be a step id, got {other}"
                )));
            }
        };
        let draft = StepDraft::from_json(&body)?;
        let editor = editor(&ctx)?;
        let step = match before {
            Some(before) => {
                let anchor = store::require_step(&catalog, before).await?;
                if anchor.checklist != checklist {
                    return Err(Error::invalid("`before` is a step of another checklist"));
                }
                edit::insert_step_before(&catalog, &registry, &editor, before, draft).await?
            }
            None => edit::append_steps(&catalog, &registry, &editor, checklist, vec![draft])
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| Error::msg("no step was written"))?,
        };
        step.to_json()
    });

    handler!("reorderChecklistSteps", |ctx, catalog, registry, apps| {
        let checklist = id(&ctx, "id")?;
        let order = sc_check::model::parse_ids(
            ctx.body.get("order").unwrap_or(&Json::Null),
            "order",
        )?;
        edit::request_change(
            &catalog,
            &registry,
            &editor(&ctx)?,
            checklist,
            ProposalChange::Reorder { order },
            "",
        )
        .await?;
        detail(&catalog, checklist).await?
    });

    handler!("runChecklist", |ctx, catalog, registry, apps| {
        let checklist = store::require_checklist(&catalog, id(&ctx, "id")?).await?;
        let run = with_env(&catalog, &registry, &apps, |env| async move {
            sc_check::run_checklist(&env, &checklist, "manual").await
        })
        .await?;
        run.to_json()
    });

    handler!("listChecklistRuns", |ctx, catalog, registry, apps| {
        let checklist = id(&ctx, "id")?;
        let limit = match ctx.query_get("limit") {
            None | Some("") => DEFAULT_RUNS,
            Some(raw) => raw
                .parse::<u64>()
                .map_err(|_| Error::invalid(format!("`limit` `{raw}` is not a number")))?
                .clamp(1, 500),
        };
        let runs = store::runs_of(&catalog, checklist, limit).await?;
        Json::Array(runs.iter().map(|r| r.to_json()).collect())
    });

    handler!("runChecklists", |ctx, catalog, registry, apps| {
        let scope = scope(ctx.body.get("application").and_then(Json::as_str))?;
        let runs = with_env(&catalog, &registry, &apps, |env| async move {
            sc_check::run_checklists(&env, scope, "manual").await
        })
        .await?;
        Json::Array(runs.iter().map(|r| r.to_json()).collect())
    });

    handler!("getChecklistRun", |ctx, catalog, registry, apps| {
        let id = id(&ctx, "id")?;
        store::load_run(&catalog, id)
            .await?
            .ok_or_else(|| Error::not_found(format!("no checklist run with id {id}")))?
            .to_json()
    });

    handler!("updateChecklistStep", |ctx, catalog, registry, apps| {
        let draft = StepDraft::from_json(&ctx.body)?;
        edit::edit_step(&catalog, &registry, &editor(&ctx)?, id(&ctx, "id")?, draft)
            .await?
            .to_json()
    });

    handler!("deleteChecklistStep", |ctx, catalog, registry, apps| {
        edit::remove_step(&catalog, &registry, &editor(&ctx)?, id(&ctx, "id")?).await?;
        json!({ "deleted": true })
    });

    handler!("acceptChecklistStep", |ctx, catalog, registry, apps| {
        edit::accept_step(&catalog, &editor(&ctx)?, id(&ctx, "id")?)
            .await?
            .to_json()
    });

    handler!("acceptChecklistProposal", |ctx, catalog, registry, apps| {
        let id = id(&ctx, "id")?;
        let proposal = store::load_proposal(&catalog, id)
            .await?
            .ok_or_else(|| Error::not_found(format!("no proposal with id {id}")))?;
        edit::accept_proposal(&catalog, &registry, &editor(&ctx)?, id).await?;
        detail(&catalog, proposal.checklist).await?
    });

    handler!("rejectChecklistProposal", |ctx, catalog, registry, apps| {
        let id = id(&ctx, "id")?;
        let proposal = store::load_proposal(&catalog, id)
            .await?
            .ok_or_else(|| Error::not_found(format!("no proposal with id {id}")))?;
        edit::reject_proposal(&catalog, &editor(&ctx)?, id).await?;
        detail(&catalog, proposal.checklist).await?
    });

    handler!("listChecklistStepTypes", |ctx, catalog, registry, apps| {
        let with_app = ctx
            .query_get("application")
            .is_some_and(|a| !a.is_empty() && a != "none");
        Json::Array(
            registry
                .offered(with_app)
                .iter()
                .map(|t| StepRegistry::describe(t.as_ref()))
                .collect(),
        )
    });

    handler!("setCheckUser", |ctx, catalog, registry, apps| {
        let on = ctx
            .body
            .get("check_user")
            .and_then(Json::as_bool)
            .ok_or_else(|| Error::invalid("`check_user` should be true or false"))?;
        let user = sc_auth::set_check_user(&catalog, id(&ctx, "id")?, on).await?;
        json!({ "id": user.id.to_string(), "check_user": user.is_check_user() })
    });

    handler!("listChecklistUsers", |ctx, catalog, registry, apps| {
        let mut select =
            Select::from(Source::table(USERS_TABLE)).columns(vec![Projection::all()]);
        select.order = vec![OrderBy::asc(Expr::col(COL_EMAIL))];
        let rows = catalog
            .primary()
            .query(&Statement::from(select))
            .await?
            .try_collect()
            .await?;
        let mut out = Vec::new();
        for row in &rows {
            let user = User::from_row(row)?;
            out.push(json!({
                "id": user.id.to_string(),
                "email": user.get(COL_EMAIL).and_then(sc_query::Value::as_text).unwrap_or_default(),
                "role": i64::from(user.role),
                "check_user": user.is_check_user(),
            }));
        }
        Json::Array(out)
    });
}

/// Run `f` with this server's capabilities: its trigger dispatcher and
/// evaluator, and the application router where it has one.
async fn with_env<'a, T, F, Fut>(
    catalog: &'a Catalog,
    registry: &'a StepRegistry,
    apps: &'a AppMounts,
    f: F,
) -> Result<T>
where
    F: FnOnce(CheckEnv<'a>) -> Fut,
    Fut: std::future::Future<Output = Result<T>> + 'a,
{
    let requests = apps
        .agents()
        .and_then(|a| a.registry().view_services().requests())
        .map(|r| r.as_ref() as &dyn sc_agent::AppRequester);
    let env = CheckEnv::new(catalog, registry)
        .with_triggers(apps.triggers())
        .with_requests(requests);
    f(env).await
}

/// A checklist with its steps, proposals and recent runs.
async fn detail(catalog: &Catalog, id: Uuid) -> Result<Json> {
    let checklist = store::require_checklist(catalog, id).await?;
    let steps = store::current_steps(catalog, id).await?;
    let proposals = store::proposals_of(catalog, id).await?;
    let runs = store::runs_of(catalog, id, DETAIL_RUNS).await?;
    let mut out = checklist.to_json();
    out["steps"] = json!(steps.iter().map(|s| s.to_json()).collect::<Vec<_>>());
    out["proposals"] = json!(proposals.iter().map(|p| p.to_json()).collect::<Vec<_>>());
    out["runs"] = json!(runs.iter().map(|r| r.to_json()).collect::<Vec<_>>());
    Ok(out)
}

/// The person making the change.
fn editor(ctx: &HandlerCtx) -> Result<Editor> {
    ctx.user
        .as_ref()
        .map(|u| Editor::Human(u.id))
        .ok_or_else(|| Error::auth("sign in to change checklists"))
}

fn id(ctx: &HandlerCtx, name: &str) -> Result<Uuid> {
    parse_uuid(ctx.path_param(name)?, name)
}

fn parse_uuid(raw: &str, what: &str) -> Result<Uuid> {
    raw.trim()
        .parse()
        .map_err(|_| Error::invalid(format!("`{what}` `{raw}` is not an id")))
}

/// `application=<id>`, `application=none`, or nothing.
fn scope(application: Option<&str>) -> Result<ChecklistScope> {
    Ok(match application.map(str::trim) {
        None | Some("") => ChecklistScope::All,
        Some("none") => ChecklistScope::DataOnly,
        Some(raw) => ChecklistScope::Application(parse_uuid(raw, "application")?),
    })
}

fn checklist_input(body: &Json) -> Result<ChecklistInput> {
    let obj = body
        .as_object()
        .ok_or_else(|| Error::invalid("the body should be an object"))?;
    let text = |key: &str| obj.get(key).and_then(Json::as_str).unwrap_or("").to_owned();
    let uuid = |key: &str| -> Result<Option<Uuid>> {
        match obj.get(key) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Json::String(s)) => parse_uuid(s, key).map(Some),
            Some(other) => Err(Error::invalid(format!(
                "`{key}` should be an id, got {other}"
            ))),
        }
    };
    let attributes = match obj.get("attributes") {
        None | Some(Json::Null) => sc_types::Attrs::new(),
        Some(Json::Object(map)) => map.clone(),
        Some(other) => {
            return Err(Error::invalid(format!(
                "`attributes` should be an object, got {other}"
            )));
        }
    };
    Ok(ChecklistInput {
        name: text("name"),
        description: text("description"),
        application: uuid("application")?,
        run_as: uuid("run_as")?,
        enabled: obj.get("enabled").and_then(Json::as_bool).unwrap_or(true),
        attributes,
    })
}

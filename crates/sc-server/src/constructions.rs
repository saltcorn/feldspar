//! The App Constructor's admin API (TODO-app-constructor AC1, AC2): an
//! application's construction, created with the agent that plans it and builds
//! its data model, read with its phases and tasks, and deleted with both.
//!
//! Planning (`planConstruction`) is one run of the construction agent, driven
//! in a task of its own rather than in the request: it takes as long as the
//! model does. Its run id is kept on the construction, and while it is driven
//! the construction reads `planning: true`. A plan run left `running` by a
//! server that stopped is not being driven, so it reads `false` and the admin
//! can plan again.
//!
//! Running (`runConstruction`) starts the construction's runner
//! ([`crate::construction_runner`]), a server task that plans each phase's
//! tasks when it reaches the phase and runs them. `stopConstruction` stops it;
//! `retryTask` and `skipTask` deal with a failed task and carry on.
//!
//! **React applications only** for now: a feature task is delegated to the
//! application's builder agent, and the other frameworks' builders are not what
//! the constructor's prompts are written for. Saltcorn UI applications have no
//! builder at all, and wait for view and page tools (TODO.md O1.3).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use sc_agent::{Conclusion, ModelRole, RunCaller, Runner};
use sc_app::{AppId, Application, builder_agent_name, load_application};
use sc_catalog::Catalog;
use sc_construct::{
    Construction, ConstructionStatus, PhaseStatus, PhaseWithTasks, Task, TaskStatus, store,
};
use sc_error::{Error, Result};
use sc_llm::list_llm_models;
use serde_json::{Value as Json, json};

use crate::apps::AppMounts;
use crate::handler::HandlerRegistry;
use crate::handler::HandlerResponse;

/// The trait that gives the construction agent its tools: planning, and
/// building the data model.
const CONSTRUCTOR_TRAIT: &str = "app_constructor";

/// The constructions whose plan run this process is driving, by id.
static PLANNING: Mutex<BTreeSet<uuid::Uuid>> = Mutex::new(BTreeSet::new());

fn planning(construction: uuid::Uuid) -> bool {
    PLANNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&construction)
}

/// Marks a construction as being planned for as long as it is held.
struct PlanningGuard(uuid::Uuid);

impl PlanningGuard {
    /// `None` when the construction is being planned already.
    fn claim(construction: uuid::Uuid) -> Option<PlanningGuard> {
        PLANNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(construction)
            .then_some(PlanningGuard(construction))
    }
}

impl Drop for PlanningGuard {
    fn drop(&mut self) {
        PLANNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

pub(crate) fn register(reg: &mut HandlerRegistry, catalog: Arc<Catalog>, apps: Arc<AppMounts>) {
    reg.register_admin("listConstructions", {
        let catalog = catalog.clone();
        move |_ctx| {
            let catalog = catalog.clone();
            async move {
                let mut out = Vec::new();
                for construction in store::list_constructions(&catalog).await? {
                    let phases = store::phases_with_tasks(&catalog, construction.id).await?;
                    let done = phases
                        .iter()
                        .filter(|p| p.status() == PhaseStatus::Done)
                        .count();
                    let mut item = status_json(&construction);
                    item["phases"] = json!(phases.len());
                    item["phases_done"] = json!(done);
                    out.push(item);
                }
                Ok(HandlerResponse::ok(Json::Array(out)))
            }
        }
    });

    reg.register_admin("getConstruction", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let construction = store::require_construction(&catalog, app.id.0).await?;
                Ok(HandlerResponse::ok(
                    construction_json(&catalog, &construction).await?,
                ))
            }
        }
    });

    reg.register_admin("createConstruction", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                require_constructible(&app)?;
                if store::load_construction(&catalog, app.id.0)
                    .await?
                    .is_some()
                {
                    return Err(Error::invalid(format!(
                        "`{}` already has a construction",
                        app.name
                    )));
                }
                let provider = ctx
                    .body
                    .get("provider")
                    .and_then(Json::as_str)
                    .or_else(|| chosen_provider(&app));
                let agent = create_construction_agent(&catalog, &apps, &app, provider).await?;
                let construction =
                    store::create_construction(&catalog, app.id.0, Some(&agent)).await?;
                Ok(
                    HandlerResponse::ok(construction_json(&catalog, &construction).await?)
                        .with_status(201),
                )
            }
        }
    });

    reg.register_admin("saveConstructionSpecification", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let specification = ctx
                    .body
                    .get("specification")
                    .and_then(Json::as_str)
                    .ok_or_else(|| Error::invalid("`specification` must be text"))?;
                let construction =
                    store::save_specification(&catalog, app.id.0, specification).await?;
                Ok(HandlerResponse::ok(
                    construction_json(&catalog, &construction).await?,
                ))
            }
        }
    });

    reg.register_admin("planConstruction", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let user = ctx
                    .user()
                    .cloned()
                    .ok_or_else(|| Error::invalid("planning needs a signed-in admin"))?;
                let construction =
                    start_planning(catalog.clone(), apps, &app, RunCaller::user(user)).await?;
                Ok(
                    HandlerResponse::ok(construction_json(&catalog, &construction).await?)
                        .with_status(202),
                )
            }
        }
    });

    reg.register_admin("savePhases", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let construction = store::require_construction(&catalog, app.id.0).await?;
                if planning(construction.id) {
                    return Err(Error::invalid(
                        "the construction agent is planning the phases; wait for it to finish",
                    ));
                }
                let phases =
                    sc_construct::parse_phases(ctx.body.get("phases").unwrap_or(&Json::Null))?;
                let saved = sc_construct::save_phases(&catalog, app.id.0, &phases).await?;
                Ok(HandlerResponse::ok(
                    construction_json(&catalog, &saved.construction).await?,
                ))
            }
        }
    });

    register_runner(reg, &catalog, &apps);

    reg.register_admin("deleteConstruction", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                if let Some(construction) = store::load_construction(&catalog, app.id.0).await? {
                    require_idle(&construction)?;
                }
                let agent = delete_construction_of(&catalog, &app).await?;
                Ok(HandlerResponse::ok(json!({
                    "deleted": agent.is_some(),
                    "agent": agent.flatten(),
                })))
            }
        }
    });
}

/// The runner's endpoints (AC3.4): run, stop, and retry or skip a task.
fn register_runner(reg: &mut HandlerRegistry, catalog: &Arc<Catalog>, apps: &Arc<AppMounts>) {
    reg.register_admin("runConstruction", {
        let catalog = catalog.clone();
        let apps = apps.clone();
        move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let user = ctx
                    .user()
                    .cloned()
                    .ok_or_else(|| Error::invalid("running needs a signed-in admin"))?;
                let construction = store::require_construction(&catalog, app.id.0).await?;
                if planning(construction.id) {
                    return Err(Error::invalid(
                        "the construction agent is planning the phases; wait for it to finish",
                    ));
                }
                if construction.status == ConstructionStatus::Running {
                    return Err(Error::invalid("the construction is running already"));
                }
                let phases = store::phases_of(&catalog, construction.id).await?;
                if phases.is_empty() {
                    return Err(Error::invalid(
                        "the construction has no phases yet; plan them first",
                    ));
                }
                let until = match ctx.body.get("until_phase") {
                    None | Some(Json::Null) => None,
                    Some(Json::String(raw)) => {
                        let id = uuid::Uuid::parse_str(raw)
                            .ok()
                            .filter(|id| phases.iter().any(|p| p.id == *id))
                            .ok_or_else(|| {
                                Error::invalid(format!(
                                    "`{raw}` is not a phase of this construction"
                                ))
                            })?;
                        Some(id)
                    }
                    Some(other) => {
                        return Err(Error::invalid(format!(
                            "`until_phase` must be a phase id, got {other}"
                        )));
                    }
                };
                crate::construction_runner::require_idle(construction.id)?;
                use_chosen_provider(&catalog, &apps, &app, &construction).await?;
                let construction =
                    store::mark_running(&catalog, app.id.0, user.id(), until).await?;
                crate::construction_runner::start(catalog.clone(), apps, &construction);
                Ok(
                    HandlerResponse::ok(construction_json(&catalog, &construction).await?)
                        .with_status(202),
                )
            }
        }
    });

    reg.register_admin("stopConstruction", {
        let catalog = catalog.clone();
        move |ctx| {
            let catalog = catalog.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let construction = store::require_construction(&catalog, app.id.0).await?;
                if construction.status != ConstructionStatus::Running {
                    return Err(Error::invalid("the construction is not running"));
                }
                // If this process is running the construction, ask its runner
                // to stop. The runner aborts the current run and sets the
                // status to `stopped` itself.
                //
                // If no runner is running it here, the construction was left
                // `running` by a server that stopped and has not resumed it
                // yet. Then nothing is running, so it is stopped right here:
                // the interrupted task's run is aborted as the runner would
                // abort it, so it does not stay `running` in the run history.
                //
                // One server process per database is assumed: another
                // instance's runner would not see this stop (see
                // `construction_runner`'s docs).
                if !crate::construction_runner::request_stop(construction.id) {
                    stop_without_runner(&catalog, app.id.0, construction.id).await?;
                }
                let construction = store::require_construction(&catalog, app.id.0).await?;
                Ok(HandlerResponse::ok(
                    construction_json(&catalog, &construction).await?,
                ))
            }
        }
    });

    for (handler, retry) in [("retryTask", true), ("skipTask", false)] {
        let catalog = catalog.clone();
        let apps = apps.clone();
        reg.register_admin(handler, move |ctx| {
            let catalog = catalog.clone();
            let apps = apps.clone();
            async move {
                let app = require_app(&catalog, ctx.path_param("id")?).await?;
                let user = ctx
                    .user()
                    .cloned()
                    .ok_or_else(|| Error::invalid("this needs a signed-in admin"))?;
                let construction = store::require_construction(&catalog, app.id.0).await?;
                // The runner writes the tasks while it runs, so a task is
                // changed here only when it is not running: neither by its
                // status, nor by a runner still winding down after a stop.
                if construction.status == ConstructionStatus::Running {
                    return Err(Error::invalid(
                        "the construction is running; stop it before you retry or skip a task",
                    ));
                }
                crate::construction_runner::require_idle(construction.id)?;
                let mut task =
                    require_task(&catalog, &construction, ctx.path_param("task")?).await?;
                if retry {
                    if task.status != TaskStatus::Failed {
                        return Err(Error::invalid(format!(
                            "task `{}` has not failed, so there is nothing to retry",
                            task.title
                        )));
                    }
                    task.status = TaskStatus::Todo;
                    task.summary = None;
                } else {
                    if !matches!(task.status, TaskStatus::Todo | TaskStatus::Failed) {
                        return Err(Error::invalid(format!(
                            "task `{}` is not waiting or failed, so it cannot be skipped",
                            task.title
                        )));
                    }
                    task.status = TaskStatus::Skipped;
                }
                store::update_task(&catalog, &task).await?;
                // A construction the task failed carries on.
                let construction = if construction.status == ConstructionStatus::Failed {
                    use_chosen_provider(&catalog, &apps, &app, &construction).await?;
                    let construction = store::mark_running(
                        &catalog,
                        app.id.0,
                        user.id(),
                        construction.until_phase,
                    )
                    .await?;
                    crate::construction_runner::start(catalog.clone(), apps, &construction);
                    construction
                } else {
                    construction
                };
                Ok(HandlerResponse::ok(
                    construction_json(&catalog, &construction).await?,
                ))
            }
        });
    }
}

/// Stop a construction no runner in this process drives: put its running
/// tasks back to `todo` and abort their runs, as the runner would, then mark
/// it `stopped`.
async fn stop_without_runner(
    catalog: &Catalog,
    application: uuid::Uuid,
    construction: uuid::Uuid,
) -> Result<()> {
    for phase in store::phases_with_tasks(catalog, construction).await? {
        for mut task in phase.tasks {
            if task.status == TaskStatus::Running {
                if let Some(id) = task.runs.last()
                    && let Some(mut run) = sc_agent::load_run(catalog, sc_agent::RunId(*id)).await?
                    && run.state.is_live()
                {
                    sc_agent::abort_run(catalog, &mut run).await?;
                }
                task.status = TaskStatus::Todo;
                store::update_task(catalog, &task).await?;
            }
        }
    }
    store::set_construction_status(catalog, application, ConstructionStatus::Stopped, None).await?;
    Ok(())
}

/// Delete `app`'s construction and the agent made for it. `None` when it had
/// no construction; `Some(agent)` names the agent that went with it, if one
/// did. Called by `deleteApplication` too.
pub(crate) async fn delete_construction_of(
    catalog: &Catalog,
    app: &Application,
) -> Result<Option<Option<String>>> {
    let Some(construction) = store::delete_construction(catalog, app.id.0).await? else {
        return Ok(None);
    };
    Ok(Some(
        delete_construction_agent(catalog, app, &construction).await?,
    ))
}

/// Start the run that plans `app`'s phases from its specification, answering
/// the construction with the run recorded on it. The run is driven in a task
/// of its own, as `caller`.
async fn start_planning(
    catalog: Arc<Catalog>,
    apps: Arc<AppMounts>,
    app: &Application,
    caller: RunCaller,
) -> Result<Construction> {
    let construction = store::require_construction(&catalog, app.id.0).await?;
    if construction.status == sc_construct::ConstructionStatus::Running {
        return Err(Error::invalid(
            "the construction is running; stop it before planning it again",
        ));
    }
    if construction.specification.trim().is_empty() {
        return Err(Error::invalid(
            "the construction has no specification yet; describe the application first",
        ));
    }
    use_chosen_provider(&catalog, &apps, app, &construction).await?;
    let name = construction.agent.clone().ok_or_else(|| {
        Error::invalid("the construction has no agent; delete it and start it again")
    })?;
    let services = apps
        .agents()
        .ok_or_else(|| Error::config("this server has no agents installed"))?;
    let agents = sc_agent::Agents::load(&catalog, services.registry()).await?;
    let agent = agents.require(&name)?.clone();
    if !agent.traits.iter().any(|t| t.trait_ == CONSTRUCTOR_TRAIT) {
        return Err(Error::invalid(format!(
            "agent `{name}` has no `{CONSTRUCTOR_TRAIT}` trait, so it cannot save phases; add it \
             on the agent's page"
        )));
    }
    let guard = PlanningGuard::claim(construction.id)
        .ok_or_else(|| Error::invalid("the construction agent is planning the phases already"))?;

    let executor = services
        .providers()
        .connect(&catalog, &agent, ModelRole::Executor)
        .await?;
    let run = {
        let runner = Runner::new(
            &catalog,
            services.registry(),
            &agent,
            executor.clone(),
            caller.clone(),
        );
        runner
            .new_run(plan_message(app, &construction.specification))?
            .description(format!("Plan the construction of {}", app.name))
    };
    sc_agent::save_run(&catalog, &run).await?;
    let construction = store::set_plan_run(&catalog, app.id.0, run.id.0).await?;

    let (construction_id, application) = (construction.id, app.id.0);
    sc_catalog::spawn_scoped(async move {
        let _guard = guard;
        let mut run = run;
        let Some(services) = apps.agents() else {
            return;
        };
        let mut runner = Runner::new(&catalog, services.registry(), &agent, executor, caller)
            .with_connector(services.providers());
        let evaluator = apps.evaluator();
        if let Some(evaluator) = &evaluator {
            runner = runner.with_evaluator(evaluator);
        }
        if let Some(triggers) = apps.triggers() {
            runner = runner.with_triggers(triggers);
        }
        // The driver writes a failure to the run's row; the construction gets
        // it too, as the Plan & build screen shows the construction.
        let failure = match runner.drive(&mut run).await {
            Ok(Conclusion::Answered { .. }) => {
                match store::phases_of(&catalog, construction_id).await {
                    Ok(phases) if phases.is_empty() => Some(
                        "the construction agent finished without saving any phases; see its run"
                            .to_owned(),
                    ),
                    _ => None,
                }
            }
            Ok(Conclusion::MaxSteps) => Some(
                "the construction agent ran out of steps before it saved the phases; raise its \
                 step limit on the agent's page, or shorten the specification"
                    .to_owned(),
            ),
            Ok(_) => None,
            Err(e) => {
                crate::router::log_failure("a construction's plan run failed", &e);
                Some(format!("planning the phases failed: {e}"))
            }
        };
        if let Some(failure) = failure {
            store::set_plan_error(&catalog, application, &failure)
                .await
                .ok();
        }
    });
    Ok(construction)
}

/// What the plan run is asked.
fn plan_message(app: &Application, specification: &str) -> String {
    let description = match app.description.trim() {
        "" => String::new(),
        text => format!("\n\nThe application's description: {text}"),
    };
    format!(
        "Plan the construction of the `{}` application (subdomain `{}`) in phases.{description}\n\n\
         The specification:\n{}\n\n\
         First look at the data model there is with `{}`, so the phases build on it rather \
         than repeat it. Then save the phases with `{}`. Only plan in this conversation: \
         nothing is built until the phases run.",
        app.name,
        app.subdomain.trim(),
        specification.trim(),
        sc_api::mcp::TOOL_DESCRIBE,
        sc_agent_builtins::TOOL_SAVE_PHASES,
    )
}

/// The name the construction agent of `app` is created under: next to
/// `build-<subdomain>`, and stable under a rename of the application.
pub(crate) fn construction_agent_name(app: &Application) -> String {
    format!("construct-{}", app.subdomain.trim())
}

/// Refuse an application the constructor cannot build: anything but React.
fn require_constructible(app: &Application) -> Result<()> {
    if app.framework.name == sc_app::REACT_FRAMEWORK {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "the App Constructor builds React applications; `{}` uses the `{}` framework",
            app.name, app.framework.name
        )))
    }
}

/// The application setting naming the LLM provider Plan & build uses, set on
/// the application's form.
pub const CONSTRUCTOR_PROVIDER: &str = "constructor_provider";

/// The LLM provider `app`'s settings choose for Plan & build, if any.
fn chosen_provider(app: &Application) -> Option<&str> {
    app.attributes
        .get(CONSTRUCTOR_PROVIDER)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
}

/// The provider called `name`, refused unless it has a default chat model to
/// plan and build with.
async fn usable_provider(catalog: &Catalog, name: &str) -> Result<sc_llm::LlmProviderDef> {
    let provider = sc_llm::require_llm_provider(catalog, name).await?;
    if !list_llm_models(catalog, &provider)
        .await?
        .iter()
        .any(|m| m.is_default && m.kind == sc_llm::ModelKind::Chat)
    {
        return Err(Error::config(format!(
            "LLM provider `{}` has no default model; mark one of its models as the default",
            provider.name
        )));
    }
    Ok(provider)
}

/// Before planning or running: if the application's settings choose a
/// provider and the construction agent uses another, switch it to that one
/// (with its default model), so a change on the form applies.
async fn use_chosen_provider(
    catalog: &Catalog,
    apps: &AppMounts,
    app: &Application,
    construction: &Construction,
) -> Result<()> {
    let Some(chosen) = chosen_provider(app) else {
        return Ok(());
    };
    let Some(name) = construction.agent.as_deref() else {
        return Ok(());
    };
    let Some(mut agent) = sc_agent::load_agent_by_name(catalog, name).await? else {
        return Ok(());
    };
    if agent.provider == chosen {
        return Ok(());
    }
    let services = apps
        .agents()
        .ok_or_else(|| Error::config("this server has no agents installed"))?;
    agent.provider = usable_provider(catalog, chosen).await?.name;
    agent.model = None;
    sc_agent::save_agent(catalog, services.registry(), &agent).await
}

/// Create the agent that plans `app`'s construction and builds its data model,
/// returning its name. An agent already under that name is used as it is: an
/// admin's edits to it survive a construction deleted and started again.
///
/// The provider is the one named (by the request, else the application's
/// settings), else the builder agent's, so planning and building talk to the
/// same model the admin already chose, else the installation's default.
async fn create_construction_agent(
    catalog: &Catalog,
    apps: &AppMounts,
    app: &Application,
    provider: Option<&str>,
) -> Result<String> {
    let name = construction_agent_name(app);
    let services = apps
        .agents()
        .ok_or_else(|| Error::config("this server has no agents installed"))?;
    if sc_agent::load_agent_by_name(catalog, &name)
        .await?
        .is_some()
    {
        return Ok(name);
    }
    let provider =
        match provider.map(str::trim).filter(|p| !p.is_empty()) {
            Some(named) => usable_provider(catalog, named).await?,
            None => match sc_agent::load_agent_by_name(catalog, &builder_agent_name(app)).await? {
                Some(builder) => usable_provider(catalog, &builder.provider).await?,
                None => {
                    let default = sc_llm::default_llm_provider(catalog).await?.ok_or_else(|| {
                    Error::config(
                        "no LLM provider is connected; the App Constructor needs one to plan \
                         and build",
                    )
                })?;
                    usable_provider(catalog, &default.name).await?
                }
            },
        };

    let agent = sc_agent::Agent::new(&name, &provider.name)
        .description(format!("Plans and builds the data model of `{}`", app.name))
        .system_prompt(format!(
            "You construct the `{}` application (subdomain `{}`). You plan its \
             construction in phases and build its data model: tables, fields and the \
             application's queries. Its code is built by its builder agent, `{}`.",
            app.name,
            app.subdomain.trim(),
            builder_agent_name(app)
        ))
        // The constructor trait plans and builds the data model itself, with
        // only the tools that needs (`DATA_MODEL_TOOLS`).
        .with_trait(
            sc_agent::EnabledTrait::new(CONSTRUCTOR_TRAIT).configuration(
                sc_agent_builtins::app_constructor_config(app.subdomain.trim()),
            ),
        );
    sc_agent::save_agent(catalog, services.registry(), &agent).await?;
    Ok(name)
}

/// Delete the agent `construction` was made with, if it is still that: named
/// for this application and carrying the constructor trait. One an admin renamed or
/// re-purposed stays.
async fn delete_construction_agent(
    catalog: &Catalog,
    app: &Application,
    construction: &Construction,
) -> Result<Option<String>> {
    let name = construction_agent_name(app);
    if construction.agent.as_deref() != Some(name.as_str())
        || catalog.get(sc_agent::AGENTS_TABLE)?.is_none()
    {
        return Ok(None);
    }
    let Some(agent) = sc_agent::load_agent_by_name(catalog, &name).await? else {
        return Ok(None);
    };
    if !agent.traits.iter().any(|t| t.trait_ == CONSTRUCTOR_TRAIT) {
        return Ok(None);
    }
    sc_agent::delete_agent(catalog, agent.id).await?;
    Ok(Some(name))
}

async fn require_app(catalog: &Catalog, raw: &str) -> Result<Application> {
    let id = uuid::Uuid::parse_str(raw)
        .map(AppId)
        .map_err(|_| Error::invalid(format!("`{raw}` is not a valid application id")))?;
    load_application(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no application with id {raw}")))
}

/// Refuse while the agent plans `construction` or the runner runs it.
fn require_idle(construction: &Construction) -> Result<()> {
    if planning(construction.id) {
        Err(Error::invalid(
            "the construction agent is planning the phases; wait for it to finish",
        ))
    } else if construction.status == ConstructionStatus::Running {
        Err(Error::invalid("the construction is running; stop it first"))
    } else {
        Ok(())
    }
}

/// The task `raw` of `construction`, or a not-found error.
async fn require_task(catalog: &Catalog, construction: &Construction, raw: &str) -> Result<Task> {
    let not_found = || Error::not_found(format!("no task {raw} in this construction"));
    let id = uuid::Uuid::parse_str(raw).map_err(|_| not_found())?;
    let task = store::load_task(catalog, id).await?.ok_or_else(not_found)?;
    let phases = store::phases_of(catalog, construction.id).await?;
    if phases.iter().any(|p| p.id == task.phase) {
        Ok(task)
    } else {
        Err(not_found())
    }
}

fn status_json(construction: &Construction) -> Json {
    json!({
        "id": construction.id,
        "application": construction.application,
        "status": construction.status,
        "agent": construction.agent,
    })
}

/// A construction with its phases, each with its status and tasks.
async fn construction_json(catalog: &Catalog, construction: &Construction) -> Result<Json> {
    let mut phases = Vec::new();
    for with_tasks in store::phases_with_tasks(catalog, construction.id).await? {
        let status = with_tasks.status();
        let PhaseWithTasks { phase, tasks } = with_tasks;
        phases.push(json!({
            "id": phase.id,
            "title": phase.title,
            "description": phase.description,
            "requirements": phase.requirements,
            "status": status,
            "tasks": tasks,
        }));
    }
    let mut out = status_json(construction);
    out["specification"] = json!(construction.specification);
    out["started_by"] = json!(construction.started_by);
    out["plan_run"] = json!(construction.plan_run);
    out["planning"] = json!(planning(construction.id));
    out["error"] = json!(construction.error);
    out["until_phase"] = json!(construction.until_phase);
    out["updated_at"] = json!(construction.updated_at.map(|at| at.to_rfc3339()));
    out["phases"] = Json::Array(phases);
    Ok(out)
}

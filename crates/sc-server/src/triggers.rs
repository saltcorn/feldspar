//! Bringing the trigger system up at boot (§10.2, TODO Phase 4).
//!
//! Everything the row layer needs to raise an event lives below here — the
//! [seam](sc_catalog::TableEvents) in the catalog, the
//! [dispatcher](sc_action::TriggerDispatcher) in `sc-action`, the actions in
//! `sc-core-actions` — and none of them knows about the others. This is where
//! they are put together, once, by the process that is going to serve requests:
//! the built-in actions are registered, the stored triggers are loaded and
//! validated against them, and the dispatcher is installed into the catalog.
//!
//! The same call brings up the **workflow** half of a trigger body (§10.3): the
//! `_fd_workflow_versions` a run pins itself to and the `_fd_run_traces` a traced
//! workflow writes. They belong here rather than in a boot step of their own
//! because a workflow is not a second kind of thing to start — it is what one of
//! these triggers *is*.
//!
//! Until that call, a write is simply unobserved. That is what makes a build
//! tool, a test, or an admin script safe to run against the same catalog without
//! firing anything.

use std::sync::Arc;

use sc_action::{Scheduler, TriggerDispatcher, bootstrap_triggers};
use sc_catalog::Catalog;
use sc_core_actions::builtin_actions;
use sc_error::{Context, Result};
use sc_expr::JsEvaluator;
use sc_workflow::WorkflowEngineTask;

use crate::agents::AgentServices;

/// Install the trigger dispatcher into `catalog` and return it.
///
/// The **agent services come first** (§11.5): `run_agent` is one of the actions
/// a trigger may name, and it needs the assembled trait registry and this
/// deployment's provider connector — so `install_agents` runs before this, not
/// after it. That is the whole ordering constraint between the two, and it
/// points this way because a trait never needs a trigger dispatcher at
/// registration time while an agent-running action always needs the traits.
///
/// Fails only on the things a server must not start without: the built-in action
/// set not assembling (a TLS stack that will not initialise, §10.1), the
/// `_fd_triggers` table not being creatable, or the database being unreadable. A
/// **trigger** that does not validate is not one of those — it is dropped from
/// the live set with its reason reported, exactly as a file store that will not
/// connect is, because the rest of the server works and the admin can fix it in
/// the UI.
pub async fn install_triggers(
    catalog: &Arc<Catalog>,
    evaluator: Arc<dyn JsEvaluator>,
    agents: &AgentServices,
    models: &crate::ModelServices,
) -> Result<Arc<TriggerDispatcher>> {
    install_triggers_with_adapters(
        catalog,
        evaluator,
        agents,
        models,
        [crate::default_python_adapter() as Arc<dyn sc_expr::CodeAdapter>],
    )
    .await
}

/// [`install_triggers`], with the guest-language adapters this process built
/// from its own flags (§15's seam).
///
/// The two exist because the knobs are a **process's**, not an installation's:
/// `serve` has parsed `--python`, `--python-max-inflight` and the rest and hands
/// the adapter it built, while every other caller — a test, a script, a tool
/// that boots a dispatcher to fire one trigger — wants the same set with the
/// defaults and no configuration to write. Neither is the "real" one: what makes
/// them the same boot is that both register an adapter for every language this
/// binary knows, so a stored Python trigger means the same thing in a test as in
/// production and fails, where it cannot run, with a sentence naming why.
pub async fn install_triggers_with_adapters(
    catalog: &Arc<Catalog>,
    evaluator: Arc<dyn JsEvaluator>,
    agents: &AgentServices,
    models: &crate::ModelServices,
    adapters: impl IntoIterator<Item = Arc<dyn sc_expr::CodeAdapter>>,
) -> Result<Arc<TriggerDispatcher>> {
    bootstrap_triggers(catalog)
        .await
        .context("ensuring the triggers table exists")?;
    // A workflow is a trigger body (§10.3), so its storage comes up with the
    // triggers': the versions a run is pinned to, and the per-step traces a
    // traced workflow writes. Both are needed by the time a trigger is loaded,
    // because loading one may be loading a workflow.
    sc_workflow::bootstrap_workflow_versions(catalog)
        .await
        .context("ensuring the workflow versions table exists")?;
    sc_workflow::bootstrap_run_traces(catalog)
        .await
        .context("ensuring the run traces table exists")?;
    let registry = base_action_registry(agents, models)?;
    // The mail transport is the **settings-backed** one, not a transport built
    // here from the settings as they are now: an admin who fixes an SMTP
    // password gets it on the next message, which is what the Email section's
    // own help text promises. It is installed unconditionally, because "this
    // installation sends no mail" is a message for the trigger that tries to,
    // not a reason to start the server without a mailer.
    let mailer = Arc::new(sc_email::SettingsMailer::new(Arc::clone(catalog)));
    // The other languages a code body may be written in, each registered under
    // its own name (§15). JavaScript is deliberately not among them: it is the
    // evaluator above, which is also what carries the formula isolate, and a
    // second way in would be two answers to which engine runs a JavaScript body.
    // The same evaluator computes a calculated field the read path cannot
    // translate to SQL (a `predict("…")`, a module call): one formula engine
    // per process, installed where every boot path passes.
    catalog
        .set_formula_evaluator(Arc::clone(&evaluator))
        .context("installing the formula evaluator")?;
    let mut dispatcher = TriggerDispatcher::new(Arc::new(registry))
        .with_evaluator(evaluator)
        .with_mailer(mailer);
    for adapter in adapters {
        dispatcher = dispatcher.with_adapter(adapter);
    }
    let dispatcher = Arc::new(dispatcher);
    dispatcher
        .reload(catalog)
        .await
        .context("loading the stored triggers")?;
    for issue in dispatcher.triggers()?.issues() {
        eprintln!(
            "feldspar: trigger `{}` is stored but not usable: {}",
            issue.trigger, issue.problem
        );
    }
    catalog.set_table_events(Arc::clone(&dispatcher) as Arc<dyn sc_catalog::TableEvents>)?;
    Ok(dispatcher)
}

/// The action set a server runs with **before its modules**: the built-ins plus
/// `run_agent` and `fit_model`.
///
/// Its own function because it is assembled twice — once at boot, here, and
/// again every time a module is installed, configured or removed
/// ([`ModuleServices::reload`](crate::ModuleServices::reload)), which rebuilds
/// the whole set from this base rather than mutating the live one. Two copies of
/// the assembly would be two chances for a module reload to quietly lose
/// `run_agent`.
pub fn base_action_registry(
    agents: &AgentServices,
    models: &crate::ModelServices,
) -> Result<sc_action::ActionRegistry> {
    let mut registry = builtin_actions().context("registering the built-in actions")?;
    sc_core_traits::register_agent_actions(
        &mut registry,
        Arc::clone(agents.registry()),
        Arc::clone(agents.providers()),
    )
    .context("registering the agent action")?;
    // And the model action, for the same reason and in the same place: it
    // needs the model provider registry a fit is validated against and the fits
    // themselves, neither of which exists until a server has assembled them.
    // Rebuilding the base set on a module change is therefore what gives the
    // action the *new* registry — which is how a model over a module's
    // provider can still be refitted after that module is reinstalled.
    sc_core_actions::register_model_actions(
        &mut registry,
        models.registry(),
        Arc::new(models.clone()),
    )
    .context("registering the model action")?;
    Ok(registry)
}

/// Start the periodic scheduler: the one task that fires `often`/`hourly`/
/// `daily`/`weekly` triggers (§10.2).
///
/// Started **only by `serve`**, for the same reason the dispatcher is installed
/// only there: a `build-app` or an admin script must not start firing scheduled
/// jobs because it happened to open the same database.
///
/// The first tick lands on the next minute boundary, which is also when a run
/// missed while the server was down is caught up — once, from the persisted
/// `last_run_at`. The returned handle is the caller's to abort; dropping it
/// leaves the task running for the life of the process, which is what a server
/// wants.
pub fn start_scheduler(
    catalog: &Arc<Catalog>,
    dispatcher: &Arc<TriggerDispatcher>,
) -> (Arc<Scheduler>, tokio::task::JoinHandle<()>) {
    let scheduler = Arc::new(Scheduler::new(Arc::clone(catalog), Arc::clone(dispatcher)));
    let handle = scheduler.start();
    (scheduler, handle)
}

/// Start the **workflow engine**: the seam a trigger whose body is a workflow is
/// run by, and the one task that advances runs nobody is waiting for (§10.3).
///
/// Started **only by `serve`**, for the reason the scheduler is: a `build-app` or
/// an admin script must not start advancing suspended workflow runs because it
/// happened to open the same database. A process without it runs every action
/// trigger normally and tells the caller, of a workflow, that nothing here can
/// run one — rather than returning success for a run that was never started.
///
/// Installed **on** the dispatcher, which it also holds: the engine's steps run
/// actions through the same registry and may run another trigger, so the two
/// reference each other and both live for the process.
///
/// The returned handle is the caller's to abort; dropping it leaves the task
/// running, which is what a server wants.
pub fn start_workflow_engine(
    catalog: &Arc<Catalog>,
    dispatcher: &Arc<TriggerDispatcher>,
) -> (Arc<WorkflowEngineTask>, tokio::task::JoinHandle<()>) {
    let engine = Arc::new(WorkflowEngineTask::new(
        Arc::clone(catalog),
        Arc::clone(dispatcher),
    ));
    dispatcher.set_workflow_engine(Arc::clone(&engine) as Arc<dyn sc_action::WorkflowEngine>);
    let handle = engine.start();
    (engine, handle)
}

/// Fire the **`startup`** event: the server is up (§10.2).
///
/// Called once, after the catalog, the file stores, the applications *and* the
/// triggers are all up and before the listener is announced — so a startup
/// trigger's action finds a server that works, and anything it writes is visible
/// to the first request rather than racing it.
///
/// Reports rather than fails, like every other fire-and-forget event: a
/// misconfigured startup trigger must not be the reason a server refuses to
/// boot, which is the one moment nobody can fix it from the admin UI.
pub async fn fire_startup(catalog: &Arc<Catalog>, dispatcher: &Arc<TriggerDispatcher>) {
    dispatcher.fire(catalog, &sc_action::Event::startup()).await;
}

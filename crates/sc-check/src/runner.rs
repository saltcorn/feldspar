//! The runner (TODO C1.4): load a checklist's current steps, run them in order
//! as its `run_as`, and record the run.
//!
//! - **A step the host cannot run is skipped, and the run is `incomplete`** —
//!   never `passed`. A host without the application router still runs every
//!   `data.*` step and reports the `http.*` ones as skipped.
//! - **The first failure ends the run**, unless the checklist's
//!   `stop_on_failure` is false; the steps not reached are listed as such, so
//!   a run always shows the whole checklist.
//! - **Rows `data.insert` wrote are deleted at the end**, newest first, as the
//!   system rather than as `run_as` (whose rights are what is under test, not
//!   what the clean-up may rely on). Best effort: a row that will not go is
//!   reported on the run, and a row a later step already deleted is not an
//!   error.
//! - **A mutating checklist is refused on a production environment** unless
//!   its `allow_on_production` is set (TODO C1.6). The refusal is recorded as a
//!   run with status `error`, so the admin UI shows why it did not run.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use sc_action::TriggerDispatcher;
use sc_agent::AppRequester;
use sc_app::{AppId, Application};
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::{Error, Repr, Result};
use sc_expr::JsEvaluator;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::model::{Checklist, ChecklistRun, Outcome, RunStatus, SkipReason, Step, StepResult};
use crate::registry::{Capabilities, InsertedRow, StepContext, StepOutcome, StepRegistry};
use crate::store::{self, ChecklistScope};
use crate::vars;

/// The target of a run against the live mount.
pub const TARGET_LIVE: &str = "live";

/// What a run runs with: the catalog, the step types, and what this host can
/// offer them.
#[derive(Clone, Copy)]
pub struct CheckEnv<'a> {
    /// The catalog.
    pub catalog: &'a Catalog,
    /// The step types.
    pub registry: &'a StepRegistry,
    /// The trigger dispatcher, for `data.run_trigger` and `app.builds`.
    pub triggers: Option<&'a Arc<TriggerDispatcher>>,
    /// The formula evaluator; the dispatcher's when not given.
    pub evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    /// The application router, for `http.*`.
    pub requests: Option<&'a dyn AppRequester>,
}

impl<'a> CheckEnv<'a> {
    /// A host with only the catalog: data steps run, the rest are skipped.
    pub fn new(catalog: &'a Catalog, registry: &'a StepRegistry) -> CheckEnv<'a> {
        CheckEnv {
            catalog,
            registry,
            triggers: None,
            evaluator: None,
            requests: None,
        }
    }

    /// With the trigger dispatcher.
    pub fn with_triggers(mut self, triggers: Option<&'a Arc<TriggerDispatcher>>) -> Self {
        self.triggers = triggers;
        self
    }

    /// With the application router.
    pub fn with_requests(mut self, requests: Option<&'a dyn AppRequester>) -> Self {
        self.requests = requests;
        self
    }

    /// With a formula evaluator.
    pub fn with_evaluator(mut self, evaluator: Option<&'a Arc<dyn JsEvaluator>>) -> Self {
        self.evaluator = evaluator;
        self
    }

    fn capabilities(&self) -> Capabilities<'a> {
        Capabilities {
            triggers: self.triggers,
            evaluator: self
                .evaluator
                .or_else(|| self.triggers.and_then(|t| t.services().evaluator.as_ref())),
            requests: self.requests,
        }
    }
}

/// Run every **enabled** checklist in `scope`, one after the other.
pub async fn run_checklists(
    env: &CheckEnv<'_>,
    scope: ChecklistScope,
    trigger: &str,
) -> Result<Vec<ChecklistRun>> {
    let mut runs = Vec::new();
    for checklist in store::list_checklists(env.catalog, scope).await? {
        if checklist.enabled {
            runs.push(run_checklist(env, &checklist, trigger).await?);
        }
    }
    Ok(runs)
}

/// Run one checklist and record the run. `trigger` says what started it
/// (`manual`, `cli`, `agent:<run id>`, …).
///
/// An `Err` is only for a run that could not be *recorded*; everything that
/// goes wrong inside it — a refusal, a missing user, a broken step — is in the
/// run.
pub async fn run_checklist(
    env: &CheckEnv<'_>,
    checklist: &Checklist,
    trigger: &str,
) -> Result<ChecklistRun> {
    let mut run = ChecklistRun {
        id: Uuid::new_v4(),
        checklist: checklist.id,
        checklist_name: checklist.name.clone(),
        trigger: trigger.to_owned(),
        target: TARGET_LIVE.to_owned(),
        status: RunStatus::Error,
        run_as: checklist.run_as,
        started_at: Utc::now(),
        finished_at: Utc::now(),
        steps: Vec::new(),
        cleanup_errors: Vec::new(),
        message: None,
    };
    match prepare(env, checklist).await {
        Err(e) => run.message = Some(sc_error::format_chain(&e)),
        Ok(prepared) => execute(env, checklist, &prepared, &mut run).await,
    }
    run.finished_at = Utc::now();
    store::insert_run(env.catalog, &run).await?;
    Ok(run)
}

/// What a run needs before its first step: the guard passed, the user and the
/// application loaded, the steps read.
struct Prepared {
    user: Option<User>,
    application: Option<Application>,
    steps: Vec<Step>,
}

async fn prepare(env: &CheckEnv<'_>, checklist: &Checklist) -> Result<Prepared> {
    if env.catalog.is_production() && checklist.mutates && !checklist.allow_on_production() {
        return Err(Error::invalid(format!(
            "refused: checklist `{}` writes data, and this is a production environment. A \
             person can allow it with the checklist's `allow_on_production` setting.",
            checklist.name
        )));
    }
    let user = match checklist.run_as {
        None => None,
        Some(id) => {
            let user = sc_auth::load_user(env.catalog, id).await?.ok_or_else(|| {
                Error::not_found(format!(
                    "checklist `{}` runs as user {id}, who no longer exists",
                    checklist.name
                ))
            })?;
            if user.is_disabled() {
                return Err(Error::invalid(format!(
                    "checklist `{}` runs as a disabled user",
                    checklist.name
                )));
            }
            Some(user)
        }
    };
    let application = match checklist.application {
        None => None,
        Some(id) => Some(
            sc_app::load_application(env.catalog, AppId(id))
                .await?
                .ok_or_else(|| {
                    Error::not_found(format!(
                        "checklist `{}` checks application {id}, which no longer exists",
                        checklist.name
                    ))
                })?,
        ),
    };
    let steps = store::current_steps(env.catalog, checklist.id).await?;
    Ok(Prepared {
        user,
        application,
        steps,
    })
}

async fn execute(
    env: &CheckEnv<'_>,
    checklist: &Checklist,
    prepared: &Prepared,
    run: &mut ChecklistRun,
) {
    let caps = env.capabilities();
    let mut vars = vars::initial(run.id);
    let mut inserted: Vec<InsertedRow> = Vec::new();
    let mut last_response: Option<Json> = None;
    let mut stopped = false;

    for step in &prepared.steps {
        if stopped {
            run.steps.push(result(
                step,
                StepOutcome::skipped(SkipReason::NotReached, "not run: an earlier step failed"),
                Duration::ZERO,
            ));
            continue;
        }
        let started = Instant::now();
        let outcome = run_step(
            env,
            checklist,
            prepared,
            step,
            caps,
            run.id,
            &vars,
            &mut inserted,
            &mut last_response,
        )
        .await;
        if outcome.outcome == Outcome::Pass
            && let (Some(name), Some(value)) = (&step.draft.capture, &outcome.value)
        {
            vars::capture(&mut vars, name, value.clone());
        }
        if matches!(outcome.outcome, Outcome::Fail | Outcome::Error) && checklist.stop_on_failure()
        {
            stopped = true;
        }
        run.steps.push(result(step, outcome, started.elapsed()));
    }

    run.cleanup_errors = clean_up(env.catalog, &inserted).await;
    run.status = RunStatus::of(&run.steps);
}

#[allow(clippy::too_many_arguments)]
async fn run_step(
    env: &CheckEnv<'_>,
    checklist: &Checklist,
    prepared: &Prepared,
    step: &Step,
    caps: Capabilities<'_>,
    run_id: Uuid,
    vars: &Json,
    inserted: &mut Vec<InsertedRow>,
    last_response: &mut Option<Json>,
) -> StepOutcome {
    let Some(step_type) = env.registry.get(&step.draft.type_) else {
        return StepOutcome::error(format!(
            "there is no step type `{}` on this server (a module that supplied it may have \
             been removed)",
            step.draft.type_
        ));
    };
    let needs = step_type.needs();
    if let Some(reason) = caps.missing(needs) {
        return StepOutcome::skipped(SkipReason::Unavailable, format!("skipped: {reason}"));
    }
    if needs.application() && prepared.application.is_none() {
        return StepOutcome::error(format!(
            "`{}` checks an application, and this checklist has none",
            step.draft.type_
        ));
    }
    let params = match vars::render(&Json::Object(step.draft.params.clone()), vars) {
        Ok(params) => params,
        Err(e) => return StepOutcome::error(format!("a parameter does not render: {e}")),
    };
    let timeout = checklist.step_timeout().max(step_type.min_timeout());
    let mut cx = StepContext {
        catalog: env.catalog,
        run_as: prepared.user.as_ref(),
        run_id,
        application: prepared.application.as_ref(),
        vars,
        timeout,
        needs,
        caps,
        inserted,
        last_response,
        step_type: step_type.name(),
    };
    match tokio::time::timeout(timeout, step_type.run(&mut cx, &params)).await {
        Ok(outcome) => outcome,
        Err(_) => StepOutcome::fail(format!(
            "took longer than {} seconds",
            timeout.as_secs()
        )),
    }
}

fn result(step: &Step, outcome: StepOutcome, elapsed: Duration) -> StepResult {
    StepResult {
        step: step.id,
        type_: step.draft.type_.clone(),
        label: step.draft.label.clone(),
        outcome: outcome.outcome,
        skip: outcome.skip,
        message: outcome.message,
        duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        captured: step.draft.capture.as_ref().and(outcome.value),
        console_errors: Vec::new(),
        failed_requests: Vec::new(),
    }
}

/// Delete the rows `data.insert` wrote, newest first. Returns what could not
/// be deleted.
async fn clean_up(catalog: &Catalog, inserted: &[InsertedRow]) -> Vec<String> {
    let mut errors = Vec::new();
    for row in inserted.iter().rev() {
        let deleted = async {
            let table = catalog
                .get(&row.table)?
                .ok_or_else(|| Error::msg(format!("table `{}` is gone", row.table)))?;
            sc_api::rows::delete_row(catalog, &table, &row.key).await
        }
        .await;
        match deleted {
            Ok(_) => {}
            // Already gone: a later step deleted it, which is not a failure.
            Err(e) if matches!(e.repr(), Repr::NotFound(_)) => {}
            Err(e) => errors.push(format!(
                "row {} of `{}` could not be deleted: {}",
                row.key,
                row.table,
                sc_error::format_chain(&e)
            )),
        }
    }
    errors
}

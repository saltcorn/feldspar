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
//! - **Browser steps share one browser context per run**, signed in as
//!   `run_as`, on the live mount or — for a run against an agent's preview
//!   (C2.6) — on the preview. What each step's browser calls saw (console
//!   errors, failed requests) is on its result, and **a failed browser step is
//!   photographed** (C2.5): the screenshot goes to the internal
//!   [screenshot store](crate::screenshots) and the result refers to it. The
//!   context is closed, and the run's scratch files removed, when it ends.
//! - **A mutating checklist is refused on a production environment** unless
//!   its `allow_on_production` is set (TODO C1.6). The refusal is recorded as a
//!   run with status `error`, so the admin UI shows why it did not run.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use sc_action::TriggerDispatcher;
use sc_agent::AppRequester;
use sc_browser::{BrowserAction, BrowserDriver, BrowserRequest, BrowserTarget, PreviewTarget};
use sc_app::{AppId, Application};
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::{Error, Repr, Result};
use sc_expr::JsEvaluator;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::model::{Checklist, ChecklistRun, Outcome, RunStatus, SkipReason, Step, StepResult};
use crate::registry::{
    Capabilities, InsertedRow, Observed, StepContext, StepOutcome, StepRegistry, serves_ui,
};
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
    /// The headless browser, for `browser.*`.
    pub browser: Option<&'a dyn BrowserDriver>,
    /// An agent run's preview to check instead of the live mount (C2.6). Only
    /// a checklist of the previewed application should be run with one.
    pub preview: Option<&'a PreviewTarget>,
    /// Where failed browser steps' screenshots are kept; the data directory's
    /// [`SCREENSHOTS_DIR`](crate::screenshots::SCREENSHOTS_DIR) when not given.
    pub screenshots: Option<&'a Path>,
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
            browser: None,
            preview: None,
            screenshots: None,
        }
    }

    /// With the headless browser.
    pub fn with_browser(mut self, browser: Option<&'a dyn BrowserDriver>) -> Self {
        self.browser = browser;
        self
    }

    /// Against an agent run's preview instead of the live mount.
    pub fn with_preview(mut self, preview: Option<&'a PreviewTarget>) -> Self {
        self.preview = preview;
        self
    }

    /// With the screenshots kept in `dir`.
    pub fn with_screenshots(mut self, dir: &'a Path) -> Self {
        self.screenshots = Some(dir);
        self
    }

    /// The run's target, as the run row records it: `live`, or the preview's
    /// label.
    pub fn target(&self) -> String {
        self.preview
            .map_or_else(|| TARGET_LIVE.to_owned(), |p| p.label.clone())
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
            browser: self.browser,
            preview: self.preview,
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
        target: env.target(),
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

    let mut used_browser = false;

    for (index, step) in prepared.steps.iter().enumerate() {
        if stopped {
            run.steps.push(result(
                step,
                StepOutcome::skipped(SkipReason::NotReached, "not run: an earlier step failed"),
                Duration::ZERO,
                Observed::default(),
            ));
            continue;
        }
        let started = Instant::now();
        let (outcome, mut observed) = run_step(
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
        let failed = matches!(outcome.outcome, Outcome::Fail | Outcome::Error);
        if failed && checklist.stop_on_failure() {
            stopped = true;
        }
        used_browser |= observed.used_browser;
        // A failed browser step is photographed: what the page showed is
        // usually the quickest explanation of why.
        if failed && observed.used_browser && observed.screenshot.is_none() {
            observed.screenshot = failure_screenshot(env, prepared, run.id).await;
        }
        let elapsed = started.elapsed();
        let shot = observed.screenshot.take();
        let mut step_result = result(step, outcome, elapsed, observed);
        if let Some(jpeg) = shot {
            match keep_screenshot(env, run.id, index, jpeg).await {
                Ok(path) => step_result.screenshot = Some(path),
                Err(e) => run.cleanup_errors.push(format!(
                    "the screenshot of step {} could not be kept: {}",
                    index + 1,
                    sc_error::format_chain(&e)
                )),
            }
        }
        run.steps.push(step_result);
    }

    if used_browser && let Some(browser) = env.browser {
        browser.close(run.id);
    }
    let scratch = crate::registry::scratch_dir(run.id);
    if scratch.exists() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    run.cleanup_errors
        .extend(clean_up(env.catalog, &inserted).await);
    run.status = RunStatus::of(&run.steps);
}

/// A JPEG of the run's page as it is now, for a failed browser step; `None`
/// when one cannot be taken (the browser is what failed).
async fn failure_screenshot(env: &CheckEnv<'_>, prepared: &Prepared, run: Uuid) -> Option<Vec<u8>> {
    let browser = env.browser?;
    let app = prepared.application.as_ref()?;
    let target = BrowserTarget {
        subdomain: app.subdomain.clone(),
        preview: env.preview.cloned(),
    };
    browser
        .act(BrowserRequest {
            context: run,
            target: &target,
            user: prepared.user.as_ref(),
            action: BrowserAction::Screenshot { full_page: false },
            timeout: Duration::from_secs(15),
        })
        .await
        .ok()
        .and_then(|report| report.screenshot)
}

async fn keep_screenshot(
    env: &CheckEnv<'_>,
    run: Uuid,
    index: usize,
    jpeg: Vec<u8>,
) -> Result<String> {
    let dir = match env.screenshots {
        Some(dir) => dir.to_path_buf(),
        None => crate::screenshots::default_dir()?,
    };
    crate::screenshots::save(&dir, run, index, jpeg).await
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
) -> (StepOutcome, Observed) {
    let unobserved = |outcome: StepOutcome| (outcome, Observed::default());
    let Some(step_type) = env.registry.get(&step.draft.type_) else {
        return unobserved(StepOutcome::error(format!(
            "there is no step type `{}` on this server (a module that supplied it may have \
             been removed)",
            step.draft.type_
        )));
    };
    let needs = step_type.needs();
    if needs.application() && prepared.application.is_none() {
        return unobserved(StepOutcome::error(format!(
            "`{}` checks an application, and this checklist has none",
            step.draft.type_
        )));
    }
    if needs.browser
        && let Some(app) = &prepared.application
        && !serves_ui(app)
    {
        return unobserved(StepOutcome::error(format!(
            "`{}` opens the application in a browser, and `{}`'s framework `{}` serves no UI",
            step.draft.type_, app.subdomain, app.framework.name
        )));
    }
    if let Some(reason) = caps.missing(needs) {
        return unobserved(StepOutcome::skipped(
            SkipReason::Unavailable,
            format!("skipped: {reason}"),
        ));
    }
    let params = match vars::render(&Json::Object(step.draft.params.clone()), vars) {
        Ok(params) => params,
        Err(e) => {
            return unobserved(StepOutcome::error(format!(
                "a parameter does not render: {e}"
            )));
        }
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
        started: Instant::now(),
        observed: Observed::default(),
        implicit_clean: checklist.implicit_clean(),
    };
    let outcome = match tokio::time::timeout(timeout, step_type.run(&mut cx, &params)).await {
        Ok(outcome) => outcome,
        Err(_) => StepOutcome::fail(format!(
            "took longer than {} seconds",
            timeout.as_secs()
        )),
    };
    (outcome, cx.observed)
}

fn result(step: &Step, outcome: StepOutcome, elapsed: Duration, observed: Observed) -> StepResult {
    StepResult {
        step: step.id,
        type_: step.draft.type_.clone(),
        label: step.draft.label.clone(),
        outcome: outcome.outcome,
        skip: outcome.skip,
        message: outcome.message,
        duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        captured: step.draft.capture.as_ref().and(outcome.value),
        console_errors: observed.console_errors,
        failed_requests: observed.failed_requests,
        screenshot: None,
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

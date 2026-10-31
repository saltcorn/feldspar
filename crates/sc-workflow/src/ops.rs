//! The four things somebody does **to** a run that the run does not do to
//! itself (§10.3, phase 4): answer its form, cancel it, retry it, and list the
//! runs of one workflow.
//!
//! Everything else the engine does is driven by the [queue](crate::queue): a
//! retry's backoff comes round, a `Wait` ends, a crashed node's lease expires.
//! These four have a person behind them, and that is the whole reason they are a
//! module rather than more of the driver.
//!
//! ## Resuming drives, it does not merely enqueue
//!
//! [`resume_run`] writes the answers, claims the run under a lease and then
//! **drives it** to wherever it gets to next, exactly as
//! [`start_run`](crate::start_run) drives a run it has just created. The
//! alternative — write the row and let the poll pick it up — would mean an admin
//! who clicks *Approve* is told "running" and has to reload to find out what
//! happened, which is not what pressing the button meant.
//!
//! Holding the lease across the drive matters as much as the drive: without it
//! the engine task on this node (or another) could claim the same row a
//! millisecond later, and the step would run twice for no reason at all.
//!
//! ## Who may answer a form
//!
//! The form's own `min_role`, defaulting to **admin** — the same floor, the same
//! default and the same reading a trigger's own exposure has
//! (`role <= min_role`, and `None` means role 1). [`check_may_resume`] is the
//! one place that rule is written; the API layer calls it, because *who is
//! asking* is a question about a request and not about a run.

use sc_types::Role;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use sc_action::TriggerDispatcher;
use sc_agent::run_store::{
    COL_CREATED_AT, COL_KIND, COL_STATE, COL_SUBJECT, RUNS_TABLE, run_from_row,
};
use sc_agent::{Run, RunId, RunKind, RunState, require_run, save_run};
use sc_catalog::Catalog;
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Expr, OrderBy, Select, Source, Statement};
use sc_types::{Attrs, validate_attrs};

use crate::driver::{Clock, Driver};
use crate::machine::PendingForm;
use crate::run::{mark_aborted, record, release, run_pending_form, run_state};

/// How long the lease taken for an inline drive lasts.
///
/// Longer than the engine task's, because nothing renews it: an admin's resume
/// drives the run on the request's own stack, and a run that walks twenty steps
/// must not have its lease expire underneath it half way through. If the process
/// dies mid-drive the run is simply picked up when this runs out, which is the
/// ordinary recovery path and not a special case.
pub const INLINE_LEASE_SECONDS: i64 = 300;

/// Answer the form a suspended run is waiting on, and carry the run on (§10.3,
/// phase 4.2).
///
/// The values are checked against the step's **own declaration** — the
/// [`PendingForm`] recorded on the run when it suspended, not the workflow's
/// current text — through the same [`validate_attrs`] every other settings form
/// goes through. That is what decision 2 buys here: the workflow may have been
/// edited twice while somebody had the form open, and what they are answering is
/// the form they were shown.
///
/// The answers are merged into the context whole, under the step's `assign_to`.
///
/// Refuses, by name, a run that is not waiting for a person: a resume that
/// silently did nothing would leave an admin believing an approval had gone
/// through.
pub async fn resume_run(
    catalog: &Arc<Catalog>,
    dispatcher: &TriggerDispatcher,
    clock: &dyn Clock,
    id: RunId,
    values: Attrs,
) -> Result<Run> {
    let mut run = require_workflow_run(catalog, id).await?;
    let form = run_pending_form(&run)?.ok_or_else(|| {
        Error::invalid(format!(
            "run {id} is not waiting for anybody to fill in a form (it is {})",
            run.state
        ))
    })?;
    let fields = form
        .fields
        .iter()
        .map(|f| f.to_form_field())
        .collect::<Result<Vec<_>>>()?;
    // The same check a trigger's configuration and a file store's settings get,
    // so a required answer that is missing and a value of the wrong type are
    // refused in the same words the admin already knows.
    validate_attrs(&fields, &values)?;
    // Defaults are the declaration's, not the answerer's problem: a field with a
    // default that nobody filled in means what it says it means.
    let mut answers = Attrs::new();
    for field in &fields {
        if let Some(value) = field.resolve(&values) {
            answers.insert(field.base.name.clone(), value.clone());
        }
    }

    let mut state = run_state(&run)?;
    state.resumed(answers)?;
    let now = clock.now();
    record(&mut run, &state, now);
    lease(&mut run, "resume", now);
    save_run(catalog, &run).await?;
    drive(catalog, dispatcher, clock, &mut run).await?;
    Ok(run)
}

/// Stop a running or waiting run, for a reason (§10.3, phase 4.4).
///
/// The state becomes `aborted` rather than `failed`, because nothing went wrong
/// — somebody decided. The machine state is kept exactly as it was, so the run
/// detail still shows where it had got to.
///
/// A run that has already finished, failed or been cancelled is refused rather
/// than cancelled again: the row is a record of what happened, and rewriting the
/// ending of one that already has one loses the ending.
pub async fn cancel_run(
    catalog: &Arc<Catalog>,
    clock: &dyn Clock,
    id: RunId,
    reason: Option<&str>,
) -> Result<Run> {
    let mut run = require_workflow_run(catalog, id).await?;
    if !run.state.is_live() {
        return Err(Error::invalid(format!(
            "run {id} has already stopped ({}), so there is nothing to cancel",
            run.state
        )));
    }
    mark_aborted(&mut run, reason, clock.now());
    save_run(catalog, &run).await?;
    sc_log::log_info!(
        "workflow `{}` run {id}: cancelled{}",
        run.subject,
        match reason {
            Some(reason) => format!(": {reason}"),
            None => String::new(),
        }
    );
    Ok(run)
}

/// Run a failed run's failing step again, and carry on from there (§10.3, phase
/// 4.4).
///
/// It resumes **at the step that failed**, with the attempt count reset, on the
/// version it was pinned to — not from the beginning. The steps before it
/// already had their effects, and re-running those is the thing at-least-once is
/// trying to do less of, not more.
pub async fn retry_run(
    catalog: &Arc<Catalog>,
    dispatcher: &TriggerDispatcher,
    clock: &dyn Clock,
    id: RunId,
) -> Result<Run> {
    let mut run = require_workflow_run(catalog, id).await?;
    if run.state != RunState::Failed {
        return Err(Error::invalid(format!(
            "run {id} is {}, and only a run that failed can be retried",
            run.state
        )));
    }
    let driver = Driver::new(catalog, dispatcher, clock);
    let workflow = driver.workflow_of(&run).await?;
    let mut state = run_state(&run)?;
    state.retry(&workflow)?;
    let now = clock.now();
    record(&mut run, &state, now);
    // The reason it stopped goes with the state that stopped: a run that is
    // running again must not carry the error that ended its last life.
    run.error = None;
    lease(&mut run, "retry", now);
    save_run(catalog, &run).await?;
    sc_log::log_info!(
        "workflow `{}` run {id}: retrying step `{}`",
        run.subject,
        state.current_step().unwrap_or("(the end)")
    );
    drive(catalog, dispatcher, clock, &mut run).await?;
    Ok(run)
}

/// Whether `role` may answer this form: its floor, or admin when it names none.
///
/// The same comparison every other floor in the system uses — a **lower** role
/// number is more privileged, so `role <= min_role` — and the same safe default:
/// a form that says nothing about who may answer it is a form only an admin may
/// answer.
pub fn may_resume(form: &PendingForm, role: Role) -> bool {
    role.meets(form.min_role.unwrap_or(Role::ADMIN))
}

/// [`may_resume`] as a refusal, for the API layer to call before it resumes.
pub fn check_may_resume(form: &PendingForm, role: Role) -> Result<()> {
    if may_resume(form, role) {
        return Ok(());
    }
    Err(Error::auth(format!(
        "answering this step needs role {} or better",
        form.min_role.unwrap_or(Role::ADMIN)
    )))
}

/// The runs of one workflow, newest first, optionally of one state, one page at
/// a time (§10.3, phase 5.2).
///
/// By the trigger's **name**, which is what `_fd_runs.subject` holds, for the
/// reason an agent's runs are listed by its name: a run outlives the trigger it
/// was of, deliberately, and a finished run stays readable after somebody
/// deletes the workflow.
pub async fn list_workflow_runs(
    catalog: &Catalog,
    workflow: &str,
    state: Option<RunState>,
    limit: usize,
    offset: usize,
) -> Result<Vec<Run>> {
    let mut filter = Expr::col(COL_KIND)
        .eq(Expr::lit(RunKind::Workflow.as_str()))
        .and(Expr::col(COL_SUBJECT).eq(Expr::lit(workflow)));
    if let Some(state) = state {
        filter = filter.and(Expr::col(COL_STATE).eq(Expr::lit(state.as_str())));
    }
    let mut select = Select::from(Source::table(RUNS_TABLE)).filter(filter);
    select.order = vec![OrderBy::desc(Expr::col(COL_CREATED_AT))];
    select.limit = Some(limit as u64);
    select.offset = Some(offset as u64);
    let rows: Vec<Row> = catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    rows.iter().map(run_from_row).collect()
}

// ---- the pieces the three share -----------------------------------------

/// The run of this id, refusing an agent's — the operations here all read a
/// [`WorkflowRun`](crate::WorkflowRun) out of the context, and an agent run's
/// context is a conversation.
async fn require_workflow_run(catalog: &Catalog, id: RunId) -> Result<Run> {
    let run = require_run(catalog, id).await?;
    if run.kind != RunKind::Workflow {
        return Err(Error::invalid(format!(
            "run {id} is a {} run, not a workflow run",
            run.kind
        )));
    }
    Ok(run)
}

/// Claim the run for this request, so the engine task does not take the same row
/// out from under an inline drive.
fn lease(run: &mut Run, what: &str, now: DateTime<Utc>) {
    run.lease_until = Some(now + Duration::seconds(INLINE_LEASE_SECONDS));
    run.claimed_by = Some(format!("admin-{what}"));
}

/// Drive the run as far as it goes, and let go of the claim if it stopped
/// somewhere the driver did not release it itself.
///
/// A failure to drive is not a failure of the operation: the answers are already
/// written, the run exists and its row says what state it is in, so the caller
/// gets the run back rather than an error that loses what was just recorded.
/// That is exactly how [`start_run`](crate::start_run) treats the same case.
async fn drive(
    catalog: &Arc<Catalog>,
    dispatcher: &TriggerDispatcher,
    clock: &dyn Clock,
    run: &mut Run,
) -> Result<()> {
    let driver = Driver::new(catalog, dispatcher, clock);
    match driver.drive(run).await {
        Ok(_) => Ok(()),
        Err(e) => {
            sc_log::log_error!("workflow `{}` run {}: {e}", run.subject, run.id);
            run.error = Some(e.to_string());
            run.state = RunState::Failed;
            release(run);
            save_run(catalog, run).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::FieldDecl;

    fn form(min_role: Option<Role>) -> PendingForm {
        PendingForm {
            fields: vec![FieldDecl::new("approved", "bool").required()],
            assign_to: "approval".to_owned(),
            min_role,
        }
    }

    #[test]
    fn a_form_that_says_nothing_about_who_may_answer_it_is_admin_only() {
        let form = form(None);
        assert!(may_resume(&form, Role::ADMIN));
        assert!(!may_resume(&form, Role::of(40)));
        assert!(!may_resume(&form, Role::PUBLIC));
        let err = check_may_resume(&form, Role::of(40))
            .unwrap_err()
            .to_string();
        assert!(err.contains("role 1"), "{err}");
    }

    #[test]
    fn a_floor_lets_in_everybody_at_least_that_privileged() {
        let form = form(Some(Role::of(40)));
        // A lower role number is more privileged, which is the rule everywhere
        // else in the system and is why this is `<=` and not `>=`.
        assert!(may_resume(&form, Role::ADMIN));
        assert!(may_resume(&form, Role::of(40)));
        assert!(!may_resume(&form, Role::of(41)));
    }
}

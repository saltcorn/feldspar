//! The driver: the one thing that does IO for a [`WorkflowRun`] (§10.3, phase 3).
//!
//! The machine decides; this drives. It loads the version the run is **pinned**
//! to, asks the machine what to do, does it — runs an action through the
//! registry, or evaluates formulas through the isolate — feeds the outcome back,
//! and **writes once**.
//!
//! ```text
//! load version ─▶ next_step ─▶ run the action / evaluate ─▶ feed back ─▶ write once
//!                    ▲                                                      │
//!                    └──────────── still the same step? ────────────────────┘
//! ```
//!
//! ## One advance is one step, and one write
//!
//! [`advance`](Driver::advance) services **one step**, not one decision: a `Set`
//! of three assignments is three trips to the evaluator and one step, and its
//! `Next` — a branch whose guards need evaluating — belongs to the step that is
//! ending rather than to the one that has not begun. So the loop inside an
//! advance continues while the machine keeps asking about the same **entry** of
//! a step, and stops the moment the work belongs to another one.
//!
//! The entry, not the name: a `ForEach` whose body is a single step asks about
//! that one name once per item, and a pass that serviced all hundred of them
//! would give the loop one write and one trace row for the lot — the durability
//! granularity of the loop rather than of the item. `steps_taken` moves on every
//! entry, so comparing it is what makes a hundred iterations a hundred advances.
//!
//! What that write contains is decision 6's guarantee: the context, the cursor,
//! the attempt count, the run's state, its `wake_at` and the step's trace row,
//! committed **together, once**, in a single transaction — **and the rows the
//! step itself wrote go in it too**. A run is therefore never observed
//! half-advanced, and never advanced past a step whose writes were lost.
//!
//! ## One step, one transaction
//!
//! The transaction is opened for the step, not for the write at the end of it: a
//! [`SharedTx`] is handed to the action through its [`ActionContext`], the row
//! layer runs every statement for a table that transaction serves inside it, and
//! the events those writes raise are dispatched **in the same transaction** — so
//! a trigger that writes an audit row for a row this step wrote commits with the
//! step or vanishes with it. Then the run's advance and its trace rows go in
//! before the single commit.
//!
//! A step that **fails** is rolled back before anything else happens: the rows it
//! wrote are undone, and only then is the failure itself recorded (the attempt
//! count, the retry deadline, the error) — in a transaction of its own, because a
//! record of the failure that rolled back with the failure would be no record at
//! all.
//!
//! It begins when the step first touches the database, so a step that calls an
//! HTTP endpoint and writes nothing holds no transaction while it waits.
//!
//! What the transaction cannot contain is what the database does not hold: an
//! HTTP request, an email, an LLM call, a write to a table on another connection
//! or one a module serves, and the runs (agent or workflow) a step starts — each
//! of those is its own unit of durability, and §10.3 records the deviation rather
//! than implying otherwise. The consequence is the honest one: a step is **at
//! least once**. A node that dies between running an action and committing comes
//! back, is told to run the same step again, and runs no other. Steps SHOULD be
//! idempotent.
//!
//! ## The clock is a parameter
//!
//! Every method that could want the time takes a [`Clock`], and the task loop is
//! the only thing that holds a real one. A retry's backoff, a day-long `Wait` and
//! an abandoned approval are all tested by moving the clock; **no test sleeps**.

use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use sc_action::{ActionContext, Event, EventBindings, Trigger, TriggerDispatcher};
use sc_agent::{Run, note_wakeup, run_update};
use sc_catalog::{Catalog, SharedTx};
use sc_error::{Error, Result};
use sc_expr::{Formula, Operation};
use sc_types::Attrs;
use serde_json::Value as Json;

use crate::machine::{Conclusion, Decision, WorkflowRun};
use crate::run::{record, release, run_chain, run_event, run_state, run_version, run_workflow_id};
use crate::traces::{RunTrace, TraceOutcome, trace_insert};
use crate::versions::{require_current_workflow, require_workflow_version};
use crate::workflow::Workflow;

/// Where the engine reads the time.
///
/// A trait rather than `Utc::now()` at the call site for the reason the
/// scheduler's `tick(now)` is a parameter: a workflow's whole vocabulary is
/// deadlines — a retry's backoff, a `Wait`, an approval's timeout — and a test
/// that had to wait for one would be a test nobody runs.
pub trait Clock: Send + Sync {
    /// What time it is.
    fn now(&self) -> DateTime<Utc>;
}

/// The clock a server runs on.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock a test moves by hand.
///
/// Public rather than `#[cfg(test)]` because the engine's callers test through it
/// too: an end-to-end test that suspends a run for a day has to be able to make
/// tomorrow happen.
pub struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    /// A clock reading `at`.
    pub fn new(at: DateTime<Utc>) -> ManualClock {
        ManualClock(Mutex::new(at))
    }

    /// Move it forward.
    pub fn advance(&self, by: chrono::Duration) {
        if let Ok(mut now) = self.0.lock() {
            *now += by;
        }
    }

    /// Move it to an instant.
    pub fn set(&self, at: DateTime<Utc>) {
        if let Ok(mut now) = self.0.lock() {
            *now = at;
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        match self.0.lock() {
            Ok(now) => *now,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

/// What one [`advance`](Driver::advance) did.
#[derive(Debug, Clone, PartialEq)]
pub enum Advanced {
    /// A step was serviced and the run is runnable again at once.
    Stepped,
    /// The run suspended: until an instant, or — `None` — until a person answers.
    Suspended {
        /// When it next wants the engine.
        until: Option<DateTime<Utc>>,
    },
    /// It reached the end.
    Finished,
    /// It stopped at a step, for this reason.
    Failed {
        /// Where it stopped.
        step: String,
        /// Why.
        error: String,
    },
}

impl Advanced {
    /// Whether the run wants the engine again straight away.
    pub fn is_runnable(&self) -> bool {
        matches!(self, Advanced::Stepped)
    }
}

/// Everything advancing a run needs: the catalog it is stored in, the dispatcher
/// whose registry and services its steps run with, and a clock.
///
/// Holds no run: a driver advances whichever run it is handed, so the queue can
/// hand it several and a test can hand it one it built itself.
pub struct Driver<'a> {
    catalog: &'a Catalog,
    dispatcher: &'a TriggerDispatcher,
    clock: &'a dyn Clock,
}

impl<'a> Driver<'a> {
    /// A driver over `catalog`, running steps through `dispatcher`.
    pub fn new(
        catalog: &'a Catalog,
        dispatcher: &'a TriggerDispatcher,
        clock: &'a dyn Clock,
    ) -> Driver<'a> {
        Driver {
            catalog,
            dispatcher,
            clock,
        }
    }

    /// The workflow version `run` is pinned to.
    ///
    /// Loaded per advance rather than cached on the run, which is the whole
    /// implementation of "a suspended run finishes on its own version": the
    /// workflow may have been saved twice while this run waited, and the row this
    /// reads is the one it started on.
    pub async fn workflow_of(&self, run: &Run) -> Result<Workflow> {
        let id = run_workflow_id(run)?;
        let version = run_version(run)?;
        require_workflow_version(self.catalog, id, version).await
    }

    /// Advance `run` by **one step**, and write the result once.
    ///
    /// The run value is updated in place, so a caller driving several steps in a
    /// row does not reload between them; what is on the row and what is in hand
    /// are the same thing after every call.
    pub async fn advance(&self, run: &mut Run) -> Result<Advanced> {
        let workflow = self.workflow_of(run).await?;
        self.advance_on(run, &workflow).await
    }

    /// [`advance`](Driver::advance) with the version already in hand — what
    /// [`drive`](Driver::drive) uses so a hundred steps of one run are not a
    /// hundred loads of the same document.
    async fn advance_on(&self, run: &mut Run, workflow: &Workflow) -> Result<Advanced> {
        // The step's transaction. Nothing has begun on the database yet — it
        // opens on the first statement anything makes through it, and a step that
        // writes nothing never opens one at all.
        let tx = SharedTx::begin_primary(self.catalog)?;
        let mut state = run_state(run)?;
        let event = run_event(run)?;
        let chain = run_chain(run);
        let started_at = self.clock.now();
        let spent = state.steps_taken();

        // What this pass serviced, if anything: which **entry** of a step
        // (`steps_taken` as it was entered), its name, the attempt at it. One
        // trace row per step attempt (§9), so this is filled in once and never
        // twice.
        let mut serviced: Option<(u32, String, u32)> = None;
        let mut failure: Option<String> = None;
        // Whether the step's **own work** failed, which is what the step's
        // transaction lives or dies by. Not the same question as `failure`: a
        // step can do its work perfectly and the run still stop, because the
        // *program* turned out to be inconsistent (a `Next` naming a step the
        // workflow does not have). That failure is the workflow's, not the
        // step's, and rolling the step back for it would discard work that
        // succeeded — and leave a context that records what the rows no longer
        // say.
        let mut step_failed = false;

        // A **later** step this pass entered which stopped the run without doing
        // any work of its own — a `UserForm` reached from the step before it, or
        // a step the program turns out not to have. It gets a trace row of its
        // own rather than borrowing the serviced step's, because a timeline that
        // labelled the branch before an approval "suspended" and then never
        // mentioned the approval would be a timeline of the wrong run.
        let mut stopped_at: Option<(String, u32, TraceOutcome)> = None;

        // Whether the run was still going when this pass began. A pass over a
        // run that had already stopped writes the same row again and must not
        // report the same failure a second time — an alert per poll for a
        // failure that happened once is the report becoming the noise.
        let was_live = run.state.is_live();

        let advanced = loop {
            let now = self.clock.now();
            match state.next_step(workflow, now) {
                Decision::Done { .. } => break Advanced::Finished,
                Decision::Failed { step, error } => {
                    // A workflow that turned out to be inconsistent fails here
                    // rather than through a policy, and the step it stopped at is
                    // still worth a trace row when this pass is what entered it.
                    stop_at(
                        &mut serviced,
                        &mut stopped_at,
                        &step,
                        &state,
                        spent,
                        TraceOutcome::Error,
                    );
                    failure.get_or_insert_with(|| error.clone());
                    break Advanced::Failed { step, error };
                }
                Decision::Suspend { step, until, .. } => {
                    stop_at(
                        &mut serviced,
                        &mut stopped_at,
                        &step,
                        &state,
                        spent,
                        TraceOutcome::Suspended,
                    );
                    break Advanced::Suspended { until };
                }
                Decision::RunAction {
                    step,
                    action,
                    configuration,
                } => {
                    if serviced
                        .as_ref()
                        .is_some_and(|(entry, ..)| *entry != state.steps_taken())
                    {
                        // A different step entry wants the world: this pass is
                        // over, and the state already records that the next one
                        // has begun — so the next pass is told the same thing
                        // rather than entering it twice. The *entry* rather than
                        // the name, so one iteration of a single-step loop body
                        // is one advance and one write.
                        break Advanced::Stepped;
                    }
                    let attempt = state.attempt();
                    serviced = Some((state.steps_taken(), step.clone(), attempt));
                    let outcome = self
                        .run_action(&event, &chain, &step, &action, &configuration, &state, &tx)
                        .await;
                    match outcome {
                        Ok((value, context)) => {
                            state.context_written(context)?;
                            state.step_succeeded(value)?;
                        }
                        Err(e) => {
                            let error = sc_error::format_chain(&e);
                            state.step_failed(workflow, &error, now)?;
                            failure = Some(error);
                            step_failed = true;
                        }
                    }
                }
                Decision::Evaluate { step, formulas } => {
                    if serviced
                        .as_ref()
                        .is_some_and(|(entry, ..)| *entry != state.steps_taken())
                    {
                        break Advanced::Stepped;
                    }
                    let attempt = state.attempt();
                    serviced = Some((state.steps_taken(), step.clone(), attempt));
                    match self
                        .evaluate(&event, state.context(), &formulas, &step)
                        .await
                    {
                        Ok(values) => state.evaluated(workflow, values, now)?,
                        Err(e) => {
                            let error = sc_error::format_chain(&e);
                            state.step_failed(workflow, &error, now)?;
                            failure = Some(error);
                            step_failed = true;
                        }
                    }
                }
            }
        };

        // The trace rows, when this workflow asked for them and this pass
        // actually did something. Their `seq` comes off the run state, so a
        // recovered run's trace carries on rather than colliding with the rows it
        // already wrote — and both rows go in the **same** write as the advance,
        // which is decision 6's guarantee.
        //
        // Usually there is one: the step this pass serviced. There are two when
        // that step's `next` led straight into one that stopped the run.
        let mut traces = Vec::new();
        if workflow.trace {
            let finished_at = self.clock.now();
            if let Some((_, step, attempt)) = &serviced {
                let outcome = match (&failure, stopped_at.is_none() && state.is_suspended()) {
                    (Some(_), _) => TraceOutcome::Error,
                    (None, true) => TraceOutcome::Suspended,
                    (None, false) => TraceOutcome::Ok,
                };
                let mut trace =
                    RunTrace::new(run.id.0, state.next_trace_seq(), step, started_at, outcome)
                        .attempt(*attempt)
                        .finished_at(finished_at)
                        .context(state.context().clone());
                if let Some(error) = &failure {
                    trace = trace.error(error);
                }
                traces.push(trace);
            }
            if let Some((step, attempt, outcome)) = &stopped_at {
                // It did no work, so it started and finished now: what the row
                // records is that the run reached this step and stopped there.
                let mut trace = RunTrace::new(
                    run.id.0,
                    state.next_trace_seq(),
                    step,
                    finished_at,
                    *outcome,
                )
                .attempt(*attempt)
                .finished_at(finished_at)
                .context(state.context().clone());
                if let Some(error) = &failure {
                    trace = trace.error(error);
                }
                traces.push(trace);
            }
        }

        let now = self.clock.now();
        record(run, &state, now);
        // The row this step is about to write is the engine's queue: a `Wait`'s
        // deadline, a retry's backoff, or "at once" for a run that has more to
        // do. Telling the catalog now is what lets the queue stop asking the
        // database whether anything is due (`sc_catalog::RunWakeups`) — the
        // driver writes the row itself, so `save_run`'s note never sees it.
        note_wakeup(self.catalog, run);
        if !advanced.is_runnable() {
            // Nothing is working on it any more: a finished run must not sit
            // leased, and a suspended one must be claimable the instant its
            // deadline arrives rather than when a lease it no longer needs runs
            // out.
            release(run);
        }
        match step_failed {
            // The step failed. Whatever it wrote is undone **first** — half a
            // step is not a step — and the failure is then recorded on its own,
            // because a record that rolled back with the failure would leave a
            // run that had never heard of it.
            true => {
                if let Err(e) = tx.rollback().await {
                    sc_log::log_error!(
                        "workflow `{}` run {}: rolling back the failed step: {e}",
                        run.subject,
                        run.id
                    );
                }
                self.write(run, &traces).await?;
            }
            // The step succeeded: its rows, the advance and the trace rows commit
            // together, once (decision 6). Including when the run stops here
            // anyway because the *program* is inconsistent — the step's work is
            // still the step's work.
            false => {
                self.write_on(run, &traces, &tx).await?;
                tx.commit().await?;
            }
        }

        if let Advanced::Failed { step, error } = &advanced
            && was_live
        {
            self.report_failure(run, step, error).await;
        }
        Ok(advanced)
    }

    /// Advance `run` until it stops wanting the engine — it suspends, finishes or
    /// fails — renewing its lease as it goes.
    ///
    /// Resuming is the same call: the state came off the row, so a run reloaded
    /// after a restart carries on from the step it was on.
    pub async fn drive(&self, run: &mut Run) -> Result<Advanced> {
        let workflow = self.workflow_of(run).await?;
        let started = std::time::Instant::now();
        sc_log::log_verbose!(
            "workflow `{}` run {}: driving version {} from step {}",
            run.subject,
            run.id,
            workflow.version,
            run_state(run)?.current_step().unwrap_or("(the end)")
        );
        loop {
            let advanced = self.advance_on(run, &workflow).await?;
            if !advanced.is_runnable() {
                sc_log::log_info!(
                    "workflow `{}` run {}: {} in {}",
                    run.subject,
                    run.id,
                    label(&advanced),
                    sc_log::human_duration(started.elapsed())
                );
                return Ok(advanced);
            }
        }
    }

    /// Run one action step, with the run's context in hand.
    ///
    /// Everything an action gets from a trigger it gets here: the catalog, the
    /// event that started the run, its own configuration, the evaluator and the
    /// mailer this process installed, and the dispatcher — so a step may run
    /// another trigger exactly as `run_js_code` does. The **chain** is the run's
    /// plus this step's name, which is what bounds a cascade that goes through a
    /// workflow by `MAX_DEPTH` exactly as one through an action is bounded.
    ///
    /// The authority is the one §10.1 gives an action: the row actions write as
    /// admin carrying the event's user, which is a property of those actions and
    /// not something the engine gets to change.
    #[allow(clippy::too_many_arguments)]
    async fn run_action(
        &self,
        event: &Event,
        chain: &[String],
        step: &str,
        action: &str,
        configuration: &Attrs,
        state: &WorkflowRun,
        tx: &SharedTx,
    ) -> Result<(Json, Attrs)> {
        let registry = self.dispatcher.registry();
        let action = registry.require(action.trim())?;
        let services = self.dispatcher.services();
        let mut step_chain = chain.to_vec();
        step_chain.push(step.to_owned());

        // The seam §10.1 left for exactly this: the step reads and writes the
        // run's context, and — decision 8 — its own **settings** may name it, so
        // `insert_row`'s `context.total` is the value the `Set` before it wrote.
        let mut ctx = ActionContext::new(self.catalog, event, configuration, step)
            .with_chain(step_chain)
            .with_triggers(self.dispatcher)
            // The step's transaction: what this action writes — and what the
            // triggers its writes fire write — commits with this step's advance
            // or is rolled back with it (decision 6).
            .with_transaction(tx.clone())
            .with_run_context(state.context().clone());
        if let Some(evaluator) = &services.evaluator {
            ctx = ctx.with_evaluator(evaluator);
        }
        if let Some(mailer) = &services.mailer {
            ctx = ctx.with_mailer(mailer);
        }
        // And the other guest languages, so a `run_python_code` step reaches the
        // same adapter a `run_python_code` trigger does — the reason `services`
        // is exposed at all.
        ctx = ctx.with_adapters(&services.adapters);
        // The action's return value is what the step contributes to the context;
        // what it wrote there directly travels with it. It runs inside the
        // event's origin, as a trigger's action does (TODO-payments P1.3, P1.5):
        // a step that resumes tomorrow still writes for the application and the
        // guest session that started the run.
        let value = sc_catalog::with_origin(event.origin(), action.run(&mut ctx)).await?;
        Ok((value, ctx.context))
    }

    /// Evaluate a step's formulas, in order, in the scope
    /// [`workflow_shape`](crate::workflow_shape) decides: what an action's
    /// configuration sees, plus the ambient `context`.
    ///
    /// One failure fails the step, and the message names the formula: a `Set`
    /// whose third assignment throws must not leave the first two applied and the
    /// run carrying on as though nothing happened.
    async fn evaluate(
        &self,
        event: &Event,
        context: &Attrs,
        formulas: &[String],
        step: &str,
    ) -> Result<Vec<Json>> {
        let evaluator = self
            .dispatcher
            .services()
            .evaluator
            .clone()
            .ok_or_else(|| {
                Error::config(format!(
                    "step `{step}` has a formula to evaluate but no JavaScript engine \
                 is available in this context"
                ))
            })?;
        let bindings = EventBindings::of(event).with_context(context);
        let mut values = Vec::with_capacity(formulas.len());
        for source in formulas {
            let formula = Formula::parse(source)
                .map_err(|e| Error::invalid(format!("step `{step}`: `{source}`: {e}")))?;
            let call = bindings.call(&formula, Operation::Read, &BTreeMap::new());
            let value = evaluator
                .eval_value(call)
                .await
                .map_err(|e| Error::invalid(format!("step `{step}`: `{source}`: {e}")))?;
            values.push(value);
        }
        Ok(values)
    }

    /// The advance, in a transaction of its own: the run's row and, when the
    /// workflow is traced, its step's trace rows.
    ///
    /// What a **failed** step's advance takes, after its own transaction has been
    /// rolled back — the failure has to be recorded even though everything the
    /// step did is being undone.
    async fn write(&self, run: &Run, traces: &[RunTrace]) -> Result<()> {
        let tx = SharedTx::begin_primary(self.catalog)?;
        match self.write_on(run, traces, &tx).await {
            Ok(()) => tx.commit().await,
            Err(e) => {
                let _ = tx.rollback().await;
                Err(e)
            }
        }
    }

    /// The advance's statements on `tx`, committing nothing: the run's row and,
    /// when the workflow is traced, its step's trace rows.
    ///
    /// On the **step's** transaction for a step that succeeded, which is the
    /// whole of decision 6 in one call — the rows the step wrote and the fact
    /// that it took it are one commit, so a reader never sees a trace row for an
    /// advance that did not happen, an advance whose writes were lost, or a run
    /// half way through a step.
    async fn write_on(&self, run: &Run, traces: &[RunTrace], tx: &SharedTx) -> Result<()> {
        // No caller context: these are the engine's own `_fd_*` tables, which
        // carry no policies (§9) and are not reachable by an application's rows.
        tx.run(None, &sc_query::Statement::from(run_update(run)))
            .await?;
        for trace in traces {
            tx.run(None, &sc_query::Statement::from(trace_insert(trace)))
                .await?;
        }
        Ok(())
    }

    /// A failed run is a **record**, not a lost report (§10.3, phase 3.6).
    ///
    /// The reason is already on the row, with the step named — this is the other
    /// half: the server-side log, and the `error` **event**, so an alerting
    /// trigger sees a workflow that stopped exactly as it sees a request that
    /// failed (§16). Firing it is guarded against re-entrancy by the dispatcher,
    /// so an alert that itself fails cannot become a loop.
    async fn report_failure(&self, run: &Run, step: &str, error: &str) {
        sc_log::log_error!(
            "workflow `{}` run {}: stopped at step `{step}`: {error}",
            run.subject,
            run.id
        );
        let event = sc_action::Event::error(
            sc_error::ErrorKind::Application,
            format!(
                "workflow `{}` stopped at step `{step}`: {error}",
                run.subject
            ),
            "WORKFLOW",
            &format!("/{}/{step}", run.subject),
        );
        self.dispatcher.fire(self.catalog, &event).await;
    }
}

/// Start a run of `trigger`'s workflow for `event`, and drive it as far as it
/// goes right now.
///
/// The run row is written **before** the first step, so a crash during the first
/// step leaves a run to recover rather than nothing at all — and so the caller
/// has something addressable even if the engine dies immediately.
pub async fn start_run(
    catalog: &Catalog,
    dispatcher: &TriggerDispatcher,
    clock: &dyn Clock,
    trigger: &Trigger,
    event: &Event,
    chain: Vec<String>,
) -> Result<Run> {
    let workflow = require_current_workflow(catalog, trigger.id, &trigger.name).await?;
    // Checked again here, not only on save (decision 11). The world moves
    // underneath a stored document — a table dropped, a plugin that provided a
    // step's action removed, a dump restored somewhere its formulas do not
    // resolve — and a run of a workflow that no longer holds together would
    // discover that half way through, with the steps before the broken one
    // already done. Refusing costs one pass and is the same fail-closed reading
    // an invalid trigger gets from the live set.
    crate::validate::validate_workflow(
        catalog,
        &dispatcher.registry(),
        &workflow,
        trigger.channel.as_deref(),
    )
    .await
    .map_err(|e| Error::invalid(format!("trigger `{}`: {e}", trigger.name)))?;
    let state = WorkflowRun::new(&workflow);
    let mut run = crate::run::new_run(trigger, workflow.version, event, chain, &state);
    sc_agent::save_run(catalog, &run).await?;
    sc_log::log_info!(
        "workflow `{}`: started run {} on version {}",
        trigger.name,
        run.id,
        workflow.version
    );
    let driver = Driver::new(catalog, dispatcher, clock);
    // A failure to *drive* is not a failure to start: the run exists, its state
    // says what happened, and the caller gets the run back rather than an error
    // that loses the id.
    if let Err(e) = driver.drive(&mut run).await {
        sc_log::log_error!("workflow `{}` run {}: {e}", trigger.name, run.id);
        run.error = Some(e.to_string());
        run.state = sc_agent::RunState::Failed;
        release(&mut run);
        sc_agent::save_run(catalog, &run).await?;
    }
    Ok(run)
}

/// Record which step a pass stopped at, for the trace.
///
/// It is the **serviced** step when this pass did that step's work — a step whose
/// own action failed, a retry's backoff, a `Wait` whose deadline was just
/// evaluated. It is a *second* row when the pass finished one step and its `next`
/// led straight into a step that stopped without doing anything: an approval
/// reached from the branch before it is a step attempt of its own, and a timeline
/// that never mentioned it would be a timeline of the wrong run.
///
/// And it is neither when the run is merely being told again about a stop it was
/// already in, which is what an idempotent `next_step` answers to a pass that has
/// nothing to do (`steps_taken` has not moved).
///
/// Which of the three it is, is decided by the step **entry** rather than by its
/// name, for the reason [`advance_on`](Driver::advance_on)'s loop is: the same
/// name entered twice — an iteration of a single-step loop body — is two step
/// attempts and two rows, not one.
fn stop_at(
    serviced: &mut Option<(u32, String, u32)>,
    stopped_at: &mut Option<(String, u32, TraceOutcome)>,
    step: &str,
    state: &WorkflowRun,
    spent: u32,
    outcome: TraceOutcome,
) {
    match serviced {
        Some((entry, ..)) if *entry == state.steps_taken() => {}
        Some(_) => *stopped_at = Some((step.to_owned(), state.attempt(), outcome)),
        None if state.steps_taken() > spent => {
            *serviced = Some((state.steps_taken(), step.to_owned(), state.attempt()));
        }
        None => {}
    }
}

/// How an ending reads in one phrase on the run's closing log line.
fn label(advanced: &Advanced) -> String {
    match advanced {
        Advanced::Stepped => "stepped".to_owned(),
        Advanced::Suspended { until: Some(at) } => format!("suspended until {at}"),
        Advanced::Suspended { until: None } => "suspended, waiting for a person".to_owned(),
        Advanced::Finished => "finished".to_owned(),
        Advanced::Failed { step, .. } => format!("failed at step `{step}`"),
    }
}

/// How a conclusion reads for a caller that wants one word.
pub fn conclusion_label(conclusion: &Conclusion) -> &'static str {
    match conclusion {
        Conclusion::Finished => "finished",
        Conclusion::Failed { .. } => "failed",
    }
}

/// A node identifier for this process: readable, and unique enough that two
/// nodes never claim each other's runs.
pub fn node_id() -> String {
    format!("saltcorn-{}", uuid::Uuid::new_v4())
}

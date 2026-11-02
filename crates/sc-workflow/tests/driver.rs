//! The driver, the queue and the engine against a real Postgres (§10.3, phase 3;
//! TODO 7.2 and 7.3).
//!
//! The machine's own rules are pinned synchronously in `src/machine.rs`; what is
//! under test here is everything the machine deliberately cannot decide — that a
//! step's effect actually happens, that the advance is **written**, that the run
//! is pinned to the version it started on however many times the workflow is
//! edited under it, that a crashed node's run is picked up when its lease runs
//! out, and that a step runs **once more** on recovery rather than twice from the
//! start.
//!
//! Two things are deliberately real: the **isolate** (a step's formulas go
//! through the same reified evaluator every other formula in the tree does) and
//! the **database**. What is scripted is the actions, through a recording
//! registry — that is the point of the seam — and the **clock**, which is moved
//! rather than waited on. No test here sleeps.

use sc_catalog::Authority;
use sc_types::Role;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use sc_action::{
    Action, ActionContext, ActionRegistry, Event, EventKind, Trigger, TriggerBody,
    TriggerDispatcher, TriggerId,
};
use sc_agent::{Run, RunState, bootstrap_runs, load_run, require_run, save_run};
use sc_catalog::{Catalog, DataField, Principal};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use sc_types::clock::{Clock, ManualClock};
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use sc_workflow::machine::WorkflowRun;
use sc_workflow::queue::WorkQueue;
use sc_workflow::{
    Advanced, Assignment, Backoff, BranchArm, DatabaseQueue, Driver, ErrorPolicy, Next, RunTrace,
    Step, StepKind, TraceOutcome, Workflow, WorkflowEngineTask, bootstrap_run_traces,
    bootstrap_workflow_versions, list_run_traces, run_state, save_workflow, start_run,
};
use serde_json::{Value as Json, json};

// ---------------------------------------------------------------------------
// The recording action: what a step *does*, scripted.
// ---------------------------------------------------------------------------

/// What the recorded action should do on its nth call.
#[derive(Debug, Clone)]
enum Scripted {
    /// Return this value.
    Returns(Json),
    /// Fail with this message.
    Fails(String),
    /// Write this key into the run context, and return null.
    Writes(String, Json),
}

/// One action, registered under a name, that records every call it gets and does
/// what the script says.
///
/// The seam the whole engine is built on: "run any registered action" is what
/// makes `send_email`, `run_js_code` and `run_agent` workflow steps with no code
/// here, and it is what makes the driver testable without any of them.
struct Recorder {
    name: String,
    calls: Arc<Mutex<Vec<(String, Attrs)>>>,
    script: Arc<Mutex<HashMap<String, Vec<Scripted>>>>,
}

impl Recorder {
    fn new(name: &str) -> (Recorder, Recording) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(HashMap::new()));
        (
            Recorder {
                name: name.to_owned(),
                calls: Arc::clone(&calls),
                script: Arc::clone(&script),
            },
            Recording { calls, script },
        )
    }
}

/// The handle a test keeps: what was called, and what the next call should do.
#[derive(Clone)]
struct Recording {
    calls: Arc<Mutex<Vec<(String, Attrs)>>>,
    script: Arc<Mutex<HashMap<String, Vec<Scripted>>>>,
}

impl Recording {
    /// What step `step`'s calls should do, in order. A call past the end of the
    /// script returns null.
    fn script(&self, step: &str, outcomes: Vec<Scripted>) {
        if let Ok(mut script) = self.script.lock() {
            script.insert(step.to_owned(), outcomes);
        }
    }

    /// The step names that were called, in order.
    fn steps(&self) -> Vec<String> {
        match self.calls.lock() {
            Ok(calls) => calls.iter().map(|(step, _)| step.clone()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// How many times `step` was called.
    fn count(&self, step: &str) -> usize {
        self.steps().iter().filter(|s| *s == step).count()
    }

    /// The configuration the nth call to `step` was given.
    fn config(&self, step: &str, nth: usize) -> Option<Attrs> {
        let calls = self.calls.lock().ok()?;
        calls
            .iter()
            .filter(|(s, _)| s == step)
            .nth(nth)
            .map(|(_, c)| c.clone())
    }
}

#[async_trait::async_trait]
impl Action for Recorder {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Record the call and do what the script says"
    }
    fn config_spec(&self) -> Vec<FormField> {
        vec![FormField::new("note", BasicType::Text)]
    }
    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        // The step's name is what the driver puts in `ActionContext::trigger`, so
        // this is how the script is keyed and how the chain is asserted.
        let step = ctx.trigger.to_owned();
        let nth = {
            let mut calls = self
                .calls
                .lock()
                .map_err(|_| Error::msg("recorder poisoned"))?;
            let nth = calls.iter().filter(|(s, _)| *s == step).count();
            calls.push((step.clone(), ctx.config.clone()));
            nth
        };
        // The chain a step's own writes carry: the run's, plus this step.
        ctx.context.insert(
            format!("chain_at_{step}"),
            Json::Array(ctx.chain.iter().map(|n| json!(n)).collect()),
        );
        let outcome = {
            let script = self
                .script
                .lock()
                .map_err(|_| Error::msg("recorder poisoned"))?;
            script.get(&step).and_then(|s| s.get(nth)).cloned()
        };
        match outcome {
            None => Ok(Json::Null),
            Some(Scripted::Returns(value)) => Ok(value),
            Some(Scripted::Fails(message)) => Err(Error::invalid(message)),
            Some(Scripted::Writes(key, value)) => {
                ctx.context.insert(key, value);
                Ok(Json::Null)
            }
        }
    }
}

/// An action that runs **another trigger**, as a step's write would.
///
/// The cascade bound (§10.2's `MAX_DEPTH`) is enforced where every other event's
/// is: `Event::firing`, reading the chain the caller carries. A step's chain is
/// the run's plus the step, so this is how a workflow that keeps starting work
/// is stopped at the same depth an action that keeps writing rows is.
struct Firer {
    trigger: String,
}

#[async_trait::async_trait]
impl Action for Firer {
    fn name(&self) -> &str {
        "fire"
    }
    fn description(&self) -> &str {
        "Run another trigger, carrying this step's chain"
    }
    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }
    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let dispatcher = ctx
            .triggers()
            .ok_or_else(|| Error::config("no dispatcher in this context"))?;
        let caller = Principal::new(Role::ADMIN, None).chained(ctx.chain.clone());
        dispatcher
            .run_trigger(
                ctx.catalog,
                &self.trigger,
                Json::Null,
                Authority::Caller(&caller),
            )
            .await
    }
}

// ---------------------------------------------------------------------------
// The fixture.
// ---------------------------------------------------------------------------

/// A catalog with the workflow tables, the runs table and one ordinary table up.
async fn catalog(db: &TestDb) -> Result<Arc<Catalog>> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    catalog
        .create_table(
            "orders",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key(),
                DataField::plain("total", TypeRef::Basic(BasicType::Int)),
            ],
        )
        .await?;
    bootstrap_workflow_versions(&catalog).await?;
    bootstrap_run_traces(&catalog).await?;
    bootstrap_runs(&catalog).await?;
    Ok(Arc::new(catalog))
}

/// The same catalog opened again over a database that already has all of that —
/// the process that comes after a restart.
async fn reopen(db: &TestDb) -> Result<Arc<Catalog>> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Ok(Arc::new(
        Catalog::init(driver as Arc<dyn DatabaseDriver>).await?,
    ))
}

/// A dispatcher with the recording action registered and a real isolate — what a
/// step actually runs through.
fn dispatcher(recorder: Recorder) -> Result<Arc<TriggerDispatcher>> {
    let mut registry = ActionRegistry::new();
    registry.register(Arc::new(recorder))?;
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    Ok(Arc::new(
        TriggerDispatcher::new(Arc::new(registry)).with_evaluator(evaluator),
    ))
}

/// The trigger a workflow is the body of: an insert on `orders`.
fn trigger(name: &str) -> Trigger {
    Trigger::with_body(
        TriggerId::new(),
        name,
        EventKind::Insert,
        TriggerBody::Workflow,
    )
    .on("orders")
}

/// The event that starts a run: an order of 120 was inserted.
fn insert_event() -> Event {
    Event::new(EventKind::Insert)
        .on("orders")
        .row(json!({ "id": 1, "total": 120 }))
}

/// One `Action` step running the recorder.
fn action_step(name: &str) -> Step {
    Step::new(
        name,
        StepKind::Action {
            action: "record".to_owned(),
            configuration: Attrs::new(),
        },
    )
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_walks_its_steps_writing_the_context_after_every_one() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;
    recording.script("fetch", vec![Scripted::Returns(json!({ "amount": 40 }))]);

    let trigger = trigger("bill");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            action_step("fetch").then(Next::step("total")),
            Step::new(
                "total",
                StepKind::Set {
                    assignments: vec![
                        Assignment::new("subtotal", "context.fetch.amount"),
                        // A later assignment reads what an earlier one wrote, and
                        // the event that started the run is still in scope.
                        Assignment::new("grand", "context.subtotal + row.total"),
                    ],
                },
            )
            .then(Next::End),
        ],
    )
    .traced();
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["bill".to_owned()],
    )
    .await?;

    let stored = require_run(&catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Done, "{:?}", stored.error);
    let state = run_state(&stored)?;
    assert_eq!(state.context()["fetch"], json!({ "amount": 40 }));
    assert_eq!(state.context()["subtotal"], json!(40));
    assert_eq!(state.context()["grand"], json!(160));
    assert_eq!(recording.steps(), vec!["fetch".to_owned()]);

    // One trace row per step attempt — not one per formula: the `Set` has two
    // assignments and is one step.
    let traces = list_run_traces(&catalog, run.id.0).await?;
    let steps: Vec<&str> = traces.iter().map(|t| t.step.as_str()).collect();
    assert_eq!(steps, vec!["fetch", "total"]);
    assert!(traces.iter().all(|t| t.outcome == TraceOutcome::Ok));
    assert_eq!(traces[0].seq, 1);
    assert_eq!(traces[1].seq, 2);
    // The context **after** the step, which is what the timeline draws.
    assert_eq!(traces[1].context["grand"], json!(160));
    Ok(())
}

#[tokio::test]
async fn a_step_that_fails_is_retried_with_backoff_and_then_succeeds() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;
    recording.script(
        "call",
        vec![
            Scripted::Fails("the endpoint hung up".to_owned()),
            Scripted::Returns(json!("ok")),
        ],
    );

    let trigger = trigger("retrying");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            action_step("call")
                .then(Next::End)
                .on_error(ErrorPolicy::Retry {
                    max: 3,
                    backoff: Backoff {
                        initial_ms: 1_000,
                        factor: 2.0,
                        max_ms: 60_000,
                        jitter: false,
                    },
                }),
        ],
    )
    .traced();
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let started = Utc::now();
    let clock = ManualClock::new(started);
    let mut run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["retrying".to_owned()],
    )
    .await?;

    // The first attempt failed, so the run is *waiting* — not failed — and its
    // `wake_at` is the backoff deadline. Nothing holds a lease on it.
    assert_eq!(run.state, RunState::Waiting);
    let wake_at = run.wake_at.expect("a retry names an instant");
    assert_eq!(wake_at, started + Duration::milliseconds(1_000));
    assert_eq!(run.lease_until, None);
    assert_eq!(recording.count("call"), 1);

    // Before the deadline the run is not runnable, and asking anyway does not
    // run the step again.
    let queue = DatabaseQueue::new(
        Arc::clone(&catalog),
        "node-a",
        std::time::Duration::from_millis(1),
    );
    assert!(queue.due(started, 10).await?.is_empty());

    // Move the clock past the backoff: now it is due, and the *same step* runs
    // again — the second attempt, which succeeds.
    clock.set(wake_at);
    let driver = Driver::new(&catalog, &dispatcher, &clock);
    let advanced = driver.drive(&mut run).await?;
    assert_eq!(advanced, Advanced::Finished);
    assert_eq!(recording.count("call"), 2);

    let stored = require_run(&catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Done);
    assert_eq!(run_state(&stored)?.context()["call"], json!("ok"));

    // Both attempts are in the trace, the first as an error and the second as
    // the attempt it was.
    let traces = list_run_traces(&catalog, run.id.0).await?;
    assert_eq!(traces.len(), 2);
    assert_eq!(traces[0].outcome, TraceOutcome::Error);
    assert_eq!(traces[0].attempt, 1);
    assert!(
        traces[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("hung up"),
        "{:?}",
        traces[0].error
    );
    assert_eq!(traces[1].outcome, TraceOutcome::Ok);
    assert_eq!(traces[1].attempt, 2);
    Ok(())
}

#[tokio::test]
async fn a_step_that_keeps_failing_jumps_to_the_handler_with_the_reason() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;
    recording.script("call", vec![Scripted::Fails("no route to host".to_owned())]);

    let trigger = trigger("handled");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            action_step("call").then(Next::End),
            action_step("apologise").then(Next::End),
        ],
    )
    .on_error(ErrorPolicy::Handler {
        step: "apologise".to_owned(),
    });
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["handled".to_owned()],
    )
    .await?;

    assert_eq!(run.state, RunState::Done, "{:?}", run.error);
    assert_eq!(
        recording.steps(),
        vec!["call".to_owned(), "apologise".to_owned()]
    );
    // The handler can read what went wrong, under the reserved key.
    let context = run_state(&run)?.context().clone();
    assert_eq!(context["error"]["step"], json!("call"));
    assert!(
        context["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no route to host"),
        "{context:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_run_is_a_record_naming_the_step() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;
    recording.script("charge", vec![Scripted::Fails("card declined".to_owned())]);

    let trigger = trigger("charging");
    let workflow = Workflow::of(trigger.id, 1, vec![action_step("charge").then(Next::End)]);
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["charging".to_owned()],
    )
    .await?;

    let stored = require_run(&catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Failed);
    let error = stored.error.unwrap_or_default();
    assert!(error.contains("charge"), "{error}");
    assert!(error.contains("card declined"), "{error}");
    // A failed run leaves the queue's reach: nothing wakes it, and nothing holds
    // it.
    assert_eq!(stored.wake_at, None);
    assert_eq!(stored.lease_until, None);
    Ok(())
}

#[tokio::test]
async fn a_run_branches_loops_and_carries_the_chain_into_every_step() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;

    let trigger = trigger("lines");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            // A branch on the event's own row: 120 is over 100, so the loop runs.
            Step::new(
                "check",
                StepKind::Set {
                    assignments: vec![Assignment::new("lines", "[1, 2, 3]")],
                },
            )
            .then(Next::Branch {
                arms: vec![BranchArm::new("row.total > 100", "each")],
                otherwise: Some("skip".to_owned()),
            }),
            Step::new(
                "each",
                StepKind::ForEach {
                    over: "context.lines".to_owned(),
                    var: "line".to_owned(),
                    body: "handle".to_owned(),
                },
            )
            .then(Next::End),
            action_step("handle").then(Next::End),
            action_step("skip").then(Next::End),
        ],
    );
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["outer".to_owned(), "lines".to_owned()],
    )
    .await?;

    assert_eq!(run.state, RunState::Done, "{:?}", run.error);
    // The branch chose the loop, which ran the body once per item.
    assert_eq!(recording.count("handle"), 3);
    assert_eq!(recording.count("skip"), 0);

    // A step's writes descend from the chain that led to the run, plus the step
    // — which is what makes `MAX_DEPTH` bound a cascade through a workflow.
    let chain = run_state(&run)?.context()["chain_at_handle"].clone();
    assert_eq!(chain, json!(["outer", "lines", "handle"]));
    Ok(())
}

#[tokio::test]
async fn a_run_finishes_on_the_version_it_started_with() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;

    let trigger = trigger("versioned");
    // Version 1: wait an hour, then run `first`.
    let v1 = Workflow::of(
        trigger.id,
        1,
        vec![
            Step::new(
                "hold",
                StepKind::Wait {
                    until: "1000 * 60 * 60".to_owned(),
                },
            )
            .then(Next::step("first")),
            action_step("first").then(Next::End),
        ],
    );
    save_workflow(&catalog, &v1, "v1", None).await?;

    let started = Utc::now();
    let clock = ManualClock::new(started);
    let mut run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["versioned".to_owned()],
    )
    .await?;
    assert_eq!(run.state, RunState::Waiting);
    assert_eq!(run.subject_version, Some(1));
    assert_eq!(run.wake_at, Some(started + Duration::hours(1)));

    // The workflow is edited twice while the run waits.
    for (version, step) in [(2u32, "second"), (3, "third")] {
        let edited = Workflow::of(
            trigger.id,
            version,
            vec![
                Step::new(
                    "hold",
                    StepKind::Wait {
                        until: "1000 * 60 * 60".to_owned(),
                    },
                )
                .then(Next::step(step)),
                action_step(step).then(Next::End),
            ],
        );
        save_workflow(&catalog, &edited, "edited", None).await?;
    }

    // Tomorrow. The run carries on — on **its** version.
    clock.set(started + Duration::hours(2));
    let driver = Driver::new(&catalog, &dispatcher, &clock);
    assert_eq!(driver.drive(&mut run).await?, Advanced::Finished);
    assert_eq!(recording.steps(), vec!["first".to_owned()]);
    assert_eq!(
        require_run(&catalog, run.id).await?.subject_version,
        Some(1)
    );
    Ok(())
}

#[tokio::test]
async fn a_run_that_waited_across_a_restart_is_finished_by_the_process_that_comes_after()
-> Result<()> {
    let db = TestDb::new().await?;
    let started = Utc::now();

    // --- the process that starts the run ------------------------------------
    //
    // Everything it holds is inside this block: its catalog and pool handle, its
    // action registry, its dispatcher, its clock and the run value itself. What
    // crosses the line is a UUID, which is what "the server is restarted while
    // the run waits" amounts to.
    let id = {
        let catalog = catalog(&db).await?;
        let (recorder, recording) = Recorder::new("record");
        let dispatcher = dispatcher(recorder)?;
        let trigger = trigger("restarted");
        let workflow = Workflow::of(
            trigger.id,
            1,
            vec![
                Step::new(
                    "hold",
                    StepKind::Wait {
                        until: "1000 * 60 * 60".to_owned(),
                    },
                )
                .then(Next::step("after")),
                action_step("after").then(Next::End),
            ],
        )
        .traced();
        save_workflow(&catalog, &workflow, "v1", None).await?;
        let clock = ManualClock::new(started);
        let run = start_run(
            &catalog,
            &dispatcher,
            &clock,
            &trigger,
            &insert_event(),
            vec!["restarted".to_owned()],
        )
        .await?;
        assert_eq!(run.state, RunState::Waiting);
        assert_eq!(run.wake_at, Some(started + Duration::hours(1)));
        assert_eq!(recording.count("after"), 0, "it has not got there yet");
        run.id
    };

    // --- the process that comes after ---------------------------------------
    //
    // A fresh catalog over the same database, a fresh registry, and a **fresh
    // recording** — so "the step ran" below is this process's observation and
    // not a leftover of the one that is gone.
    let catalog = reopen(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;
    let clock = ManualClock::new(started + Duration::hours(2));

    let mut run = require_run(&catalog, id).await?;
    assert_eq!(run.state, RunState::Waiting);
    assert_eq!(run.subject_version, Some(1));
    let driver = Driver::new(&catalog, &dispatcher, &clock);
    assert_eq!(driver.drive(&mut run).await?, Advanced::Finished);

    // The step the first process never reached ran here, exactly once, and the
    // whole path is in one trace across the two lives of the run: the state was
    // in the database, so resuming is a load and not a reconstruction.
    assert_eq!(recording.steps(), vec!["after".to_owned()]);
    let traces = list_run_traces(&catalog, id.0).await?;
    let steps: Vec<&str> = traces.iter().map(|t| t.step.as_str()).collect();
    assert_eq!(steps, vec!["hold", "after"]);
    assert_eq!(require_run(&catalog, id).await?.state, RunState::Done);
    Ok(())
}

#[tokio::test]
async fn a_crashed_nodes_run_is_picked_up_when_its_lease_runs_out() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;

    let trigger = trigger("recovering");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            action_step("one").then(Next::step("two")),
            action_step("two").then(Next::End),
        ],
    );
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let now = Utc::now();
    let clock = ManualClock::new(now);

    // A run written as a node that died *while running the second step* would
    // have: the cursor on `two`, a lease in the past, and `one` already done.
    // Building the row rather than killing a process is what makes the recovery
    // path testable at all.
    let mut state = WorkflowRun::new(&workflow);
    state.next_step(&workflow, now); // enters `one`
    state.step_succeeded(json!("done"))?;
    state.next_step(&workflow, now); // resolves on to `two`, and enters it
    let mut crashed = sc_workflow::new_run(
        &trigger,
        1,
        &insert_event(),
        vec!["recovering".to_owned()],
        &state,
    );
    crashed.wake_at = Some(now - Duration::minutes(5));
    crashed.lease_until = Some(now - Duration::minutes(4));
    crashed.claimed_by = Some("node-that-died".to_owned());
    save_run(&catalog, &crashed).await?;

    // The next poll picks it up: an expired lease reads exactly like no lease.
    let queue = DatabaseQueue::new(
        Arc::clone(&catalog),
        "node-b",
        std::time::Duration::from_millis(1),
    );
    let claimed = queue.claim(now, now + Duration::seconds(60), 10).await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, crashed.id);
    assert_eq!(claimed[0].claimed_by.as_deref(), Some("node-b"));

    // And a second node asking at the same moment gets nothing: the lease is now
    // in the future, so the compare-and-set matches no row.
    let other = DatabaseQueue::new(
        Arc::clone(&catalog),
        "node-c",
        std::time::Duration::from_millis(1),
    );
    assert!(
        other
            .claim(now, now + Duration::seconds(60), 10)
            .await?
            .is_empty()
    );

    let mut run = claimed.into_iter().next().unwrap_or(crashed);
    let driver = Driver::new(&catalog, &dispatcher, &clock);
    assert_eq!(driver.drive(&mut run).await?, Advanced::Finished);

    // The step that was in flight ran **once more**, and the one already done did
    // not run again — which is at-least-once, not from-the-start.
    assert_eq!(recording.steps(), vec!["two".to_owned()]);
    let stored = require_run(&catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Done);
    // A finished run holds no lease.
    assert_eq!(stored.lease_until, None);
    assert_eq!(stored.claimed_by, None);
    Ok(())
}

#[tokio::test]
async fn the_engine_task_advances_a_run_whose_timer_has_come_round() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;

    let trigger = trigger("timed");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            Step::new(
                "hold",
                StepKind::Wait {
                    until: "60000".to_owned(),
                },
            )
            .then(Next::step("after")),
            action_step("after").then(Next::End),
        ],
    );
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let started = Utc::now();
    let clock = Arc::new(ManualClock::new(started));
    let queue = Arc::new(DatabaseQueue::new(
        Arc::clone(&catalog),
        "node-a",
        std::time::Duration::from_millis(1),
    ));
    let engine = Arc::new(WorkflowEngineTask::with_queue(
        Arc::clone(&catalog),
        Arc::clone(&dispatcher),
        queue,
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));
    // Installed on the dispatcher: from here a workflow trigger starts a run.
    dispatcher.set_workflow_engine(Arc::clone(&engine) as Arc<dyn sc_action::WorkflowEngine>);

    let run = start_run(
        &catalog,
        &dispatcher,
        clock.as_ref(),
        &trigger,
        &insert_event(),
        vec!["timed".to_owned()],
    )
    .await?;
    assert_eq!(run.state, RunState::Waiting);

    // Nothing is due yet, so a tick does nothing at all.
    assert!(engine.tick().await.is_empty());
    assert_eq!(recording.count("after"), 0);

    // A minute later it is, and the tick finishes it.
    clock.advance(Duration::seconds(61));
    let advanced = engine.tick().await;
    assert_eq!(advanced, vec![run.id]);
    assert_eq!(recording.count("after"), 1);
    assert_eq!(require_run(&catalog, run.id).await?.state, RunState::Done);
    Ok(())
}

#[tokio::test]
async fn a_form_step_suspends_with_no_deadline_and_leaves_the_queues_reach() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, _recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;

    let trigger = trigger("approval");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            Step::new(
                "approve",
                StepKind::UserForm {
                    fields: vec![sc_workflow::FieldDecl::new("ok", "bool")],
                    assign_to: "approval".to_owned(),
                    min_role: None,
                    timeout: None,
                },
            )
            .then(Next::End),
        ],
    )
    .traced();
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let now = Utc::now();
    let clock = ManualClock::new(now);
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["approval".to_owned()],
    )
    .await?;

    let stored = require_run(&catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Waiting);
    // A NULL `wake_at` is what "no clock will make this runnable" means.
    assert_eq!(stored.wake_at, None);
    let state = run_state(&stored)?;
    let form = state.pending_form().expect("a form to fill in");
    assert_eq!(form.assign_to, "approval");
    assert_eq!(form.fields.len(), 1);

    // Even a year later the queue does not offer it: it is live, and runnable at
    // no point.
    let queue = DatabaseQueue::new(
        Arc::clone(&catalog),
        "node-a",
        std::time::Duration::from_millis(1),
    );
    assert!(queue.due(now + Duration::days(365), 10).await?.is_empty());

    // The suspension is in the trace, as a suspension.
    let traces = list_run_traces(&catalog, run.id.0).await?;
    assert_eq!(traces.len(), 1);
    assert_eq!(traces[0].outcome, TraceOutcome::Suspended);
    assert_eq!(traces[0].step, "approve");
    Ok(())
}

#[tokio::test]
async fn a_hundred_iterations_stay_inside_the_budget_and_the_budget_stops_a_loop() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;

    let hundred = trigger("hundred");
    let mut workflow = Workflow::of(
        hundred.id,
        1,
        vec![
            Step::new(
                "items",
                StepKind::Set {
                    assignments: vec![Assignment::new(
                        "items",
                        "Array(100).fill(0).map((_, i) => i)",
                    )],
                },
            )
            .then(Next::step("each")),
            Step::new(
                "each",
                StepKind::ForEach {
                    over: "context.items".to_owned(),
                    var: "item".to_owned(),
                    body: "one".to_owned(),
                },
            )
            .then(Next::End),
            action_step("one").then(Next::End),
        ],
    );
    workflow.max_steps = 1_000;
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &hundred,
        &insert_event(),
        vec!["hundred".to_owned()],
    )
    .await?;
    assert_eq!(run.state, RunState::Done, "{:?}", run.error);
    assert_eq!(recording.count("one"), 100);

    // And the same shape with a budget too small stops, naming the number rather
    // than looping forever.
    let tight = trigger("tight");
    let mut small = workflow.clone();
    small.id = tight.id;
    small.max_steps = 20;
    save_workflow(&catalog, &small, "v1", None).await?;
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &tight,
        &insert_event(),
        vec!["tight".to_owned()],
    )
    .await?;
    assert_eq!(run.state, RunState::Failed);
    let error = run.error.unwrap_or_default();
    assert!(error.contains("20"), "{error}");
    Ok(())
}

#[tokio::test]
async fn each_iteration_of_a_one_step_loop_body_is_its_own_advance() -> Result<()> {
    // TODO 9.2. An advance services one step *entry*, not one step name: the body
    // of this loop is a single step, so continuing while "the same step" is asked
    // about would run all hundred iterations inside one pass — one write and one
    // trace row for the lot, which would make the durability granularity of a
    // loop the loop rather than the item. A node that died half way through would
    // then come back and start the loop again from the top.
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;

    let trigger = trigger("each_item");
    let mut workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            Step::new(
                "items",
                StepKind::Set {
                    assignments: vec![Assignment::new(
                        "items",
                        "Array(100).fill(0).map((_, i) => i)",
                    )],
                },
            )
            .then(Next::step("each")),
            Step::new(
                "each",
                StepKind::ForEach {
                    over: "context.items".to_owned(),
                    var: "item".to_owned(),
                    body: "one".to_owned(),
                },
            )
            .then(Next::End),
            action_step("one").then(Next::End),
        ],
    )
    .traced();
    workflow.max_steps = 1_000;
    save_workflow(&catalog, &workflow, "v1", None).await?;

    // Started but not driven, so the passes can be counted one at a time.
    let clock = ManualClock::new(Utc::now());
    let state = WorkflowRun::new(&workflow);
    let mut run = sc_workflow::new_run(
        &trigger,
        1,
        &insert_event(),
        vec!["each_item".to_owned()],
        &state,
    );
    save_run(&catalog, &run).await?;

    let driver = Driver::new(&catalog, &dispatcher, &clock);
    let mut done = 0;
    let mut passes = 0;
    loop {
        let advanced = driver.advance(&mut run).await?;
        passes += 1;
        // The assertion: **at most one item per advance**, and an advance is one
        // write. Before this was fixed the first pass to reach the body ran all
        // hundred of them.
        let ran = recording.count("one");
        assert!(
            ran - done <= 1,
            "pass {passes} ran {} iterations in one write",
            ran - done
        );
        done = ran;
        if !advanced.is_runnable() {
            break;
        }
    }
    assert_eq!(require_run(&catalog, run.id).await?.state, RunState::Done);
    assert_eq!(done, 100);

    // A hundred iterations, a hundred writes: one trace row per body step, each
    // committed with the advance that made it (decision 6) and each its own step
    // attempt — so a run recovered mid-loop resumes at the item it was on.
    let traces = list_run_traces(&catalog, run.id.0).await?;
    let body: Vec<&RunTrace> = traces.iter().filter(|t| t.step == "one").collect();
    assert_eq!(
        body.len(),
        100,
        "one row per iteration, not one for the loop"
    );
    assert!(
        body.iter()
            .all(|t| t.attempt == 1 && t.outcome == TraceOutcome::Ok)
    );
    assert!(
        body.windows(2).all(|w| w[0].seq < w[1].seq),
        "the timeline is in the order the items were serviced"
    );
    Ok(())
}

#[tokio::test]
async fn a_workflow_with_no_saved_version_refuses_to_start_a_run() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, _recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;

    let trigger = trigger("unsaved");
    let clock = ManualClock::new(Utc::now());
    let err = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["unsaved".to_owned()],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("unsaved"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_action_step_is_given_its_own_configuration_and_the_run_context() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;
    recording.script(
        "note",
        vec![Scripted::Writes("wrote".to_owned(), json!("by the action"))],
    );

    let trigger = trigger("configured");
    let mut configuration = Attrs::new();
    configuration.insert("note".to_owned(), json!("hello"));
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            Step::new(
                "note",
                StepKind::Action {
                    action: "record".to_owned(),
                    configuration,
                },
            )
            .then(Next::End),
        ],
    );
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["configured".to_owned()],
    )
    .await?;

    assert_eq!(
        recording
            .config("note", 0)
            .and_then(|c| c.get("note").cloned()),
        Some(json!("hello"))
    );
    // What the action wrote through `ActionContext::context` is in the run's
    // context, beside what it returned under the step's name.
    let context = run_state(&run)?.context().clone();
    assert_eq!(context["wrote"], json!("by the action"));
    assert_eq!(context["note"], Json::Null);
    Ok(())
}

#[tokio::test]
async fn an_advance_writes_the_run_and_its_trace_together_or_not_at_all() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;
    recording.script("a", vec![Scripted::Returns(json!(1))]);

    let trigger = trigger("stepwise");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            action_step("a").then(Next::step("b")),
            action_step("b").then(Next::End),
        ],
    )
    .traced();
    save_workflow(&catalog, &workflow, "v1", None).await?;

    // Started, but not driven: the row exists before the first step, so a crash
    // during it leaves a run to recover rather than nothing at all.
    let clock = ManualClock::new(Utc::now());
    let state = WorkflowRun::new(&workflow);
    let mut run = sc_workflow::new_run(
        &trigger,
        1,
        &insert_event(),
        vec!["stepwise".to_owned()],
        &state,
    );
    save_run(&catalog, &run).await?;

    let driver = Driver::new(&catalog, &dispatcher, &clock);

    // One advance is one step and one write, and the two land together. After
    // the first, `a` has run and been written and `b` has not run at all.
    assert_eq!(driver.advance(&mut run).await?, Advanced::Stepped);
    let after_one = load_run(&catalog, run.id).await?.expect("the run's row");
    assert_eq!(after_one.state, RunState::Running);
    assert_eq!(run_state(&after_one)?.context()["a"], json!(1));
    let traces = list_run_traces(&catalog, run.id.0).await?;
    assert_eq!(traces.len(), 1);
    assert_eq!(traces[0].step, "a");
    assert_eq!(recording.count("b"), 0);

    // The second advance services `b` and sees the end in the same pass — the
    // run reached it without needing the world again.
    assert_eq!(driver.advance(&mut run).await?, Advanced::Finished);
    let traces = list_run_traces(&catalog, run.id.0).await?;
    assert_eq!(traces.len(), 2);
    assert_eq!(traces[1].step, "b");
    assert_eq!(require_run(&catalog, run.id).await?.state, RunState::Done);
    Ok(())
}

#[tokio::test]
async fn tracing_off_writes_no_trace_rows_at_all() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (recorder, _recording) = Recorder::new("record");
    let dispatcher = dispatcher(recorder)?;

    let trigger = trigger("untraced");
    let workflow = Workflow::of(trigger.id, 1, vec![action_step("a").then(Next::End)]);
    assert!(!workflow.trace);
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["untraced".to_owned()],
    )
    .await?;
    assert_eq!(run.state, RunState::Done);
    assert!(list_run_traces(&catalog, run.id.0).await?.is_empty());
    Ok(())
}

/// A run that never got a lease is claimable, and one this node holds is not
/// offered to another — the two halves of the claim, without a driver in the way.
#[tokio::test]
async fn claiming_is_exclusive_and_the_run_that_waited_longest_goes_first() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let trigger = trigger("queued");
    let workflow = Workflow::of(trigger.id, 1, vec![action_step("a").then(Next::End)]);
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let now = Utc::now();
    let state = WorkflowRun::new(&workflow);
    let mut ids = Vec::new();
    for minutes in [30i64, 10, 20] {
        let mut run: Run = sc_workflow::new_run(
            &trigger,
            1,
            &insert_event(),
            vec!["queued".to_owned()],
            &state,
        );
        run.wake_at = Some(now - Duration::minutes(minutes));
        save_run(&catalog, &run).await?;
        ids.push((minutes, run.id));
    }

    let queue = DatabaseQueue::new(
        Arc::clone(&catalog),
        "node-a",
        std::time::Duration::from_millis(1),
    );
    let claimed = queue.claim(now, now + Duration::seconds(60), 10).await?;
    let order: Vec<i64> = claimed
        .iter()
        .map(|run| {
            ids.iter()
                .find(|(_, id)| *id == run.id)
                .map(|(m, _)| *m)
                .unwrap_or_default()
        })
        .collect();
    // Longest wait first, so a busy engine cannot starve one run behind newer
    // ones.
    assert_eq!(order, vec![30, 20, 10]);

    // Nothing is claimable a second time while the lease holds.
    assert!(
        queue
            .claim(now, now + Duration::seconds(60), 10)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_workflow_that_starts_a_workflow_carries_the_chain_and_is_bounded_by_it() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    sc_action::bootstrap_triggers(&catalog).await?;
    let (recorder, recording) = Recorder::new("record");

    let mut registry = ActionRegistry::new();
    registry.register(Arc::new(recorder))?;
    registry.register(Arc::new(Firer {
        trigger: "child".to_owned(),
    }))?;
    let registry = Arc::new(registry);
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let dispatcher =
        Arc::new(TriggerDispatcher::new(Arc::clone(&registry)).with_evaluator(evaluator));

    // The other direction of §3.5: not a workflow whose step runs an action, but
    // a workflow whose step starts **another run**. The engine has to be
    // installed for that to be possible at all — dispatch reaches for the
    // `WorkflowEngine` seam and refuses by name when there is none.
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let queue = Arc::new(DatabaseQueue::new(
        Arc::clone(&catalog),
        "node-a",
        std::time::Duration::from_millis(1),
    ));
    let engine = Arc::new(WorkflowEngineTask::with_queue(
        Arc::clone(&catalog),
        Arc::clone(&dispatcher),
        queue,
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));
    dispatcher.set_workflow_engine(Arc::clone(&engine) as Arc<dyn sc_action::WorkflowEngine>);

    // The child: a trigger nothing fires but another trigger, whose body is a
    // workflow of one recording step.
    let child = Trigger::with_body(
        TriggerId::new(),
        "child",
        EventKind::None,
        TriggerBody::Workflow,
    );
    save_workflow(
        &catalog,
        &Workflow::of(child.id, 1, vec![action_step("childstep").then(Next::End)]),
        "v1",
        None,
    )
    .await?;
    sc_action::save_trigger(&catalog, &registry, &child).await?;
    dispatcher.reload(&catalog).await?;

    // The parent: one step, which fires the child.
    let parent = trigger("parent");
    save_workflow(
        &catalog,
        &Workflow::of(
            parent.id,
            1,
            vec![
                Step::new(
                    "onwards",
                    StepKind::Action {
                        action: "fire".to_owned(),
                        configuration: Attrs::new(),
                    },
                )
                .then(Next::End),
            ],
        ),
        "v1",
        None,
    )
    .await?;

    // Shallow: the parent's step starts a child **run**, which walks its own
    // step. Two runs, and the chain the child's step writes shows the whole
    // descent — the parent's chain, the step that fired, the child trigger, and
    // the child's own step.
    let run = start_run(
        &catalog,
        &dispatcher,
        clock.as_ref(),
        &parent,
        &insert_event(),
        vec!["parent".to_owned()],
    )
    .await?;
    assert_eq!(run.state, RunState::Done, "{:?}", run.error);
    assert_eq!(recording.count("childstep"), 1);
    let chain = run_state(&require_run(&catalog, run.id).await?)?.context()["onwards"].clone();
    assert!(chain["run"].is_string(), "the step got the child run's id");
    let child_runs = sc_workflow::list_workflow_runs(&catalog, "child", None, 10, 0).await?;
    assert_eq!(child_runs.len(), 1);
    assert_eq!(child_runs[0].state, RunState::Done);
    assert_eq!(
        run_state(&child_runs[0])?.context()["chain_at_childstep"],
        json!(["parent", "onwards", "child", "childstep"])
    );

    // Deep: `MAX_DEPTH - 1` triggers already, so the step is the last level the
    // bound allows and starting the child would be one past it. The bound
    // refuses it before any run is created — a workflow that starts a workflow
    // is stopped exactly where an action that writes a row is.
    let deep: Vec<String> = (1..sc_action::MAX_DEPTH).map(|n| format!("t{n}")).collect();
    let run = start_run(
        &catalog,
        &dispatcher,
        clock.as_ref(),
        &parent,
        &insert_event(),
        deep,
    )
    .await?;
    assert_eq!(run.state, RunState::Failed);
    let error = run.error.unwrap_or_default();
    assert!(error.contains("child"), "{error}");
    assert!(
        error.contains(&format!("{}", sc_action::MAX_DEPTH)),
        "{error}"
    );
    assert_eq!(recording.count("childstep"), 1, "no second child run");
    assert_eq!(
        sc_workflow::list_workflow_runs(&catalog, "child", None, 10, 0)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn the_cascade_bound_still_holds_when_it_goes_through_a_workflow() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    sc_action::bootstrap_triggers(&catalog).await?;
    let (recorder, recording) = Recorder::new("record");

    // Two actions: one that records, and one that runs another trigger — which
    // is what a step whose write fires a trigger amounts to.
    let mut registry = ActionRegistry::new();
    registry.register(Arc::new(recorder))?;
    registry.register(Arc::new(Firer {
        trigger: "audit".to_owned(),
    }))?;
    let registry = Arc::new(registry);
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let dispatcher =
        Arc::new(TriggerDispatcher::new(Arc::clone(&registry)).with_evaluator(evaluator));

    // The trigger a step reaches for, in the live set.
    let audit = Trigger::new("audit", EventKind::None, "record");
    sc_action::save_trigger(&catalog, &registry, &audit).await?;
    dispatcher.reload(&catalog).await?;

    let trigger = trigger("cascading");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            Step::new(
                "onwards",
                StepKind::Action {
                    action: "fire".to_owned(),
                    configuration: Attrs::new(),
                },
            )
            .then(Next::End),
        ],
    );
    save_workflow(&catalog, &workflow, "v1", None).await?;
    let clock = ManualClock::new(Utc::now());

    // Shallow: the step's chain is two deep, so the trigger it reaches for runs.
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["cascading".to_owned()],
    )
    .await?;
    assert_eq!(run.state, RunState::Done, "{:?}", run.error);
    assert_eq!(recording.count("audit"), 1);

    // Deep: a run that already descends from `MAX_DEPTH - 1` triggers makes its
    // step the last level allowed, and `MAX_DEPTH` refuses the trigger after
    // it — naming the whole chain, which is the diagnosis rather than a number.
    let deep: Vec<String> = (1..sc_action::MAX_DEPTH).map(|n| format!("t{n}")).collect();
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        deep,
    )
    .await?;
    assert_eq!(run.state, RunState::Failed);
    let error = run.error.unwrap_or_default();
    assert!(error.contains("onwards"), "{error}");
    assert!(error.contains("audit"), "{error}");
    assert!(
        error.contains(&format!("{}", sc_action::MAX_DEPTH)),
        "{error}"
    );
    // And nothing ran: the bound is checked before the trigger fires.
    assert_eq!(recording.count("audit"), 1);
    Ok(())
}

//! The queue **seam**, exercised by something that is not the database (§10.3,
//! decision 5; TODO 7.2).
//!
//! Decision 5's claim is a strong one: *nothing above the seam knows which queue
//! it is talking to*. `tests/driver.rs` pins the shipping implementation — the
//! runs table, polled, with its compare-and-set claim — and pins it against real
//! Postgres, which is the only place that claim can be true. What it cannot show
//! is the seam itself: a test whose only queue is `DatabaseQueue` would still
//! pass if [`WorkQueue`] were a fiction and the engine reached for the table
//! directly.
//!
//! So this file drives [`WorkflowEngineTask`] over an **in-memory queue** that
//! shares no code and no storage with the database one: a `Vec` of runs, a lease
//! kept in a `Mutex`, and a `wake` that is a `Notify` rather than a sleep — which
//! is the shape `sc-bus` will have. Everything above it is the real engine: the
//! real driver, the real machine, the real isolate, and the database still doing
//! the one job the queue never had, which is holding what the run *is*.
//!
//! What that buys, in the three tests here:
//!
//! - the engine drives a run claimed from a queue that is not the database, and
//!   the advance lands in the database all the same;
//! - the task loop advances a run **as soon as the queue says there may be one**,
//!   with no poll interval in sight — the whole point of the seam;
//! - a step that outlives half its lease has that lease **renewed through the
//!   seam**, so a slow workflow is not picked up by somebody else half way
//!   through.
//!
//! The clock is a parameter here as everywhere else in this crate: a step that
//! takes forty seconds is written as an action that *moves the clock*, so the
//! renewal rule is tested at full speed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sc_action::{
    Action, ActionContext, ActionRegistry, Event, EventKind, Trigger, TriggerBody,
    TriggerDispatcher, TriggerId,
};
use sc_agent::{Run, RunId, RunState, bootstrap_runs, require_run};
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use sc_types::clock::{Clock, ManualClock};
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use sc_workflow::queue::WorkQueue;
use sc_workflow::{
    Next, Step, StepKind, Workflow, WorkflowEngineTask, bootstrap_run_traces,
    bootstrap_workflow_versions, save_workflow, start_run,
};
use serde_json::{Value as Json, json};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// The queue that is not a database.
// ---------------------------------------------------------------------------

/// A [`WorkQueue`] over a list in memory: what a bus-backed queue will look like
/// from above.
///
/// It keeps the two things the trait is about — who holds a run, and until when
/// — and nothing else. `wake` is a `Notify`, so a test can say "there is work
/// now" instead of waiting for a poll to come round, and so the engine's loop is
/// observed reacting to the queue rather than to the clock.
struct MemoryQueue {
    node: String,
    /// Runs handed to the queue, in the order they were enqueued.
    queued: Mutex<Vec<Run>>,
    /// What this node holds, and until when: the lease, kept where the database
    /// one keeps it in a column.
    leases: Mutex<Vec<(RunId, DateTime<Utc>)>>,
    /// Every renewal asked for, so the test can see the rule fire.
    renewals: Mutex<Vec<(RunId, DateTime<Utc>)>>,
    /// How many times the engine asked for work at all — including the times the
    /// answer was "none", which is how "the loop is blocked on `wake`" is
    /// asserted rather than assumed.
    claims: AtomicUsize,
    notify: Notify,
}

impl MemoryQueue {
    fn new(node: &str) -> MemoryQueue {
        MemoryQueue {
            node: node.to_owned(),
            queued: Mutex::new(Vec::new()),
            leases: Mutex::new(Vec::new()),
            renewals: Mutex::new(Vec::new()),
            claims: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    /// Put a run in the queue. A bus would do this from a `NOTIFY`; a test does
    /// it by hand.
    fn enqueue(&self, run: &Run) {
        if let Ok(mut queued) = self.queued.lock() {
            queued.push(run.clone());
        }
    }

    /// Tell the loop there may be work — `notify_one`, so a wake that arrives
    /// before the loop is waiting is kept rather than lost.
    fn ring(&self) {
        self.notify.notify_one();
    }

    /// How many times the engine has asked for work.
    fn asked(&self) -> usize {
        self.claims.load(Ordering::Relaxed)
    }

    /// The renewals asked for, in order.
    fn renewals(&self) -> Vec<(RunId, DateTime<Utc>)> {
        self.renewals.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// The lease this queue believes it holds on `id`.
    fn lease(&self, id: RunId) -> Option<DateTime<Utc>> {
        let leases = self.leases.lock().ok()?;
        leases.iter().find(|(run, _)| *run == id).map(|(_, at)| *at)
    }
}

#[async_trait]
impl WorkQueue for MemoryQueue {
    async fn claim(
        &self,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Run>> {
        self.claims.fetch_add(1, Ordering::Relaxed);
        let mut claimed = Vec::new();
        let mut queued = self
            .queued
            .lock()
            .map_err(|_| Error::msg("memory queue poisoned"))?;
        let mut kept = Vec::new();
        for mut run in queued.drain(..) {
            // The same predicate the database one is a query for: due, and
            // nothing else working on it.
            let due = run.wake_at.is_some_and(|at| at <= now);
            let free = run.lease_until.is_none_or(|at| at < now);
            if claimed.len() < limit && due && free {
                run.lease_until = Some(until);
                run.claimed_by = Some(self.node.clone());
                if let Ok(mut leases) = self.leases.lock() {
                    leases.retain(|(id, _)| *id != run.id);
                    leases.push((run.id, until));
                }
                claimed.push(run);
            } else {
                kept.push(run);
            }
        }
        *queued = kept;
        Ok(claimed)
    }

    async fn renew(&self, run: &Run, until: DateTime<Utc>) -> Result<()> {
        if let Ok(mut renewals) = self.renewals.lock() {
            renewals.push((run.id, until));
        }
        if let Ok(mut leases) = self.leases.lock() {
            leases.retain(|(id, _)| *id != run.id);
            leases.push((run.id, until));
        }
        Ok(())
    }

    async fn wake(&self) {
        self.notify.notified().await;
    }

    fn node(&self) -> &str {
        &self.node
    }
}

// ---------------------------------------------------------------------------
// The actions a step runs.
// ---------------------------------------------------------------------------

/// An action that records the step it was called as, and returns null.
struct Counter {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Action for Counter {
    fn name(&self) -> &str {
        "record"
    }
    fn description(&self) -> &str {
        "Record the step it was called as"
    }
    fn config_spec(&self) -> Vec<FormField> {
        vec![FormField::new("note", BasicType::Text)]
    }
    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(ctx.trigger.to_owned());
        }
        Ok(Json::Null)
    }
}

/// An action that takes `by` on the clock to run — a slow step, written so that
/// no test has to be slow.
struct Slow {
    clock: Arc<ManualClock>,
    by: Duration,
}

#[async_trait]
impl Action for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "Take a while: move the clock on by a fixed amount"
    }
    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }
    async fn run(&self, _ctx: &mut ActionContext<'_>) -> Result<Json> {
        self.clock.advance(self.by);
        Ok(Json::Null)
    }
}

// ---------------------------------------------------------------------------
// The fixture.
// ---------------------------------------------------------------------------

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

/// A dispatcher over the actions given, with a real isolate behind the formulas.
fn dispatcher(actions: Vec<Arc<dyn Action>>) -> Result<Arc<TriggerDispatcher>> {
    let mut registry = ActionRegistry::new();
    for action in actions {
        registry.register(action)?;
    }
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    Ok(Arc::new(
        TriggerDispatcher::new(Arc::new(registry)).with_evaluator(evaluator),
    ))
}

fn trigger(name: &str) -> Trigger {
    Trigger::with_body(
        TriggerId::new(),
        name,
        EventKind::Insert,
        TriggerBody::Workflow,
    )
    .on("orders")
}

fn insert_event() -> Event {
    Event::new(EventKind::Insert)
        .on("orders")
        .row(json!({ "id": 1, "total": 120 }))
}

/// One step running `action`.
fn step(name: &str, action: &str) -> Step {
    Step::new(
        name,
        StepKind::Action {
            action: action.to_owned(),
            configuration: Attrs::new(),
        },
    )
}

/// A `Wait` of an hour, so a started run leaves the engine's reach at once and
/// the queue is the only thing that can bring it back.
fn hold(next: &str) -> Step {
    Step::new(
        "hold",
        StepKind::Wait {
            until: "1000 * 60 * 60".to_owned(),
        },
    )
    .then(Next::step(next))
}

/// Wait for `check` to hold, giving up after a second. Not a timer standing in
/// for a workflow's — the run is being advanced on **another task**, and this is
/// how a test joins it.
async fn until<F>(check: F) -> bool
where
    F: Fn() -> bool,
{
    for _ in 0..200 {
        if check() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    check()
}

/// The same wait, over the one thing only the database can answer: has the run
/// the loop is driving reached the end?
async fn finished(catalog: &Arc<Catalog>, id: RunId) -> bool {
    for _ in 0..200 {
        if require_run(catalog, id)
            .await
            .is_ok_and(|run| run.state == RunState::Done)
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    false
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_engine_drives_a_run_claimed_from_a_queue_that_is_not_the_database() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = dispatcher(vec![Arc::new(Counter {
        calls: Arc::clone(&calls),
    })])?;

    let trigger = trigger("seamed");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![hold("after"), step("after", "record").then(Next::End)],
    );
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let started = Utc::now();
    let clock = Arc::new(ManualClock::new(started));
    let queue = Arc::new(MemoryQueue::new("in-memory"));
    let engine = Arc::new(WorkflowEngineTask::with_queue(
        Arc::clone(&catalog),
        Arc::clone(&dispatcher),
        Arc::clone(&queue) as Arc<dyn WorkQueue>,
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));

    let run = start_run(
        &catalog,
        &dispatcher,
        clock.as_ref(),
        &trigger,
        &insert_event(),
        vec!["seamed".to_owned()],
    )
    .await?;
    assert_eq!(run.state, RunState::Waiting);
    assert_eq!(engine.node(), "in-memory", "the seam names the node, too");

    // An empty queue is not an error, and answers nothing.
    assert!(engine.tick().await.is_empty());
    assert_eq!(queue.asked(), 1);

    // Nothing is due until the hour is up, whatever the queue holds: the run
    // stays in the queue and the tick answers nothing.
    assert_eq!(run.wake_at, Some(started + Duration::hours(1)));
    queue.enqueue(&run);
    assert!(engine.tick().await.is_empty());

    // An hour later it is due, and the engine that has never seen the runs table
    // as a queue drives it to the end all the same.
    clock.advance(Duration::hours(1) + Duration::seconds(1));
    assert_eq!(engine.tick().await, vec![run.id]);

    assert_eq!(
        calls.lock().map(|c| c.clone()).unwrap_or_default(),
        vec!["after".to_owned()]
    );
    // And the *durable* half is untouched by the swap: the advance was written to
    // the database, which is where a run lives whatever hands it to the engine.
    let stored = require_run(&catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Done);
    assert_eq!(stored.lease_until, None, "a finished run holds no lease");
    assert_eq!(stored.claimed_by, None);
    Ok(())
}

#[tokio::test]
async fn the_loop_advances_a_run_the_moment_the_queue_says_there_is_one() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = dispatcher(vec![Arc::new(Counter {
        calls: Arc::clone(&calls),
    })])?;

    let trigger = trigger("woken");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![hold("after"), step("after", "record").then(Next::End)],
    );
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let started = Utc::now();
    let clock = Arc::new(ManualClock::new(started));
    let queue = Arc::new(MemoryQueue::new("in-memory"));
    let engine = Arc::new(WorkflowEngineTask::with_queue(
        Arc::clone(&catalog),
        Arc::clone(&dispatcher),
        Arc::clone(&queue) as Arc<dyn WorkQueue>,
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));

    let run = start_run(
        &catalog,
        &dispatcher,
        clock.as_ref(),
        &trigger,
        &insert_event(),
        vec!["woken".to_owned()],
    )
    .await?;

    let loop_handle = engine.start();
    // The loop's first act is to wait on the queue, so until the queue rings
    // nothing has been asked for at all. A polling queue would have had to sleep
    // here; this one does not, and the engine cannot tell the difference.
    tokio::task::yield_now().await;
    assert_eq!(queue.asked(), 0, "the loop is waiting on the seam");

    clock.advance(Duration::hours(1) + Duration::seconds(1));
    queue.enqueue(&run);
    queue.ring();

    assert!(
        finished(&catalog, run.id).await,
        "the loop woke and drove the run"
    );
    assert_eq!(
        calls.lock().map(|c| c.clone()).unwrap_or_default(),
        vec!["after".to_owned()]
    );

    // And it stops when asked: the wake it is blocked on is released, the loop
    // sees the flag, and the task ends rather than being aborted under a step.
    engine.stop();
    queue.ring();
    assert!(until(|| loop_handle.is_finished()).await, "the loop ended");
    Ok(())
}

#[tokio::test]
async fn a_step_that_outlives_half_its_lease_has_it_renewed_through_the_seam() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let started = Utc::now();
    let clock = Arc::new(ManualClock::new(started));
    let calls = Arc::new(Mutex::new(Vec::new()));
    // Three steps: the first takes forty seconds of a sixty-second lease, so the
    // renewal rule fires between it and the second.
    let dispatcher = dispatcher(vec![
        Arc::new(Counter {
            calls: Arc::clone(&calls),
        }),
        Arc::new(Slow {
            clock: Arc::clone(&clock),
            by: Duration::seconds(40),
        }),
    ])?;

    let trigger = trigger("slow");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            hold("long"),
            step("long", "slow").then(Next::step("after")),
            step("after", "record").then(Next::End),
        ],
    );
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let queue = Arc::new(MemoryQueue::new("in-memory"));
    let engine = Arc::new(
        WorkflowEngineTask::with_queue(
            Arc::clone(&catalog),
            Arc::clone(&dispatcher),
            Arc::clone(&queue) as Arc<dyn WorkQueue>,
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .with_lease(Duration::seconds(60)),
    );

    let run = start_run(
        &catalog,
        &dispatcher,
        clock.as_ref(),
        &trigger,
        &insert_event(),
        vec!["slow".to_owned()],
    )
    .await?;

    clock.advance(Duration::hours(1) + Duration::seconds(1));
    let woken = clock.now();
    queue.enqueue(&run);
    assert_eq!(engine.tick().await, vec![run.id]);

    // The claim leased it for a minute; the slow step spent forty seconds of
    // that, which is past the half-way mark, so the engine asked the queue —
    // whatever the queue is — to extend it before running the next step.
    let renewals = queue.renewals();
    assert_eq!(renewals.len(), 1, "{renewals:?}");
    assert_eq!(renewals[0].0, run.id);
    assert_eq!(renewals[0].1, woken + Duration::seconds(40 + 60));
    assert_eq!(queue.lease(run.id), Some(renewals[0].1));

    assert_eq!(
        calls.lock().map(|c| c.clone()).unwrap_or_default(),
        vec!["after".to_owned()],
        "and the run carried on through the renewal"
    );
    assert_eq!(require_run(&catalog, run.id).await?.state, RunState::Done);
    Ok(())
}

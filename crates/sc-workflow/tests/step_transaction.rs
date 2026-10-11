//! **Each step runs in one transaction** (§10.3, decision 6), against a real
//! Postgres.
//!
//! The driver's other tests pin what the machine decides and that the advance is
//! written. What is under test here is the promise those cannot see: that the
//! rows a step writes and the fact that the run took it are **one commit** — so a
//! step that fails leaves nothing behind, a step that succeeds leaves everything,
//! and a trigger that cascades off a step's write lands wherever the step lands.
//!
//! Everything is real except the clock: the database, the isolate, the row layer
//! (a step's action writes through `rows`, exactly as `insert_row` does) and the
//! dispatcher, which is installed as the catalog's table-events observer so a
//! write inside a step really does fire the trigger listening for it.
//!
//! And the live hub, for the one thing that must **not** share the step's fate
//! by joining it: telling a page about the step's write. That waits for the
//! commit, and a rollback means it never happens (TODO.md "Live updates" L3.1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::Utc;
use sc_action::{
    Action, ActionContext, ActionRegistry, Event, EventKind, Trigger, TriggerBody,
    TriggerDispatcher, TriggerId,
};
use sc_agent::{RunState, bootstrap_runs, require_run};
use sc_api::rows;
use sc_catalog::{CallerContext, Catalog, DataField, TableEvents};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use sc_workflow::{
    ErrorPolicy, ManualClock, Next, Step, StepKind, Workflow, bootstrap_run_traces,
    bootstrap_workflow_versions, save_workflow, start_run,
};
use serde_json::{Value as Json, json};

// ---------------------------------------------------------------------------
// The actions: real row writes, through the row layer, on the step's executor.
// ---------------------------------------------------------------------------

/// The `table_changes` stream on `orders`, once a test has started one — what
/// [`WriteOrder`] looks at to say how many changes the stream had been handed
/// while the step was still running.
type Watched = Arc<OnceLock<Arc<sc_stream::RunningStream>>>;

/// Insert one order and read it back — then, if the step says so, fail.
///
/// The write goes through [`rows::create_row_in`] with the executor the step's
/// transaction supplies, which is the one line every row action has (§10.3):
/// nothing here knows whether there *is* a transaction, only that
/// `ActionContext::transaction` answers for it.
struct WriteOrder {
    watched: Watched,
}

#[async_trait::async_trait]
impl Action for WriteOrder {
    fn name(&self) -> &str {
        "write_order"
    }
    fn description(&self) -> &str {
        "Insert an order through the row layer, optionally failing afterwards"
    }
    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("id", BasicType::Int),
            FormField::new("fail", BasicType::Bool),
        ]
    }
    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let id = ctx.setting("id").and_then(Json::as_i64).unwrap_or(1);
        let orders = ctx.catalog.require("orders")?;
        let executor = rows::Executor::of(ctx.transaction());
        let caller = CallerContext::new(sc_auth::ROLE_ADMIN, None).chained(ctx.chain.clone());
        rows::create_row_in(
            ctx.catalog,
            &orders,
            &json!({ "id": id, "total": 100 }),
            Some(&caller),
            &executor,
        )
        .await?;
        // Read back **on the same executor**: inside a step this sees what the
        // step has written and has not committed, which is what makes a step a
        // unit of work rather than a sequence of independent statements.
        let seen = rows::select_values_in(ctx.catalog, &orders, None, Some(&caller), &executor)
            .await?
            .len();
        if ctx.setting("fail").and_then(Json::as_bool).unwrap_or(false) {
            return Err(Error::invalid("the step failed after writing"));
        }
        // How many changes the live stream on `orders` had been handed by now —
        // after this step's write, before its commit.
        let published = self
            .watched
            .get()
            .map(|running| running.counters().elements);
        Ok(json!({ "wrote": id, "visible": seen, "published_during_step": published }))
    }
}

/// The cascade: a trigger listening for an order writes an audit row.
struct WriteAudit;

#[async_trait::async_trait]
impl Action for WriteAudit {
    fn name(&self) -> &str {
        "write_audit"
    }
    fn description(&self) -> &str {
        "Insert an audit row for the order that was written"
    }
    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }
    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let id = ctx
            .event
            .row
            .as_ref()
            .and_then(|row| row.get("id"))
            .and_then(Json::as_i64)
            .ok_or_else(|| Error::invalid("the event carries no order"))?;
        let audit = ctx.catalog.require("audit")?;
        let caller = CallerContext::new(sc_auth::ROLE_ADMIN, None).chained(ctx.chain.clone());
        rows::create_row_in(
            ctx.catalog,
            &audit,
            &json!({ "id": id, "note": "seen" }),
            Some(&caller),
            &rows::Executor::of(ctx.transaction()),
        )
        .await?;
        Ok(Json::Null)
    }
}

// ---------------------------------------------------------------------------
// The fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    catalog: Arc<Catalog>,
    dispatcher: Arc<TriggerDispatcher>,
    trigger: Trigger,
    watched: Watched,
    _db: TestDb,
}

/// A catalog with `orders`, `audit`, the workflow tables and the runs table, and
/// a dispatcher that can write both — installed as the table-events observer, so
/// a step's write really fires what listens for it.
async fn fixture() -> Result<Fixture> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    for name in ["orders", "audit"] {
        catalog
            .create_table(
                name,
                &[
                    DataField::plain("id", TypeRef::Basic(BasicType::Int))
                        .required()
                        .primary_key(),
                    match name {
                        "orders" => DataField::plain("total", TypeRef::Basic(BasicType::Int)),
                        _ => DataField::plain("note", TypeRef::Basic(BasicType::Text)),
                    },
                ],
            )
            .await?;
    }
    sc_action::bootstrap_triggers(&catalog).await?;
    bootstrap_workflow_versions(&catalog).await?;
    bootstrap_run_traces(&catalog).await?;
    bootstrap_runs(&catalog).await?;

    let watched = Watched::default();
    let mut registry = ActionRegistry::new();
    registry.register(Arc::new(WriteOrder {
        watched: Arc::clone(&watched),
    }))?;
    registry.register(Arc::new(WriteAudit))?;
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let dispatcher =
        Arc::new(TriggerDispatcher::new(Arc::new(registry)).with_evaluator(Arc::clone(&evaluator)));
    catalog.add_table_events(
        sc_catalog::TRIGGER_EVENTS,
        Arc::clone(&dispatcher) as Arc<dyn TableEvents>,
    )?;

    let trigger = Trigger::with_body(
        TriggerId::new(),
        "billing",
        EventKind::Insert,
        TriggerBody::Workflow,
    )
    .on("orders");
    Ok(Fixture {
        catalog,
        dispatcher,
        trigger,
        watched,
        _db: db,
    })
}

/// The workflow: one step that writes an order, with `fail` as configured.
fn one_step(f: &Fixture, fail: bool) -> Workflow {
    let mut configuration = Attrs::new();
    configuration.insert("id".into(), json!(7));
    configuration.insert("fail".into(), json!(fail));
    Workflow::of(
        f.trigger.id,
        1,
        vec![
            Step::new(
                "write",
                StepKind::Action {
                    action: "write_order".to_owned(),
                    configuration,
                },
            )
            // No retry: the question here is what the failure leaves behind, and
            // three attempts would only ask it three times.
            .on_error(ErrorPolicy::Fail)
            .then(Next::End),
        ],
    )
    .traced()
}

/// The event that starts a run — a `none`-payload insert the workflow's own
/// steps do not read.
fn start_event() -> Event {
    Event::new(EventKind::Insert)
        .on("orders")
        .row(json!({ "id": 1, "total": 10 }))
}

/// The ids in a table, as the outside world sees them: a pooled read on another
/// connection, which is the whole point — it sees only what committed.
async fn ids(catalog: &Catalog, table: &str) -> Result<Vec<i64>> {
    let table = catalog.require(table)?;
    let rows = rows::list_rows(catalog, &table).await?;
    Ok(rows
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("id").and_then(Json::as_i64))
                .collect()
        })
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

/// A step that succeeds commits its rows **with** its advance: the order is
/// there, and the run says it took the step.
#[tokio::test]
async fn a_step_that_succeeds_commits_its_rows_with_its_advance() -> Result<()> {
    let f = fixture().await?;
    save_workflow(&f.catalog, &one_step(&f, false), "v1", None).await?;
    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &f.catalog,
        &f.dispatcher,
        &clock,
        &f.trigger,
        &start_event(),
        vec!["billing".to_owned()],
    )
    .await?;

    let stored = require_run(&f.catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Done, "{:?}", stored.error);
    assert_eq!(ids(&f.catalog, "orders").await?, vec![7]);
    // And the step saw its own write while it was still uncommitted: one row
    // visible to the statement that read it back, on the step's transaction.
    let state = sc_workflow::run_state(&stored)?;
    assert_eq!(state.context()["write"]["visible"], json!(1));
    Ok(())
}

/// A step that fails leaves **nothing**: the row it wrote before failing is
/// rolled back — and the failure itself is recorded anyway, because the record
/// of a failure must not roll back with the failure.
#[tokio::test]
async fn a_step_that_fails_rolls_back_what_it_wrote_and_still_records_the_failure() -> Result<()> {
    let f = fixture().await?;
    save_workflow(&f.catalog, &one_step(&f, true), "v1", None).await?;
    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &f.catalog,
        &f.dispatcher,
        &clock,
        &f.trigger,
        &start_event(),
        vec!["billing".to_owned()],
    )
    .await?;

    let stored = require_run(&f.catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Failed);
    let error = stored.error.clone().unwrap_or_default();
    assert!(error.contains("the step failed after writing"), "{error}");
    // The write is gone.
    assert!(
        ids(&f.catalog, "orders").await?.is_empty(),
        "the failed step's row survived its rollback"
    );
    // And the trace of the attempt is not: it is written in the transaction the
    // failure is recorded in, not in the one that was rolled back.
    let traces = sc_workflow::list_run_traces(&f.catalog, stored.id.0).await?;
    assert_eq!(traces.len(), 1, "{traces:?}");
    assert_eq!(traces[0].step, "write");
    Ok(())
}

/// A trigger that cascades off a step's write is **part of the step**: its own
/// write commits with the step, and is rolled back with it.
#[tokio::test]
async fn a_cascade_off_a_step_lands_wherever_the_step_lands() -> Result<()> {
    for fail in [false, true] {
        let f = fixture().await?;
        // The trigger that listens for what the step writes.
        let audit = Trigger::with_body(
            TriggerId::new(),
            "audit",
            EventKind::Insert,
            TriggerBody::Action {
                action: "write_audit".to_owned(),
                configuration: Attrs::new(),
            },
        )
        .on("orders");
        sc_action::save_trigger(&f.catalog, &f.dispatcher.registry(), &audit).await?;
        f.dispatcher.reload(&f.catalog).await?;

        save_workflow(&f.catalog, &one_step(&f, fail), "v1", None).await?;
        let clock = ManualClock::new(Utc::now());
        let run = start_run(
            &f.catalog,
            &f.dispatcher,
            &clock,
            &f.trigger,
            &start_event(),
            vec!["billing".to_owned()],
        )
        .await?;

        let stored = require_run(&f.catalog, run.id).await?;
        let orders = ids(&f.catalog, "orders").await?;
        let audited = ids(&f.catalog, "audit").await?;
        match fail {
            false => {
                assert_eq!(stored.state, RunState::Done, "{:?}", stored.error);
                assert_eq!(orders, vec![7]);
                assert_eq!(audited, vec![7], "the cascade committed with the step");
            }
            true => {
                assert_eq!(stored.state, RunState::Failed);
                assert!(orders.is_empty(), "the step's own write survived");
                assert!(
                    audited.is_empty(),
                    "the cascade's write survived a step that was rolled back"
                );
            }
        }
    }
    Ok(())
}

/// The live hub on the write seam beside the dispatcher, a running
/// `table_changes` stream on `orders`, and one subscription to it — the
/// subscription is what makes the hub observe the step's write at all.
async fn live(
    f: &Fixture,
) -> Result<(
    Arc<sc_stream::StreamSupervisor>,
    sc_live::Connection,
    sc_live::Attached,
    tokio::sync::mpsc::Receiver<sc_live::Delivery>,
)> {
    let mut registry = sc_stream::StreamRegistry::new();
    registry.register(Arc::new(sc_stream::TableChanges) as Arc<dyn sc_stream::StreamProvider>)?;
    let supervisor = Arc::new(sc_stream::StreamSupervisor::new(
        Arc::new(registry),
        sc_stream::StreamConfig::default(),
    ));
    let mut stream = sc_stream::Stream::new("orders_live", "table_changes");
    stream.configuration = json!({ "table": "orders" }).as_object().cloned().unwrap();
    let running = supervisor.start(stream).await?;
    let _ = f.watched.set(Arc::clone(&running));

    let hub = sc_live::LiveHub::new(sc_live::LiveLimits::default());
    hub.set_streams(Arc::clone(&supervisor));
    f.catalog.add_table_events(
        sc_catalog::LIVE_EVENTS,
        Arc::clone(&hub) as Arc<dyn TableEvents>,
    )?;
    let (connection, queue) = hub
        .connect(sc_live::ConnectionInfo {
            app: "shop".to_owned(),
            user: None,
            session: None,
        })
        .expect("connects");
    // An admin, with no filter: sent every committed change.
    connection.set_user(Some(sc_auth::User::new(
        uuid::Uuid::from_u128(0xad),
        sc_auth::ROLE_ADMIN,
    )?));
    let access = sc_live::ChangeAccess {
        catalog: Arc::clone(&f.catalog),
        evaluator: None,
        filter: sc_live::RowFilter::all(),
    };
    let (attached, _) = connection.attach_changes(&running, access).await;
    Ok((supervisor, connection, attached, queue))
}

/// A step's write is announced **once, after the commit**: while the step
/// ran, the stream had been handed nothing; when the run is done, it has been
/// handed exactly the one insert. A step that fails after the same write
/// announces nothing at all — the page is never shown a row that was rolled
/// back.
#[tokio::test]
async fn a_steps_write_reaches_a_live_stream_after_the_commit_and_never_after_a_rollback()
-> Result<()> {
    for fail in [false, true] {
        let f = fixture().await?;
        let (_supervisor, _connection, _attached, mut queue) = live(&f).await?;
        assert!(
            f.catalog
                .observes_writes("orders", sc_catalog::WriteOp::Insert)
        );

        save_workflow(&f.catalog, &one_step(&f, fail), "v1", None).await?;
        let clock = ManualClock::new(Utc::now());
        let run = start_run(
            &f.catalog,
            &f.dispatcher,
            &clock,
            &f.trigger,
            &start_event(),
            vec!["billing".to_owned()],
        )
        .await?;
        let stored = require_run(&f.catalog, run.id).await?;
        let running = f.watched.get().unwrap();

        if fail {
            assert_eq!(stored.state, RunState::Failed);
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                running.counters().elements,
                0,
                "the rollback announced nothing"
            );
            assert!(queue.try_recv().is_err(), "and no page heard anything");
            continue;
        }

        assert_eq!(stored.state, RunState::Done, "{:?}", stored.error);
        let state = sc_workflow::run_state(&stored)?;
        assert_eq!(
            state.context()["write"]["published_during_step"],
            json!(0),
            "nothing was announced before the commit"
        );
        assert_eq!(running.counters().elements, 1, "once, after it");
        let delivery = tokio::time::timeout(Duration::from_secs(10), queue.recv())
            .await
            .expect("delivered")
            .expect("the queue is open");
        assert_eq!(delivery.envelope.value["op"], "insert");
        assert_eq!(delivery.envelope.value["row"]["id"], json!(7));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(queue.try_recv().is_err(), "exactly once");
    }
    Ok(())
}

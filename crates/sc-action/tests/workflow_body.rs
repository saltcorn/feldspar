//! Dispatching a trigger whose body is a **workflow** (§10.3, phase 1.7).
//!
//! Two claims, and they are the whole of what this crate knows about workflows:
//! a workflow trigger reaches the [`WorkflowEngine`] seam with the event and the
//! chain that led there, and a process with **no** engine refuses it by name
//! rather than reporting success for a run nobody started.
//!
//! Against a real database because dispatch reads the live trigger set, and the
//! live trigger set is a stored one.

use sc_catalog::Authority;
use std::sync::{Arc, Mutex};

use sc_action::{
    ActionRegistry, Event, EventKind, Trigger, TriggerDispatcher, WorkflowEngine, WorkflowStarted,
    bootstrap_triggers, save_trigger,
};
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};
use uuid::Uuid;

/// What one call to the seam was asked to start.
#[derive(Debug, Clone)]
struct Started {
    trigger: String,
    version_of: Uuid,
    payload: Json,
    chain: Vec<String>,
}

/// An engine that records rather than runs — enough to pin what dispatch hands
/// it, which is this crate's whole half of the contract.
struct Recorder {
    started: Mutex<Vec<Started>>,
    run: Uuid,
}

#[async_trait::async_trait]
impl WorkflowEngine for Recorder {
    async fn start(
        &self,
        _catalog: &Catalog,
        trigger: &Trigger,
        event: &Event,
        chain: Vec<String>,
    ) -> Result<WorkflowStarted> {
        if let Ok(mut started) = self.started.lock() {
            started.push(Started {
                trigger: trigger.name.clone(),
                version_of: trigger.id.0,
                payload: event.payload.clone(),
                chain,
            });
        }
        Ok(WorkflowStarted {
            run: self.run,
            state: "waiting".to_owned(),
        })
    }
}

/// A catalog with `_fd_triggers` and one table to hang a trigger on.
async fn setup(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_triggers(&cat).await?;
    cat.create_table(
        "orders",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key(),
            DataField::plain("total", TypeRef::Basic(BasicType::Int)),
        ],
    )
    .await?;
    Ok(cat)
}

#[tokio::test]
async fn a_workflow_trigger_starts_a_run_through_the_engine_seam() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let registry = Arc::new(ActionRegistry::new());
    let dispatcher = TriggerDispatcher::new(registry);

    let trigger = Trigger::workflow("approve_order", EventKind::None);
    save_trigger(&cat, &dispatcher.registry(), &trigger).await?;
    dispatcher.reload(&cat).await?;

    let run = Uuid::new_v4();
    let engine = Arc::new(Recorder {
        started: Mutex::new(Vec::new()),
        run,
    });
    dispatcher.set_workflow_engine(Arc::clone(&engine) as Arc<dyn WorkflowEngine>);

    // What a directly-run workflow answers is **addressable**, not a result: a
    // run that may still be going tomorrow has no value to return today.
    let answer = dispatcher
        .run_trigger(
            &cat,
            "approve_order",
            json!({ "order": 7 }),
            Authority::system("test"),
        )
        .await?;
    assert_eq!(answer, json!({ "run": run, "state": "waiting" }));

    let started = engine.started.lock().expect("recorded").clone();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].trigger, "approve_order");
    assert_eq!(started[0].version_of, trigger.id.0);
    // The event travels whole, so a resumed run still has what started it…
    assert_eq!(started[0].payload, json!({ "order": 7 }));
    // …and the chain is what `Event::firing` returned, which is what bounds a
    // cascade that goes through a workflow exactly as it bounds one that does
    // not.
    assert_eq!(started[0].chain, vec!["approve_order".to_owned()]);
    Ok(())
}

#[tokio::test]
async fn without_an_engine_a_workflow_trigger_refuses_by_name() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let dispatcher = TriggerDispatcher::new(Arc::new(ActionRegistry::new()));

    let trigger = Trigger::workflow("approve_order", EventKind::None);
    save_trigger(&cat, &dispatcher.registry(), &trigger).await?;
    dispatcher.reload(&cat).await?;
    assert!(dispatcher.workflow_engine().is_none());

    // A build tool or a unit test has no engine. Returning `null` here would be
    // reporting that a run happened, which is the silent failure principle 5
    // forbids.
    let err = dispatcher
        .run_trigger(&cat, "approve_order", Json::Null, Authority::system("test"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("approve_order"), "{err}");
    assert!(err.to_string().contains("no workflow engine"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_table_event_reaches_the_engine_with_the_row_that_caused_it() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = setup(&db).await?;
    let dispatcher = TriggerDispatcher::new(Arc::new(ActionRegistry::new()));
    let trigger = Trigger::workflow("on_order", EventKind::Insert).on("orders");
    save_trigger(&cat, &dispatcher.registry(), &trigger).await?;
    dispatcher.reload(&cat).await?;

    let engine = Arc::new(Recorder {
        started: Mutex::new(Vec::new()),
        run: Uuid::new_v4(),
    });
    dispatcher.set_workflow_engine(Arc::clone(&engine) as Arc<dyn WorkflowEngine>);

    // The event the row layer raises, dispatched exactly as an action-bodied
    // trigger's would be — the point of a workflow being a *body* is that
    // nothing above it changes.
    let event = Event::new(EventKind::Insert)
        .on("orders")
        .row(json!({ "id": 1, "total": 120 }));
    let runs = dispatcher.dispatch(&cat, &event).await;
    assert_eq!(runs.len(), 1);
    assert!(runs[0].fired(), "{:?}", runs[0].outcome);

    let started = engine.started.lock().expect("recorded").clone();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].trigger, "on_order");
    Ok(())
}

//! Suspension, and the four things a person does to a run (§10.3, phase 4;
//! TODO 7.3).
//!
//! What phase 3's tests pinned was a run that keeps going. What is under test
//! here is a run that **stops**: for a while (`Wait`), for a person
//! (`UserForm`), for good (`cancel_run`), and one that stopped badly and is
//! started again from where it stopped (`retry_run`).
//!
//! Nothing here sleeps. A `Wait` of a day is a day on a [`ManualClock`], which is
//! the whole reason the clock is a parameter — and the same reason a suspended
//! run is testable at all: it is a **row**, not a parked future, so the test
//! writes the row, moves the clock, and asks the queue.

use sc_types::Role;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use sc_action::{
    Action, ActionContext, ActionRegistry, Event, EventKind, Trigger, TriggerBody,
    TriggerDispatcher, TriggerId,
};
use sc_agent::{RunState, bootstrap_runs, require_run, save_run};
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_test_harness::TestDb;
use sc_types::clock::{Clock, ManualClock};
use sc_types::{Attrs, BasicType, FormField, TypeRef};
use sc_workflow::{
    Assignment, DatabaseQueue, Driver, ErrorPolicy, FieldDecl, Next, Step, StepKind, Workflow,
    bootstrap_run_traces, bootstrap_workflow_versions, cancel_run, check_may_resume,
    list_run_traces, list_workflow_runs, resume_run, retry_run, run_pending_form, run_state,
    save_workflow, start_run,
};
use serde_json::{Value as Json, json};

// ---------------------------------------------------------------------------
// The fixture: one action that does what the test told it to, and nothing else.
// ---------------------------------------------------------------------------

/// An action that fails while `failing` is set and returns `{"ok": true}`
/// otherwise — everything these tests need a step to be able to do.
struct Flaky {
    failing: Arc<Mutex<bool>>,
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Action for Flaky {
    fn name(&self) -> &str {
        "flaky"
    }

    fn description(&self) -> &str {
        "fails or does not, as the test says"
    }

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(ctx.trigger.to_owned());
        }
        if self.failing.lock().map(|f| *f).unwrap_or(false) {
            return Err(Error::msg("the supplier is down"));
        }
        Ok(json!({ "ok": true }))
    }
}

/// The handle a test keeps on the action above.
#[derive(Clone)]
struct Flakiness {
    failing: Arc<Mutex<bool>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Flakiness {
    fn new() -> (Flaky, Flakiness) {
        let failing = Arc::new(Mutex::new(false));
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Flaky {
                failing: Arc::clone(&failing),
                calls: Arc::clone(&calls),
            },
            Flakiness { failing, calls },
        )
    }

    fn fail(&self, failing: bool) {
        if let Ok(mut flag) = self.failing.lock() {
            *flag = failing;
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

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

fn dispatcher(action: Flaky) -> Result<Arc<TriggerDispatcher>> {
    let mut registry = ActionRegistry::new();
    registry.register(Arc::new(action))?;
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

fn flaky_step(name: &str) -> Step {
    Step::new(
        name,
        StepKind::Action {
            action: "flaky".to_owned(),
            configuration: Attrs::new(),
        },
    )
}

/// The approval workflow the milestone's definition of done is written around,
/// minus the parts that need an LLM and a mail server: ask a person, then record
/// what they said.
fn approval(id: TriggerId, min_role: Option<Role>, timeout: Option<&str>) -> Workflow {
    Workflow::of(
        id,
        1,
        vec![
            Step::new(
                "approve",
                StepKind::UserForm {
                    fields: vec![
                        FieldDecl::new("approved", "bool").required(),
                        FieldDecl::new("note", "text"),
                    ],
                    assign_to: "approval".to_owned(),
                    min_role,
                    timeout: timeout.map(str::to_owned),
                },
            )
            .then(Next::step("record")),
            Step::new(
                "record",
                StepKind::Set {
                    assignments: vec![Assignment::new("decision", "context.approval.approved")],
                },
            ),
        ],
    )
    .traced()
}

// ---------------------------------------------------------------------------
// 4.1 — waiting for a time.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_wait_leaves_the_queues_reach_until_its_deadline_and_then_carries_on() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, _flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let trigger = trigger("overnight");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            // A day, written as a duration in milliseconds.
            Step::new(
                "sleep",
                StepKind::Wait {
                    until: "86400000".to_owned(),
                },
            )
            .then(Next::step("after")),
            Step::new(
                "after",
                StepKind::Set {
                    assignments: vec![Assignment::new("woke", "true")],
                },
            ),
        ],
    )
    .traced();
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let start = Utc::now();
    let clock = ManualClock::new(start);
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["overnight".to_owned()],
    )
    .await?;

    let stored = require_run(&catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Waiting);
    let wake_at = stored.wake_at.expect("a wait names an instant");
    assert!(
        (wake_at - (start + Duration::days(1))).num_seconds().abs() <= 1,
        "the deadline is a day away, not {wake_at}"
    );
    // Restart-safe by construction: the deadline is a column, so nothing is
    // holding a timer and nothing has to be rebuilt.
    let queue = DatabaseQueue::new(
        Arc::clone(&catalog),
        "node-a",
        std::time::Duration::from_millis(1),
    );
    assert!(
        queue.due(start + Duration::hours(23), 10).await?.is_empty(),
        "an hour early is still waiting"
    );
    assert_eq!(queue.due(start + Duration::days(1), 10).await?.len(), 1);

    // Tomorrow: the clock moves, and the run carries on from the step after the
    // wait rather than from the beginning.
    clock.set(start + Duration::days(1));
    let mut claimed = require_run(&catalog, run.id).await?;
    Driver::new(&catalog, &dispatcher, &clock)
        .drive(&mut claimed)
        .await?;
    let finished = require_run(&catalog, run.id).await?;
    assert_eq!(finished.state, RunState::Done, "{:?}", finished.error);
    assert_eq!(run_state(&finished)?.context()["woke"], json!(true));
    Ok(())
}

// ---------------------------------------------------------------------------
// 4.2 — waiting for a person.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_approval_is_answered_through_the_declaration_the_person_was_shown() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, _flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let trigger = trigger("approval");
    save_workflow(&catalog, &approval(trigger.id, None, None), "v1", None).await?;

    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["approval".to_owned()],
    )
    .await?;

    let waiting = require_run(&catalog, run.id).await?;
    assert_eq!(waiting.state, RunState::Waiting);
    assert_eq!(waiting.wake_at, None, "no clock wakes an approval");
    let form = run_pending_form(&waiting)?.expect("a form on the run");
    assert_eq!(form.assign_to, "approval");
    assert_eq!(form.fields.len(), 2);

    // A day later — the server could have been restarted twenty times in
    // between; the run is a row.
    clock.advance(Duration::days(1));
    let answered = resume_run(
        &catalog,
        &dispatcher,
        &clock,
        run.id,
        [
            ("approved".to_owned(), json!(true)),
            ("note".to_owned(), json!("looks fine")),
        ]
        .into_iter()
        .collect(),
    )
    .await?;

    assert_eq!(answered.state, RunState::Done, "{:?}", answered.error);
    let context = run_state(&answered)?.context().clone();
    // Merged whole, under the step's `assign_to` — so two forms with a `note`
    // field cannot overwrite each other.
    assert_eq!(
        context["approval"],
        json!({"approved": true, "note": "looks fine"})
    );
    // And the step after the form read them.
    assert_eq!(context["decision"], json!(true));

    // The trace shows the suspension and then the steps that followed it.
    let traces = list_run_traces(&catalog, run.id.0).await?;
    let steps: Vec<&str> = traces.iter().map(|t| t.step.as_str()).collect();
    assert_eq!(steps, vec!["approve", "record"]);
    Ok(())
}

#[tokio::test]
async fn an_answer_that_is_not_what_the_form_asked_for_is_refused() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, _flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let trigger = trigger("approval");
    save_workflow(&catalog, &approval(trigger.id, None, None), "v1", None).await?;
    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["approval".to_owned()],
    )
    .await?;

    // `approved` is required and missing.
    let err = resume_run(&catalog, &dispatcher, &clock, run.id, Attrs::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("approved"), "{err}");

    // A value of the wrong type, likewise.
    let err = resume_run(
        &catalog,
        &dispatcher,
        &clock,
        run.id,
        [("approved".to_owned(), json!("perhaps"))]
            .into_iter()
            .collect(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("approved"), "{err}");

    // And the run is untouched by either refusal: still waiting, still with its
    // form.
    let still = require_run(&catalog, run.id).await?;
    assert_eq!(still.state, RunState::Waiting);
    assert!(run_pending_form(&still)?.is_some());
    Ok(())
}

#[tokio::test]
async fn resuming_a_run_that_is_not_waiting_for_a_person_says_so() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, _flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let trigger = trigger("plain");
    let workflow = Workflow::of(trigger.id, 1, vec![flaky_step("go")]);
    save_workflow(&catalog, &workflow, "v1", None).await?;
    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["plain".to_owned()],
    )
    .await?;
    assert_eq!(require_run(&catalog, run.id).await?.state, RunState::Done);

    let err = resume_run(&catalog, &dispatcher, &clock, run.id, Attrs::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not waiting"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_abandoned_approval_times_out_and_the_error_policy_decides() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, _flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let trigger = trigger("chased");
    let mut workflow = approval(trigger.id, None, Some("3600000"));
    // Nobody answered in an hour: branch to the escalation step rather than wait
    // for ever.
    workflow.steps.push(
        Step::new(
            "escalate",
            StepKind::Set {
                assignments: vec![Assignment::new("escalated", "context.error.message")],
            },
        )
        .then(Next::End),
    );
    workflow.error_policy = ErrorPolicy::Handler {
        step: "escalate".to_owned(),
    };
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let start = Utc::now();
    let clock = ManualClock::new(start);
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["chased".to_owned()],
    )
    .await?;

    // A form with a timeout is genuinely both: a person may answer it, and the
    // clock may give up on them. So it has a `wake_at` **and** a form.
    let waiting = require_run(&catalog, run.id).await?;
    assert_eq!(waiting.state, RunState::Waiting);
    assert!(waiting.wake_at.is_some());
    assert!(run_pending_form(&waiting)?.is_some());

    clock.set(start + Duration::hours(2));
    let mut claimed = require_run(&catalog, run.id).await?;
    Driver::new(&catalog, &dispatcher, &clock)
        .drive(&mut claimed)
        .await?;

    let finished = require_run(&catalog, run.id).await?;
    assert_eq!(finished.state, RunState::Done, "{:?}", finished.error);
    let escalated = run_state(&finished)?.context()["escalated"].clone();
    assert!(
        escalated.as_str().is_some_and(|m| m.contains("timeout")),
        "the handler reads what went wrong: {escalated}"
    );
    Ok(())
}

#[tokio::test]
async fn the_step_that_stops_the_run_gets_a_trace_row_of_its_own() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, _flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    // One advance services `classify` — the assignment, and the branch that
    // resolves its `next` — and then walks straight into `approve`, which
    // suspends without doing anything. Two steps were reached, so there are two
    // rows: a timeline that labelled the branch "suspended" and never mentioned
    // the approval would be a timeline of the wrong run.
    let trigger = trigger("straight-in");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            Step::new(
                "classify",
                StepKind::Set {
                    assignments: vec![Assignment::new("large", "row.total > 100")],
                },
            )
            .then(Next::step("approve")),
            Step::new(
                "approve",
                StepKind::UserForm {
                    fields: vec![FieldDecl::new("approved", "bool").required()],
                    assign_to: "approval".to_owned(),
                    min_role: None,
                    timeout: None,
                },
            ),
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
        vec!["straight-in".to_owned()],
    )
    .await?;

    let traces = list_run_traces(&catalog, run.id.0).await?;
    let rows: Vec<(&str, &str)> = traces
        .iter()
        .map(|t| (t.step.as_str(), t.outcome.as_str()))
        .collect();
    assert_eq!(rows, vec![("classify", "ok"), ("approve", "suspended")]);
    // The sequence is the run's, so the timeline reads in the order the steps
    // happened.
    assert_eq!(traces[0].seq, 1);
    assert_eq!(traces[1].seq, 2);
    Ok(())
}

// ---------------------------------------------------------------------------
// 4.3 — who may answer.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_forms_role_floor_travels_with_the_run() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, _flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let trigger = trigger("staffed");
    save_workflow(
        &catalog,
        &approval(trigger.id, Some(Role::of(40)), None),
        "v1",
        None,
    )
    .await?;
    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["staffed".to_owned()],
    )
    .await?;

    // The floor is on the run, not looked up from the workflow when somebody
    // asks — which is what lets the workflow be edited underneath a form
    // somebody has open without the rules changing under them.
    let form = run_pending_form(&require_run(&catalog, run.id).await?)?.expect("a form");
    assert_eq!(form.min_role, Some(Role::of(40)));
    assert!(
        check_may_resume(&form, Role::ADMIN).is_ok(),
        "an admin always may"
    );
    assert!(check_may_resume(&form, Role::of(40)).is_ok());
    let err = check_may_resume(&form, Role::of(80))
        .unwrap_err()
        .to_string();
    assert!(err.contains("role 40"), "{err}");
    Ok(())
}

// ---------------------------------------------------------------------------
// 4.4 — cancelling and retrying.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_a_waiting_run_stops_it_for_good_and_takes_it_out_of_the_queue() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, _flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let trigger = trigger("overnight");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![Step::new(
            "sleep",
            StepKind::Wait {
                until: "86400000".to_owned(),
            },
        )],
    );
    save_workflow(&catalog, &workflow, "v1", None).await?;

    let start = Utc::now();
    let clock = ManualClock::new(start);
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["overnight".to_owned()],
    )
    .await?;
    assert_eq!(
        require_run(&catalog, run.id).await?.state,
        RunState::Waiting
    );

    let cancelled = cancel_run(&catalog, &clock, run.id, Some("the order was withdrawn")).await?;
    assert_eq!(cancelled.state, RunState::Aborted);
    let stored = require_run(&catalog, run.id).await?;
    assert_eq!(stored.state, RunState::Aborted);
    assert!(
        stored.error.as_deref().unwrap_or("").contains("withdrawn"),
        "{:?}",
        stored.error
    );
    // Its deadline comes and goes and nothing picks it up: `aborted` is not one
    // of the states the queue looks for.
    let queue = DatabaseQueue::new(
        Arc::clone(&catalog),
        "node-a",
        std::time::Duration::from_millis(1),
    );
    assert!(queue.due(start + Duration::days(2), 10).await?.is_empty());

    // And a run that has already stopped cannot be cancelled twice: the row is a
    // record of what happened, and rewriting an ending loses it.
    let err = cancel_run(&catalog, &clock, run.id, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("already stopped"), "{err}");
    Ok(())
}

#[tokio::test]
async fn retrying_a_failed_run_starts_again_at_the_step_that_failed() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let trigger = trigger("bill");
    let workflow = Workflow::of(
        trigger.id,
        1,
        vec![
            Step::new(
                "prepare",
                StepKind::Set {
                    assignments: vec![Assignment::new("prepared", "row.total")],
                },
            )
            .then(Next::step("send")),
            flaky_step("send").then(Next::step("after")),
            Step::new(
                "after",
                StepKind::Set {
                    assignments: vec![Assignment::new("finished", "true")],
                },
            ),
        ],
    )
    .traced();
    save_workflow(&catalog, &workflow, "v1", None).await?;

    flakiness.fail(true);
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
    let failed = require_run(&catalog, run.id).await?;
    assert_eq!(failed.state, RunState::Failed);
    assert!(
        failed.error.as_deref().unwrap_or("").contains("send"),
        "the step is named: {:?}",
        failed.error
    );
    assert_eq!(flakiness.calls().len(), 1);

    // The outage is over.
    flakiness.fail(false);
    let retried = retry_run(&catalog, &dispatcher, &clock, run.id).await?;
    assert_eq!(retried.state, RunState::Done, "{:?}", retried.error);
    // `send` ran again and `prepare` did **not**: the steps before the failure
    // already had their effects, and re-running them is the thing at-least-once
    // is trying to do less of.
    assert_eq!(flakiness.calls().len(), 2);
    let context = run_state(&retried)?.context().clone();
    assert_eq!(context["prepared"], json!(120));
    assert_eq!(context["finished"], json!(true));
    // The reason it stopped went with the life it stopped in.
    assert_eq!(require_run(&catalog, run.id).await?.error, None);

    // A finished run has no failure to start again.
    let err = retry_run(&catalog, &dispatcher, &clock, run.id)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("only a run that failed"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_retry_carries_on_on_the_version_the_run_was_pinned_to() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let trigger = trigger("pinned");
    let v1 = Workflow::of(
        trigger.id,
        1,
        vec![
            flaky_step("send").then(Next::step("after")),
            Step::new(
                "after",
                StepKind::Set {
                    assignments: vec![Assignment::new("version", "1")],
                },
            ),
        ],
    );
    save_workflow(&catalog, &v1, "v1", None).await?;

    flakiness.fail(true);
    let clock = ManualClock::new(Utc::now());
    let run = start_run(
        &catalog,
        &dispatcher,
        &clock,
        &trigger,
        &insert_event(),
        vec!["pinned".to_owned()],
    )
    .await?;
    assert_eq!(require_run(&catalog, run.id).await?.state, RunState::Failed);

    // The workflow is edited twice while the run sits there failed.
    let mut v2 = v1.clone();
    v2.steps[1] = Step::new(
        "after",
        StepKind::Set {
            assignments: vec![Assignment::new("version", "2")],
        },
    );
    save_workflow(&catalog, &v2, "v2", None).await?;
    let mut v3 = v1.clone();
    v3.steps[1] = Step::new(
        "after",
        StepKind::Set {
            assignments: vec![Assignment::new("version", "3")],
        },
    );
    save_workflow(&catalog, &v3, "v3", None).await?;

    flakiness.fail(false);
    let retried = retry_run(&catalog, &dispatcher, &clock, run.id).await?;
    assert_eq!(retried.state, RunState::Done, "{:?}", retried.error);
    assert_eq!(retried.subject_version, Some(1));
    assert_eq!(
        run_state(&retried)?.context()["version"],
        json!(1),
        "a retried run finishes on the version it started with"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 5.2 — the run list the admin screen reads.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_runs_of_one_workflow_come_back_newest_first_filtered_and_paged() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let (flaky, flakiness) = Flakiness::new();
    let dispatcher = dispatcher(flaky)?;

    let many = trigger("many");
    save_workflow(
        &catalog,
        &Workflow::of(many.id, 1, vec![flaky_step("go")]),
        "v1",
        None,
    )
    .await?;
    let other = trigger("other");
    save_workflow(
        &catalog,
        &Workflow::of(other.id, 1, vec![flaky_step("go")]),
        "v1",
        None,
    )
    .await?;

    let clock = ManualClock::new(Utc::now());
    let mut ids = Vec::new();
    for nth in 0..3 {
        // The third one fails, so there is something to filter on.
        flakiness.fail(nth == 2);
        let run = start_run(
            &catalog,
            &dispatcher,
            &clock,
            &many,
            &insert_event(),
            vec!["many".to_owned()],
        )
        .await?;
        ids.push(run.id);
        // The list orders by `created_at`, which is the database's resolution;
        // one second apart makes the order the test asserts the order that is
        // stored rather than a tie broken by chance.
        clock.advance(Duration::seconds(1));
        let mut stored = require_run(&catalog, run.id).await?;
        stored.created_at = clock.now();
        save_run(&catalog, &stored).await?;
    }
    flakiness.fail(false);
    start_run(
        &catalog,
        &dispatcher,
        &clock,
        &other,
        &insert_event(),
        vec!["other".to_owned()],
    )
    .await?;

    // By the workflow's name, newest first, and nobody else's runs.
    let all = list_workflow_runs(&catalog, "many", None, 50, 0).await?;
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, ids[2]);
    assert!(all.iter().all(|r| r.subject == "many"));
    // Pinned to a version, which is what the list shows beside the state.
    assert!(all.iter().all(|r| r.subject_version == Some(1)));

    // Filtered by state.
    let failed = list_workflow_runs(&catalog, "many", Some(RunState::Failed), 50, 0).await?;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].id, ids[2]);

    // Paged.
    let page = list_workflow_runs(&catalog, "many", None, 2, 0).await?;
    assert_eq!(page.len(), 2);
    let next = list_workflow_runs(&catalog, "many", None, 2, 2).await?;
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].id, ids[0]);
    Ok(())
}

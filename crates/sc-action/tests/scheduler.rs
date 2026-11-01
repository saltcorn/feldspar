//! Phase 8: the periodic scheduler, driven by a clock the test owns (§10.2).
//!
//! Every claim here is about *when* something runs, so waiting for it would make
//! the suite take a day to assert one daily trigger. Instead the clock is a
//! parameter — [`Scheduler::tick`] takes the time — and the test hands it
//! Wednesday, then Thursday, then three days later.
//!
//! What is pinned:
//!
//! - an `often` and a `daily` trigger fire **exactly** on their schedule, and a
//!   freshly created one is not instantly overdue;
//! - the run is **persisted**, which is what makes the next one land in the right
//!   place after a restart;
//! - a run missed while the process was down is caught up **once**, not once per
//!   missed period and not lost;
//! - a slow action does not stack up: overlapping occurrences are skipped, not
//!   queued;
//! - a disabled trigger does not fire, and switching it back on does not make it
//!   run everything it missed.
//!
//! The action is written here rather than borrowed from `sc-action-builtins`
//! because two of those claims need one that is *slow* and one that counts its
//! own runs, and neither is something a built-in action should be able to do.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use sc_action::{
    ATTR_HOUR, ATTR_MINUTE, Action, ActionContext, ActionRegistry, EventKind, Scheduler, Trigger,
    TriggerDispatcher, bootstrap_triggers, load_trigger_by_name, record_trigger_run, save_trigger,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use sc_types::FormField;
use serde_json::{Value as Json, json};

/// An action that counts its runs per trigger, and optionally takes a while.
///
/// The delay is what makes the overlap rule testable: without an action that is
/// still running when the next occurrence comes round, "skipped rather than
/// queued" is a claim about a state that never happens.
#[derive(Default)]
struct Counter {
    runs: Mutex<BTreeMap<String, usize>>,
    /// Milliseconds each run takes.
    delay_ms: AtomicUsize,
}

impl Counter {
    fn count(&self, trigger: &str) -> usize {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(trigger)
            .copied()
            .unwrap_or(0)
    }

    fn slow(&self, ms: usize) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Action for Counter {
    fn name(&self) -> &str {
        "count"
    }

    fn description(&self) -> &str {
        "Count this trigger's runs (test only)"
    }

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
        }
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let count = runs.entry(ctx.trigger.to_owned()).or_insert(0);
        *count += 1;
        Ok(json!({ "runs": *count }))
    }
}

struct Harness {
    catalog: Arc<Catalog>,
    dispatcher: Arc<TriggerDispatcher>,
    scheduler: Arc<Scheduler>,
    counter: Arc<Counter>,
    registry: Arc<ActionRegistry>,
}

impl Harness {
    /// Save `trigger` and reload the live set, so the scheduler sees it.
    async fn add(&self, trigger: &Trigger) -> Result<()> {
        save_trigger(&self.catalog, &self.registry, trigger).await?;
        self.dispatcher.reload(&self.catalog).await
    }

    /// One tick, awaited to completion — the runs it starts are tasks of their
    /// own, so a test that asserts on their effects has to let them finish.
    async fn tick(&self, now: DateTime<Utc>) -> Vec<String> {
        let fired = self.scheduler.tick(now).await;
        self.scheduler.wait_until_idle().await;
        fired
    }

    /// What the *row* says about the last run — the part that survives a restart.
    async fn stored_last_run(&self, name: &str) -> Result<Option<DateTime<Utc>>> {
        Ok(load_trigger_by_name(&self.catalog, name)
            .await?
            .and_then(|t| t.last_run_at))
    }

    /// A second scheduler over the same database: what a restart looks like from
    /// the scheduler's point of view — no in-memory clocks, only the rows.
    fn restart(&self) -> Arc<Scheduler> {
        Arc::new(Scheduler::new(
            Arc::clone(&self.catalog),
            Arc::clone(&self.dispatcher),
        ))
    }
}

async fn setup(db: &TestDb) -> Result<Harness> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    bootstrap_triggers(&catalog).await?;

    let counter = Arc::new(Counter::default());
    let mut registry = ActionRegistry::new();
    registry.register(Arc::clone(&counter) as Arc<dyn Action>)?;
    let registry = Arc::new(registry);

    let dispatcher = Arc::new(TriggerDispatcher::new(Arc::clone(&registry)));
    dispatcher.reload(&catalog).await?;
    let scheduler = Arc::new(Scheduler::new(
        Arc::clone(&catalog),
        Arc::clone(&dispatcher),
    ));
    Ok(Harness {
        catalog,
        dispatcher,
        scheduler,
        counter,
        registry,
    })
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .expect("a valid timestamp")
        .with_timezone(&Utc)
}

#[tokio::test]
async fn an_often_and_a_daily_trigger_fire_exactly_when_they_are_due() -> Result<()> {
    let db = TestDb::new().await?;
    let h = setup(&db).await?;
    h.add(&Trigger::new("every_five", EventKind::Often, "count"))
        .await?;
    h.add(
        &Trigger::new("nightly", EventKind::Daily, "count")
            .timing(ATTR_HOUR, 3)
            .timing(ATTR_MINUTE, 30),
    )
    .await?;

    // The first tick only *starts the clock*. A trigger created a minute ago is
    // not overdue by the age of the epoch, which is what seeding from now buys.
    let start = at("2026-07-25T10:00:00Z");
    assert_eq!(h.tick(start).await, Vec::<String>::new());
    assert_eq!(h.counter.count("every_five"), 0);

    // Four minutes later: not yet.
    assert!(h.tick(start + Duration::minutes(4)).await.is_empty());
    // Five: the `often` trigger, and only it — the daily one is not due until
    // 03:30 tomorrow.
    assert_eq!(
        h.tick(start + Duration::minutes(5)).await,
        vec!["every_five".to_owned()]
    );
    assert_eq!(h.counter.count("every_five"), 1);
    assert_eq!(h.counter.count("nightly"), 0);

    // The next four minutes are quiet: the spacing runs from the last run.
    for minute in 6..10 {
        assert!(h.tick(start + Duration::minutes(minute)).await.is_empty());
    }
    assert_eq!(h.tick(start + Duration::minutes(10)).await.len(), 1);
    assert_eq!(h.counter.count("every_five"), 2);

    // Tomorrow at 03:29, the daily one is still not due…
    let almost = at("2026-07-26T03:29:00Z");
    assert!(!h.tick(almost).await.contains(&"nightly".to_owned()));
    assert_eq!(h.counter.count("nightly"), 0);
    // …and at 03:30 it is, once.
    assert!(
        h.tick(at("2026-07-26T03:30:00Z"))
            .await
            .contains(&"nightly".to_owned())
    );
    assert_eq!(h.counter.count("nightly"), 1);
    // Later the same day it does not run again.
    assert!(
        !h.tick(at("2026-07-26T09:00:00Z"))
            .await
            .contains(&"nightly".to_owned())
    );
    assert_eq!(h.counter.count("nightly"), 1);

    // And the run is on the row, at the time it was *due* rather than whenever
    // the action happened to finish — which is what stops a slow job drifting
    // later every day.
    assert_eq!(
        h.stored_last_run("nightly").await?,
        Some(at("2026-07-26T03:30:00Z"))
    );
    Ok(())
}

#[tokio::test]
async fn a_run_missed_while_the_server_was_down_fires_once_at_startup() -> Result<()> {
    let db = TestDb::new().await?;
    let h = setup(&db).await?;
    let nightly = Trigger::new("nightly", EventKind::Daily, "count").timing(ATTR_HOUR, 3);
    h.add(&nightly).await?;

    // The server ran it three days ago and was then down. That is a stored fact,
    // not an in-memory one — which is the whole reason `last_run_at` is
    // persisted.
    record_trigger_run(&h.catalog, nightly.id, at("2026-07-22T03:00:00Z")).await?;
    h.dispatcher.reload(&h.catalog).await?;

    // Coming back up: the first tick catches it up.
    let scheduler = h.restart();
    let now = at("2026-07-25T09:12:00Z");
    assert_eq!(scheduler.tick(now).await, vec!["nightly".to_owned()]);
    scheduler.wait_until_idle().await;
    assert_eq!(h.counter.count("nightly"), 1);

    // **Once**, not once per missed day: the next tick a minute later does
    // nothing, and the one after that waits for tomorrow's 03:00.
    assert!(scheduler.tick(now + Duration::minutes(1)).await.is_empty());
    assert!(scheduler.tick(at("2026-07-25T23:59:00Z")).await.is_empty());
    assert_eq!(h.counter.count("nightly"), 1);
    assert!(
        scheduler
            .tick(at("2026-07-26T03:00:00Z"))
            .await
            .contains(&"nightly".to_owned())
    );
    scheduler.wait_until_idle().await;
    assert_eq!(h.counter.count("nightly"), 2);
    Ok(())
}

#[tokio::test]
async fn a_slow_action_is_skipped_rather_than_queued() -> Result<()> {
    let db = TestDb::new().await?;
    let h = setup(&db).await?;
    h.add(&Trigger::new("slow", EventKind::Often, "count"))
        .await?;
    // Slow enough that the ticks below all land while it is still running.
    h.counter.slow(300);

    let start = at("2026-07-25T10:00:00Z");
    h.scheduler.tick(start).await;
    assert_eq!(
        h.scheduler.tick(start + Duration::minutes(5)).await,
        vec!["slow".to_owned()],
        "the first due occurrence starts a run"
    );
    assert_eq!(h.scheduler.running(), vec!["slow".to_owned()]);

    // The next two occurrences come round while it is still going. They are
    // **dropped**: five queued copies of a report nobody read is worse than one
    // late one.
    assert!(
        h.scheduler
            .tick(start + Duration::minutes(10))
            .await
            .is_empty()
    );
    assert!(
        h.scheduler
            .tick(start + Duration::minutes(15))
            .await
            .is_empty()
    );

    h.scheduler.wait_until_idle().await;
    assert_eq!(h.counter.count("slow"), 1, "one run, not three");

    // Once it has finished, the schedule resumes from the run that happened —
    // last run at +5, so the next tick past +10 fires **once**, catching up on
    // the whole gap with a single run rather than one per missed occurrence.
    h.counter.slow(0);
    assert_eq!(
        h.tick(start + Duration::minutes(16)).await,
        vec!["slow".to_owned()]
    );
    assert_eq!(h.counter.count("slow"), 2, "one catch-up run, not two");
    // And from there the ordinary spacing: nothing until five minutes after that
    // run.
    assert!(h.tick(start + Duration::minutes(20)).await.is_empty());
    assert_eq!(
        h.tick(start + Duration::minutes(21)).await,
        vec!["slow".to_owned()]
    );
    assert_eq!(h.counter.count("slow"), 3);
    Ok(())
}

#[tokio::test]
async fn a_disabled_periodic_trigger_does_not_fire_and_does_not_catch_up() -> Result<()> {
    let db = TestDb::new().await?;
    let h = setup(&db).await?;
    let mut off = Trigger::new("paused", EventKind::Often, "count");
    off.set_enabled(false);
    h.add(&off).await?;

    let start = at("2026-07-25T10:00:00Z");
    h.tick(start).await;
    assert!(h.tick(start + Duration::minutes(30)).await.is_empty());
    assert_eq!(h.counter.count("paused"), 0);
    // Its clock advanced anyway, which is the rule stated from the inside: the
    // occurrences it was off for are *skipped*, not saved up.
    assert_eq!(
        h.scheduler.last_run("paused"),
        Some(start + Duration::minutes(30))
    );
    // Nothing ran, so the row still says it never has.
    assert_eq!(h.stored_last_run("paused").await?, None);

    // Switched back on, it runs on the *next* occurrence — it does not replay the
    // half hour it was off for. Switching a trigger off is a decision to skip
    // those runs, not to save them up; the missed-run catch-up is for downtime,
    // which nobody chose.
    off.set_enabled(true);
    h.add(&off).await?;
    assert!(h.tick(start + Duration::minutes(31)).await.is_empty());
    assert_eq!(h.counter.count("paused"), 0);
    assert_eq!(
        h.tick(start + Duration::minutes(35)).await,
        vec!["paused".to_owned()],
        "five minutes after the clock last advanced, and once"
    );
    assert_eq!(h.counter.count("paused"), 1);
    Ok(())
}

#[tokio::test]
async fn a_trigger_that_is_not_periodic_is_never_the_schedulers_business() -> Result<()> {
    let db = TestDb::new().await?;
    let h = setup(&db).await?;
    h.add(&Trigger::new("on_login", EventKind::Login, "count"))
        .await?;
    h.add(&Trigger::new("on_demand", EventKind::None, "count"))
        .await?;

    let start = at("2026-07-25T10:00:00Z");
    for hours in 0..30 {
        assert!(h.tick(start + Duration::hours(hours)).await.is_empty());
    }
    assert_eq!(h.counter.count("on_login"), 0);
    assert_eq!(h.counter.count("on_demand"), 0);
    Ok(())
}

//! The periodic scheduler: one task, one minute at a time (design §10.2).
//!
//! Every other event has something that causes it — a write, a login, a request.
//! A periodic trigger has only the clock, so something has to be watching it.
//! [`Scheduler`] is that: one tokio task started at boot, waking on the minute
//! boundary, asking each periodic trigger whether it is due, and firing the ones
//! that are **through the same [`run_trigger`](TriggerDispatcher::run_trigger)
//! every other direct run goes through**. A scheduled run is not a second kind of
//! run: the `only_if`, the cascade bound and the action are the ones the admin
//! configured.
//!
//! ## The clock is a parameter
//!
//! [`tick`](Scheduler::tick) takes the current time rather than reading it, and
//! the task loop is the only place `Utc::now()` appears. That is what lets a test
//! drive a week of schedule in a millisecond, and it is why the due-time rules
//! ([`Schedule`]) can be asserted against a table of instants rather than by
//! waiting for them.
//!
//! ## What one tick decides
//!
//! For each periodic trigger in the live set:
//!
//! 1. **Seed its clock** if this process has not seen it: from the stored
//!    [`last_run_at`](Trigger::last_run_at), or — for a trigger that has never
//!    run — from *now*. Never from the epoch, which would make every newly
//!    created trigger instantly overdue.
//! 2. **Skip it if it is still running.** A slow action must not stack up: the
//!    missed occurrences are dropped, not queued, because five queued copies of a
//!    report nobody read is worse than one late one.
//! 3. **Fire it if it is due**, in a task of its own, so one slow action delays
//!    neither the clock nor any other trigger — unless it is switched off, in
//!    which case its clock advances and nothing runs (see
//!    [`claim`](Scheduler::claim)).
//!
//! A run's outcome — including its failure — is recorded as a run: a trigger
//! whose action throws every time must not retry every minute for ever, and the
//! failure is reported where every other server-side failure goes.
//!
//! ## Catching up exactly once
//!
//! `last_run_at` is persisted, so a server that was down over a daily trigger's
//! hour comes up, finds it overdue, and fires it **once**. That falls out of the
//! rule rather than being special-cased: the trigger is due when
//! `next_due(last_run) <= now`, and after one run `last_run` is now.

use sc_catalog::Authority;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Timelike, Utc};
use sc_catalog::Catalog;
use serde_json::Value as Json;

use crate::dispatch::TriggerDispatcher;
use crate::schedule::Schedule;
use crate::store::record_trigger_run;
use crate::trigger::Trigger;

/// How often the task loop wakes: on the minute, because that is the finest
/// resolution any schedule has.
const TICK_SECONDS: i64 = 60;

/// What this process knows about one periodic trigger, keyed by name.
#[derive(Debug, Clone, Copy)]
struct Clock {
    /// When it last ran — stored, or when it was first seen.
    last_run: DateTime<Utc>,
    /// Whether a run of it is in flight (the overlap skip).
    running: bool,
}

/// The periodic trigger scheduler.
///
/// Cheap to share (`Arc` it): the per-trigger clocks are behind a `Mutex` that is
/// never held across an await, and everything else is an `Arc` handle.
pub struct Scheduler {
    catalog: Arc<Catalog>,
    dispatcher: Arc<TriggerDispatcher>,
    /// An `Arc` rather than a plain field because each firing runs in its own
    /// task, which outlives the `&self` that started it and has to be able to
    /// clear the trigger's `running` flag when it finishes.
    clocks: Arc<Mutex<HashMap<String, Clock>>>,
}

impl Scheduler {
    /// A scheduler over `catalog`'s triggers, firing through `dispatcher`.
    pub fn new(catalog: Arc<Catalog>, dispatcher: Arc<TriggerDispatcher>) -> Scheduler {
        Scheduler {
            catalog,
            dispatcher,
            clocks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start the task loop: wake on each minute boundary and
    /// [`tick`](Scheduler::tick).
    ///
    /// Returns the handle so a caller can abort it; a dropped handle leaves the
    /// task running, which is what a server wants (it ends with the process).
    /// **The one place the clock is read**: everything below takes the time as an
    /// argument.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let scheduler = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(until_next_minute(Utc::now())).await;
                scheduler.tick(Utc::now()).await;
            }
        })
    }

    /// One pass of the clock: fire every periodic trigger due at `now`, and
    /// return their names.
    ///
    /// Never fails as a whole — a trigger whose schedule will not parse is
    /// reported and the others still run, exactly as one whose action fails is.
    /// The names come back for the caller that is watching (a test, and a future
    /// admin "what fired?" view); the task loop ignores them.
    pub async fn tick(&self, now: DateTime<Utc>) -> Vec<String> {
        let live = match self.dispatcher.triggers() {
            Ok(live) => live,
            Err(e) => {
                eprintln!("feldspar: the scheduler could not read the trigger set: {e}");
                return Vec::new();
            }
        };
        let periodic: Vec<&Trigger> = live.all().iter().filter(|t| t.when.is_periodic()).collect();
        self.forget_all_but(&periodic);

        let mut fired = Vec::new();
        for trigger in periodic {
            let schedule = match Schedule::of(trigger) {
                Ok(Some(schedule)) => schedule,
                // `is_periodic` was true, so `of` returned Some or an error; a
                // trigger that fails validation never reaches the live set, so
                // this is the belt for a row edited around the API.
                Ok(None) => continue,
                Err(e) => {
                    eprintln!(
                        "feldspar: periodic trigger `{}` has no usable timing: {e}",
                        trigger.name
                    );
                    continue;
                }
            };
            if !self.claim(trigger, schedule, now) {
                continue;
            }
            fired.push(trigger.name.clone());
            self.spawn_run(trigger.clone(), now);
        }
        fired
    }

    /// The triggers this scheduler currently has a run in flight for.
    ///
    /// Exposed for a caller that needs to know the work has settled: a test
    /// asserting on what a tick did, and a shutdown that would rather not cut a
    /// report in half.
    pub fn running(&self) -> Vec<String> {
        let clocks = self.lock();
        let mut names: Vec<String> = clocks
            .iter()
            .filter(|(_, clock)| clock.running)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Wait until nothing this scheduler started is still running.
    ///
    /// Polls rather than signals, because the thing being waited for is rare and
    /// the wait is never on a request path.
    pub async fn wait_until_idle(&self) {
        while !self.running().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// When this scheduler believes `trigger` last ran — its stored time, the
    /// moment it was first seen, or the last run this process made.
    pub fn last_run(&self, trigger: &str) -> Option<DateTime<Utc>> {
        self.lock().get(trigger).map(|clock| clock.last_run)
    }

    /// Whether `trigger` should be fired now — seeding its clock if this process
    /// has not seen it, and **claiming** it (marking it running) when the answer
    /// is yes.
    ///
    /// The decision and the claim happen under one lock, so two overlapping ticks
    /// cannot both start the same trigger.
    ///
    /// **A disabled trigger's clock still advances.** Switching one off is a
    /// decision to skip the runs it covers, not to save them up: an admin who
    /// pauses a nightly report for a week and switches it back on on Tuesday
    /// morning wants it tonight, not immediately. That is the opposite of a
    /// missed run — nobody *chose* the downtime, so that one is caught up — and
    /// the two are different on purpose.
    fn claim(&self, trigger: &Trigger, schedule: Schedule, now: DateTime<Utc>) -> bool {
        let mut clocks = self.lock();
        let clock = clocks.entry(trigger.name.clone()).or_insert(Clock {
            // A trigger that has never run starts its schedule *now*: from the
            // epoch it would be overdue by years, and would fire the moment it
            // was created.
            last_run: trigger.last_run_at.unwrap_or(now),
            running: false,
        });
        if clock.running || !schedule.is_due(now, clock.last_run) {
            return false;
        }
        if !trigger.is_enabled() {
            // Nothing ran, so the row is deliberately not touched: `last_run_at`
            // records runs, and a skipped occurrence is not one.
            clock.last_run = now;
            return false;
        }
        clock.running = true;
        true
    }

    /// Fire one due trigger, in a task of its own.
    ///
    /// `at` is the tick's time, not the completion time, and that is what is
    /// recorded: a daily job that takes twenty minutes must not drift twenty
    /// minutes later every day.
    fn spawn_run(&self, trigger: Trigger, at: DateTime<Utc>) {
        let catalog = Arc::clone(&self.catalog);
        let dispatcher = Arc::clone(&self.dispatcher);
        let clocks = Arc::clone(&self.clocks);
        tokio::spawn(async move {
            // Nobody asked for this run, so there is no caller: the event's user
            // is null and its role is public, which is what a formula in the
            // action will see.
            let outcome = dispatcher
                .run_trigger(
                    &catalog,
                    &trigger.name,
                    Json::Null,
                    Authority::system("scheduled trigger"),
                )
                .await;
            if let Err(e) = outcome {
                eprintln!(
                    "feldspar: scheduled trigger `{}`: {}",
                    trigger.name,
                    sc_error::format_chain(&e)
                );
            }
            // Recorded whatever happened: a trigger whose action fails every time
            // is still a trigger that ran, and retrying it every minute would
            // turn one misconfiguration into a flood.
            if let Err(e) = record_trigger_run(&catalog, trigger.id, at).await {
                eprintln!(
                    "feldspar: could not record the run of scheduled trigger `{}`, so it \
                     may run again after a restart: {}",
                    trigger.name,
                    sc_error::format_chain(&e)
                );
            }
            // In-memory last, and unconditionally: whatever the database did, this
            // process has run the trigger and must not run it again this period.
            if let Some(clock) = lock(&clocks).get_mut(&trigger.name) {
                clock.last_run = at;
                clock.running = false;
            }
        });
    }

    /// Drop the clocks of triggers that are no longer periodic (deleted, or
    /// changed to another event), so a long-lived process does not accumulate
    /// them. A trigger with a run in flight keeps its clock until it finishes.
    fn forget_all_but(&self, periodic: &[&Trigger]) {
        let names: HashSet<&str> = periodic.iter().map(|t| t.name.as_str()).collect();
        self.lock()
            .retain(|name, clock| clock.running || names.contains(name.as_str()));
    }

    /// The clocks, recovering from a poisoned lock: a panic while deciding what
    /// is due must not stop the scheduler for ever — the worst a torn write
    /// leaves is one trigger's clock, which the next tick re-seeds.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Clock>> {
        lock(&self.clocks)
    }
}

/// [`Scheduler::lock`] for the spawned firing task, which holds the clocks by
/// their own handle rather than through `&self`.
fn lock(
    clocks: &Mutex<HashMap<String, Clock>>,
) -> std::sync::MutexGuard<'_, HashMap<String, Clock>> {
    clocks.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long until the next minute boundary — the wait the task loop makes, so
/// ticks land on the minute rather than drifting by however long the last one
/// took.
fn until_next_minute(now: DateTime<Utc>) -> std::time::Duration {
    let seconds = i64::from(now.second());
    let remaining = TICK_SECONDS - seconds;
    std::time::Duration::from_secs(u64::try_from(remaining.max(1)).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("a valid timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn the_loop_waits_for_the_minute_boundary() {
        assert_eq!(until_next_minute(at("2026-07-25T10:00:00Z")).as_secs(), 60);
        assert_eq!(until_next_minute(at("2026-07-25T10:00:59Z")).as_secs(), 1);
        assert_eq!(until_next_minute(at("2026-07-25T10:00:30Z")).as_secs(), 30);
    }
}

//! [`WorkflowEngineTask`]: the process's workflow engine — the
//! [`WorkflowEngine`] seam filled in, and the one tokio task that advances runs
//! nobody is waiting for (§10.3, phase 3.3).
//!
//! Two jobs, and they are the same machinery pointed at two moments:
//!
//! - **Starting a run.** A trigger whose body is a workflow fires — from an
//!   event, from the admin's Run button, from the scheduler, or from an
//!   application's exposed endpoint, all four of which come through
//!   [`fire_trigger`](sc_action::fire_trigger) — and the engine creates a run
//!   pinned to the workflow's current version and drives it as far as it goes
//!   right now. What the caller gets is the run's **id and state**, not a result:
//!   a workflow may suspend for a day, so what is returned is something
//!   addressable rather than a wait (§10.2).
//! - **Advancing runs whose time has come.** A retry's backoff, a `Wait`'s end, a
//!   resumed approval, and a run a crashed node was holding. Those have no caller
//!   at all, which is what the task and the [queue](crate::queue) are for.
//!
//! ## Started only by `serve`
//!
//! For the reason the scheduler is (§10.2): a `build-app`, a backup or an admin
//! script must not start advancing workflow runs because it happened to open the
//! same database. A process with no engine runs every action trigger normally and
//! tells the caller, of a workflow, that nothing here can run one.
//!
//! ## Shutdown lets the step finish
//!
//! [`stop`](WorkflowEngineTask::stop) asks the loop to end; a tick already in
//! flight finishes its steps and writes them, because a step abandoned between
//! its effect and its write is the one case at-least-once turns into "twice" for
//! no reason.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sc_action::{Event, Trigger, TriggerDispatcher, WorkflowEngine, WorkflowStarted};
use sc_agent::{Run, RunId};
use sc_catalog::Catalog;
use sc_error::Result;

use crate::driver::{Advanced, Driver, node_id, start_run};
use crate::queue::{DEFAULT_BATCH, DatabaseQueue, WorkQueue};
use sc_types::clock::{Clock, SystemClock};

/// How long a claim holds a run before another node may take it.
///
/// Long enough that an ordinary step finishes inside it, short enough that a
/// crashed node's run is picked up while somebody is still looking at it. The
/// driver renews it between steps, so a long run is not a long lease.
pub const DEFAULT_LEASE_SECONDS: i64 = 60;

/// How often the polling queue looks for work.
pub const DEFAULT_POLL_SECONDS: u64 = 5;

/// The engine: the seam a workflow trigger is run by, and the task that advances
/// runs nobody is waiting for.
pub struct WorkflowEngineTask {
    catalog: Arc<Catalog>,
    dispatcher: Arc<TriggerDispatcher>,
    queue: Arc<dyn WorkQueue>,
    clock: Arc<dyn Clock>,
    lease: Duration,
    /// How many runs one tick claims — the bound on how much is in flight.
    batch: usize,
    stopping: AtomicBool,
}

impl WorkflowEngineTask {
    /// The engine a server runs: the runs table as the queue, the system clock,
    /// and this process's own node identity.
    ///
    /// It holds the dispatcher it is installed **on**, which is a cycle of
    /// `Arc`s and is deliberate: both live for the process, and the alternative —
    /// a weak handle that can fail to upgrade — would make "the engine could not
    /// reach the dispatcher" a runtime error on a path that cannot happen.
    pub fn new(catalog: Arc<Catalog>, dispatcher: Arc<TriggerDispatcher>) -> WorkflowEngineTask {
        let queue = Arc::new(DatabaseQueue::new(
            Arc::clone(&catalog),
            node_id(),
            std::time::Duration::from_secs(DEFAULT_POLL_SECONDS),
        ));
        WorkflowEngineTask::with_queue(catalog, dispatcher, queue, Arc::new(SystemClock))
    }

    /// The engine over a queue and a clock of the caller's choosing — what a test
    /// builds, and what a bus-backed queue will be installed through.
    pub fn with_queue(
        catalog: Arc<Catalog>,
        dispatcher: Arc<TriggerDispatcher>,
        queue: Arc<dyn WorkQueue>,
        clock: Arc<dyn Clock>,
    ) -> WorkflowEngineTask {
        WorkflowEngineTask {
            catalog,
            dispatcher,
            queue,
            clock,
            lease: Duration::seconds(DEFAULT_LEASE_SECONDS),
            batch: DEFAULT_BATCH,
            stopping: AtomicBool::new(false),
        }
    }

    /// How many runs one tick claims.
    pub fn with_batch(mut self, batch: usize) -> WorkflowEngineTask {
        self.batch = batch.max(1);
        self
    }

    /// How long a claim holds a run.
    pub fn with_lease(mut self, lease: Duration) -> WorkflowEngineTask {
        self.lease = lease;
        self
    }

    /// Start the task loop: wait for the queue to say there may be work, then
    /// [`tick`](WorkflowEngineTask::tick).
    ///
    /// Returns the handle so a caller can abort it; a dropped handle leaves the
    /// task running, which is what a server wants (it ends with the process).
    #[allow(
        clippy::disallowed_methods,
        reason = "tokio::spawn: a background loop for the life of the process, at no caller's behalf, so it must not inherit the starting task's scope"
    )]
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            while !engine.stopping.load(Ordering::Relaxed) {
                engine.queue.wake().await;
                if engine.stopping.load(Ordering::Relaxed) {
                    break;
                }
                engine.tick().await;
            }
        })
    }

    /// Ask the loop to stop after the tick it is in.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    /// One pass of the queue: claim what is due and drive each of it as far as it
    /// goes, answering the runs that moved.
    ///
    /// Never fails as a whole — a run whose pinned version has been deleted is
    /// reported and the others still run, exactly as the scheduler treats a
    /// trigger whose schedule will not parse. The ids come back for the caller
    /// that is watching (a test, and the admin's run list); the task loop ignores
    /// them.
    pub async fn tick(&self) -> Vec<RunId> {
        let now = self.clock.now();
        let claimed = match self.queue.claim(now, now + self.lease, self.batch).await {
            Ok(claimed) => claimed,
            Err(e) => {
                sc_log::log_error!("the workflow engine could not claim any runs: {e}");
                return Vec::new();
            }
        };
        let mut advanced = Vec::with_capacity(claimed.len());
        for mut run in claimed {
            let id = run.id;
            match self.advance_claimed(&mut run).await {
                Ok(_) => advanced.push(id),
                Err(e) => {
                    sc_log::log_error!("workflow run {id}: could not be advanced: {e}");
                    // The lease is left to expire rather than released: a run
                    // whose failure is the *engine's* (an unreadable version, a
                    // database that went away) must not be retried at full speed
                    // by every node at once.
                }
            }
        }
        advanced
    }

    /// Drive one claimed run, renewing its lease between steps so a slow workflow
    /// is not picked up by somebody else half way through.
    async fn advance_claimed(&self, run: &mut Run) -> Result<Advanced> {
        let driver = Driver::new(&self.catalog, &self.dispatcher, self.clock.as_ref());
        loop {
            let advanced = driver.advance(run).await?;
            if !advanced.is_runnable() {
                return Ok(advanced);
            }
            // Halfway through the lease is when it is worth renewing: any later
            // and a slow step races the expiry, any sooner and every step is a
            // write nobody needed.
            let now = self.clock.now();
            if run
                .lease_until
                .is_some_and(|until| now >= renew_at(until, self.lease))
            {
                let until = now + self.lease;
                self.queue.renew(run, until).await?;
                run.lease_until = Some(until);
            }
        }
    }

    /// This process's node identity, as it appears in `_fd_runs.claimed_by`.
    pub fn node(&self) -> &str {
        self.queue.node()
    }

    /// The clock this engine reads — what a test moves.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }
}

#[async_trait]
impl WorkflowEngine for WorkflowEngineTask {
    async fn start(
        &self,
        catalog: &Catalog,
        trigger: &Trigger,
        event: &Event,
        chain: Vec<String>,
    ) -> Result<WorkflowStarted> {
        let run = start_run(
            catalog,
            &self.dispatcher,
            self.clock.as_ref(),
            trigger,
            event,
            chain,
        )
        .await?;
        Ok(WorkflowStarted {
            run: run.id.0,
            state: run.state.as_str().to_owned(),
        })
    }
}

/// When a run leased until `until` should be renewed, given a lease of `lease`.
///
/// Its own function so the rule is stated once and can be read without the loop
/// around it.
pub fn renew_at(until: DateTime<Utc>, lease: Duration) -> DateTime<Utc> {
    until - lease / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_is_renewed_halfway_through_it() {
        let now = Utc::now();
        let lease = Duration::seconds(60);
        assert_eq!(renew_at(now + lease, lease), now + Duration::seconds(30));
    }
}

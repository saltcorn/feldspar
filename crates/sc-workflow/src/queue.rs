//! The queue: which runs want the engine, and who is working on them (§10.3,
//! decision 5).
//!
//! The design says the engine is driven by "a durable queue on the bus", and
//! `sc-bus` does not exist. Building one to hold a queue would be building the
//! wrong thing first: **a durable queue's authority has to be the database
//! anyway**, or a crashed node loses the runs it was holding. So the runnable set
//! is a query —
//!
//! ```text
//! kind = 'workflow'
//!   AND state IN ('running', 'waiting')
//!   AND wake_at IS NOT NULL AND wake_at <= now
//!   AND (lease_until IS NULL OR lease_until < now)
//! ```
//!
//! — and claiming a run is a conditional `UPDATE` on that same predicate, which
//! is correct for two nodes as well as for one: both may read the same row, but
//! the first `UPDATE` puts a lease in the *future* and the second's `WHERE` no
//! longer matches, so it claims nothing. Nothing here needs a transaction, a lock
//! or a queue table.
//!
//! ## The seam, and what it is for
//!
//! [`WorkQueue`] is the shape `sc-bus` will implement later: "claim what is due"
//! and "**wake me when something might be**". Only the second is interesting —
//! today it is a sleep, tomorrow it is a `NOTIFY` or a Redis subscription, and
//! **nothing above the seam knows which it is talking to**. The polling
//! implementation is what ships, and it is twenty lines.
//!
//! ## The query an idle deployment does not run
//!
//! That predicate is cheap, and it was still the most expensive thing an idle
//! server did: a round trip every poll, forever, to be told nothing is due. So
//! the catalog keeps [`RunWakeups`](sc_catalog::RunWakeups) — the earliest
//! instant any live run might want the engine — and a poll that the cache says
//! is pointless **runs no query at all**. The cache is maintained by whoever
//! writes a run row (`sc_agent::note_wakeup`), refreshed by
//! [`next_wakeup`](DatabaseQueue::next_wakeup) whenever a poll finds nothing
//! due, and re-checked against the database once per
//! [`trust window`](DatabaseQueue::with_rescan) so a run another process started
//! is picked up even with no bus to hear about it.
//!
//! ```text
//! nothing at all:    poll → cache says no → 0 queries    (one scan per window)
//! a wait until 3pm:  poll → cache says no → 0 queries    (until 3pm)
//! something due:     poll → the runnable set → claim
//! ```
//!
//! ## Recovery is not a special case
//!
//! A lease that has run out is a crashed node's run, and the next poll picks it
//! up because an expired lease reads exactly like no lease at all. There is no
//! crash detector, no heartbeat table and no recovery pass — which is why
//! recovery is tested by writing a row with a stale lease rather than by killing
//! a process.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sc_agent::run_store::{
    COL_CLAIMED_BY, COL_ID, COL_KIND, COL_LEASE_UNTIL, COL_STATE, COL_WAKE_AT, RUNS_TABLE,
    run_from_row,
};
use sc_agent::{Run, RunKind, RunState, load_run};
use sc_catalog::Catalog;
use sc_db::Row;
use sc_error::Result;
use sc_query::{
    Assignment, BinOp, Expr, InSet, OrderBy, Select, Source, Statement, UnOp, Update, Value,
};

/// How many runs one poll claims when the caller does not say.
pub const DEFAULT_BATCH: usize = 8;

/// How long the queue trusts its cached answer to "is anything due" before
/// asking the database again.
///
/// The floor under how stale a quiet process may be: nothing this process wrote
/// can go unnoticed for any of it — a run written here is noted the moment it is
/// written — but a run another process started is invisible until the window
/// ends. Five minutes turns a poll every five seconds into a query every five
/// minutes on an idle deployment, and a bus that carries the notification will
/// make even that unnecessary.
pub const DEFAULT_RESCAN_SECONDS: i64 = 300;

/// Where the engine gets runnable runs from, and how it says it is working on
/// one.
///
/// Object-safe, because which implementation is behind it is a property of the
/// deployment rather than of the engine: the polling one below is what a
/// single-node server runs, and a bus-backed one is the same three methods with a
/// cheaper [`wake`](WorkQueue::wake).
#[async_trait]
pub trait WorkQueue: Send + Sync {
    /// Claim up to `limit` runs that want the engine at `now`, leasing each until
    /// `until`, and answer the ones actually claimed.
    ///
    /// A run that another node claimed between the read and the write is simply
    /// not in the answer. That is not an error and must not be reported as one:
    /// it is two nodes doing their job.
    async fn claim(
        &self,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Run>>;

    /// Extend the lease on a run this node is working on, so a step that takes
    /// longer than one lease is not picked up by somebody else half way through.
    async fn renew(&self, run: &Run, until: DateTime<Utc>) -> Result<()>;

    /// Wait until it is worth asking again.
    ///
    /// The whole reason the seam exists: a `NOTIFY` or a subscription replaces
    /// this and nothing else changes. Returning early is always allowed — the
    /// caller polls, it does not trust this to be precise.
    async fn wake(&self);

    /// This node's identity, for the operator reading a run that is not moving.
    /// Never authority — the lease is.
    fn node(&self) -> &str;
}

/// The queue that ships: the runs table, polled.
pub struct DatabaseQueue {
    catalog: Arc<Catalog>,
    node: String,
    poll: std::time::Duration,
    /// How long a scan's answer is trusted before the database is asked again.
    rescan: Duration,
}

impl DatabaseQueue {
    /// A queue over `catalog`, claiming as `node`, polling every `poll`.
    pub fn new(catalog: Arc<Catalog>, node: impl Into<String>, poll: std::time::Duration) -> Self {
        DatabaseQueue {
            catalog,
            node: node.into(),
            poll,
            rescan: Duration::seconds(DEFAULT_RESCAN_SECONDS),
        }
    }

    /// How long a scan's answer is trusted before the database is asked again
    /// (see [`DEFAULT_RESCAN_SECONDS`]).
    pub fn with_rescan(mut self, rescan: Duration) -> DatabaseQueue {
        self.rescan = rescan;
        self
    }

    /// The runs that want the engine at `now` and that nothing is working on.
    ///
    /// Ordered by `wake_at`, so the run that has been waiting longest goes first
    /// and a busy engine cannot starve one run behind a stream of newer ones.
    ///
    /// Always asks the database: this is the question itself, and
    /// [`claim`](DatabaseQueue::claim) is where the answer is skipped when the
    /// catalog already knows there is nothing to ask about.
    pub async fn due(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<Run>> {
        let mut select = Select::from(Source::table(RUNS_TABLE)).filter(runnable(now));
        select.order = vec![OrderBy::asc(Expr::col(COL_WAKE_AT))];
        select.limit = Some(limit as u64);
        let rows: Vec<Row> = self
            .catalog
            .primary(crate::INTERNAL_TABLES)
            .query(&Statement::from(select))
            .await?
            .try_collect()
            .await?;
        rows.iter().map(run_from_row).collect()
    }

    /// The earliest instant **any** live run wants the engine, or `None` if none
    /// does — what the cache in the catalog holds, read from the authority.
    ///
    /// Deliberately blind to leases: a run another node holds and has not
    /// finished still has a `wake_at` in the past, and answering with that keeps
    /// the cache's one-sided invariant (never later than the truth) at the cost
    /// of polling while somebody else works. Taking the lease into account would
    /// mean answering later than the truth for the run behind it, and a run that
    /// wakes late by a lease is the bug this cache must not introduce.
    pub async fn next_wakeup(&self) -> Result<Option<DateTime<Utc>>> {
        let mut select = Select::from(Source::table(RUNS_TABLE)).filter(live());
        select.order = vec![OrderBy::asc(Expr::col(COL_WAKE_AT))];
        select.limit = Some(1);
        let rows: Vec<Row> = self
            .catalog
            .primary(crate::INTERNAL_TABLES)
            .query(&Statement::from(select))
            .await?
            .try_collect()
            .await?;
        match rows.first() {
            Some(row) => Ok(run_from_row(row)?.wake_at),
            None => Ok(None),
        }
    }
}

#[async_trait]
impl WorkQueue for DatabaseQueue {
    async fn claim(
        &self,
        now: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<Run>> {
        let wakeups = self.catalog.run_wakeups();
        // The whole point: a poll on a deployment where nothing is due does no
        // database work at all.
        if !wakeups.due_by(now, self.rescan) {
            return Ok(Vec::new());
        }
        // Taken before the query, so a run written while it is in flight is not
        // overwritten by an answer that could not have seen it.
        let scan = wakeups.begin_scan();
        let due = self.due(now, limit).await?;
        if due.is_empty() {
            // Nothing was due after all — the cache was stale, or this is the
            // first poll since boot. One more query says when to bother next,
            // and the polls until then are free.
            wakeups.scanned(scan, now, self.next_wakeup().await?);
            return Ok(Vec::new());
        }
        // Something was due, so the cache stays as it is: these runs are about
        // to move, and each write of them notes where they moved to. What it
        // must not do is go quiet — there may be more due runs than the batch.
        let mut claimed = Vec::new();
        for run in due {
            // The compare-and-set: the same predicate that made the run look
            // claimable, re-checked by the database at the moment of the write.
            // A second node that got there first has put a lease in the future,
            // so this matches nothing and claims nothing.
            let update = Update::new(
                RUNS_TABLE,
                vec![
                    Assignment::new(COL_LEASE_UNTIL, Expr::lit(until)),
                    Assignment::new(COL_CLAIMED_BY, Expr::lit(self.node.clone())),
                ],
            )
            .filter(Expr::col(COL_ID).eq(Expr::lit(run.id.0)).and(unleased(now)));
            self.catalog
                .primary(crate::INTERNAL_TABLES)
                .query(&Statement::from(update))
                .await?
                .try_collect()
                .await?;
            // Who actually holds it. Read back rather than assumed, because the
            // update above is allowed to have matched nothing — and because a
            // backend that cannot report affected rows must still give a
            // truthful answer here.
            //
            // The test is "this node holds it, and the lease is live", **not**
            // "the lease is the instant we wrote": a timestamp goes to the
            // database at nanosecond precision and comes back at the column's,
            // and a claim that depended on those agreeing would claim nothing on
            // Postgres and everything on a backend that happened to round the
            // other way.
            let Some(fresh) = load_run(&self.catalog, run.id).await? else {
                continue;
            };
            if fresh.claimed_by.as_deref() == Some(self.node.as_str())
                && fresh.lease_until.is_some_and(|until| until > now)
            {
                claimed.push(fresh);
            }
        }
        Ok(claimed)
    }

    async fn renew(&self, run: &Run, until: DateTime<Utc>) -> Result<()> {
        let update = Update::new(
            RUNS_TABLE,
            vec![Assignment::new(COL_LEASE_UNTIL, Expr::lit(until))],
        )
        .filter(
            Expr::col(COL_ID)
                .eq(Expr::lit(run.id.0))
                .and(Expr::col(COL_CLAIMED_BY).eq(Expr::lit(self.node.clone()))),
        );
        self.catalog
            .primary(crate::INTERNAL_TABLES)
            .query(&Statement::from(update))
            .await?
            .try_collect()
            .await?;
        Ok(())
    }

    async fn wake(&self) {
        tokio::time::sleep(self.poll).await;
    }

    fn node(&self) -> &str {
        &self.node
    }
}

/// The runnable set (see the module docs): a live workflow run whose time has
/// come and that nothing is working on.
fn runnable(now: DateTime<Utc>) -> Expr {
    live()
        .and(Expr::binary(
            BinOp::Le,
            Expr::col(COL_WAKE_AT),
            Expr::Lit(Value::Timestamp(now)),
        ))
        .and(unleased(now))
}

/// A workflow run that some clock will make runnable: not finished, not failed,
/// not cancelled, and not waiting on a **person**.
///
/// A NULL `wake_at` is that last case — live for a week and runnable at no point
/// in it, which is why "is it live" and "is it runnable now" are two questions,
/// and why the cache is a cache of *this* set's earliest instant.
fn live() -> Expr {
    Expr::col(COL_KIND)
        .eq(Expr::lit(RunKind::Workflow.as_str()))
        .and(Expr::In {
            e: Box::new(Expr::col(COL_STATE)),
            set: InSet::List(vec![
                Expr::lit(RunState::Running.as_str()),
                Expr::lit(RunState::Waiting.as_str()),
            ]),
        })
        .and(Expr::unary(UnOp::IsNotNull, Expr::col(COL_WAKE_AT)))
}

/// Nothing is working on this run: no lease, or one that has run out — which is
/// a crashed node's, and is the whole of the recovery path.
fn unleased(now: DateTime<Utc>) -> Expr {
    Expr::unary(UnOp::IsNull, Expr::col(COL_LEASE_UNTIL)).or(Expr::binary(
        BinOp::Lt,
        Expr::col(COL_LEASE_UNTIL),
        Expr::Lit(Value::Timestamp(now)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // What the queue does needs a database, and is pinned in
    // `tests/engine_runs.rs`. What can be asserted here is the predicate's
    // shape, which is the part a reader has to take on trust otherwise.
    #[test]
    fn the_runnable_set_excludes_a_run_waiting_on_a_person() {
        let sql = format!("{:?}", runnable(Utc::now()));
        // A null `wake_at` is excluded, and a live lease is.
        assert!(sql.contains("IsNotNull"), "{sql}");
        assert!(sql.contains("IsNull"), "{sql}");
        assert!(sql.contains("wake_at"), "{sql}");
        assert!(sql.contains("lease_until"), "{sql}");
    }
}

//! [`StreamSupervisor`]: the thing that keeps subscriptions running (TODO §6,
//! tasks 3.2–3.5).
//!
//! `sc-server`'s `ModelServices` in role and `Scheduler` in shape — one
//! supervising task, started at boot, holding one [`RunningStream`] per enabled
//! row. It is the only thing in this tree that owns a [`Subscription`](crate::Subscription), which
//! is what makes "is this stream running?" a question with one answer.
//!
//! ## Reconnection is the supervisor's, not the provider's
//!
//! A `subscribe` that returns `Err`, and a subscription that *ends*, are the
//! same event here: the flow stopped, and it should be tried again with
//! [capped exponential backoff](backoff_delay), counting attempts. Written once
//! here rather than once per provider, because "retry properly" is the part
//! every provider gets subtly wrong — and the two ways to get it wrong are
//! opposite and both common. A provider that retries in a tight loop turns a
//! broker restart into a DoS on the broker; one that gives up after three
//! attempts turns a twenty-minute outage into a stream that is silently dead
//! until somebody notices next week. A provider's whole job here is to fail
//! honestly and quickly.
//!
//! A **successful** subscribe resets the count, so a broker that accepts a
//! connection and immediately drops it is retried at the floor delay for ever
//! rather than backing off. That is deliberate: the floor is a second, one
//! connection a second is not a load anybody notices, and the alternative —
//! remembering failures across a connection that worked — would make a stream
//! that recovered from a morning's outage slow to recover from the afternoon's.
//!
//! ## The clock is a parameter
//!
//! [`tick`](StreamSupervisor::tick) takes `now` rather than reading it, exactly
//! as `Scheduler::tick` does, and [`start`](StreamSupervisor::start) — the task
//! loop — is the only place `Utc::now()` is read for a retry decision. That is
//! what lets a test drive an hour of backoff in a millisecond, so task 3.3's
//! "the third subscribe succeeds" is an assertion rather than a sleep.
//!
//! ## A reload is a diff, not a restart
//!
//! [`reload`](StreamSupervisor::reload) compares the stored rows against the
//! running set **by id**:
//!
//! | The row | What happens |
//! | --- | --- |
//! | new, or newly enabled | started |
//! | deleted | stopped and forgotten |
//! | disabled | stopped, kept, status `stopped` |
//! | `provider` or `configuration` changed | stopped and started |
//! | anything else changed (name, description, `min_role`) | **connection kept**, row swapped |
//! | unchanged | nothing at all |
//!
//! That last row is the load-bearing one. An admin fixing a typo in a
//! description must not drop a broker session — and a `SIGHUP`, which reloads
//! everything, must not drop all of them.
//!
//! ## One process, one subscription
//!
//! Two servers against one database both subscribe, so a stream trigger fires
//! twice. That is real, it is §6's, and it is not a bug to be discovered:
//! `sc-bus` does not exist, and until it does a flow is process-local. MQTT's
//! own shared subscriptions (`$share/`) are the escape hatch an admin has
//! today.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sc_catalog::Catalog;
use sc_error::Result;

use crate::envelope::{Element, Envelope};
use crate::observer::StreamObserver;
use crate::provider::{Replay, StreamSink};
use crate::registry::StreamRegistry;
use crate::running::{RunningStream, StreamStatus};
use crate::store::list_streams;
use crate::stream::{Stream, StreamId};

/// What a [`StreamConsumer`] did with an element (§7).
///
/// A two-case answer rather than a `Result`, because "I could not take this"
/// is not an error: it is the drop rule working, and the only thing anybody
/// wants from it is a counter. An error here would be a log line per element
/// on a stream that is behaving exactly as designed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Taken. For the trigger bridge this means "a firing was spawned", not
    /// "the trigger finished" — nothing waits for an action.
    Accepted,
    /// Dropped, because this consumer is still busy with an earlier element.
    /// Counted as `dropped_for_triggers` and shown on the Streams list.
    Dropped,
}

/// Where a published element goes after this crate is done with it — §2's
/// second seam.
///
/// Declared here, implemented in `sc-server::streams` (which fires the trigger
/// dispatcher), and in tests by something that appends to a `Vec`. That is what
/// lets the supervisor be tested with no trigger dispatcher, no socket and no
/// broker behind it.
///
/// **It takes an [`Envelope`], not an [`Element`]**, and that is the difference
/// from [`StreamSink`] — which is the *provider's* side of the same journey.
/// By the time a consumer sees it, the element has been stamped with the
/// stream's name and the moment this server saw it, and the result is the exact
/// JSON a trigger's payload, an `element` frame and a generated client all
/// carry (§4). A consumer that was handed the un-stamped element would have to
/// re-derive a wire contract, and the two copies would drift.
pub trait StreamConsumer: Send + Sync {
    /// Take one element. **Returns immediately and cannot fail** — §7's rule,
    /// written into the type: a broker does not wait for a trigger.
    fn consume(&self, envelope: &Envelope) -> Delivery;
}

/// How a supervisor behaves, in the two numbers §7 makes configuration and the
/// three the backoff needs.
///
/// Defaults chosen to be survivable rather than generous: a stream that exceeds
/// them is misbehaving, and the point of the limit is that the process stays up
/// to tell you so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// How many envelopes the per-stream broadcast channel buffers before the
    /// slowest receiver starts being told it lagged (§7).
    pub channel_capacity: usize,
    /// The per-stream element-rate cap, in elements per second. Enforced by
    /// counting and dropping, **never** by pausing the provider. Zero means no
    /// cap.
    pub max_elements_per_second: u64,
    /// How many envelopes the replay ring keeps, so a screen opened on a slow
    /// stream is not blank (§9).
    pub ring_capacity: usize,
    /// The first retry delay after a failure.
    pub retry_initial: Duration,
    /// The cap on the retry delay — "capped at a minute" (§6).
    pub retry_max: Duration,
}

impl Default for StreamConfig {
    fn default() -> StreamConfig {
        StreamConfig {
            channel_capacity: 1_024,
            max_elements_per_second: 1_000,
            ring_capacity: 100,
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
        }
    }
}

/// How long to wait before attempt `attempt + 1`: doubling, capped.
///
/// **No jitter**, unlike `sc-workflow`'s retry, and the difference is the
/// population. A hundred workflow runs that failed on one API outage would
/// retry in the same millisecond, so their delays are spread. A server has a
/// handful of streams, usually against different brokers, and the one case
/// where they share a broker is one where a hundred milliseconds of spread
/// buys nothing a doubling delay has not already bought. Determinism is worth
/// more here: a test asserts the third attempt happens at `t + 1 + 2 + 4`.
pub fn backoff_delay(attempt: u32, config: &StreamConfig) -> Duration {
    let exponent = attempt.saturating_sub(1).min(32);
    match config.retry_initial.checked_mul(1u32 << exponent.min(31)) {
        Some(delay) => delay.min(config.retry_max),
        None => config.retry_max,
    }
}

/// The supervisor: every enabled stream, and the subscriptions keeping them
/// flowing.
///
/// Cheap to share (`Arc` it). The map of running streams is behind a `Mutex`
/// that is **never held across an `await`** — every path that needs to
/// subscribe takes the handle out, drops the lock, and awaits — because
/// `subscribe` opens sockets and a lock held for a TCP timeout would stop the
/// admin API from answering "how is it going?" for exactly as long as the
/// answer is interesting.
pub struct StreamSupervisor {
    registry: RwLock<Arc<StreamRegistry>>,
    consumer: RwLock<Option<Arc<dyn StreamConsumer>>>,
    observer: RwLock<Option<Arc<dyn StreamObserver>>>,
    config: StreamConfig,
    running: Mutex<BTreeMap<StreamId, Arc<RunningStream>>>,
}

/// A dropped supervisor stops what it ran.
///
/// Each running stream holds its sink beside its subscription, and the sink holds
/// the running stream back — and the consumer, which is the trigger bridge with
/// its catalog and dispatcher. So a supervisor dropped without a
/// [`shutdown`](StreamSupervisor::shutdown) left every stream it had started,
/// and everything that consumer reaches, alive for the rest of the process. A
/// server's supervisor lives as long as the server; a test's is dropped at the
/// end of each test.
impl Drop for StreamSupervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl StreamSupervisor {
    /// A supervisor over `registry`, running nothing yet — a server calls
    /// [`reload`](StreamSupervisor::reload) once the catalog is up.
    pub fn new(registry: Arc<StreamRegistry>, config: StreamConfig) -> StreamSupervisor {
        StreamSupervisor {
            registry: RwLock::new(registry),
            consumer: RwLock::new(None),
            observer: RwLock::new(None),
            config,
            running: Mutex::new(BTreeMap::new()),
        }
    }

    /// The numbers this supervisor was built with.
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// The providers a stream can resolve to.
    pub fn registry(&self) -> Arc<StreamRegistry> {
        match self.registry.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Replace the provider set — what installing or removing a **module**
    /// does (Phase 9).
    ///
    /// The caller rebuilds the whole set and swaps it in one act, as
    /// `set_registry` does for actions, and then reloads: a stream whose
    /// provider has just arrived starts, and one whose provider has just gone
    /// becomes `failed` with a sentence naming it. A subscription that is
    /// already running keeps the provider it started with, because it holds an
    /// `Arc` to the code and not a name.
    pub fn set_registry(&self, registry: Arc<StreamRegistry>) {
        if let Ok(mut guard) = self.registry.write() {
            *guard = registry;
        }
    }

    /// Install where a published element goes next — the trigger bridge (§8).
    ///
    /// Set once, at boot, by whoever holds both handles. A supervisor with no
    /// consumer still runs every stream and still broadcasts: the Observe
    /// socket works on a process that has no triggers at all.
    pub fn set_consumer(&self, consumer: Arc<dyn StreamConsumer>) {
        if let Ok(mut guard) = self.consumer.write() {
            *guard = Some(consumer);
        }
    }

    /// Install the observer notified whenever a reload **changed** the set
    /// (task 3.5).
    pub fn set_observer(&self, observer: Arc<dyn StreamObserver>) {
        if let Ok(mut guard) = self.observer.write() {
            *guard = Some(observer);
        }
    }

    /// Every stream this process is holding, in id order.
    pub fn streams(&self) -> Vec<Arc<RunningStream>> {
        self.lock().values().map(Arc::clone).collect()
    }

    /// One stream by id, if this process is holding it.
    pub fn get(&self, id: StreamId) -> Option<Arc<RunningStream>> {
        self.lock().get(&id).map(Arc::clone)
    }

    /// One stream by its current name — what a socket path segment and a
    /// trigger's channel arrive as.
    pub fn by_name(&self, name: &str) -> Option<Arc<RunningStream>> {
        self.lock()
            .values()
            .find(|running| running.name() == name)
            .map(Arc::clone)
    }

    /// Start `row`, replacing whatever was running for the same id.
    ///
    /// Never fails for a reason that is the *stream's*: a provider nothing
    /// implements and a configuration whose element type will not resolve both
    /// leave the stream held, listed and `failed` with the sentence, because a
    /// stream that vanished from the admin UI when its module was uninstalled
    /// is a stream nobody can repair. It fails only when the stream could not
    /// be recorded at all.
    pub async fn start(&self, row: Stream) -> Result<Arc<RunningStream>> {
        self.start_at(row, Utc::now()).await
    }

    /// [`start`](StreamSupervisor::start) with the clock supplied — the form a
    /// test uses, so the first failure's `retry_at` is on the same timeline as
    /// the [`tick`](StreamSupervisor::tick)s that will service it.
    pub async fn start_at(&self, row: Stream, now: DateTime<Utc>) -> Result<Arc<RunningStream>> {
        let registry = self.registry();
        let (provider, element_type, replay, failure) = match registry.require(&row.provider) {
            // `resolve_element_type`, not `element_type`: a module's provider
            // answers the synchronous one from what it last resolved, and this
            // — a start, a restart, a module change — is where that gets
            // filled in. Every compiled-in provider's default is the same pure
            // call.
            Ok(provider) => match (
                provider.resolve_element_type(&row.configuration).await,
                provider.replay(&row.configuration),
            ) {
                (Ok(element_type), Ok(replay)) => {
                    (Some(Arc::clone(provider)), Some(element_type), replay, None)
                }
                (Err(e), _) | (_, Err(e)) => {
                    (None, None, Replay::Ring, Some(sc_error::format_chain(&e)))
                }
            },
            Err(e) => (None, None, Replay::Ring, Some(sc_error::format_chain(&e))),
        };

        let status = match &failure {
            // Attempt 0, not 1: nothing has been *tried*. A reload or a module
            // change is what fixes this, not a retry, and `tick` leaves it
            // alone precisely because retrying a name nothing implements would
            // be a log line a second for ever.
            Some(error) => StreamStatus::Failed {
                error: error.clone(),
                since: now,
                attempt: 0,
            },
            None => StreamStatus::Starting,
        };

        let running = Arc::new(RunningStream::new(
            row,
            provider,
            element_type,
            replay,
            status,
            self.config.channel_capacity,
            self.config.ring_capacity,
            self.config.max_elements_per_second,
        ));
        // Dropping whatever was there stops its subscription, which is why the
        // old handle is taken out under the lock and released after it.
        let previous = self.lock().insert(running.id(), Arc::clone(&running));
        drop(previous);

        if failure.is_none() {
            self.connect(&running, now).await;
        }
        Ok(running)
    }

    /// Stop a stream's subscription, keeping it listed as `stopped`.
    ///
    /// Kept rather than forgotten because a **disabled** stream is still a row
    /// an admin can see, and "what is this stream doing?" wants the answer
    /// "nothing, you switched it off" rather than a blank. Use
    /// [`forget`](StreamSupervisor::forget) for a row that is gone.
    pub fn stop(&self, id: StreamId) -> bool {
        let Some(running) = self.get(id) else {
            return false;
        };
        // The subscription's `Drop` is what actually hangs up on the broker.
        running.set_subscription(None);
        running.set_status(StreamStatus::Stopped, None);
        true
    }

    /// Stop a stream and forget it — what a deleted row gets.
    pub fn forget(&self, id: StreamId) -> bool {
        let removed = self.lock().remove(&id);
        removed.is_some()
    }

    /// Stop everything, for a shutdown. Every `Drop` runs, so every provider
    /// gets its chance to say goodbye.
    pub fn shutdown(&self) {
        let mut guard = self.lock();
        let held: Vec<Arc<RunningStream>> = guard.values().map(Arc::clone).collect();
        guard.clear();
        drop(guard);
        for running in held {
            running.set_subscription(None);
            running.set_status(StreamStatus::Stopped, None);
        }
    }

    /// Bring the running set into line with the stored rows (§6), and tell the
    /// observer if that moved anything.
    ///
    /// Called at boot, after every save and delete, on a `SIGHUP`, and after a
    /// module change. Returns whether anything changed, which is what a caller
    /// wanting to log "nothing to do" reads.
    pub async fn reload(&self, catalog: &Catalog) -> Result<bool> {
        let rows = list_streams(catalog).await?;
        let changed = self.apply(rows).await;
        if changed {
            self.notify(catalog);
        }
        Ok(changed)
    }

    /// The diff itself, over rows the caller has already read — the half a test
    /// can drive without a database.
    ///
    /// **Does not notify the observer.** [`reload`](StreamSupervisor::reload) is
    /// the notifying door, and it is the one every writer goes through; this is
    /// the mechanism underneath it.
    pub async fn apply(&self, rows: Vec<Stream>) -> bool {
        self.apply_at(rows, Utc::now()).await
    }

    /// [`apply`](StreamSupervisor::apply) with the clock supplied.
    pub async fn apply_at(&self, rows: Vec<Stream>, now: DateTime<Utc>) -> bool {
        let wanted: BTreeMap<StreamId, Stream> =
            rows.into_iter().map(|row| (row.id, row)).collect();
        let held: BTreeSet<StreamId> = self.lock().keys().copied().collect();
        let mut changed = false;

        // Gone from the database: stop and forget. Done first, so a stream
        // being replaced frees its broker session before its successor asks
        // for one.
        for id in &held {
            if !wanted.contains_key(id) {
                self.stop(*id);
                self.forget(*id);
                changed = true;
            }
        }

        for (id, row) in &wanted {
            let existing = self.get(*id);
            let Some(existing) = existing else {
                if row.is_enabled() {
                    let _ = self.start_at(row.clone(), now).await;
                    changed = true;
                }
                continue;
            };

            if !row.is_enabled() {
                // Disabled: hang up, keep the row listed, so "what is this
                // stream doing?" answers "nothing, you switched it off"
                // rather than going blank.
                let was_running = existing.status() != StreamStatus::Stopped;
                if existing.row() != *row {
                    existing.set_row(row.clone());
                    changed = true;
                }
                if was_running {
                    self.stop(*id);
                    changed = true;
                }
                continue;
            }

            // Whether the registry now answers for this stream's provider,
            // against whether the running stream is holding one — which is the
            // whole of what a **module change** does to a stream (task 9.3).
            //
            // It went away: a stream still holding the old code would go on
            // polling a module nothing can reach, failing once an interval,
            // while the screen said `running`. Restarting turns that into
            // `failed` with the registry's own sentence, which names the
            // provider and the alternatives — the one case where a row nobody
            // edited loses its connection on purpose.
            //
            // It arrived: a stream that is `failed` because its provider was
            // missing when it started is fixed here and nowhere else, since
            // `tick` deliberately never retries one (waiting does not make an
            // uninstalled module come back; installing it does).
            //
            // Comparing the two rather than testing either alone is what keeps
            // a reload quiet: a stream whose provider is still missing is held
            // exactly as it was, so the observer is not told the set moved on
            // every save.
            let availability_changed =
                self.registry().get(row.provider.trim()).is_some() != existing.provider().is_some();
            let restart = existing.provider_name() != row.provider
                || existing.configuration() != row.configuration
                || existing.status() == StreamStatus::Stopped
                || availability_changed;
            if restart {
                // Stop first: two subscriptions to one broker with one
                // `client_id` is a session the broker closes, and which one it
                // closes is its choice.
                self.stop(*id);
                let _ = self.start_at(row.clone(), now).await;
                changed = true;
            } else if existing.row() != *row {
                // A name, a description or a `min_role` — everything an admin
                // can change without the flow itself being different. The
                // connection is kept (§6).
                existing.set_row(row.clone());
                changed = true;
            }
        }

        changed
    }

    /// One pass of the clock: restart what has stopped and is due (task 3.3).
    ///
    /// Returns the names it reconnected, for a test and for a caller that is
    /// watching. Never fails as a whole — a stream whose provider throws on
    /// `subscribe` is left `failed` with the error and a longer delay, and the
    /// others still get their turn.
    ///
    /// **The clock is a parameter.** A test drives an hour of backoff by
    /// passing instants; [`start`](StreamSupervisor::start_task) is the only
    /// place `Utc::now()` is read.
    pub async fn tick(&self, now: DateTime<Utc>) -> Vec<String> {
        let mut reconnected = Vec::new();
        for running in self.streams() {
            // A stream whose provider could not be resolved is not retried:
            // nothing about waiting makes a missing module arrive. A reload or
            // a module change is what fixes it.
            if running.provider().is_none() {
                continue;
            }
            match running.status() {
                StreamStatus::Running { .. } | StreamStatus::Starting => {
                    if running.has_ended() {
                        // The broker hung up, or a poll loop gave up. The same
                        // event as a `subscribe` that returned an error.
                        self.mark_failed(&running, "the subscription ended", now);
                    }
                }
                StreamStatus::Failed { .. } => {}
                // Switched off. Only a reload starts it again.
                StreamStatus::Stopped => continue,
            }

            if running.retry_at().is_some_and(|retry_at| retry_at <= now) {
                reconnected.push(running.name());
                self.connect(&running, now).await;
            }
        }
        reconnected
    }

    /// Start the task loop: [`tick`](StreamSupervisor::tick) on a short
    /// interval, for ever.
    ///
    /// Returns the handle so a caller can abort it; a dropped handle leaves the
    /// task running, which is what a server wants (it ends with the process).
    /// **The one place the clock is read.** The interval is short because the
    /// thing it decides is "is a retry due", and the retries it is deciding
    /// about start at a second.
    pub fn start_task(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                supervisor.tick(Utc::now()).await;
            }
        })
    }

    /// Subscribe one running stream, recording what happened.
    ///
    /// Takes no lock across the `await`: the handle is already an `Arc` the
    /// caller owns.
    async fn connect(&self, running: &Arc<RunningStream>, now: DateTime<Utc>) {
        let Some(provider) = running.provider().map(Arc::clone) else {
            return;
        };
        // Read the failure history **before** announcing `starting`, which
        // overwrites it. Forgetting this is what turns a capped backoff into a
        // retry every second for ever: every attempt looks like the first one.
        let attempts_so_far = running.attempt();
        let failing_since = running.failing_since(now);
        running.set_status(StreamStatus::Starting, None);
        let configuration = running.configuration();
        let sink: Arc<dyn StreamSink> = Arc::new(Tap {
            running: Arc::clone(running),
            consumer: self.consumer(),
        });

        // The **name**, not the id: it is what a provider puts in a log line
        // and what MQTT derives its stable `client_id` from (§11), and a
        // rename is deliberately allowed to reach a running connection's next
        // reconnect without restarting it now.
        match provider
            .subscribe(&running.name(), &configuration, Arc::clone(&sink))
            .await
        {
            Ok(subscription) => {
                running.set_subscription(Some(subscription));
                // Held beside the subscription for the provider whose elements
                // Saltcorn itself publishes (`internal`): the same sink, so a
                // published element takes the provider's path exactly.
                running.set_sink(Some(sink));
                running.set_status(StreamStatus::Running { since: now }, None);
            }
            Err(e) => {
                running.set_subscription(None);
                self.record_failure(
                    running,
                    &sc_error::format_chain(&e),
                    now,
                    attempts_so_far,
                    failing_since,
                );
            }
        }
    }

    /// Record a failure the current status is still describing — a
    /// subscription that ended under a `Running` status.
    fn mark_failed(&self, running: &Arc<RunningStream>, error: &str, now: DateTime<Utc>) {
        let attempts_so_far = running.attempt();
        let since = running.failing_since(now);
        self.record_failure(running, error, now, attempts_so_far, since);
    }

    /// Record a failure and when to try again, keeping one `since` across a
    /// run of them so "failing since 3am" stays readable after forty retries.
    ///
    /// The history is passed in rather than read, because the caller that most
    /// needs it — [`connect`](StreamSupervisor::connect) — has already replaced
    /// the status with `starting` by the time it knows the attempt failed.
    fn record_failure(
        &self,
        running: &Arc<RunningStream>,
        error: &str,
        now: DateTime<Utc>,
        attempts_so_far: u32,
        since: DateTime<Utc>,
    ) {
        let attempt = attempts_so_far.saturating_add(1);
        running.set_subscription(None);
        let delay = backoff_delay(attempt, &self.config);
        let retry_at = now + chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::zero());
        eprintln!(
            "feldspar: stream `{}` could not subscribe (attempt {attempt}), retrying in {}s: \
             {error}",
            running.name(),
            delay.as_secs()
        );
        running.set_status(
            StreamStatus::Failed {
                error: error.to_owned(),
                since,
                attempt,
            },
            Some(retry_at),
        );
    }

    fn consumer(&self) -> Option<Arc<dyn StreamConsumer>> {
        match self.consumer.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Tell the observer the set moved, reporting — never returning — a failed
    /// reaction (task 3.5).
    ///
    /// The streams *are* started and stopped by the time this runs, so an error
    /// here is a mounted application that keeps its previous projection, not a
    /// save that did not happen; failing the caller would report the opposite
    /// of what occurred.
    fn notify(&self, catalog: &Catalog) {
        let observer = match self.observer.read() {
            Ok(guard) => guard.clone(),
            Err(_) => return,
        };
        if let Some(observer) = observer
            && let Err(e) = observer.streams_changed(catalog)
        {
            eprintln!(
                "feldspar: the stream set changed, but an application could not be \
                 re-projected and keeps its previous mount: {}",
                sc_error::format_chain(&e)
            );
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<StreamId, Arc<RunningStream>>> {
        match self.running.lock() {
            Ok(guard) => guard,
            // A poisoned lock means a panic while the map was being changed.
            // The map is still a valid map — every value is an `Arc` — so the
            // honest answer is the one that is in there.
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// The sink a provider is handed: count, cap, stamp, broadcast, hand on.
///
/// One per connection rather than one per stream, so a restart cannot leave an
/// old provider's task publishing into the new one's channel — the old `Tap`
/// dies with the `Subscription` that held it.
struct Tap {
    running: Arc<RunningStream>,
    consumer: Option<Arc<dyn StreamConsumer>>,
}

impl StreamSink for Tap {
    fn deliver(&self, element: Element) {
        let Some(envelope) = self.running.publish(element, Utc::now()) else {
            // Over the per-second cap: already counted, and deliberately not
            // logged. A stream being capped is producing thousands a second,
            // and a log line each is the second outage.
            return;
        };
        if let Some(consumer) = &self.consumer
            && consumer.consume(&envelope) == Delivery::Dropped
        {
            self.running.count_dropped_for_triggers();
        }
    }

    fn malformed(&self, reason: &str) {
        self.running.count_malformed();
        let _ = reason;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_and_stops_at_the_cap() {
        let config = StreamConfig::default();
        assert_eq!(backoff_delay(1, &config), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, &config), Duration::from_secs(2));
        assert_eq!(backoff_delay(3, &config), Duration::from_secs(4));
        assert_eq!(backoff_delay(7, &config), Duration::from_secs(60));
        // The cap holds for ever, including for an attempt count that would
        // overflow a naive shift.
        assert_eq!(backoff_delay(1_000, &config), Duration::from_secs(60));
        assert_eq!(backoff_delay(u32::MAX, &config), Duration::from_secs(60));
    }

    #[test]
    fn a_zeroth_attempt_waits_the_initial_delay_rather_than_nothing() {
        let config = StreamConfig::default();
        assert_eq!(backoff_delay(0, &config), Duration::from_secs(1));
    }

    #[test]
    fn the_defaults_are_the_survivable_ones_the_design_names() {
        let config = StreamConfig::default();
        assert_eq!(config.channel_capacity, 1_024);
        assert_eq!(config.max_elements_per_second, 1_000);
        assert_eq!(config.ring_capacity, 100);
        assert_eq!(config.retry_max, Duration::from_secs(60));
    }
}

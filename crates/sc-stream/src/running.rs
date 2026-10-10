//! [`RunningStream`] and [`StreamStatus`]: one enabled stream as this process
//! is actually holding it (TODO §6, tasks 3.1 and 3.4).
//!
//! A [`Stream`] is a row. A *running* stream is that row plus the four things
//! only a live process knows: the provider it resolved to, the element type
//! that fell out of its configuration, the [`Subscription`] currently
//! delivering, and how it is going. The supervisor ([`crate::supervisor`]) owns
//! one of these per enabled row and is the only thing that mutates the first
//! three; everything else — the admin's list, the Observe socket, an
//! application's subscription — reads.
//!
//! ## Everything here is in memory, and says so
//!
//! There is no `_fd_errors` table yet, and there is no `_fd_stream_elements`
//! table ever (§4). So a status, a counter and the replay ring all last exactly
//! as long as the process does, and the screens that show them are labelled
//! "since this server started" rather than quietly implying a history. The day
//! the error log lands, the supervisor is one of its callers and the *status*
//! stays here regardless: "is this connection up right now" is not a question a
//! table can answer.
//!
//! ## Publishing: three counters, one fan-out, nobody blocked
//!
//! `publish` is the whole of §7 in one function, and
//! it is **synchronous and infallible** because the caller is a provider's own
//! task:
//!
//! 1. the per-second cap is checked, and an element over it is **dropped and
//!    counted** — never queued, never paused, because pausing means
//!    back-pressure on a broker that will not wait;
//! 2. the element is stamped into an [`Envelope`] with this stream's name and
//!    the moment this server saw it;
//! 3. it goes into the replay ring (the last hundred, for a screen opened on a
//!    slow stream) and onto the broadcast channel, where **no receiver can slow
//!    it down** — one that cannot keep up gets `RecvError::Lagged(n)` and is
//!    told how many it lost.
//!
//! That is the honest outcome. The alternative — an unbounded queue per
//! consumer — is a memory leak with a delay built into it, and the delay is
//! what makes it hard to diagnose.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sc_types::Attrs;
use serde::Serialize;
use serde_json::Value as Json;
use tokio::sync::broadcast;

use crate::element::ElementType;
use crate::envelope::{Element, Envelope};
use crate::provider::{StreamProvider, TopicSpec};
use crate::stream::{Stream, StreamId};
use crate::subscription::Subscription;

/// How a running stream is going (§6).
///
/// In memory only, and deliberately small: four cases an admin can act on, not
/// a log. `Failed` carries the error text rather than an [`Error`](sc_error::Error)
/// because it is read far more often than it is written — by a list, a socket
/// frame and a JSON endpoint — and every one of those wants a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum StreamStatus {
    /// Resolved and about to subscribe. The window is short, but a provider
    /// that opens a TLS connection to an unreachable broker sits here for the
    /// length of a TCP timeout, and an admin watching the list deserves to see
    /// which of "connecting" and "connected" it is.
    Starting,
    /// Subscribed. Elements may or may not be arriving — a quiet broker and a
    /// healthy one look identical from here, which is what `last_element_at` is
    /// for.
    Running {
        /// When this *connection* was established. Reset by every restart, so
        /// a stream that is flapping shows a `since` that keeps moving.
        #[serde(with = "crate::envelope::rfc3339")]
        since: DateTime<Utc>,
    },
    /// Not subscribed, and the supervisor is retrying (§6, task 3.3).
    Failed {
        /// What went wrong, as a sentence.
        error: String,
        /// When it started going wrong — the *first* failure of this run of
        /// them, not the latest attempt, so "failing for three hours" is
        /// readable at a glance.
        #[serde(with = "crate::envelope::rfc3339")]
        since: DateTime<Utc>,
        /// How many times the supervisor has tried since then.
        attempt: u32,
    },
    /// Switched off, or stopped on the way to being restarted. Nothing is
    /// retrying it: only a reload or an explicit start brings it back.
    Stopped,
}

impl StreamStatus {
    /// A one-word label for a list column and a log line.
    pub fn as_str(&self) -> &'static str {
        match self {
            StreamStatus::Starting => "starting",
            StreamStatus::Running { .. } => "running",
            StreamStatus::Failed { .. } => "failed",
            StreamStatus::Stopped => "stopped",
        }
    }

    /// Whether a subscription is believed to be delivering.
    pub fn is_running(&self) -> bool {
        matches!(self, StreamStatus::Running { .. })
    }

    /// The error, for a caller that wants to report it rather than render it.
    pub fn error(&self) -> Option<&str> {
        match self {
            StreamStatus::Failed { error, .. } => Some(error.as_str()),
            _ => None,
        }
    }
}

/// What a running stream has done since this server started (§7).
///
/// A snapshot, taken under no lock anybody holds for long: the live counters
/// are atomics on the [`RunningStream`] itself, because they are written from a
/// provider's task on the delivery path and read from an admin's request, and
/// nothing in between should have to wait for either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counters {
    /// Elements delivered to consumers.
    pub elements: u64,
    /// Elements a consumer — in practice the trigger bridge — was too busy to
    /// take (§7). Not a failure: it is the drop rule working, and it is shown
    /// so that a stream which is dropping is a thing you can *see*.
    pub dropped_for_triggers: u64,
    /// Elements refused by the per-second cap before anybody saw them.
    pub dropped_for_rate: u64,
    /// Payloads the provider could not decode against the declared element
    /// type, counted and never delivered (§11).
    pub malformed: u64,
    /// When the last element arrived, or `None` if none has.
    #[serde(with = "crate::envelope::rfc3339::option")]
    pub last_element_at: Option<DateTime<Utc>>,
}

impl Counters {
    /// The counters as JSON, for a status endpoint and a socket frame.
    pub fn to_json(&self) -> Json {
        serde_json::to_value(self).unwrap_or(Json::Null)
    }
}

/// A fixed-window rate limiter: at most `cap` elements in any one second.
///
/// A window rather than a token bucket, because the thing being defended
/// against is a publisher gone haywire — a thousand a second for an hour — and
/// against that the two behave the same while the window is four fields and no
/// arithmetic anybody has to check. A burst of `cap` at the top of a second is
/// the acceptable inaccuracy.
#[derive(Debug)]
struct RateWindow {
    cap: u64,
    started: Instant,
    seen: u64,
}

impl RateWindow {
    fn new(cap: u64) -> RateWindow {
        RateWindow {
            cap,
            started: Instant::now(),
            seen: 0,
        }
    }

    /// Whether one more element is allowed now. A cap of zero means no cap —
    /// a configuration that meant "deliver nothing" would be a stream that is
    /// off, which is what `enabled` is for.
    fn allow(&mut self) -> bool {
        if self.cap == 0 {
            return true;
        }
        let now = Instant::now();
        if now.duration_since(self.started) >= Duration::from_secs(1) {
            self.started = now;
            self.seen = 0;
        }
        if self.seen >= self.cap {
            return false;
        }
        self.seen += 1;
        true
    }
}

/// A consumer's view of a stream's elements: what it missed, and what comes
/// next (task 3.4).
///
/// The two halves are handed out **together, under one lock**, and that is the
/// whole point of the type. Taking the replay and then subscribing would lose
/// every element published in between; subscribing and then taking the replay
/// would show some of them twice. A screen opened on a slow stream must be
/// neither blank nor double.
pub struct ElementFeed {
    /// The last envelopes this process saw, oldest first — "since this server
    /// started", and the Observe screen says exactly that (§9).
    pub replay: Vec<Envelope>,
    /// Everything from here on. A receiver that falls behind the channel's
    /// capacity gets `RecvError::Lagged(n)`, which is the consumer's cue to
    /// say so rather than to show a gap (§7).
    pub receiver: broadcast::Receiver<Envelope>,
}

/// One enabled stream, as this process is holding it.
///
/// Shared behind an `Arc` — the supervisor's map, the sink handed to the
/// provider, and every reader hold the same one — so all its mutable state is
/// behind its own lock and none of those locks is ever held across an `await`.
pub struct RunningStream {
    id: StreamId,
    /// The row. Behind a lock because a **rename** does not restart the
    /// connection (§6) and the name is stamped into every envelope, so the two
    /// have to be able to move independently.
    row: RwLock<Stream>,
    /// The resolved provider, or `None` when nothing implements the name the
    /// row holds — a module that was uninstalled, a typo in a restored row.
    /// Such a stream is `Failed` with a sentence rather than absent, so it is
    /// still listed, still editable, and editing it is the repair.
    provider: Option<Arc<dyn StreamProvider>>,
    /// The element type, computed once from `provider` + `configuration` and
    /// cached here — the "computed on read and cached in the running stream"
    /// §5 promises instead of a column.
    element_type: Option<ElementType>,
    status: Mutex<StreamStatus>,
    /// When the supervisor should try again, set alongside a `Failed` status.
    retry_at: Mutex<Option<DateTime<Utc>>>,
    subscription: Mutex<Option<Subscription>>,
    elements: AtomicU64,
    dropped_for_triggers: AtomicU64,
    dropped_for_rate: AtomicU64,
    malformed: AtomicU64,
    last_element_at: Mutex<Option<DateTime<Utc>>>,
    /// The fan-out. Held even for a stream that has never connected, so a
    /// consumer can subscribe to a failing stream and start receiving the
    /// moment it comes back.
    sender: broadcast::Sender<Envelope>,
    ring: Mutex<VecDeque<Envelope>>,
    ring_capacity: usize,
    rate: Mutex<RateWindow>,
}

impl RunningStream {
    /// A stream the supervisor is about to start: resolved, not yet
    /// subscribed.
    ///
    /// `provider` and `element_type` are `Option` together — either both
    /// resolved or neither — and a `None` here means the caller has already put
    /// the reason in the status.
    pub(crate) fn new(
        row: Stream,
        provider: Option<Arc<dyn StreamProvider>>,
        element_type: Option<ElementType>,
        status: StreamStatus,
        channel_capacity: usize,
        ring_capacity: usize,
        rate_cap: u64,
    ) -> RunningStream {
        let (sender, _) = broadcast::channel(channel_capacity.max(1));
        RunningStream {
            id: row.id,
            row: RwLock::new(row),
            provider,
            element_type,
            status: Mutex::new(status),
            retry_at: Mutex::new(None),
            subscription: Mutex::new(None),
            elements: AtomicU64::new(0),
            dropped_for_triggers: AtomicU64::new(0),
            dropped_for_rate: AtomicU64::new(0),
            malformed: AtomicU64::new(0),
            last_element_at: Mutex::new(None),
            sender,
            ring: Mutex::new(VecDeque::new()),
            ring_capacity,
            rate: Mutex::new(RateWindow::new(rate_cap)),
        }
    }

    /// Its stable id — the `_fd_streams` primary key, and the supervisor's map
    /// key.
    pub fn id(&self) -> StreamId {
        self.id
    }

    /// Its current name. Read afresh each time, because a rename takes effect
    /// without dropping the broker session.
    pub fn name(&self) -> String {
        self.with_row(|row| row.name.clone())
    }

    /// The registered provider name the row asks for — which is *not* the same
    /// as having resolved it.
    pub fn provider_name(&self) -> String {
        self.with_row(|row| row.provider.clone())
    }

    /// The row as it currently stands.
    pub fn row(&self) -> Stream {
        self.with_row(Clone::clone)
    }

    /// The configuration the provider was, or would be, subscribed with.
    pub fn configuration(&self) -> Attrs {
        self.with_row(|row| row.configuration.clone())
    }

    /// The role floor for observing it through an application, or `None` for
    /// admin-only.
    pub fn min_role(&self) -> Option<u8> {
        self.with_row(|row| row.min_role)
    }

    /// The resolved provider, if the registry had one.
    pub fn provider(&self) -> Option<&Arc<dyn StreamProvider>> {
        self.provider.as_ref()
    }

    /// The cached element type — what the Observe socket's `ready` frame and
    /// the generated client are written from.
    pub fn element_type(&self) -> Option<&ElementType> {
        self.element_type.as_ref()
    }

    /// How this stream's elements are split into topics (TODO.md "Live
    /// updates" §2): the provider's answer for the configuration it runs
    /// with. `Single` for a stream whose provider could not be resolved — it
    /// delivers nothing, so there is nothing to split. A provider that cannot
    /// answer is an error rather than a guess, because the guess that costs
    /// nothing to make (`Single`) is the one that would open a per-user stream
    /// to everyone above its `min_role`.
    pub fn topic_spec(&self) -> sc_error::Result<TopicSpec> {
        match &self.provider {
            Some(provider) => provider.topic_spec(&self.configuration()),
            None => Ok(TopicSpec::Single),
        }
    }

    /// How many envelopes the replay ring keeps — what a consumer that keeps
    /// its own copy of the ring (the live hub's fan-out) sizes it by.
    pub fn ring_capacity(&self) -> usize {
        self.ring_capacity
    }

    /// Replace the row **without** touching the connection.
    ///
    /// The supervisor calls this for a change that is not a `provider` or a
    /// `configuration` change — a description, a `min_role`, a name. An admin
    /// fixing a typo in a description must not drop a broker session (§6).
    pub(crate) fn set_row(&self, row: Stream) {
        if let Ok(mut guard) = self.row.write() {
            *guard = row;
        }
    }

    /// How it is going.
    pub fn status(&self) -> StreamStatus {
        match self.status.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// What it has done since this server started.
    pub fn counters(&self) -> Counters {
        Counters {
            elements: self.elements.load(Ordering::Relaxed),
            dropped_for_triggers: self.dropped_for_triggers.load(Ordering::Relaxed),
            dropped_for_rate: self.dropped_for_rate.load(Ordering::Relaxed),
            malformed: self.malformed.load(Ordering::Relaxed),
            last_element_at: match self.last_element_at.lock() {
                Ok(guard) => *guard,
                Err(poisoned) => *poisoned.into_inner(),
            },
        }
    }

    /// Whether the provider's task has finished of its own accord — a broker
    /// that hung up, a poll loop that gave up. The supervisor's restart cue.
    ///
    /// False for a stream that has no subscription at all: "there is nothing
    /// running" is `Failed` or `Stopped`, and conflating the two would make a
    /// stream that is deliberately off look like one that just died.
    pub fn has_ended(&self) -> bool {
        match self.subscription.lock() {
            Ok(guard) => guard.as_ref().is_some_and(Subscription::has_ended),
            Err(poisoned) => poisoned
                .into_inner()
                .as_ref()
                .is_some_and(Subscription::has_ended),
        }
    }

    /// When the supervisor should next try to connect, if it is waiting to.
    pub fn retry_at(&self) -> Option<DateTime<Utc>> {
        match self.retry_at.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    /// Subscribe to this stream's elements: what was missed, and what is next
    /// (task 3.4).
    ///
    /// The receiver is independent of every other one — a lagging admin browser
    /// costs an application's socket nothing — and holding it keeps nothing
    /// alive but a channel: dropping the running stream stops the flow whether
    /// anybody is listening or not.
    pub fn subscribe_elements(&self) -> ElementFeed {
        // The ring lock is taken *first* and the receiver is made while it is
        // still held, so nothing can be published into the gap between them.
        // `publish` takes the same lock before sending, which is what makes
        // that true.
        let guard = match self.ring.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let replay = guard.iter().cloned().collect();
        let receiver = self.sender.subscribe();
        drop(guard);
        ElementFeed { replay, receiver }
    }

    /// How many consumers are currently listening. For a log line and a test;
    /// nothing branches on it, because a stream with no listeners still runs
    /// (its trigger is a consumer that is always there).
    pub fn listeners(&self) -> usize {
        self.sender.receiver_count()
    }

    /// Take one element from the provider: cap, stamp, ring, broadcast (§7).
    ///
    /// Returns the envelope that went out, or `None` if the per-second cap
    /// refused it. **Synchronous and infallible**, because the caller is the
    /// provider's own task and nothing downstream may make a broker wait.
    pub(crate) fn publish(&self, element: Element, received_at: DateTime<Utc>) -> Option<Envelope> {
        {
            let mut rate = match self.rate.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !rate.allow() {
                self.dropped_for_rate.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }

        let envelope = element.into_envelope(self.name(), received_at);
        self.elements.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = self.last_element_at.lock() {
            *last = Some(received_at);
        }

        {
            let mut ring = match self.ring.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if self.ring_capacity > 0 {
                if ring.len() == self.ring_capacity {
                    ring.pop_front();
                }
                ring.push_back(envelope.clone());
            }
            // Sent under the ring lock, so a `subscribe_elements` racing this
            // sees the element in exactly one of the two halves it hands out.
            // `send` fails only when nobody is listening, which is the normal
            // state of a stream whose only consumer is its trigger bridge.
            let _ = self.sender.send(envelope.clone());
        }

        Some(envelope)
    }

    /// Record a payload the provider refused (§11). Counted, never delivered.
    pub(crate) fn count_malformed(&self) {
        self.malformed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an element a consumer was too busy to take (§7's drop rule).
    pub(crate) fn count_dropped_for_triggers(&self) {
        self.dropped_for_triggers.fetch_add(1, Ordering::Relaxed);
    }

    /// Install the live subscription, dropping — and so stopping — whatever was
    /// there.
    pub(crate) fn set_subscription(&self, subscription: Option<Subscription>) {
        let mut guard = match self.subscription.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = subscription;
    }

    /// Set the status, and what the supervisor should do about it next.
    pub(crate) fn set_status(&self, status: StreamStatus, retry_at: Option<DateTime<Utc>>) {
        if let Ok(mut guard) = self.status.lock() {
            *guard = status;
        }
        if let Ok(mut guard) = self.retry_at.lock() {
            *guard = retry_at;
        }
    }

    /// How many failed attempts this run of failures has made, or 0 when it is
    /// not failing. The supervisor's backoff exponent.
    pub(crate) fn attempt(&self) -> u32 {
        match self.status() {
            StreamStatus::Failed { attempt, .. } => attempt,
            _ => 0,
        }
    }

    /// When this run of failures began, so a restart that keeps failing keeps
    /// one `since` rather than resetting it every minute.
    pub(crate) fn failing_since(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        match self.status() {
            StreamStatus::Failed { since, .. } => since,
            _ => now,
        }
    }

    fn with_row<T>(&self, f: impl FnOnce(&Stream) -> T) -> T {
        match self.row.read() {
            Ok(guard) => f(&guard),
            Err(poisoned) => f(&poisoned.into_inner()),
        }
    }
}

impl std::fmt::Debug for RunningStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningStream")
            .field("id", &self.id)
            .field("name", &self.name())
            .field("provider", &self.provider_name())
            .field("status", &self.status())
            .field("counters", &self.counters())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::Stream;
    use serde_json::json;

    fn running(cap: usize, ring: usize, rate: u64) -> RunningStream {
        RunningStream::new(
            Stream::new("boiler", "scripted"),
            None,
            Some(ElementType::text()),
            StreamStatus::Starting,
            cap,
            ring,
            rate,
        )
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_758_099_600 + seconds, 0).expect("a representable instant")
    }

    #[test]
    fn an_element_is_stamped_counted_and_kept_for_replay() {
        let stream = running(16, 3, 0);
        let envelope = stream
            .publish(Element::new(json!("warm")), at(0))
            .expect("no cap");
        assert_eq!(envelope.stream, "boiler");
        assert_eq!(envelope.value, json!("warm"));
        assert_eq!(envelope.received_at, at(0));

        let counters = stream.counters();
        assert_eq!(counters.elements, 1);
        assert_eq!(counters.last_element_at, Some(at(0)));

        let feed = stream.subscribe_elements();
        assert_eq!(feed.replay.len(), 1);
    }

    #[test]
    fn the_replay_ring_keeps_the_last_n_and_no_more() {
        let stream = running(16, 3, 0);
        for n in 0..5 {
            stream.publish(Element::new(json!(n)), at(n));
        }
        let feed = stream.subscribe_elements();
        assert_eq!(
            feed.replay
                .iter()
                .map(|e| e.value.clone())
                .collect::<Vec<_>>(),
            vec![json!(2), json!(3), json!(4)],
            "oldest first, the last three"
        );
        assert_eq!(stream.counters().elements, 5, "all five were still counted");
    }

    #[test]
    fn a_stream_over_its_cap_drops_and_counts_rather_than_pausing() {
        let stream = running(64, 4, 3);
        let published = (0..10)
            .filter_map(|n| stream.publish(Element::new(json!(n)), at(n)))
            .count();
        assert_eq!(published, 3, "the cap let three through");
        let counters = stream.counters();
        assert_eq!(counters.elements, 3);
        assert_eq!(counters.dropped_for_rate, 7);
    }

    #[test]
    fn a_rename_moves_the_envelopes_without_anything_else_moving() {
        let stream = running(16, 4, 0);
        let mut row = stream.row();
        row.name = "boiler_temp".to_owned();
        stream.set_row(row);
        let envelope = stream
            .publish(Element::new(json!("warm")), at(0))
            .expect("no cap");
        assert_eq!(envelope.stream, "boiler_temp");
    }

    #[test]
    fn a_status_renders_as_a_word_and_carries_its_error() {
        assert_eq!(StreamStatus::Starting.as_str(), "starting");
        assert_eq!(StreamStatus::Stopped.as_str(), "stopped");
        assert!(StreamStatus::Running { since: at(0) }.is_running());
        let failed = StreamStatus::Failed {
            error: "connection refused".to_owned(),
            since: at(0),
            attempt: 3,
        };
        assert_eq!(failed.as_str(), "failed");
        assert!(!failed.is_running());
        assert_eq!(failed.error(), Some("connection refused"));
        assert_eq!(
            serde_json::to_value(&failed).expect("a status serialises"),
            json!({
                "status": "failed",
                "error": "connection refused",
                "since": "2025-09-17T09:00:00.000Z",
                "attempt": 3,
            })
        );
    }

    #[test]
    fn counters_are_json_a_status_endpoint_can_hand_back() {
        let stream = running(16, 4, 0);
        stream.publish(Element::new(json!("one")), at(0));
        stream.count_malformed();
        stream.count_dropped_for_triggers();
        assert_eq!(
            stream.counters().to_json(),
            json!({
                "elements": 1,
                "dropped_for_triggers": 1,
                "dropped_for_rate": 0,
                "malformed": 1,
                "last_element_at": "2025-09-17T09:00:00.000Z",
            })
        );
    }
}

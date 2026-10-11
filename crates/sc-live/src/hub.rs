//! [`LiveHub`]: every live connection in this process, and one fan-out per
//! running stream that somebody is subscribed to (TODO.md "Live updates" §5).
//!
//! ## One receiver per stream, not one per subscription
//!
//! A running stream's elements arrive on a broadcast channel (§14.3). The hub
//! holds **one** receiver on it per stream — the [`Fanout`] task — and routes
//! each element through a [`TopicIndex`] to the subscriptions on its topic,
//! each of which is a slot in some connection's bounded queue. That is the
//! design's "fan-out is indexed by topic": the per-element cost is the
//! subscribers *on that topic*, not every socket in the process. It is also
//! where a `table_changes` stream's per-element access checks run, off the
//! writer's path (see "Writes somebody is subscribed to").
//!
//! ## Nobody blocks the flow, and nobody is lied to
//!
//! A connection's queue is bounded ([`LiveLimits::queue`]). An element that
//! does not fit is **dropped for that subscription and counted**, and the
//! connection is woken to send a `lagged` frame — §14.3's rule, reaching the
//! browser. The fan-out never waits on a socket, and a fan-out that falls
//! behind its broadcast channel tells every subscription how much it lost.
//!
//! ## The replay is the fan-out's, so it is never gapped or doubled
//!
//! The fan-out keeps its own copy of the stream's replay ring, taken together
//! with its receiver under the stream's lock (`subscribe_elements`), and adds
//! each element to it as it routes it. A new subscription is added **by the
//! fan-out task itself**, between two elements, and is handed the ring at that
//! moment: everything before is replay, everything after is delivered, and
//! nothing is both.
//!
//! ## What a new subscription is replayed is the stream's to say
//!
//! [`Replay::Ring`] hands over the ring's elements on the subscriber's topic —
//! the last readings of a sensor. [`Replay::Latest`] hands over the latest
//! element on the topic, if it is still fresh — the current progress of a job,
//! and nobody else's: a `user` subscription is on its own id, so the retained
//! element it is handed is its own. [`Replay::Nothing`] hands over nothing.
//! The fan-out keeps its own copy of the retained elements for the reason it
//! keeps its own ring.
//!
//! ## Connections, so a sign-out reaches them
//!
//! [`LiveHub::connect`] registers a connection with its application, user and
//! session, which is what makes two rules possible: the per-user connection
//! limit, and closing a session's sockets the moment it ends on this node
//! ([`SessionListener`]) rather than at the next re-check.
//!
//! ## Writes that could change who may read a row topic
//!
//! A `row` subscription was authorised by its row's read rule, and a write can
//! change that verdict: the row itself, or a row in a table its ownership
//! formula reads (a share removed). So a connection **watches** the tables its
//! row subscriptions depend on ([`Connection::watch`]), the hub is a
//! [`TableEvents`] listener that observes exactly the watched tables, and a
//! write to one is handed to the watching connections as a [`Touch`] — which
//! they re-check on their own task, off the write's path (§3 rule 6). A table
//! nobody watches is not observed, so the write pays nothing.
//!
//! ## Writes somebody is subscribed to
//!
//! A `table_changes` stream's elements are writes. The hub keeps, per table,
//! the `table_changes` streams some subscription is attached to and which
//! writes each carries ([`ChangeWatch`], held by the subscription), and
//! observes a `(table, op)` only while one of them carries it — so a stream
//! that is defined, even exposed, but has nobody subscribed costs a write
//! nothing. An observed write is handed to each such stream as
//! `{op, key, row, old}` ([`RunningStream::publish_change`]) and nothing more:
//! no filter, no access check, no socket. Those run in the stream's fan-out
//! task, off the write's path: a subscription to a `table_changes` stream is
//! attached with its filter and the means to read rows
//! ([`Connection::attach_changes`]), and for every element the fan-out decides
//! what each subscriber is sent ([`crate::changes`]) — once per distinct user
//! and filter, and only then queues it. A subscription attached without them
//! ([`Connection::attach`]) is sent nothing from such a stream: a raw change is
//! never anybody's.
//!
//! ## Only what committed is announced
//!
//! Both of the above — a touch and a change — are announced **after the
//! write commits**. A write on the pooled path has committed by the time it is
//! emitted, so it is announced at once; one inside a shared transaction (a
//! workflow step's, an import's) is emitted before its commit, so it is handed
//! to the transaction as an [after-commit](sc_catalog::SharedTx::after_commit)
//! callback, and a rollback drops it unannounced.
//!
//! ## Publishing
//!
//! [`LiveHub::publish`] is the one door Saltcorn's own publishers use — the
//! `publish` action, `live.publish` in a code body — through the
//! [`LiveHost`](sc_expr::LiveHost) seam the catalog holds. It finds the
//! running stream by name ([`LiveHub::set_streams`] hands it the supervisor)
//! and publishes through the sink its subscription was handed
//! ([`RunningStream::publish_value`]), so a published element takes exactly
//! the path a provider's would.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};

use chrono::Utc;
use sc_auth::{SessionEnded, SessionListener, User};
use sc_catalog::{Catalog, TableEvents, TableWrite, WriteOp};
use sc_error::{Error, Result};
use sc_expr::{LiveHost, LiveTopics};
use sc_stream::providers::table_changes::{KEY_KEY, KEY_OLD, KEY_OP, KEY_ROW};
use sc_stream::{
    ChangeSettings, Envelope, Payload, Replay, Retained, RunningStream, StreamId, StreamSupervisor,
    TABLE_CHANGES, TopicSpec,
};
use serde_json::Value as Json;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use uuid::Uuid;

use crate::changes::{CallerCell, ChangeAccess, ChangeCounters, ChangeCounts, Subscriber};
use crate::limits::LiveLimits;
use crate::protocol::{ErrorCode, FrameError};
use crate::topic::TopicIndex;

/// A connection's id within this process.
pub type ConnectionId = u64;

/// Who a connection is, as the hub needs to know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// The application it is to — its subdomain, which is what the per-user
    /// limit is counted within.
    pub app: String,
    /// The signed-in user, or `None` for an anonymous connection.
    pub user: Option<Uuid>,
    /// The session token it authenticated with, so ending that session closes
    /// it.
    pub session: Option<String>,
}

/// Why the hub closed a connection: the `error` frame it is sent before the
/// close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReason {
    /// The frame's code.
    pub code: ErrorCode,
    /// The frame's sentence, and the close frame's reason.
    pub message: String,
}

/// One element for one subscription, waiting in a connection's queue.
#[derive(Debug, Clone)]
pub struct Delivery {
    /// Which of the connection's attachments it is for. An attachment rather
    /// than the client's `sub`, because a `sub` can be unsubscribed and reused
    /// while an element for its previous life is still queued.
    pub attachment: u64,
    /// The element.
    pub envelope: Arc<Envelope>,
}

/// A write to a table some of a connection's row subscriptions depend on —
/// what it re-checks them for (§3 rule 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touch {
    /// The table written to.
    pub table: String,
    /// The written row's key, as a topic spells it — before and after, for an
    /// update that changed it. Empty for a table without a one-column key.
    pub keys: Vec<String>,
}

/// Every live connection, and the fan-outs.
pub struct LiveHub {
    limits: LiveLimits,
    next_connection: AtomicU64,
    connections: Mutex<HashMap<ConnectionId, ConnectionEntry>>,
    fanouts: Mutex<Fanouts>,
    /// The running streams, for publishing — set once at boot.
    streams: RwLock<Option<Arc<StreamSupervisor>>>,
    /// Table → connection → how many of its subscriptions watch it.
    watched: Mutex<HashMap<String, HashMap<ConnectionId, usize>>>,
    /// Table → the `table_changes` streams on it that some subscription is
    /// attached to: what [`observes`](TableEvents::observes) reads for a
    /// change, without awaiting anything.
    changes: Mutex<HashMap<String, Vec<ChangeEntry>>>,
    /// Per `table_changes` stream, what its fan-outs have decided since this
    /// server started (L3.9) — kept here rather than on a fan-out, so a
    /// fan-out that ends with its last subscriber does not take them with it.
    change_counters: Mutex<HashMap<StreamId, Arc<ChangeCounters>>>,
    /// Itself, for a task that outlives the call that spawned it.
    me: Weak<LiveHub>,
}

struct ConnectionEntry {
    info: ConnectionInfo,
    close: watch::Sender<Option<CloseReason>>,
    touches: Arc<Touches>,
}

/// The writes waiting for a connection to re-check against, and the bell.
#[derive(Default)]
struct Touches {
    pending: Mutex<Vec<Touch>>,
    bell: Notify,
}

/// One `table_changes` stream somebody is subscribed to.
struct ChangeEntry {
    running: Arc<RunningStream>,
    ops: Vec<WriteOp>,
    /// Subscriptions attached to this running stream.
    subscribers: usize,
}

/// The fan-outs a new subscription joins, and the subscriber count per stream.
/// One lock for both, so a count never disagrees with the map it describes.
#[derive(Default)]
struct Fanouts {
    current: HashMap<StreamId, Arc<Fanout>>,
    subscribers: HashMap<StreamId, usize>,
}

impl LiveHub {
    /// A hub with no connections, enforcing `limits`.
    pub fn new(limits: LiveLimits) -> Arc<LiveHub> {
        Arc::new_cyclic(|me| LiveHub {
            limits,
            next_connection: AtomicU64::new(1),
            connections: Mutex::new(HashMap::new()),
            fanouts: Mutex::new(Fanouts::default()),
            streams: RwLock::new(None),
            watched: Mutex::new(HashMap::new()),
            changes: Mutex::new(HashMap::new()),
            change_counters: Mutex::new(HashMap::new()),
            me: me.clone(),
        })
    }

    fn self_ref(&self) -> Option<Arc<LiveHub>> {
        self.me.upgrade()
    }

    /// Hand the hub the running streams, so it can publish to them. Once, at
    /// boot; a hub with none refuses every publish by name.
    pub fn set_streams(&self, supervisor: Arc<StreamSupervisor>) {
        if let Ok(mut guard) = self.streams.write() {
            *guard = Some(supervisor);
        }
    }

    /// The running stream called `name`, or the refusal a publisher sees.
    fn running(&self, name: &str) -> Result<Arc<RunningStream>> {
        let supervisor = match self.streams.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let supervisor = supervisor.ok_or_else(|| {
            Error::config("this server runs no streams, so there is nothing to publish to")
        })?;
        // The supervisor holds what it has started, so a stream that was
        // disabled before this server started is not here either; the sentence
        // covers both rather than claiming the stream does not exist.
        supervisor.by_name(name).ok_or_else(|| {
            Error::invalid(format!(
                "there is no stream `{name}` running on this server — it does not exist, or it \
                 is disabled"
            ))
        })
    }

    /// Publish `value` on `topic` of the stream called `stream` (TODO.md "Live
    /// updates" L2.2) — what the `publish` action and `live.publish` in a code
    /// body reach, through [`LiveHost`].
    ///
    /// Every refusal is an error the caller sees: an unknown stream, one whose
    /// provider Saltcorn does not publish to, one not running here, a topic of
    /// the wrong kind, a value that does not match the element type (counted as
    /// `malformed`). See [`RunningStream::publish_value`].
    pub fn publish(
        &self,
        stream: &str,
        topic: Option<String>,
        value: Json,
    ) -> Result<Option<String>> {
        self.running(stream)?.publish_value(topic, value)
    }

    /// How the stream called `stream` is split into topics, for a publisher —
    /// refused for a stream Saltcorn does not publish to.
    pub fn topics(&self, stream: &str) -> Result<LiveTopics> {
        let running = self.running(stream)?;
        if !running.provider().is_some_and(|p| p.accepts_publish()) {
            return Err(Error::invalid(format!(
                "stream `{stream}` is not an internal stream, so Saltcorn cannot publish to it"
            )));
        }
        match running.topic_spec()? {
            TopicSpec::Single => Ok(LiveTopics::Single),
            TopicSpec::User => Ok(LiveTopics::User),
            TopicSpec::Row { table } => Ok(LiveTopics::Row { table }),
            TopicSpec::PerElementRow { table } => Err(Error::invalid(format!(
                "stream `{stream}` carries the row changes of `{table}`, which only a write makes"
            ))),
        }
    }

    /// Hand a write to `table` to every connection watching it.
    pub fn touch(&self, touch: Touch) {
        let watchers: Vec<ConnectionId> = match lock(&self.watched).get(&touch.table) {
            Some(by_connection) => by_connection.keys().copied().collect(),
            None => return,
        };
        let connections = lock(&self.connections);
        for id in watchers {
            if let Some(entry) = connections.get(&id) {
                lock(&entry.touches.pending).push(touch.clone());
                entry.touches.bell.notify_one();
            }
        }
    }

    /// Whether any connection watches `table`.
    pub fn watches(&self, table: &str) -> bool {
        lock(&self.watched).contains_key(table)
    }

    /// Whether some subscription is attached to a `table_changes` stream on
    /// `table` that carries `op` — the live half of
    /// [`observes`](TableEvents::observes). One lock and a short scan; no
    /// await.
    pub fn observes_changes(&self, table: &str, op: WriteOp) -> bool {
        lock(&self.changes)
            .get(table)
            .is_some_and(|entries| entries.iter().any(|entry| entry.ops.contains(&op)))
    }

    /// The running `table_changes` streams `op` on `table` is handed to.
    fn change_streams(&self, table: &str, op: WriteOp) -> Vec<Arc<RunningStream>> {
        match lock(&self.changes).get(table) {
            Some(entries) => entries
                .iter()
                .filter(|entry| entry.ops.contains(&op))
                .map(|entry| Arc::clone(&entry.running))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Count a subscription to `running` toward observing its table's writes,
    /// for as long as the returned [`ChangeWatch`] is held. `None` for a
    /// stream that is not a `table_changes` stream (or whose settings no
    /// longer read, which a running stream's cannot).
    pub fn watch_changes(self: &Arc<Self>, running: &Arc<RunningStream>) -> Option<ChangeWatch> {
        if running.provider_name() != TABLE_CHANGES {
            return None;
        }
        let settings = ChangeSettings::from_config(&running.configuration()).ok()?;
        let mut changes = lock(&self.changes);
        let entries = changes.entry(settings.table.clone()).or_default();
        match entries
            .iter_mut()
            .find(|entry| Arc::ptr_eq(&entry.running, running))
        {
            Some(entry) => entry.subscribers += 1,
            None => entries.push(ChangeEntry {
                running: Arc::clone(running),
                ops: settings.ops,
                subscribers: 1,
            }),
        }
        Some(ChangeWatch {
            hub: Arc::clone(self),
            running: Arc::clone(running),
            table: settings.table,
        })
    }

    fn unwatch_changes(&self, table: &str, running: &Arc<RunningStream>) {
        let mut changes = lock(&self.changes);
        let Some(entries) = changes.get_mut(table) else {
            return;
        };
        if let Some(at) = entries
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.running, running))
        {
            entries[at].subscribers = entries[at].subscribers.saturating_sub(1);
            if entries[at].subscribers == 0 {
                entries.remove(at);
            }
        }
        if entries.is_empty() {
            changes.remove(table);
        }
    }

    /// Announce one committed write: a [`Touch`] to the connections watching
    /// its table, and the change to the `table_changes` streams carrying it.
    /// Both are looked up **now**, at the commit, so a subscription made or
    /// dropped while the write's transaction was open is answered as it
    /// stands.
    fn announce(&self, table: &str, op: WriteOp, keys: Vec<String>, change: &Json) {
        if self.watches(table) {
            self.touch(Touch {
                table: table.to_owned(),
                keys,
            });
        }
        for running in self.change_streams(table, op) {
            // A stream stopped between the write and its commit has nobody to
            // tell; its subscriptions are moved by the socket's status pass.
            let _ = running.publish_change(change.clone());
        }
    }

    fn unwatch(&self, connection: ConnectionId, tables: &[String]) {
        let mut watched = lock(&self.watched);
        for table in tables {
            if let Some(by_connection) = watched.get_mut(table) {
                if let Some(count) = by_connection.get_mut(&connection) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        by_connection.remove(&connection);
                    }
                }
                if by_connection.is_empty() {
                    watched.remove(table);
                }
            }
        }
    }

    /// The limits every connection is held to.
    pub fn limits(&self) -> &LiveLimits {
        &self.limits
    }

    /// Register a connection, or refuse it with `too_many_connections` when
    /// its user already holds as many to this application as they may.
    ///
    /// Returns the connection and the receiving end of its queue. Dropping the
    /// [`Connection`] deregisters it.
    pub fn connect(
        self: &Arc<Self>,
        info: ConnectionInfo,
    ) -> Result<(Connection, mpsc::Receiver<Delivery>), FrameError> {
        let mut connections = lock(&self.connections);
        if let Some(user) = info.user {
            let held = connections
                .values()
                .filter(|entry| entry.info.user == Some(user) && entry.info.app == info.app)
                .count();
            if held >= self.limits.max_connections_per_user {
                return Err(FrameError::new(
                    None,
                    ErrorCode::TooManyConnections,
                    format!(
                        "you already have {held} live connections to this application, which is \
                         as many as you may; close a tab and reload"
                    ),
                ));
            }
        }
        let id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        let (close, closed) = watch::channel(None);
        let touches = Arc::new(Touches::default());
        connections.insert(
            id,
            ConnectionEntry {
                info: info.clone(),
                close,
                touches: Arc::clone(&touches),
            },
        );
        drop(connections);
        let (queue, deliveries) = mpsc::channel(self.limits.queue.max(1));
        Ok((
            Connection {
                id,
                hub: Arc::clone(self),
                info,
                closed,
                queue,
                wake: Arc::new(Notify::new()),
                touches,
                next_attachment: AtomicU64::new(1),
                caller: CallerCell::default(),
            },
            deliveries,
        ))
    }

    /// Close every connection a session end covers. Each is sent a
    /// `signed_out` error frame and closed by its own task.
    pub fn close_sessions(&self, ended: &SessionEnded<'_>) {
        let reason = CloseReason {
            code: ErrorCode::SignedOut,
            message: "your session ended, so this connection is closed; sign in again".to_owned(),
        };
        for entry in lock(&self.connections).values() {
            let covered = match ended {
                SessionEnded::Token(token) => entry.info.session.as_deref() == Some(*token),
                SessionEnded::User(user) => entry.info.user == Some(*user),
                SessionEnded::All => entry.info.session.is_some(),
            };
            if covered {
                entry.close.send_replace(Some(reason.clone()));
            }
        }
    }

    /// How many connections are open.
    pub fn connection_count(&self) -> usize {
        lock(&self.connections).len()
    }

    /// How many subscriptions are attached to each stream, by stream id — what
    /// the admin's Streams list shows beside its counters.
    pub fn subscriber_counts(&self) -> HashMap<StreamId, usize> {
        lock(&self.fanouts).subscribers.clone()
    }

    /// What the fan-outs of the `table_changes` stream `stream` have decided
    /// since this server started — `None` for a stream that has had no
    /// subscriber here (and for every other kind of stream).
    pub fn change_counts(&self, stream: StreamId) -> Option<ChangeCounts> {
        lock(&self.change_counters)
            .get(&stream)
            .map(|counters| counters.snapshot())
    }

    /// The counters a fan-out of `stream` adds to.
    fn change_counters_for(&self, stream: StreamId) -> Arc<ChangeCounters> {
        Arc::clone(lock(&self.change_counters).entry(stream).or_default())
    }

    /// How many subscriptions are attached to one stream.
    pub fn subscribers(&self, stream: StreamId) -> usize {
        lock(&self.fanouts)
            .subscribers
            .get(&stream)
            .copied()
            .unwrap_or(0)
    }

    /// The fan-out a new subscription to `running` joins — the current one,
    /// or a new one when there is none or the stream was restarted since —
    /// counted as joined.
    fn join(&self, running: &Arc<RunningStream>) -> Arc<Fanout> {
        let mut fanouts = lock(&self.fanouts);
        let id = running.id();
        let fanout = match fanouts.current.get(&id) {
            Some(fanout) if Arc::ptr_eq(&fanout.running, running) => Arc::clone(fanout),
            // None, or a fan-out for the stream as it was before a restart:
            // that one keeps serving the subscriptions it has until they move
            // (the socket's status pass moves them), and new ones start here.
            _ => {
                let fanout = Fanout::start(Arc::clone(running), self.change_counters_for(id));
                fanouts.current.insert(id, Arc::clone(&fanout));
                fanout
            }
        };
        fanout.members.fetch_add(1, Ordering::Relaxed);
        *fanouts.subscribers.entry(id).or_insert(0) += 1;
        fanout
    }

    /// A subscription left `fanout`. The last one out removes it, which ends
    /// its task once the last handle is dropped.
    fn leave(&self, fanout: &Arc<Fanout>) {
        let mut fanouts = lock(&self.fanouts);
        let id = fanout.running.id();
        if let Some(count) = fanouts.subscribers.get_mut(&id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                fanouts.subscribers.remove(&id);
            }
        }
        if fanout.members.fetch_sub(1, Ordering::Relaxed) == 1
            && fanouts
                .current
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, fanout))
        {
            fanouts.current.remove(&id);
        }
    }
}

impl SessionListener for LiveHub {
    fn session_ended(&self, ended: &SessionEnded<'_>) {
        self.close_sessions(ended);
    }
}

impl LiveHost for LiveHub {
    fn topics(&self, stream: &str) -> Result<LiveTopics> {
        LiveHub::topics(self, stream)
    }

    fn publish(&self, stream: &str, topic: Option<String>, value: Json) -> Result<Option<String>> {
        LiveHub::publish(self, stream, topic, value)
    }
}

/// The write seam (§3 rule 6, L3.3). The hub observes a table while some
/// connection's row subscriptions depend on it, and a `(table, op)` while some
/// subscription is attached to a `table_changes` stream carrying it — and
/// nothing otherwise, so an unwatched write pays one lookup.
///
/// `emit` does no access work: it builds `{op, key, row, old}` and hands it,
/// with the touch, to whatever is listening **once the write has committed**
/// — at once on the pooled path, as an after-commit callback inside a shared
/// transaction. Filters, read checks and sockets are the fan-out's and the
/// connections', on their own tasks, so a write's latency does not grow with
/// the number of subscribers.
#[async_trait::async_trait]
impl TableEvents for LiveHub {
    fn observes(&self, table: &str, op: WriteOp) -> bool {
        self.watches(table) || self.observes_changes(table, op)
    }

    async fn emit(&self, _catalog: &Catalog, write: TableWrite<'_>) -> Result<()> {
        let Some(hub) = self.self_ref() else {
            return Ok(());
        };
        let (old, new) = match write.op {
            WriteOp::Insert => (Json::Null, write.row),
            WriteOp::Update => (write.old_row.unwrap_or(Json::Null), write.row),
            WriteOp::Delete => (write.row, Json::Null),
        };
        let mut keys = BTreeSet::new();
        let mut key = Json::Null;
        if let [pk] = write.table.primary_key.as_slice() {
            // The key as it is now, or — for a delete — as it was.
            for row in [&new, &old] {
                if let Some(topic) = row.get(pk).and_then(topic_of) {
                    keys.insert(topic);
                    if key.is_null() {
                        key = row.get(pk).cloned().unwrap_or(Json::Null);
                    }
                }
            }
        }
        // `{op, key, row, old}`: the row as the write left it (null for a
        // delete) and as it was (absent for an insert) — see the provider.
        let mut change = serde_json::Map::new();
        change.insert(KEY_OP.to_owned(), Json::from(write.op.as_str()));
        change.insert(KEY_KEY.to_owned(), key);
        change.insert(KEY_ROW.to_owned(), new);
        if !old.is_null() {
            change.insert(KEY_OLD.to_owned(), old);
        }
        let change = Json::Object(change);
        let table = write.table.name.clone();
        let op = write.op;
        let keys: Vec<String> = keys.into_iter().collect();
        match &write.tx {
            Some(tx) => tx.after_commit(move || hub.announce(&table, op, keys, &change)),
            None => hub.announce(&table, op, keys, &change),
        }
        Ok(())
    }
}

/// A row key as a topic spells it: a string as it is, a number as its digits.
pub fn topic_of(value: &Json) -> Option<String> {
    match value {
        Json::String(text) => Some(text.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// One registered connection. Dropping it deregisters it.
pub struct Connection {
    id: ConnectionId,
    hub: Arc<LiveHub>,
    info: ConnectionInfo,
    closed: watch::Receiver<Option<CloseReason>>,
    queue: mpsc::Sender<Delivery>,
    wake: Arc<Notify>,
    touches: Arc<Touches>,
    next_attachment: AtomicU64,
    /// Who it is now, as a `table_changes` subscription's verdicts read it.
    caller: CallerCell,
}

/// A connection's watch on some tables, held by the subscription it is for.
/// Dropping it stops the watch — and, with the last watcher of a table gone,
/// stops the hub observing writes to it.
pub struct Watch {
    hub: Arc<LiveHub>,
    connection: ConnectionId,
    tables: Vec<String>,
}

impl Watch {
    /// The tables watched.
    pub fn tables(&self) -> &[String] {
        &self.tables
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.hub.unwatch(self.connection, &self.tables);
    }
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("tables", &self.tables)
            .finish()
    }
}

/// A subscription's share in observing its `table_changes` stream's writes
/// ([`LiveHub::watch_changes`]). Dropping it gives the share back — and, with
/// the last one on a table and write gone, stops the hub observing it.
pub struct ChangeWatch {
    hub: Arc<LiveHub>,
    running: Arc<RunningStream>,
    table: String,
}

impl ChangeWatch {
    /// The table whose writes are observed.
    pub fn table(&self) -> &str {
        &self.table
    }
}

impl Drop for ChangeWatch {
    fn drop(&mut self) {
        self.hub.unwatch_changes(&self.table, &self.running);
    }
}

impl std::fmt::Debug for ChangeWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeWatch")
            .field("table", &self.table)
            .field("stream", &self.running.name())
            .finish()
    }
}

impl Connection {
    /// Its id.
    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// Who it is.
    pub fn info(&self) -> &ConnectionInfo {
        &self.info
    }

    /// Set who the connection's caller is — at the upgrade, and again each
    /// time the socket re-reads its session — so every per-element verdict a
    /// `table_changes` subscription is given after it uses the role and the
    /// fields the user has now (§3 rule 6).
    pub fn set_user(&self, user: Option<User>) {
        self.caller.set(user);
    }

    /// What resolves when the hub closes this connection — a session end.
    /// Owned separately from the connection, so a socket's loop can wait on it
    /// beside everything else the connection is doing.
    pub fn close_signal(&self) -> CloseSignal {
        CloseSignal(self.closed.clone())
    }

    /// Resolves when an element was dropped for one of this connection's
    /// subscriptions, so the socket can say so with `lagged` without waiting
    /// for the next element to get through.
    pub async fn lagged(&self) {
        self.wake.notified().await;
    }

    /// Watch `tables` for writes, for as long as the returned [`Watch`] is
    /// held — what a `row` subscription holds for its row's table and every
    /// table its read rule reads.
    pub fn watch(&self, tables: impl IntoIterator<Item = String>) -> Watch {
        let tables: Vec<String> = tables
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut watched = lock(&self.hub.watched);
        for table in &tables {
            *watched
                .entry(table.clone())
                .or_default()
                .entry(self.id)
                .or_insert(0) += 1;
        }
        drop(watched);
        Watch {
            hub: Arc::clone(&self.hub),
            connection: self.id,
            tables,
        }
    }

    /// Resolves when a watched table was written to; [`take_touches`] says
    /// what.
    ///
    /// [`take_touches`]: Connection::take_touches
    pub async fn touched(&self) {
        self.touches.bell.notified().await;
    }

    /// The writes waiting to be re-checked against, oldest first, taken.
    pub fn take_touches(&self) -> Vec<Touch> {
        std::mem::take(&mut *lock(&self.touches.pending))
    }

    /// Attach a subscription to `running`, on `topic`: join (or start) the
    /// stream's fan-out and get the replay that precedes the first delivery.
    ///
    /// A subscription to a `table_changes` stream also starts the hub
    /// observing that stream's writes, **before** it joins the fan-out, so
    /// every write committed after `ready` reaches it.
    pub async fn attach(
        &self,
        running: &Arc<RunningStream>,
        topic: Option<String>,
    ) -> (Attached, Vec<Arc<Envelope>>) {
        self.attach_with(running, topic, None).await
    }

    /// Attach a subscription to a `table_changes` stream: like
    /// [`attach`](Connection::attach) with no topic, carrying what its
    /// fan-out decides each element by — the subscription's filter, where rows
    /// are read, and this connection's caller as it is when each element
    /// arrives (L3.6–L3.8).
    pub async fn attach_changes(
        &self,
        running: &Arc<RunningStream>,
        access: ChangeAccess,
    ) -> (Attached, Vec<Arc<Envelope>>) {
        let change = SinkChange {
            access,
            caller: self.caller.clone(),
        };
        self.attach_with(running, None, Some(Arc::new(change)))
            .await
    }

    async fn attach_with(
        &self,
        running: &Arc<RunningStream>,
        topic: Option<String>,
        change: Option<Arc<SinkChange>>,
    ) -> (Attached, Vec<Arc<Envelope>>) {
        let changes = self.hub.watch_changes(running);
        let attachment = self.next_attachment.fetch_add(1, Ordering::Relaxed);
        let lagged = Arc::new(AtomicU64::new(0));
        let fanout = self.hub.join(running);
        let (reply, replay) = oneshot::channel();
        let _ = fanout.commands.send(Command::Add {
            key: (self.id, attachment),
            topic,
            sink: Sink {
                attachment,
                queue: self.queue.clone(),
                lagged: Arc::clone(&lagged),
                wake: Arc::clone(&self.wake),
                change,
            },
            reply,
        });
        let attached = Attached {
            attachment,
            connection: self.id,
            hub: Arc::clone(&self.hub),
            fanout,
            lagged,
            _changes: changes,
        };
        // The fan-out answers between two elements; a fan-out that ended
        // instead (its stream's channel closed) has nothing to replay.
        let replay = replay.await.unwrap_or_default();
        (attached, replay)
    }
}

/// The hub's "close this connection", and why.
pub struct CloseSignal(watch::Receiver<Option<CloseReason>>);

impl CloseSignal {
    /// Resolves when the hub closes the connection, with the reason. Never
    /// resolves otherwise, so it can sit in a `select!`.
    pub async fn closed(&mut self) -> CloseReason {
        loop {
            if let Some(reason) = self.0.borrow_and_update().clone() {
                return reason;
            }
            if self.0.changed().await.is_err() {
                // The hub dropped the sender, which it does only when the
                // connection is deregistered — by the connection's own drop.
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        lock(&self.hub.connections).remove(&self.id);
    }
}

/// One subscription attached to a stream's fan-out. Dropping it detaches it.
pub struct Attached {
    attachment: u64,
    connection: ConnectionId,
    hub: Arc<LiveHub>,
    fanout: Arc<Fanout>,
    lagged: Arc<AtomicU64>,
    /// Its share in observing the stream's writes, for a `table_changes`
    /// stream; dropped with it.
    _changes: Option<ChangeWatch>,
}

impl Attached {
    /// The attachment id its deliveries carry.
    pub fn attachment(&self) -> u64 {
        self.attachment
    }

    /// The running stream it is attached to — compared against the
    /// supervisor's current one to notice a restart.
    pub fn running(&self) -> &Arc<RunningStream> {
        &self.fanout.running
    }

    /// How many elements were dropped for it since this was last asked, and
    /// reset that to zero.
    pub fn take_lagged(&self) -> u64 {
        self.lagged.swap(0, Ordering::Relaxed)
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.fanout.commands.send(Command::Remove {
            key: (self.connection, self.attachment),
        });
        self.hub.leave(&self.fanout);
    }
}

impl std::fmt::Debug for Attached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attached")
            .field("attachment", &self.attachment)
            .field("stream", &self.fanout.running.name())
            .finish()
    }
}

/// A subscription in a fan-out's index: (connection, attachment).
type Key = (ConnectionId, u64);

/// The fan-out task's mailbox.
enum Command {
    /// Add a subscription, and answer with the replay for its topic.
    Add {
        key: Key,
        topic: Option<String>,
        sink: Sink,
        reply: oneshot::Sender<Vec<Arc<Envelope>>>,
    },
    /// Take a subscription out.
    Remove { key: Key },
}

/// Where one subscription's elements go: its connection's queue.
struct Sink {
    attachment: u64,
    queue: mpsc::Sender<Delivery>,
    lagged: Arc<AtomicU64>,
    wake: Arc<Notify>,
    /// For a subscription to a `table_changes` stream, what each element is
    /// decided by; `None` for every other subscription.
    change: Option<Arc<SinkChange>>,
}

/// A `table_changes` subscription's part of its sink.
struct SinkChange {
    access: ChangeAccess,
    caller: CallerCell,
}

impl Sink {
    /// Queue an element, or count it as dropped. Never waits.
    fn offer(&self, envelope: &Arc<Envelope>) {
        match self.queue.try_send(Delivery {
            attachment: self.attachment,
            envelope: Arc::clone(envelope),
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => self.lag(1),
            // The connection is going; its `Remove` is on the way.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Count `dropped` elements lost, and wake the connection to say so.
    fn lag(&self, dropped: u64) {
        self.lagged.fetch_add(dropped, Ordering::Relaxed);
        self.wake.notify_one();
    }
}

/// One running stream's fan-out: a task holding the one receiver.
struct Fanout {
    running: Arc<RunningStream>,
    commands: mpsc::UnboundedSender<Command>,
    /// Subscriptions attached, changed only under the hub's fan-out lock.
    members: AtomicU64,
}

impl Fanout {
    fn start(running: Arc<RunningStream>, counters: Arc<ChangeCounters>) -> Arc<Fanout> {
        let (commands, inbox) = mpsc::unbounded_channel();
        tokio::spawn(route(Arc::clone(&running), inbox, counters));
        Arc::new(Fanout {
            running,
            commands,
            members: AtomicU64::new(0),
        })
    }
}

/// The fan-out task: route each element to its topic's subscriptions until
/// every handle on the mailbox is gone.
async fn route(
    running: Arc<RunningStream>,
    mut inbox: mpsc::UnboundedReceiver<Command>,
    counters: Arc<ChangeCounters>,
) {
    // A stream of row changes is decided per element and per subscriber; the
    // table and the payload are read once, as the running stream's
    // configuration cannot change under it (a change restarts it).
    let per_element: Option<(String, Payload)> = match running.topic_spec() {
        Ok(TopicSpec::PerElementRow { table }) => {
            let payload = ChangeSettings::from_config(&running.configuration())
                .map(|settings| settings.payload)
                .unwrap_or_default();
            Some((table, payload))
        }
        _ => None,
    };
    let feed = running.subscribe_elements();
    let policy = running.replay().clone();
    // Only the copy the policy reads is kept: a retaining stream's subscriber
    // is never handed the ring, and a ring stream has nothing retained.
    let capacity = match policy {
        Replay::Ring => running.ring_capacity(),
        Replay::Nothing | Replay::Latest { .. } => 0,
    };
    let mut ring: VecDeque<Arc<Envelope>> = match policy {
        Replay::Ring => feed.replay.into_iter().map(Arc::new).collect(),
        Replay::Nothing | Replay::Latest { .. } => VecDeque::new(),
    };
    let mut retained = match &policy {
        Replay::Latest { ttl, max_topics } => {
            let mut retained = Retained::new(*ttl, *max_topics);
            let now = Utc::now();
            for envelope in feed.retained {
                retained.keep(envelope, now);
            }
            Some(retained)
        }
        Replay::Ring | Replay::Nothing => None,
    };
    let mut elements = feed.receiver;
    let mut index: TopicIndex<Key, Sink> = TopicIndex::new();
    let mut flowing = true;

    loop {
        tokio::select! {
            // Commands first: a subscription removed is removed before the
            // next element is routed to it.
            biased;
            command = inbox.recv() => match command {
                None => break,
                Some(Command::Add { key, topic, sink, reply }) => {
                    let replay = match &retained {
                        Some(retained) => retained
                            .get(topic.as_deref(), Utc::now())
                            .map(|envelope| Arc::new(envelope.clone()))
                            .into_iter()
                            .collect(),
                        None => ring
                            .iter()
                            .filter(|envelope| envelope.topic == topic)
                            .cloned()
                            .collect(),
                    };
                    index.insert(topic, key, sink);
                    let _ = reply.send(replay);
                }
                Some(Command::Remove { key }) => {
                    index.remove(&key);
                }
            },
            received = elements.recv(), if flowing => match received {
                Ok(envelope) => {
                    if let Some(retained) = retained.as_mut() {
                        retained.keep(envelope.clone(), Utc::now());
                    }
                    let envelope = Arc::new(envelope);
                    if let Some((table, payload)) = &per_element {
                        route_change(table, *payload, &envelope, &index, &counters).await;
                        continue;
                    }
                    if capacity > 0 {
                        if ring.len() >= capacity {
                            ring.pop_front();
                        }
                        ring.push_back(Arc::clone(&envelope));
                    }
                    for (_, sink) in index.on(envelope.topic.as_deref()) {
                        sink.offer(&envelope);
                    }
                }
                // This fan-out fell behind the stream's channel: every
                // subscription lost those elements, whichever topic they were
                // on, so every one is told.
                Err(RecvError::Lagged(dropped)) => {
                    for (_, sink) in index.all() {
                        sink.lag(dropped);
                    }
                }
                // Unreachable while this task holds the stream (the sender
                // lives on it); handled so it cannot become a busy loop.
                Err(RecvError::Closed) => flowing = false,
            },
        }
    }
}

/// One change from a `table_changes` stream: decide what each of its
/// subscriptions is sent ([`crate::changes::decide`]) and queue it. A
/// subscription attached without a filter and a way to read rows is sent
/// nothing.
async fn route_change(
    table: &str,
    payload: Payload,
    envelope: &Arc<Envelope>,
    index: &TopicIndex<Key, Sink>,
    counters: &ChangeCounters,
) {
    let sinks: Vec<(&Sink, &SinkChange)> = index
        .all()
        .filter_map(|(_, sink)| sink.change.as_deref().map(|change| (sink, change)))
        .collect();
    if sinks.is_empty() {
        return;
    }
    let subscribers: Vec<Subscriber<'_>> = sinks
        .iter()
        .map(|(_, change)| Subscriber {
            access: &change.access,
            caller: &change.caller,
        })
        .collect();
    let decided = crate::changes::decide(table, payload, envelope, &subscribers, counters).await;
    for ((sink, _), sent) in sinks.iter().zip(decided) {
        if let Some(sent) = sent {
            sink.offer(&sent);
        }
    }
}

/// A lock, recovered from poisoning: every map here is valid after a panic
/// elsewhere, because each update is one insert or remove.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

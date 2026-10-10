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
//! where L3's per-element access checks will run, off the publisher's path.
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
//! ## Connections, so a sign-out reaches them
//!
//! [`LiveHub::connect`] registers a connection with its application, user and
//! session, which is what makes two rules possible: the per-user connection
//! limit, and closing a session's sockets the moment it ends on this node
//! ([`SessionListener`]) rather than at the next re-check.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use sc_auth::{SessionEnded, SessionListener};
use sc_stream::{Envelope, RunningStream, StreamId};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use uuid::Uuid;

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

/// Every live connection, and the fan-outs.
pub struct LiveHub {
    limits: LiveLimits,
    next_connection: AtomicU64,
    connections: Mutex<HashMap<ConnectionId, ConnectionEntry>>,
    fanouts: Mutex<Fanouts>,
}

struct ConnectionEntry {
    info: ConnectionInfo,
    close: watch::Sender<Option<CloseReason>>,
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
        Arc::new(LiveHub {
            limits,
            next_connection: AtomicU64::new(1),
            connections: Mutex::new(HashMap::new()),
            fanouts: Mutex::new(Fanouts::default()),
        })
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
        connections.insert(
            id,
            ConnectionEntry {
                info: info.clone(),
                close,
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
                next_attachment: AtomicU64::new(1),
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
                let fanout = Fanout::start(Arc::clone(running));
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

/// One registered connection. Dropping it deregisters it.
pub struct Connection {
    id: ConnectionId,
    hub: Arc<LiveHub>,
    info: ConnectionInfo,
    closed: watch::Receiver<Option<CloseReason>>,
    queue: mpsc::Sender<Delivery>,
    wake: Arc<Notify>,
    next_attachment: AtomicU64,
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

    /// Attach a subscription to `running`, on `topic`: join (or start) the
    /// stream's fan-out and get the replay that precedes the first delivery.
    pub async fn attach(
        &self,
        running: &Arc<RunningStream>,
        topic: Option<String>,
    ) -> (Attached, Vec<Arc<Envelope>>) {
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
            },
            reply,
        });
        let attached = Attached {
            attachment,
            connection: self.id,
            hub: Arc::clone(&self.hub),
            fanout,
            lagged,
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
    fn start(running: Arc<RunningStream>) -> Arc<Fanout> {
        let (commands, inbox) = mpsc::unbounded_channel();
        tokio::spawn(route(Arc::clone(&running), inbox));
        Arc::new(Fanout {
            running,
            commands,
            members: AtomicU64::new(0),
        })
    }
}

/// The fan-out task: route each element to its topic's subscriptions until
/// every handle on the mailbox is gone.
async fn route(running: Arc<RunningStream>, mut inbox: mpsc::UnboundedReceiver<Command>) {
    let feed = running.subscribe_elements();
    let capacity = running.ring_capacity();
    let mut ring: VecDeque<Arc<Envelope>> = feed.replay.into_iter().map(Arc::new).collect();
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
                    let replay = ring
                        .iter()
                        .filter(|envelope| envelope.topic == topic)
                        .cloned()
                        .collect();
                    index.insert(topic, key, sink);
                    let _ = reply.send(replay);
                }
                Some(Command::Remove { key }) => {
                    index.remove(&key);
                }
            },
            received = elements.recv(), if flowing => match received {
                Ok(envelope) => {
                    let envelope = Arc::new(envelope);
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

/// A lock, recovered from poisoning: every map here is valid after a panic
/// elsewhere, because each update is one insert or remove.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

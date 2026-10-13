//! An application's live socket: `GET {mount}/live` (TODO.md "Live updates",
//! milestone L1).
//!
//! **One socket per page**, carrying any number of subscriptions to the streams
//! the application exposes. The protocol, the subscription states, the access
//! rules and the fan-out are `sc-live`'s; this module is where they meet a real
//! WebSocket, the session store, the catalog and the stream supervisor.
//!
//! ## Before the upgrade, a status; after it, a frame
//!
//! The router decides three things before upgrading, because a browser cannot
//! read the body of a failed handshake and these are the refusals it must be
//! able to tell from a network fault: is this an upgrade at all (400), is the
//! `Origin` this application's own (403, [`crate::security::upgrade_origin_allowed`]),
//! and who the caller is — by the **same function** the application's REST API
//! authenticates with, so the socket can never be more permissive than a read.
//! Anonymous is allowed and holds the public role; each subscription decides.
//!
//! Everything after is a frame: `unavailable` for a stream that is unknown,
//! unexposed or above the caller's role (one answer for all three), `invalid`
//! for a frame the server cannot act on, and the limits' own codes.
//!
//! ## Re-checked while it is open
//!
//! A subscription is a standing read, so one checked only at subscribe time
//! would leak after a sign-out, a role change or an edit to the stream. Every
//! [`recheck_interval`](sc_live::LiveLimits::recheck_interval) the connection
//! re-reads its session and user, re-resolves its application (a remount may
//! have dropped a stream) and asks [`may_subscribe`] again for every
//! subscription: one that no longer passes gets `revoked` and is dropped, and a
//! session that is gone closes the socket with `signed_out`. A sign-out on this
//! node does not wait for the clock — the hub listens to the session store.
//!
//! ## Topics (L2)
//!
//! A `user` stream's subscriber names no topic and is put on their own user
//! id; an anonymous caller is told the stream is not there. A `row` stream's
//! subscriber names a row's key, and is let in only if **the application's
//! REST read of that row, as them,** returns it ([`sc_api::row_readable_as`]) —
//! a missing row and a forbidden one are the same `unavailable`. A row
//! subscription then watches its row's table and every table the table's
//! ownership formula reads ([`sc_live::read_rule_tables`]); a write to one
//! wakes this connection, which re-asks the same read for the subscriptions it
//! could affect and revokes those that now fail, without waiting for the clock.
//!
//! ## Row changes (L3)
//!
//! A `table_changes` stream is subscribed without a topic and with an optional
//! `filter` in the REST read's vocabulary. Its table must be in the
//! application's subset (otherwise it is `unavailable`, like any absent
//! stream); the filter is lowered against that table by the REST read's own
//! walk ([`sc_live::RowFilter`]) and an unknown field is `invalid`. The
//! subscription is attached with the filter, the catalog and the evaluator
//! ([`sc_live::ChangeAccess`]), and the stream's fan-out decides for every
//! element what this connection's caller is sent — `insert`, `update`,
//! `enter`, `leave`, `delete` or nothing — reading the caller from the
//! connection each time, so a re-read session reaches the next element.
//!
//! ## Publishing and presence (L4)
//!
//! A page publishes on a subscription it holds. Whether it may is decided when
//! the subscription is attached, by [`sc_live::may_publish`] — the stream's
//! `client_publish`, and on a `row` topic the caller's **update** access to the
//! row ([`sc_api::row_updatable_as`]) — sent as `can_publish` in `ready`, and
//! decided again whenever the subscription's read access is (a touched row, the
//! clock). A refused publish is `unavailable`, worded like any absence. The
//! element is the hub's to stamp ([`Connection::publish`]): `source` is
//! `{user, connection}` from this connection, never from the frame. Publishes
//! and presence updates are each rate-limited per connection and answered
//! `rate_limited` past the limit, the connection staying up.
//!
//! A subscription to a stream with `presence` on takes a seat in its topic's
//! room once attached ([`Connection::join_presence`]), with the `users`
//! columns the stream shows read at that moment, and is sent the member list
//! as a `presence` frame; the hub's changes to the room arrive in the
//! connection's inbox and leave as `presence_diff`. A state update is applied
//! at once when the connection has been quiet, and the ones that follow within
//! [`presence_coalesce`](sc_live::LiveLimits::presence_coalesce) are held and
//! applied as the latest one when it ends. Unsubscribing, a revoke and the
//! socket closing give the seat back.
//!
//! ## Following the stream
//!
//! A subscription to a stream that is not running here (disabled, or failing
//! to start) gets `ready` with a `stopped` status and waits; when the stream
//! starts, or restarts because its configuration changed, the status pass
//! attaches it and sends `ready` again, so the page follows the flow without
//! reconnecting.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use sc_api::StreamOpening;
use sc_auth::{SessionStore, User};
use sc_catalog::{Catalog, TableId};
use sc_live::{
    Attached, Caller, ChangeAccess, ClientFrame, CloseReason, Connection, ConnectionInfo, Delivery,
    DocNews, DocRefusal, DocSeat, DocTarget, ErrorCode, FrameError, LikeRules, LiveHub, LiveLimits,
    Opened, PresenceEvent, PublishFacts, RateLimit, Refusal, RoomKey, RowFact, RowFilter, Seat,
    ServerFrame, StoredStream, StreamFacts, Subscription, SubscriptionSet, Touch, Watch, may_open,
    may_publish, may_subscribe, read_rule_tables,
};
use sc_stream::providers::internal;
use sc_stream::{DOCUMENT, RunningStream, Stream, StreamSupervisor, TopicSpec};
use serde_json::{Value as Json, json};
use tokio::time::{Instant, Interval, MissedTickBehavior};

use crate::apps::{AppMounts, MountedApp};
use crate::observe::{element_type_json_in, fit_close_reason, status_parts};

/// The live machinery a server holds: the hub every application's connections
/// register with. Rides on [`AppMounts`] beside the stream services, for their
/// reason — the router and the admin handlers both already hold that handle.
#[derive(Clone)]
pub struct LiveServices {
    hub: Arc<LiveHub>,
}

impl Default for LiveServices {
    fn default() -> LiveServices {
        LiveServices::new(LiveLimits::default())
    }
}

impl LiveServices {
    /// A hub enforcing `limits`.
    pub fn new(limits: LiveLimits) -> LiveServices {
        LiveServices {
            hub: LiveHub::new(limits),
        }
    }

    /// The hub.
    pub fn hub(&self) -> &Arc<LiveHub> {
        &self.hub
    }
}

/// Who opened the socket, as the application's REST API would have seen them.
pub(crate) struct LiveCaller {
    /// The signed-in user, or `None` for an anonymous caller.
    pub user: Option<User>,
    /// The session the user was resolved from — `None` whenever `user` is, so
    /// an anonymous caller who happened to carry a dead cookie is not later
    /// "signed out" of a session they never had.
    pub session: Option<String>,
}

/// The application a socket is for, followed through remounts.
pub(crate) enum AppHandle {
    /// The live mount on a subdomain: re-resolved on every re-check, so a
    /// remount that stops exposing a stream reaches the open sockets.
    Mounted {
        /// The registry.
        apps: Arc<AppMounts>,
        /// The subdomain it is mounted on.
        subdomain: String,
    },
    /// A run's preview: fixed for the socket's life, because a preview is
    /// replaced by its label rather than its subdomain.
    Fixed(Arc<MountedApp>),
}

impl AppHandle {
    fn current(&self) -> Option<Arc<MountedApp>> {
        match self {
            AppHandle::Mounted { apps, subdomain } => apps.get(subdomain),
            AppHandle::Fixed(app) => Some(Arc::clone(app)),
        }
    }
}

/// What one connection needs from the server.
pub(crate) struct LiveEnv {
    pub hub: Arc<LiveHub>,
    pub sessions: Arc<SessionStore>,
    pub catalog: Arc<Catalog>,
    pub supervisor: Option<Arc<StreamSupervisor>>,
    /// The formula evaluator a row topic's read uses when the table's
    /// ownership formula does not translate — the REST read's own.
    pub evaluator: Option<Arc<dyn sc_expr::JsEvaluator>>,
    pub app: AppHandle,
    /// The application's subdomain, which the per-user connection limit is
    /// counted within.
    pub subdomain: String,
    /// The caller's address, as the REST API resolves it — what a stream
    /// query's per-IP rate limit counts (TODO F7).
    pub client_ip: Option<std::net::IpAddr>,
}

/// Upgrade to a live socket. Everything the router had to refuse with a status
/// has been refused; from here on, refusals are frames.
pub(crate) fn live_upgrade(
    ws: WebSocketUpgrade,
    env: LiveEnv,
    caller: LiveCaller,
) -> axum::response::Response {
    // The transport's own cap sits above the frame limit, so a frame over the
    // limit is answered `too_large` rather than hung up on (`sc-live`'s limits).
    let max = env.hub.limits().transport_max_bytes();
    ws.max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| serve(socket, env, caller))
}

/// One thing that happened to a connection, taken out of the `select!` so it is
/// handled with every borrow of the connection free again.
enum Event {
    Closed(CloseReason),
    Incoming(Option<Result<Message, axum::Error>>),
    Delivery(Delivery),
    Lagged,
    Touched,
    Recheck,
    Status,
    Ping,
    Presence,
    PresenceFlush,
    Docs,
}

type Sender = SplitSink<WebSocket, Message>;

/// Serve one connection until it closes.
async fn serve(socket: WebSocket, env: LiveEnv, caller: LiveCaller) {
    let (mut sender, mut receiver) = socket.split();
    let limits = env.hub.limits().clone();
    let info = ConnectionInfo {
        app: env.subdomain.clone(),
        user: caller.user.as_ref().map(|user| user.id),
        session: caller.session.clone(),
    };
    let (connection, mut deliveries) = match env.hub.connect(info) {
        Ok(registered) => registered,
        Err(refused) => {
            close(&mut sender, refused.code, &refused.message).await;
            return;
        }
    };
    let mut close_signal = connection.close_signal();
    // Who the caller is, as a `table_changes` subscription's verdicts read it;
    // replaced at every re-read of the session.
    connection.set_user(caller.user.clone());
    let now = std::time::Instant::now();
    let mut live = Live {
        subs: SubscriptionSet::new(limits.max_subscriptions),
        by_attachment: HashMap::new(),
        row_subs: HashMap::new(),
        change_subs: HashMap::new(),
        extras: HashMap::new(),
        query_subs: HashMap::new(),
        publish_rate: RateLimit::new(limits.publishes_per_second, now),
        presence_rate: RateLimit::new(limits.presence_per_second, now),
        flush_at: None,
        connection,
        env,
        caller,
        limits: limits.clone(),
    };
    let mut recheck = ticker(limits.recheck_interval);
    let mut status = ticker(limits.status_interval);
    let mut ping = ticker(limits.ping_interval);
    let mut heard = Instant::now();

    let ending = loop {
        let flush_at = live.flush_at;
        let event = tokio::select! {
            reason = close_signal.closed() => Event::Closed(reason),
            incoming = receiver.next() => Event::Incoming(incoming),
            Some(delivery) = deliveries.recv() => Event::Delivery(delivery),
            () = live.connection.lagged() => Event::Lagged,
            () = live.connection.touched() => Event::Touched,
            () = live.connection.presence_changed() => Event::Presence,
            () = live.connection.doc_inbox().rung() => Event::Docs,
            () = sleep_until(flush_at) => Event::PresenceFlush,
            _ = recheck.tick() => Event::Recheck,
            _ = status.tick() => Event::Status,
            _ = ping.tick() => Event::Ping,
        };
        let frames = match event {
            Event::Closed(reason) => break Some(reason),
            Event::Incoming(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => break None,
            Event::Incoming(Some(Ok(message))) => {
                heard = Instant::now();
                live.receive(message).await
            }
            Event::Delivery(delivery) => live.deliver(&delivery),
            Event::Lagged => live.lagged(),
            Event::Touched => live.touched().await,
            Event::Recheck => match live.recheck().await {
                Ok(frames) => frames,
                Err(reason) => break Some(reason),
            },
            Event::Status => live.status_pass().await,
            Event::Presence => live.presence_news(),
            Event::Docs => live.doc_news(),
            Event::PresenceFlush => {
                live.flush_presence();
                Vec::new()
            }
            Event::Ping => {
                if heard.elapsed() >= limits.idle_timeout {
                    break Some(CloseReason {
                        code: ErrorCode::Idle,
                        message: format!(
                            "nothing arrived on this connection for {} seconds, not even a pong, \
                             so it is closed",
                            limits.idle_timeout.as_secs_f64()
                        ),
                    });
                }
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break None;
                }
                Vec::new()
            }
        };
        if !send_all(&mut sender, frames).await {
            break None;
        }
    };
    if let Some(reason) = ending {
        close(&mut sender, reason.code, &reason.message).await;
    }
    // Dropping `live` detaches every subscription and deregisters the
    // connection.
}

/// A connection's state.
struct Live {
    subs: SubscriptionSet<Attached>,
    /// Attachment id → the `sub` it belongs to, for routing deliveries.
    by_attachment: HashMap<u64, String>,
    /// The `row` subscriptions, by `sub`: which row, which tables its access
    /// depends on, and the watch that wakes this connection when one of them
    /// is written to.
    row_subs: HashMap<String, RowSub>,
    /// The `table_changes` subscriptions, by `sub`: what their fan-out
    /// decides each element by, kept to attach again after a restart.
    change_subs: HashMap<String, ChangeAccess>,
    /// Every subscription's publishing and presence, by `sub` (L4).
    extras: HashMap<String, Extras>,
    /// The subscriptions a stream query opened, by `sub` (TODO F7): the query
    /// and its parameters, run again at every re-check, and what it granted.
    query_subs: HashMap<String, QuerySub>,
    /// Publishes this connection may still make this second.
    publish_rate: RateLimit,
    /// Presence updates this connection may still send this second.
    presence_rate: RateLimit,
    /// When the presence window ends: until then a state update is held, and
    /// the latest one held is applied then. `None` when the connection is
    /// quiet.
    flush_at: Option<Instant>,
    connection: Connection,
    env: LiveEnv,
    caller: LiveCaller,
    limits: LiveLimits,
}

/// One subscription's publishing and presence.
#[derive(Default)]
struct Extras {
    /// Whether this connection's own elements come back to it.
    echo: bool,
    /// Whether the page may publish on it, as last decided.
    can_publish: bool,
    /// Its seat in its topic's room, on a stream with presence.
    seat: Option<Seat>,
    /// A state update held until the presence window ends.
    pending_state: Option<Json>,
    /// Its seat in the document's room, on a `document` stream (L5).
    doc: Option<DocSeat>,
    /// Whether the page may edit that document, as last decided — `doc_sync`'s
    /// `read_only` is its negation.
    can_edit: bool,
}

/// A `row` subscription's access, as far as writes can change it.
struct RowSub {
    /// The table its topic is a row of.
    table: String,
    /// The row's key — the topic.
    key: String,
    /// The tables its table's read rule reads, other than itself.
    deps: BTreeSet<String>,
    /// Held for as long as the subscription is.
    _watch: Watch,
}

impl RowSub {
    /// Whether a write could have changed this subscription's verdict: its own
    /// row, or any row of a table the read rule reads.
    fn affected_by(&self, touch: &Touch) -> bool {
        if touch.table == self.table {
            return touch.keys.is_empty() || touch.keys.contains(&self.key);
        }
        self.deps.contains(&touch.table)
    }
}

/// A subscription a stream query opened (TODO F7).
struct QuerySub {
    /// The custom query's name.
    query: String,
    /// What the page sent it — held in memory only, to run it again.
    params: Json,
    /// What its last run granted.
    grant: QueryGrant,
    /// Held for as long as the subscription is: wakes this connection when a
    /// table the grant depends on is written.
    _watch: Watch,
}

impl QuerySub {
    /// Whether a write could change what the query answers: a table its run
    /// read or named in `watch`, or the subscription's own row.
    fn affected_by(&self, touch: &Touch) -> bool {
        if self.grant.tables.contains(&touch.table) {
            return true;
        }
        touch.table == self.grant.table
            && (touch.keys.is_empty() || touch.keys.contains(&self.grant.key))
    }
}

/// What a stream query's `live.stream(...)` was found to open.
#[derive(Debug, Clone)]
struct QueryGrant {
    /// The stream.
    stream: String,
    /// The row's key: the topic.
    key: String,
    /// Whether the page may edit the document or publish on the topic.
    write: bool,
    /// The stream's row table.
    table: String,
    /// The tables the run read and those it asked to `watch`.
    tables: BTreeSet<String>,
    /// What `ready` tells the page.
    opened: Opened,
}

impl Live {
    /// Who this connection is, as access is decided.
    fn caller(&self) -> Caller {
        let user = self.caller.user.as_ref();
        Caller::new(user.map(|u| u.role), user.map(|u| u.id))
    }

    /// One message from the client.
    async fn receive(&mut self, message: Message) -> Vec<ServerFrame> {
        match message {
            Message::Text(text) => {
                if text.len() > self.limits.max_frame_bytes {
                    // A `doc_update` has its own, larger cap (pasting a long
                    // passage is ordinary editing); every other frame keeps
                    // the small one.
                    if text.len() <= self.limits.max_doc_update_bytes
                        && let Ok(frame @ ClientFrame::DocUpdate { .. }) =
                            ClientFrame::parse(text.as_str())
                    {
                        return self.frame(frame).await;
                    }
                    let cap = if text.contains("\"doc_update\"") {
                        self.limits.max_doc_update_bytes
                    } else {
                        self.limits.max_frame_bytes
                    };
                    return vec![
                        FrameError::new(
                            None,
                            ErrorCode::TooLarge,
                            format!(
                                "a frame may be at most {cap} bytes and this one was {}; it was not \
                                 read",
                                text.len()
                            ),
                        )
                        .frame(),
                    ];
                }
                match ClientFrame::parse(text.as_str()) {
                    Ok(frame) => self.frame(frame).await,
                    Err(error) => vec![error.frame()],
                }
            }
            Message::Binary(_) => vec![
                FrameError::invalid(None, "frames on this socket are JSON text, not binary")
                    .frame(),
            ],
            // A ping is answered by the transport, and a pong is only proof of
            // life, which receiving it already was.
            _ => Vec::new(),
        }
    }

    /// One frame from the client.
    async fn frame(&mut self, frame: ClientFrame) -> Vec<ServerFrame> {
        match frame {
            ClientFrame::Subscribe {
                sub,
                stream,
                topic,
                filter,
                echo,
            } => self.subscribe(sub, stream, topic, filter, echo).await,
            ClientFrame::Open {
                sub,
                query,
                params,
                echo,
            } => self.open(sub, query, params, echo).await,
            ClientFrame::Unsubscribe { sub } => match self.subs.remove(&sub) {
                Ok(gone) => {
                    self.forget(&gone);
                    Vec::new()
                }
                Err(error) => vec![error.frame()],
            },
            ClientFrame::Publish { sub, value } => self.publish(sub, value),
            ClientFrame::Presence { sub, state } => self.set_presence(sub, state),
            ClientFrame::DocUpdate { sub, update } => self.doc_update(sub, &update),
            ClientFrame::Ping => vec![ServerFrame::Pong],
        }
    }

    /// Open a subscription: the rules that do not depend on the stream, then
    /// access, then the frame's own shape, then `ready`.
    async fn subscribe(
        &mut self,
        sub: String,
        stream: String,
        topic: Option<String>,
        filter: Option<Json>,
        echo: bool,
    ) -> Vec<ServerFrame> {
        if let Err(error) = self.subs.check_new(&sub) {
            return vec![error.frame()];
        }
        let Some(app) = self.env.app.current() else {
            return vec![FrameError::unavailable(&sub, &stream).frame()];
        };
        let caller = self.caller();
        let facts = self.facts(&app, &stream, topic.as_deref(), &caller).await;
        let grant = match may_subscribe(&facts, &caller, topic.as_deref()) {
            Ok(grant) => grant,
            Err(Refusal::Denied) => return vec![FrameError::unavailable(&sub, &stream).frame()],
            // The caller may know the stream is there now, so a malformed
            // request for it is answered as one.
            Err(Refusal::Invalid(message)) => {
                return vec![
                    FrameError::invalid(Some(sub), format!("`{stream}`: {message}")).frame(),
                ];
            }
        };
        let change = match &grant.stored.topics {
            TopicSpec::PerElementRow { table } => {
                match self.change_access(table, filter.as_ref()) {
                    Ok(access) => Some(access),
                    Err(message) => {
                        return vec![
                            FrameError::invalid(Some(sub), format!("`{stream}`: {message}"))
                                .frame(),
                        ];
                    }
                }
            }
            _ if filter.is_some() => {
                return vec![
                    FrameError::invalid(
                        Some(sub),
                        format!("`{stream}` takes no filter; only a stream of row changes does"),
                    )
                    .frame(),
                ];
            }
            _ => None,
        };
        let row_sub = self.watch_row(&grant);
        let can_publish = self
            .publish_right(&stream, grant.topic.as_deref(), None)
            .await;
        let mut subscription = Subscription::waiting(sub.clone(), stream, grant.topic);
        let mut frames = start(
            &self.connection,
            &mut self.by_attachment,
            self.env.supervisor.as_ref(),
            &self.env.catalog,
            change.as_ref(),
            Rights { echo, can_publish },
            &mut subscription,
        )
        .await;
        // `check_new` passed above and nothing has been added since, so this
        // cannot fail; if it somehow did, dropping the subscription detaches it.
        if self.subs.insert(subscription).is_ok() {
            if let Some(row_sub) = row_sub {
                self.row_subs.insert(sub.clone(), row_sub);
            }
            if let Some(change) = change {
                self.change_subs.insert(sub.clone(), change);
            }
            self.extras.insert(
                sub.clone(),
                Extras {
                    echo,
                    can_publish,
                    ..Extras::default()
                },
            );
            frames.extend(self.seat(&sub).await);
            frames.extend(self.join_doc(&sub).await);
        }
        frames
    }

    /// Open a subscription by running stream query `query` (TODO F7): the
    /// rules that do not depend on the stream, then the query, then what its
    /// handle names, then `ready` with what was opened.
    ///
    /// The stream's `min_role` and the row's read rule are not asked: the
    /// query was the access check, as an admin-authority file handle's is.
    async fn open(
        &mut self,
        sub: String,
        query: String,
        params: Json,
        echo: bool,
    ) -> Vec<ServerFrame> {
        if let Err(error) = self.subs.check_new(&sub) {
            return vec![error.frame()];
        }
        let Some(app) = self.env.app.current() else {
            return vec![FrameError::unavailable(&sub, &query).frame()];
        };
        let grant = match self
            .run_query(&app, &sub, &query, params.clone(), false)
            .await
        {
            Ok(grant) => grant,
            Err(refusal) => return vec![*refusal],
        };
        let can_publish = self
            .publish_right(&grant.stream, Some(&grant.key), Some(grant.write))
            .await;
        let mut subscription =
            Subscription::waiting(sub.clone(), grant.stream.clone(), Some(grant.key.clone()));
        let mut frames = start(
            &self.connection,
            &mut self.by_attachment,
            self.env.supervisor.as_ref(),
            &self.env.catalog,
            None,
            Rights { echo, can_publish },
            &mut subscription,
        )
        .await;
        mark_opened(&mut frames, &sub, &grant.opened);
        if self.subs.insert(subscription).is_ok() {
            let watch = self.watch_query(&grant);
            self.query_subs.insert(
                sub.clone(),
                QuerySub {
                    query,
                    params,
                    grant,
                    _watch: watch,
                },
            );
            self.extras.insert(
                sub.clone(),
                Extras {
                    echo,
                    can_publish,
                    ..Extras::default()
                },
            );
            frames.extend(self.seat(&sub).await);
            frames.extend(self.join_doc(&sub).await);
        }
        frames
    }

    /// Run stream query `query` as this connection's caller and check what its
    /// handle names. `Err` is the frame the page is answered with: anything
    /// short of a handle this application may open is `unavailable`, worded
    /// as an absent stream is.
    async fn run_query(
        &self,
        app: &MountedApp,
        sub: &str,
        query: &str,
        params: Json,
        recheck: bool,
    ) -> Result<QueryGrant, Box<ServerFrame>> {
        let unavailable = || Box::new(FrameError::unavailable(sub, query).frame());
        let user = self.caller.user.as_ref();
        let mut opening = None;
        for provider in &app.providers {
            opening = provider
                .open_stream(
                    query,
                    params.clone(),
                    self.env.client_ip,
                    recheck,
                    &self.env.catalog,
                    user,
                )
                .await;
            if opening.is_some() {
                break;
            }
        }
        let reply = match opening {
            None | Some(StreamOpening::Denied) => return Err(unavailable()),
            Some(StreamOpening::RateLimited { retry_after }) => {
                let message =
                    format!("`{query}` was opened too often; try again in {retry_after} s");
                let refusal =
                    FrameError::new(Some(sub.to_owned()), ErrorCode::RateLimited, message);
                return Err(Box::new(refusal.frame()));
            }
            Some(StreamOpening::Invalid(message)) => {
                let refusal =
                    FrameError::invalid(Some(sub.to_owned()), format!("`{query}`: {message}"));
                return Err(Box::new(refusal.frame()));
            }
            Some(StreamOpening::Granted(reply)) => reply,
        };
        self.check_handle(app, &reply).await.map_err(|why| {
            sc_log::log_warn!(
                "stream query `{query}` returned live.stream(\"{}\", …), which cannot be \
                 opened: {why}",
                reply.name
            );
            unavailable()
        })
    }

    /// What a stream query's handle opens, if this application may open it:
    /// a stream it exposes, a `document` stream or an `internal` one whose
    /// topics are rows, of a table in the application.
    async fn check_handle(
        &self,
        app: &MountedApp,
        reply: &sc_expr::StreamReply,
    ) -> Result<QueryGrant, String> {
        if !app.app.exposes_stream(&reply.name) {
            return Err("the application does not expose that stream".to_owned());
        }
        let row = match sc_stream::load_stream_by_name(&self.env.catalog, &reply.name).await {
            Ok(Some(row)) => row,
            Ok(None) => return Err("there is no such stream".to_owned()),
            Err(e) => return Err(sc_error::format_chain(&e)),
        };
        let Some(TopicSpec::Row { table }) = self.topics_of(&row) else {
            return Err("its topics are not rows".to_owned());
        };
        let document =
            sc_stream::providers::document::settings_of(&row.provider, &row.configuration);
        if document.is_none() && row.provider != internal::INTERNAL {
            return Err("it is neither a document stream nor an internal stream".to_owned());
        }
        if !app.app.can_access_table(&TableId(table.clone())) {
            return Err(format!("its table `{table}` is not in the application"));
        }
        Ok(QueryGrant {
            stream: reply.name.clone(),
            key: reply.key.clone(),
            write: reply.write,
            tables: reply.read.iter().chain(&reply.watch).cloned().collect(),
            opened: Opened {
                stream: reply.name.clone(),
                kind: if document.is_some() {
                    "document"
                } else {
                    "row"
                }
                .to_owned(),
                key: reply.key.clone(),
                root: document.as_ref().map(|d| d.root.clone()),
                root_kind: document.as_ref().map(|d| d.root_kind.clone()),
            },
            table,
        })
    }

    /// Watch what `grant` depends on: its row's table and the tables its run
    /// read or named.
    fn watch_query(&self, grant: &QueryGrant) -> Watch {
        self.connection
            .watch(std::iter::once(grant.table.clone()).chain(grant.tables.iter().cloned()))
    }

    /// The `write` a stream query granted `sub`, or `None` for a subscription
    /// that was not opened by one.
    fn query_write(&self, sub: &str) -> Option<bool> {
        self.query_subs.get(sub).map(|q| q.grant.write)
    }

    /// Run each of `names`' stream queries again, with the parameters they
    /// were opened with (TODO F7). A query that no longer hands back the same
    /// stream and row revokes its subscription; one whose `write` changed is
    /// told by [`refresh_rights`](Live::refresh_rights), which follows.
    async fn recheck_queries(&mut self, app: &MountedApp, names: Vec<String>) -> Vec<ServerFrame> {
        let mut frames = Vec::new();
        for name in names {
            let Some((query, params, before)) = self
                .query_subs
                .get(&name)
                .map(|q| (q.query.clone(), q.params.clone(), q.grant.clone()))
            else {
                continue;
            };
            match self.run_query(app, &name, &query, params, true).await {
                Ok(grant) if grant.stream == before.stream && grant.key == before.key => {
                    let watch = (grant.tables != before.tables).then(|| self.watch_query(&grant));
                    if let Some(q) = self.query_subs.get_mut(&name) {
                        q.grant = grant;
                        if let Some(watch) = watch {
                            q._watch = watch;
                        }
                    }
                }
                _ => {
                    if let Some(gone) = self.subs.revoke(&name) {
                        self.forget(&gone);
                        frames.push(ServerFrame::Revoked { sub: name });
                    }
                }
            }
        }
        frames
    }

    /// Whether this connection's page may publish on a subscription to
    /// `stream` on `topic` (§3 rule 7): the running stream's settings, and on
    /// a `row` topic the caller's update access to the row. `false` while the
    /// stream is not running here — there is nothing to publish into.
    ///
    /// `granted` is a stream query's `write` (TODO F7), which stands in for the
    /// caller's update access to the row.
    async fn publish_right(
        &self,
        stream: &str,
        topic: Option<&str>,
        granted: Option<bool>,
    ) -> bool {
        let Some(running) = self
            .env
            .supervisor
            .as_ref()
            .and_then(|supervisor| supervisor.by_name(stream))
        else {
            return false;
        };
        let client_publish = running.provider().is_some_and(|p| p.accepts_publish())
            && internal::client_publish(&running.configuration());
        if !client_publish {
            return false;
        }
        let Ok(spec) = running.topic_spec() else {
            return false;
        };
        let row_updatable = match (&spec, topic, granted) {
            (TopicSpec::Row { .. }, Some(_), Some(write)) => write,
            (TopicSpec::Row { table }, Some(key), None) => self.row_updatable(table, key).await,
            _ => false,
        };
        may_publish(
            &spec,
            PublishFacts {
                client_publish,
                row_updatable,
            },
        )
    }

    /// Whether this connection's caller may update `table`'s row `key`.
    async fn row_updatable(&self, table: &str, key: &str) -> bool {
        let found = match self.env.catalog.get(table) {
            Ok(Some(found)) => found,
            _ => return false,
        };
        match sc_api::row_updatable_as(
            &self.env.catalog,
            &found,
            key,
            self.caller().role,
            self.caller.user.as_ref(),
            self.env.evaluator.as_ref(),
        )
        .await
        {
            Ok(verdict) => verdict,
            // Refused rather than guessed at, as a read that fails is.
            Err(e) => {
                sc_log::log_warn!(
                    "the live socket could not decide whether row `{key}` of `{table}` may be \
                     updated, so publishing on it is refused: {}",
                    sc_error::format_chain(&e)
                );
                false
            }
        }
    }

    /// Decide again whether each of `names` may be published on — after
    /// access was re-checked, which is when an update right can change too.
    ///
    /// A document member whose update right changed is sent the document
    /// again with the new `read_only` (L5.3): a reader who becomes an editor
    /// may type, and an editor who loses the right stops.
    async fn refresh_rights(&mut self, names: &[String]) -> Vec<ServerFrame> {
        let mut frames = Vec::new();
        for name in names {
            let Some((stream, topic)) = self
                .subs
                .get(name)
                .map(|s| (s.stream.clone(), s.topic.clone()))
            else {
                continue;
            };
            let granted = self.query_write(name);
            let can_publish = self.publish_right(&stream, topic.as_deref(), granted).await;
            if let Some(extras) = self.extras.get_mut(name) {
                extras.can_publish = can_publish;
            }
            let doc_table = self
                .extras
                .get(name)
                .and_then(|extras| extras.doc.as_ref())
                .map(|seat| seat.room().target().table.clone());
            if let (Some(table), Some(key)) = (doc_table, topic) {
                let can_edit = match granted {
                    Some(write) => write,
                    None => self.row_updatable(&table, &key).await,
                };
                let Some(extras) = self.extras.get_mut(name) else {
                    continue;
                };
                if extras.can_edit != can_edit {
                    extras.can_edit = can_edit;
                    if let Some(seat) = &extras.doc {
                        frames.push(doc_sync(name, seat, can_edit));
                    }
                }
            }
        }
        frames
    }

    /// Give `sub` its seat in its document's room if it is attached to a
    /// `document` stream, and send it the document (L5.3): `doc_sync`, with
    /// `read_only` unless the caller may update the row. On a restart that
    /// kept the room, the page has had a fresh `ready` and is sent the
    /// document again.
    async fn join_doc(&mut self, sub: &str) -> Vec<ServerFrame> {
        let Some(subscription) = self.subs.get(sub) else {
            return Vec::new();
        };
        let target = subscription.attached().and_then(|attached| {
            let running = attached.running();
            (running.provider_name() == DOCUMENT)
                .then(|| {
                    subscription
                        .topic
                        .as_deref()
                        .map(|key| DocTarget::of(running, key))
                })
                .flatten()
        });
        let target = match target {
            None => {
                if let Some(extras) = self.extras.get_mut(sub) {
                    extras.doc = None;
                }
                return Vec::new();
            }
            Some(Err(e)) => {
                return vec![
                    FrameError::invalid(Some(sub.to_owned()), sc_error::format_chain(&e)).frame(),
                ];
            }
            Some(Ok(target)) => target,
        };
        let can_edit = match self.query_write(sub) {
            Some(write) => write,
            None => self.row_updatable(&target.table, &target.key).await,
        };
        let same_room = self
            .extras
            .get(sub)
            .and_then(|extras| extras.doc.as_ref())
            .is_some_and(|seat| *seat.room().target() == target);
        if !same_room {
            // Leave the old room (if any) before joining the new one.
            if let Some(extras) = self.extras.get_mut(sub) {
                extras.doc = None;
            }
            match self.connection.join_doc(&self.env.catalog, &target).await {
                Ok((seat, _)) => {
                    if let Some(extras) = self.extras.get_mut(sub) {
                        extras.doc = Some(seat);
                    }
                }
                Err(e) => {
                    sc_log::log_warn!(
                        "the live socket could not open document `{}` of `{}`: {}",
                        target.key,
                        target.stream_name,
                        sc_error::format_chain(&e)
                    );
                    return vec![
                        FrameError::new(
                            Some(sub.to_owned()),
                            ErrorCode::Unavailable,
                            format!("the document could not be opened: {}", e),
                        )
                        .frame(),
                    ];
                }
            }
        }
        let Some(extras) = self.extras.get_mut(sub) else {
            return Vec::new();
        };
        extras.can_edit = can_edit;
        match &extras.doc {
            Some(seat) => vec![doc_sync(sub, seat, can_edit)],
            None => Vec::new(),
        }
    }

    /// A page's `doc_update` frame: an edit of the document its subscription
    /// is on (L5.3). Refused `unavailable` for a member who may not edit —
    /// worded like any absence — and counted against the publish rate.
    fn doc_update(&mut self, sub: String, update: &str) -> Vec<ServerFrame> {
        let Some(found) = self.subs.get(&sub) else {
            return vec![not_subscribed(sub)];
        };
        let Some(extras) = self.extras.get(&sub) else {
            return vec![not_subscribed(sub)];
        };
        let Some(seat) = &extras.doc else {
            return vec![
                FrameError::invalid(
                    Some(sub.clone()),
                    format!("`{}` is not a document", found.stream),
                )
                .frame(),
            ];
        };
        if !extras.can_edit {
            return vec![
                FrameError::new(
                    Some(sub.clone()),
                    ErrorCode::Unavailable,
                    format!("there is nothing on `{}` you can edit", found.stream),
                )
                .frame(),
            ];
        }
        if !self.publish_rate.take(std::time::Instant::now()) {
            return vec![
                FrameError::new(
                    Some(sub),
                    ErrorCode::RateLimited,
                    format!(
                        "this connection may send at most {} edits and publishes a second; this \
                         edit was not applied — send it again",
                        self.limits.publishes_per_second
                    ),
                )
                .frame(),
            ];
        }
        let Ok(bytes) = BASE64.decode(update) else {
            return vec![FrameError::invalid(Some(sub), "`update` is not base64").frame()];
        };
        match seat.apply(&bytes) {
            Ok(()) => Vec::new(),
            Err(DocRefusal::TooLarge(why)) => {
                vec![FrameError::new(Some(sub), ErrorCode::TooLarge, why).frame()]
            }
            Err(DocRefusal::Invalid(why)) => vec![FrameError::invalid(Some(sub), why).frame()],
        }
    }

    /// Other members edited documents this connection has open: send each
    /// edit as `doc_update`, or the whole document where edits were dropped.
    fn doc_news(&mut self) -> Vec<ServerFrame> {
        let news = self.connection.doc_inbox().take();
        if news.is_empty() {
            return Vec::new();
        }
        let by_seat: HashMap<u64, String> = self
            .extras
            .iter()
            .filter_map(|(sub, extras)| extras.doc.as_ref().map(|seat| (seat.id(), sub.clone())))
            .collect();
        let mut frames = Vec::new();
        for (seat_id, item) in news {
            let Some(sub) = by_seat.get(&seat_id) else {
                continue;
            };
            match item {
                DocNews::Update(update) => frames.push(ServerFrame::DocUpdate {
                    sub: sub.clone(),
                    update: BASE64.encode(update.as_slice()),
                }),
                DocNews::Resync => {
                    if let Some(extras) = self.extras.get(sub)
                        && let Some(seat) = &extras.doc
                    {
                        frames.push(doc_sync(sub, seat, extras.can_edit));
                    }
                }
            }
        }
        frames
    }

    /// Give `sub` its seat in its topic's room if its stream has presence on
    /// and it is attached, and say who is there; give it back if not.
    async fn seat(&mut self, sub: &str) -> Vec<ServerFrame> {
        let Some(subscription) = self.subs.get(sub) else {
            return Vec::new();
        };
        let wanted: Option<(RoomKey, Vec<String>)> = subscription.attached().and_then(|attached| {
            let running = attached.running();
            let config = running.configuration();
            let accepts = running.provider().is_some_and(|p| p.accepts_publish());
            // A document's members always see each other: their presence
            // state is their editor's awareness (L5.6).
            let document = running.provider_name() == DOCUMENT;
            ((accepts && internal::presence(&config)) || document).then(|| {
                (
                    (running.id(), subscription.topic.clone()),
                    internal::presence_fields(&config).unwrap_or_default(),
                )
            })
        });
        let Some(extras) = self.extras.get_mut(sub) else {
            return Vec::new();
        };
        let Some((room, shown)) = wanted else {
            extras.seat = None;
            extras.pending_state = None;
            return Vec::new();
        };
        let members = match &extras.seat {
            // Still in the same room (a restart that kept presence on): the
            // page has had a fresh `ready`, so it is handed the list again.
            Some(seat) if *seat.room() == room => self.env.hub.presence_members(&room),
            _ => {
                let fields =
                    shown_fields(&self.env.catalog, self.caller.user.as_ref(), &shown).await;
                let (seat, members) = self.connection.join_presence(
                    room,
                    self.caller.user.as_ref().map(|user| user.id),
                    fields,
                );
                if let Some(extras) = self.extras.get_mut(sub) {
                    extras.seat = Some(seat);
                }
                members
            }
        };
        vec![ServerFrame::Presence {
            sub: sub.to_owned(),
            connection: self.connection.tag().to_owned(),
            members,
        }]
    }

    /// A page's `publish` frame (§3 rule 7).
    fn publish(&mut self, sub: String, value: Json) -> Vec<ServerFrame> {
        let Some(found) = self.subs.get(&sub) else {
            return vec![not_subscribed(sub)];
        };
        let allowed = self
            .extras
            .get(&sub)
            .is_some_and(|extras| extras.can_publish);
        let Some(attached) = found.attached().filter(|_| allowed) else {
            // Worded like any other "not here": a reader who may not update
            // the row learns nothing from it that `ready` did not say.
            return vec![
                FrameError::new(
                    Some(sub.clone()),
                    ErrorCode::Unavailable,
                    format!("there is nowhere on `{}` you can publish", found.stream),
                )
                .frame(),
            ];
        };
        if !self.publish_rate.take(std::time::Instant::now()) {
            return vec![
                FrameError::new(
                    Some(sub),
                    ErrorCode::RateLimited,
                    format!(
                        "this connection may publish at most {} times a second; this one was not \
                         published",
                        self.limits.publishes_per_second
                    ),
                )
                .frame(),
            ];
        }
        match self
            .connection
            .publish(attached.running(), found.topic.clone(), value)
        {
            Ok(_) => Vec::new(),
            Err(e) => vec![FrameError::invalid(Some(sub), sc_error::format_chain(&e)).frame()],
        }
    }

    /// A page's `presence` frame: its state on the subscription's topic.
    fn set_presence(&mut self, sub: String, state: Json) -> Vec<ServerFrame> {
        let Some(found) = self.subs.get(&sub) else {
            return vec![not_subscribed(sub)];
        };
        let seated = self
            .extras
            .get(&sub)
            .is_some_and(|extras| extras.seat.is_some());
        if !seated {
            return vec![
                FrameError::invalid(
                    Some(sub.clone()),
                    format!("`{}` has no presence", found.stream),
                )
                .frame(),
            ];
        }
        let size = state.to_string().len();
        if size > self.limits.max_presence_state_bytes {
            return vec![
                FrameError::new(
                    Some(sub),
                    ErrorCode::TooLarge,
                    format!(
                        "a presence state may be at most {} bytes of JSON and this one was {size}; \
                         the state is unchanged",
                        self.limits.max_presence_state_bytes
                    ),
                )
                .frame(),
            ];
        }
        if !self.presence_rate.take(std::time::Instant::now()) {
            return vec![
                FrameError::new(
                    Some(sub),
                    ErrorCode::RateLimited,
                    format!(
                        "this connection may update its presence at most {} times a second; this \
                         update was not applied",
                        self.limits.presence_per_second
                    ),
                )
                .frame(),
            ];
        }
        let Some(extras) = self.extras.get_mut(&sub) else {
            return Vec::new();
        };
        let now = Instant::now();
        if self.flush_at.is_some_and(|at| at > now) {
            // Within the window: only the latest is kept, and applied when it
            // ends.
            extras.pending_state = Some(state);
        } else {
            if let Some(seat) = &extras.seat {
                seat.set_state(state);
            }
            extras.pending_state = None;
            self.flush_at = Some(now + self.limits.presence_coalesce);
        }
        Vec::new()
    }

    /// The presence window ended: apply the latest state held for each
    /// subscription, and open another window if anything was.
    fn flush_presence(&mut self) {
        self.flush_at = None;
        let mut applied = false;
        for extras in self.extras.values_mut() {
            if let Some(state) = extras.pending_state.take()
                && let Some(seat) = &extras.seat
            {
                seat.set_state(state);
                applied = true;
            }
        }
        if applied {
            self.flush_at = Some(Instant::now() + self.limits.presence_coalesce);
        }
    }

    /// Rooms this connection is in changed: tell each subscription in them,
    /// in order — patched with `presence_diff`, or whole with `presence` when
    /// the hub had to drop the room's changes.
    fn presence_news(&mut self) -> Vec<ServerFrame> {
        let news = self.connection.take_presence();
        if news.events.is_empty() && news.resync.is_empty() {
            return Vec::new();
        }
        let mut by_room: HashMap<RoomKey, Vec<String>> = HashMap::new();
        for (sub, extras) in &self.extras {
            if let Some(seat) = &extras.seat {
                by_room
                    .entry(seat.room().clone())
                    .or_default()
                    .push(sub.clone());
            }
        }
        for subs in by_room.values_mut() {
            subs.sort();
        }
        let mut frames = Vec::new();
        for room in &news.resync {
            let Some(subs) = by_room.get(room) else {
                continue;
            };
            let members = self.env.hub.presence_members(room);
            for sub in subs {
                frames.push(ServerFrame::Presence {
                    sub: sub.clone(),
                    connection: self.connection.tag().to_owned(),
                    members: members.clone(),
                });
            }
        }
        for (room, diff) in presence_diffs(news.events) {
            let Some(subs) = by_room.get(&room) else {
                // A room this connection has left since.
                continue;
            };
            for sub in subs {
                frames.push(ServerFrame::PresenceDiff {
                    sub: sub.clone(),
                    joined: diff.joined.clone(),
                    left: diff.left.clone(),
                    updated: diff.updated.clone(),
                });
            }
        }
        frames
    }

    /// What a subscription to a `table_changes` stream on `table` is attached
    /// with: its filter, lowered by the REST read's own walk against the table
    /// (an `Err` is the sentence an `invalid` frame carries), and the catalog
    /// and evaluator its elements are decided with.
    fn change_access(&self, table: &str, filter: Option<&Json>) -> Result<ChangeAccess, String> {
        let found = match self.env.catalog.get(table) {
            Ok(Some(found)) => found,
            // Access already found the table in the app; one dropped since is
            // a stream nobody can be sent anything from.
            _ => return Err(format!("its table `{table}` is not there")),
        };
        let like = match self.env.catalog.driver_for(&found) {
            Ok(driver) => LikeRules::of(driver.as_ref()),
            Err(_) => LikeRules::POSTGRES,
        };
        let filter = RowFilter::parse(&found, filter, like)
            .map_err(|e| format!("`filter`: {}", sc_error::format_chain(&e)))?;
        Ok(ChangeAccess {
            catalog: Arc::clone(&self.env.catalog),
            evaluator: self.env.evaluator.clone(),
            filter,
        })
    }

    /// For a `row` subscription, watch its table and every table its read rule
    /// reads, so a write to any of them wakes this connection to re-check it.
    fn watch_row(&self, grant: &sc_live::Grant) -> Option<RowSub> {
        let TopicSpec::Row { table } = &grant.stored.topics else {
            return None;
        };
        let key = grant.topic.clone()?;
        let deps = match self.env.catalog.get(table) {
            Ok(Some(found)) => read_rule_tables(&self.env.catalog, &found),
            _ => BTreeSet::new(),
        };
        let watch = self
            .connection
            .watch(std::iter::once(table.clone()).chain(deps.iter().cloned()));
        Some(RowSub {
            table: table.clone(),
            key,
            deps,
            _watch: watch,
        })
    }

    /// What access to `name` is decided by, for `app` and `caller` — and, for
    /// a `row` stream asked about `topic`, what the caller's REST read of that
    /// row finds. The row is read only once the stream-level rules have passed,
    /// so a caller below the floor never causes a read.
    async fn facts(
        &self,
        app: &MountedApp,
        name: &str,
        topic: Option<&str>,
        caller: &Caller,
    ) -> StreamFacts {
        let exposed = app.app.exposes_stream(name);
        if !exposed {
            // Not even read: an unexposed stream does not exist for this app.
            return StreamFacts {
                exposed,
                stored: None,
                row: RowFact::NotAsked,
            };
        }
        let stored = match sc_stream::load_stream_by_name(&self.env.catalog, name).await {
            Ok(Some(row)) => self.topics_of(&row).map(|topics| StoredStream {
                min_role: row.min_role,
                topics,
            }),
            Ok(None) => None,
            Err(e) => {
                // Refused rather than guessed at: a read that failed is not a
                // stream anybody may watch.
                sc_log::log_warn!(
                    "the live socket could not read stream `{name}`, so it is refused: {}",
                    sc_error::format_chain(&e)
                );
                None
            }
        };
        let mut facts = StreamFacts {
            exposed,
            stored,
            row: RowFact::NotAsked,
        };
        if let Ok(stored) = may_open(&facts, caller) {
            match (&stored.topics, topic) {
                (TopicSpec::Row { table }, Some(key)) => {
                    facts.row = self.row_fact(app, table, key).await;
                }
                // Rule 3: a stream of a table's changes exists for this app
                // only if the table does — in its subset, and in the catalog.
                (TopicSpec::PerElementRow { table }, _) => {
                    let exposed = app.app.can_access_table(&TableId(table.clone()))
                        && matches!(self.env.catalog.get(table), Ok(Some(_)));
                    if !exposed {
                        facts.row = RowFact::TableNotExposed;
                    }
                }
                _ => {}
            }
        }
        facts
    }

    /// What this connection's caller's REST read of `table`'s row `key` finds.
    async fn row_fact(&self, app: &MountedApp, table: &str, key: &str) -> RowFact {
        // Rule 3: a table outside the application's subset does not exist for
        // its pages, whatever the caller may read elsewhere.
        if !app.app.can_access_table(&TableId(table.to_owned())) {
            return RowFact::TableNotExposed;
        }
        let found = match self.env.catalog.get(table) {
            Ok(Some(found)) => found,
            _ => return RowFact::Unreadable,
        };
        let user = self.caller.user.as_ref();
        match sc_api::row_readable_as(
            &self.env.catalog,
            &found,
            key,
            self.caller().role,
            user,
            self.env.evaluator.as_ref(),
        )
        .await
        {
            Ok(true) => RowFact::Readable,
            Ok(false) => RowFact::Unreadable,
            // Refused rather than guessed at, as an unreadable stream is.
            Err(e) => {
                sc_log::log_warn!(
                    "the live socket could not read row `{key}` of `{table}`, so its topic is \
                     refused: {}",
                    sc_error::format_chain(&e)
                );
                RowFact::Unreadable
            }
        }
    }

    /// How `row`'s elements are split into topics: the running stream's
    /// answer, or the provider's for the stored configuration. `None` — and so
    /// a refusal — when the provider cannot say.
    fn topics_of(&self, row: &Stream) -> Option<TopicSpec> {
        if let Some(running) = self
            .env
            .supervisor
            .as_ref()
            .and_then(|supervisor| supervisor.by_name(&row.name))
        {
            return running.topic_spec().ok();
        }
        let registry = match &self.env.supervisor {
            Some(supervisor) => supervisor.registry(),
            None => sc_app::catalog_stream_registry(&self.env.catalog),
        };
        match registry.get(&row.provider) {
            Some(provider) => provider.topic_spec(&row.configuration).ok(),
            // Not installed: it cannot run, so nothing would flow either way.
            None => Some(TopicSpec::Single),
        }
    }

    /// One element from the fan-out, for one of this connection's
    /// subscriptions — or for one it has since ended, which is dropped.
    fn deliver(&mut self, delivery: &Delivery) -> Vec<ServerFrame> {
        let Some(sub) = self.by_attachment.get(&delivery.attachment) else {
            return Vec::new();
        };
        let Some(attached) = self.subs.get(sub).and_then(Subscription::attached) else {
            return Vec::new();
        };
        if attached.attachment() != delivery.attachment {
            return Vec::new();
        }
        let mut frames = Vec::with_capacity(2);
        // A loss is reported before the element that came after it.
        let dropped = attached.take_lagged();
        if dropped > 0 {
            frames.push(ServerFrame::Lagged {
                sub: sub.clone(),
                dropped,
            });
        }
        frames.push(ServerFrame::Element {
            sub: sub.clone(),
            envelope: (*delivery.envelope).clone(),
        });
        frames
    }

    /// The fan-out dropped elements somewhere: say how many, per subscription.
    fn lagged(&self) -> Vec<ServerFrame> {
        self.subs
            .iter()
            .filter_map(|subscription| {
                let dropped = subscription.attached()?.take_lagged();
                (dropped > 0).then(|| ServerFrame::Lagged {
                    sub: subscription.sub.clone(),
                    dropped,
                })
            })
            .collect()
    }

    /// Follow each subscription's stream: attach one that has started, move one
    /// whose stream restarted, and report a status that changed.
    async fn status_pass(&mut self) -> Vec<ServerFrame> {
        let mut frames = Vec::new();
        let supervisor = self.env.supervisor.clone();
        let catalog = Arc::clone(&self.env.catalog);
        let mut followed = Vec::new();
        for name in self.subs.names() {
            let Some(subscription) = self.subs.get(&name) else {
                continue;
            };
            let current = supervisor
                .as_ref()
                .and_then(|supervisor| supervisor.by_name(&subscription.stream));
            let follow = match (subscription.attached(), &current) {
                (Some(attached), Some(current)) => !Arc::ptr_eq(attached.running(), current),
                (None, Some(_)) => true,
                _ => false,
            };
            if follow {
                // A restart may have changed the settings publishing and
                // presence are decided by.
                let (stream, topic) = (subscription.stream.clone(), subscription.topic.clone());
                let granted = self.query_write(&name);
                let can_publish = self.publish_right(&stream, topic.as_deref(), granted).await;
                let opened = self.query_subs.get(&name).map(|q| q.grant.opened.clone());
                let echo = self.extras.get(&name).is_some_and(|extras| extras.echo);
                if let Some(extras) = self.extras.get_mut(&name) {
                    extras.can_publish = can_publish;
                }
                let Some(subscription) = self.subs.get_mut(&name) else {
                    continue;
                };
                let mut started = start(
                    &self.connection,
                    &mut self.by_attachment,
                    supervisor.as_ref(),
                    &catalog,
                    self.change_subs.get(&name),
                    Rights { echo, can_publish },
                    subscription,
                )
                .await;
                if let Some(opened) = &opened {
                    mark_opened(&mut started, &name, opened);
                }
                frames.extend(started);
                followed.push(name);
                continue;
            }
            let Some(subscription) = self.subs.get_mut(&name) else {
                continue;
            };
            if let Some(attached) = subscription.attached() {
                let (status, counters) = status_parts(attached.running());
                if subscription.last_status.as_ref() != Some(&status) {
                    subscription.last_status = Some(status.clone());
                    frames.push(ServerFrame::Status {
                        sub: subscription.sub.clone(),
                        status,
                        counters,
                    });
                }
            }
        }
        for name in followed {
            frames.extend(self.seat(&name).await);
            frames.extend(self.join_doc(&name).await);
        }
        frames
    }

    /// Re-read the session and re-ask access for every subscription (§3 rule
    /// 6). `Err` closes the connection.
    async fn recheck(&mut self) -> Result<Vec<ServerFrame>, CloseReason> {
        if let Some(token) = &self.caller.session {
            match self.env.sessions.user_for(token).await {
                Ok(Some(user)) => {
                    // The next element of every `table_changes` subscription
                    // is decided as the user is now.
                    self.connection.set_user(Some(user.clone()));
                    self.caller.user = Some(user);
                }
                Ok(None) => {
                    return Err(CloseReason {
                        code: ErrorCode::SignedOut,
                        message: "your session ended, so this connection is closed; sign in again"
                            .to_owned(),
                    });
                }
                // The store could not answer: keep what was true a minute
                // ago and ask again next time, rather than signing everyone
                // out because a database blinked.
                Err(e) => {
                    sc_log::log_warn!(
                        "the live socket could not re-read its session: {}",
                        sc_error::format_chain(&e)
                    );
                    return Ok(Vec::new());
                }
            }
        }
        let Some(app) = self.env.app.current() else {
            return Err(CloseReason {
                code: ErrorCode::Unavailable,
                message: "this application is no longer served here".to_owned(),
            });
        };
        let names = self.subs.names();
        let mut frames = self.recheck_subs(&app, names).await;
        let kept = self.subs.names();
        frames.extend(self.refresh_rights(&kept).await);
        Ok(frames)
    }

    /// Writes this connection watches have happened: re-check every `row`
    /// subscription one of them could affect, at once rather than at the next
    /// clock (§3 rule 6).
    async fn touched(&mut self) -> Vec<ServerFrame> {
        let touches = self.connection.take_touches();
        if touches.is_empty() {
            return Vec::new();
        }
        let mut affected: Vec<String> = self
            .row_subs
            .iter()
            .filter(|(_, row)| touches.iter().any(|touch| row.affected_by(touch)))
            .map(|(sub, _)| sub.clone())
            .collect();
        affected.extend(
            self.query_subs
                .iter()
                .filter(|(_, q)| touches.iter().any(|touch| q.affected_by(touch)))
                .map(|(sub, _)| sub.clone()),
        );
        if affected.is_empty() {
            return Vec::new();
        }
        let Some(app) = self.env.app.current() else {
            return Vec::new();
        };
        let mut frames = self.recheck_subs(&app, affected.clone()).await;
        // A write that could change who may read a row could change who may
        // update it.
        let kept: Vec<String> = affected
            .into_iter()
            .filter(|name| self.subs.get(name).is_some())
            .collect();
        frames.extend(self.refresh_rights(&kept).await);
        frames
    }

    /// Ask access again for each of `names`, and revoke those that no longer
    /// pass — the same function a subscribe asks, with the same facts looked up
    /// afresh. A grant onto a different topic than the subscription is on (a
    /// stream whose topics were reconfigured) is no longer this subscription.
    async fn recheck_subs(&mut self, app: &MountedApp, names: Vec<String>) -> Vec<ServerFrame> {
        let caller = self.caller();
        let mut decided: HashMap<(String, Option<String>), Option<Option<String>>> = HashMap::new();
        let (queries, names): (Vec<String>, Vec<String>) = names
            .into_iter()
            .partition(|name| self.query_subs.contains_key(name));
        let mut frames = self.recheck_queries(app, queries).await;
        for name in names {
            let Some((stream, topic)) = self
                .subs
                .get(&name)
                .map(|s| (s.stream.clone(), s.topic.clone()))
            else {
                continue;
            };
            // Only a row subscription named its topic; every other kind is
            // asked as it was subscribed, without one.
            let asked = self.row_subs.get(&name).map(|row| row.key.clone());
            let key = (stream.clone(), asked.clone());
            let granted = match decided.get(&key) {
                Some(granted) => granted.clone(),
                None => {
                    let facts = self.facts(app, &stream, asked.as_deref(), &caller).await;
                    let granted = may_subscribe(&facts, &caller, asked.as_deref())
                        .ok()
                        .map(|grant| grant.topic);
                    decided.insert(key, granted.clone());
                    granted
                }
            };
            if granted != Some(topic)
                && let Some(gone) = self.subs.revoke(&name)
            {
                self.forget(&gone);
                frames.push(ServerFrame::Revoked { sub: name });
            }
        }
        frames
    }

    /// A subscription that has ended: forget its attachment, so anything still
    /// queued for it is dropped. Dropping it (the caller's) detaches it.
    fn forget(&mut self, gone: &Subscription<Attached>) {
        if let Some(attached) = gone.attached() {
            self.by_attachment.remove(&attached.attachment());
        }
        // Its watch goes with it, and with the last watcher of a table the
        // hub stops observing writes to it.
        self.row_subs.remove(&gone.sub);
        self.change_subs.remove(&gone.sub);
        self.query_subs.remove(&gone.sub);
        // And its seat: the member leaves the room.
        self.extras.remove(&gone.sub);
    }
}

/// Attach `subscription` to its stream if it is running here, and say so with
/// `ready` (and the replay); otherwise `ready` with a `stopped` status, and it
/// waits. A free function over the connection's parts, so a pass over every
/// subscription can call it while it holds one of them.
async fn start(
    connection: &Connection,
    by_attachment: &mut HashMap<u64, String>,
    supervisor: Option<&Arc<StreamSupervisor>>,
    catalog: &Catalog,
    change: Option<&ChangeAccess>,
    rights: Rights,
    subscription: &mut Subscription<Attached>,
) -> Vec<ServerFrame> {
    let running = supervisor.and_then(|supervisor| supervisor.by_name(&subscription.stream));
    let Some(running) = running else {
        let status = json!({ "status": "stopped" });
        subscription.last_status = Some(status.clone());
        return vec![ServerFrame::Ready {
            sub: subscription.sub.clone(),
            stream: subscription.stream.clone(),
            element_type: Json::Null,
            replayed: 0,
            can_publish: false,
            status,
            counters: Json::Null,
            opened: None,
        }];
    };
    attach(
        connection,
        by_attachment,
        catalog,
        change,
        rights,
        subscription,
        &running,
    )
    .await
}

/// Say on `sub`'s `ready` what its stream query opened (TODO F7).
fn mark_opened(frames: &mut [ServerFrame], sub: &str, what: &Opened) {
    for frame in frames {
        if let ServerFrame::Ready {
            sub: ready, opened, ..
        } = frame
            && ready == sub
        {
            *opened = Some(what.clone());
        }
    }
}

/// The whole document `seat` is in, as `doc_sync` for `sub`.
fn doc_sync(sub: &str, seat: &DocSeat, can_edit: bool) -> ServerFrame {
    match seat.state() {
        Ok(state) => ServerFrame::DocSync {
            sub: sub.to_owned(),
            update: BASE64.encode(state),
            read_only: !can_edit,
        },
        Err(e) => FrameError::invalid(Some(sub.to_owned()), sc_error::format_chain(&e)).frame(),
    }
}

/// What a subscription is attached with besides its stream and topic.
#[derive(Debug, Clone, Copy)]
struct Rights {
    /// Whether this connection's own elements come back to it.
    echo: bool,
    /// Whether the page may publish on it — `ready`'s `can_publish`.
    can_publish: bool,
}

/// Attach `subscription` to `running`: `ready`, then the replay. A
/// `table_changes` subscription is attached with what its elements are decided
/// by.
async fn attach(
    connection: &Connection,
    by_attachment: &mut HashMap<u64, String>,
    catalog: &Catalog,
    change: Option<&ChangeAccess>,
    rights: Rights,
    subscription: &mut Subscription<Attached>,
    running: &Arc<RunningStream>,
) -> Vec<ServerFrame> {
    let (attached, replay) = match change {
        Some(change) => connection.attach_changes(running, change.clone()).await,
        None => {
            connection
                .attach_echo(running, subscription.topic.clone(), rights.echo)
                .await
        }
    };
    by_attachment.insert(attached.attachment(), subscription.sub.clone());
    if let Some(previous) = subscription.activate(attached) {
        by_attachment.remove(&previous.attachment());
    }
    let (status, counters) = status_parts(running);
    subscription.last_status = Some(status.clone());
    let mut frames = Vec::with_capacity(replay.len() + 1);
    frames.push(ServerFrame::Ready {
        sub: subscription.sub.clone(),
        stream: subscription.stream.clone(),
        element_type: element_type_json_in(running, catalog),
        replayed: replay.len(),
        can_publish: rights.can_publish,
        status,
        counters,
        opened: None,
    });
    frames.extend(replay.into_iter().map(|envelope| ServerFrame::Element {
        sub: subscription.sub.clone(),
        envelope: (*envelope).clone(),
    }));
    frames
}

/// The `users` columns `shown` of `user`, read now — what a presence member
/// shows the others (§3 rule 9). Nothing for an anonymous page, and a column
/// that is not there is left out rather than shown as null.
async fn shown_fields(
    catalog: &Catalog,
    user: Option<&User>,
    shown: &[String],
) -> serde_json::Map<String, Json> {
    let mut fields = serde_json::Map::new();
    let Some(user) = user else {
        return fields;
    };
    if shown.is_empty() {
        return fields;
    }
    // Read afresh rather than taken from the session's copy, which can be a
    // minute old: a name changed a moment ago is the name shown.
    let current = match sc_auth::load_user(catalog, user.id).await {
        Ok(Some(current)) => current,
        Ok(None) => return fields,
        Err(e) => {
            sc_log::log_warn!(
                "the live socket could not read the fields a presence member shows: {}",
                sc_error::format_chain(&e)
            );
            user.clone()
        }
    };
    for name in shown {
        // Never the hash, whatever the settings say (`users` rows never carry
        // it onto a `User`, and the stream form refuses it).
        if let Some(value) = current.extra.get(name) {
            fields.insert(name.clone(), sc_types::value_to_json(value));
        }
    }
    fields
}

/// One `presence_diff`'s worth of changes to a room.
#[derive(Debug, Default, Clone, PartialEq)]
struct Diff {
    joined: Vec<Json>,
    left: Vec<String>,
    updated: Vec<Json>,
}

/// Group a connection's presence changes, in order, into `presence_diff`s per
/// room. A client applies a diff's `joined`, then `updated`, then `left`, so a
/// change that would be applied out of order in the current diff — a join, or
/// an update, of a connection that already left in it — starts the next one.
/// Updates of one member within a diff keep only the latest.
fn presence_diffs(events: Vec<(RoomKey, PresenceEvent)>) -> Vec<(RoomKey, Diff)> {
    let mut out: Vec<(RoomKey, Diff)> = Vec::new();
    // Per room, the index in `out` of its diff being filled.
    let mut open: HashMap<RoomKey, usize> = HashMap::new();
    for (room, event) in events {
        let connection = match &event {
            PresenceEvent::Joined(member) => member
                .get("connection")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned(),
            PresenceEvent::Left(connection) | PresenceEvent::Updated { connection, .. } => {
                connection.clone()
            }
        };
        let starts_over = !matches!(event, PresenceEvent::Left(_))
            && open
                .get(&room)
                .is_some_and(|at| out[*at].1.left.contains(&connection));
        let at = match open.get(&room) {
            Some(at) if !starts_over => *at,
            _ => {
                out.push((room.clone(), Diff::default()));
                open.insert(room.clone(), out.len() - 1);
                out.len() - 1
            }
        };
        let diff = &mut out[at].1;
        match event {
            PresenceEvent::Joined(member) => diff.joined.push(member),
            PresenceEvent::Left(connection) => diff.left.push(connection),
            PresenceEvent::Updated { connection, state } => {
                diff.updated
                    .retain(|u| u.get("connection").and_then(Json::as_str) != Some(&connection));
                diff.updated
                    .push(json!({ "connection": connection, "state": state }));
            }
        }
    }
    out
}

/// Resolves at `at`, or never.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The answer to a frame about a subscription this connection does not have.
fn not_subscribed(sub: String) -> ServerFrame {
    FrameError::invalid(
        Some(sub.clone()),
        format!("`{sub}` is not subscribed on this connection"),
    )
    .frame()
}

/// Send `frames` in order. `false` when the socket has gone.
async fn send_all(sender: &mut Sender, frames: Vec<ServerFrame>) -> bool {
    for frame in frames {
        if sender.send(Message::text(frame.to_text())).await.is_err() {
            return false;
        }
    }
    true
}

/// Close the connection, saying why twice: as an `error` frame a client's code
/// reads, and as the close frame's reason a browser's devtools shows.
async fn close(sender: &mut Sender, code: ErrorCode, message: &str) {
    let frame = FrameError::new(None, code, message).frame();
    let _ = sender.send(Message::text(frame.to_text())).await;
    let _ = sender
        .send(Message::Close(Some(CloseFrame {
            code: close_code::POLICY,
            reason: fit_close_reason(message.to_owned()).into(),
        })))
        .await;
    let _ = sender.close().await;
}

/// A clock that first ticks one `period` from now (not immediately), and that
/// does not try to catch up on ticks it missed.
fn ticker(period: Duration) -> Interval {
    let period = period.max(Duration::from_millis(1));
    let mut interval = tokio::time::interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_stream::StreamId;

    fn room(topic: &str) -> RoomKey {
        (StreamId(uuid::Uuid::from_u128(1)), Some(topic.to_owned()))
    }

    fn joined(connection: &str) -> PresenceEvent {
        PresenceEvent::Joined(json!({ "connection": connection, "user_id": null, "state": null }))
    }

    fn updated(connection: &str, state: Json) -> PresenceEvent {
        PresenceEvent::Updated {
            connection: connection.to_owned(),
            state,
        }
    }

    #[test]
    fn changes_to_one_room_are_one_diff_with_the_latest_state() {
        let diffs = presence_diffs(vec![
            (room("1"), joined("a")),
            (room("1"), updated("a", json!(1))),
            (room("1"), updated("a", json!(2))),
            (room("2"), joined("b")),
            (room("1"), PresenceEvent::Left("c".to_owned())),
        ]);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].0, room("1"));
        assert_eq!(diffs[0].1.joined.len(), 1);
        assert_eq!(
            diffs[0].1.updated,
            vec![json!({ "connection": "a", "state": 2 })]
        );
        assert_eq!(diffs[0].1.left, vec!["c".to_owned()]);
        assert_eq!(diffs[1].0, room("2"));
    }

    #[test]
    fn a_return_after_a_leave_is_a_second_diff_so_it_is_applied_after_it() {
        // Applied as one diff — joined, then left — `a` would end up gone.
        let diffs = presence_diffs(vec![
            (room("1"), PresenceEvent::Left("a".to_owned())),
            (room("1"), joined("a")),
        ]);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].1.left, vec!["a".to_owned()]);
        assert_eq!(diffs[1].1.joined.len(), 1);
        // A join and then a leave in one diff is applied in that order.
        let diffs = presence_diffs(vec![
            (room("1"), joined("b")),
            (room("1"), PresenceEvent::Left("b".to_owned())),
        ]);
        assert_eq!(diffs.len(), 1);
    }
}

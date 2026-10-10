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
//! ## Following the stream
//!
//! A subscription to a stream that is not running here (disabled, or failing
//! to start) gets `ready` with a `stopped` status and waits; when the stream
//! starts, or restarts because its configuration changed, the status pass
//! attaches it and sends `ready` again, so the page follows the flow without
//! reconnecting.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use sc_auth::{SessionStore, User};
use sc_catalog::Catalog;
use sc_live::{
    Attached, ClientFrame, CloseReason, Connection, ConnectionInfo, Delivery, Denied, ErrorCode,
    FrameError, LiveHub, LiveLimits, ServerFrame, StoredStream, StreamFacts, Subscription,
    SubscriptionSet, caller_role, may_subscribe,
};
use sc_stream::{RunningStream, Stream, StreamSupervisor, TopicSpec};
use serde_json::{Value as Json, json};
use tokio::time::{Instant, Interval, MissedTickBehavior};

use crate::apps::{AppMounts, MountedApp};
use crate::observe::{element_type_json, fit_close_reason, status_parts};

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
    pub app: AppHandle,
    /// The application's subdomain, which the per-user connection limit is
    /// counted within.
    pub subdomain: String,
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
    Recheck,
    Status,
    Ping,
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
    let mut live = Live {
        subs: SubscriptionSet::new(limits.max_subscriptions),
        by_attachment: HashMap::new(),
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
        let event = tokio::select! {
            reason = close_signal.closed() => Event::Closed(reason),
            incoming = receiver.next() => Event::Incoming(incoming),
            Some(delivery) = deliveries.recv() => Event::Delivery(delivery),
            () = live.connection.lagged() => Event::Lagged,
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
            Event::Recheck => match live.recheck().await {
                Ok(frames) => frames,
                Err(reason) => break Some(reason),
            },
            Event::Status => live.status_pass().await,
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
    connection: Connection,
    env: LiveEnv,
    caller: LiveCaller,
    limits: LiveLimits,
}

impl Live {
    /// One message from the client.
    async fn receive(&mut self, message: Message) -> Vec<ServerFrame> {
        match message {
            Message::Text(text) => {
                if text.len() > self.limits.max_frame_bytes {
                    return vec![
                        FrameError::new(
                            None,
                            ErrorCode::TooLarge,
                            format!(
                                "a frame may be at most {} bytes and this one was {}; it was not read",
                                self.limits.max_frame_bytes,
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
            } => self.subscribe(sub, stream, topic, filter).await,
            ClientFrame::Unsubscribe { sub } => match self.subs.remove(&sub) {
                Ok(gone) => {
                    self.forget(&gone);
                    Vec::new()
                }
                Err(error) => vec![error.frame()],
            },
            ClientFrame::Publish { sub, .. } => match self.subs.get(&sub) {
                // Not a stream a page may publish on — the only kind there is
                // until L4's `internal` streams with `client_publish` — and
                // worded like any other "not here".
                Some(found) => vec![
                    FrameError::new(
                        Some(sub.clone()),
                        ErrorCode::Unavailable,
                        format!("there is nowhere on `{}` you can publish", found.stream),
                    )
                    .frame(),
                ],
                None => vec![not_subscribed(sub)],
            },
            ClientFrame::Presence { sub, .. } => match self.subs.get(&sub) {
                Some(found) => vec![
                    FrameError::invalid(
                        Some(sub.clone()),
                        format!("`{}` has no presence", found.stream),
                    )
                    .frame(),
                ],
                None => vec![not_subscribed(sub)],
            },
            ClientFrame::DocUpdate { sub, .. } => match self.subs.get(&sub) {
                Some(found) => vec![
                    FrameError::invalid(
                        Some(sub.clone()),
                        format!("`{}` is not a document", found.stream),
                    )
                    .frame(),
                ],
                None => vec![not_subscribed(sub)],
            },
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
    ) -> Vec<ServerFrame> {
        if let Err(error) = self.subs.check_new(&sub) {
            return vec![error.frame()];
        }
        let Some(app) = self.env.app.current() else {
            return vec![FrameError::unavailable(&sub, &stream).frame()];
        };
        let facts = self.facts(&app, &stream).await;
        let role = caller_role(self.caller.user.as_ref().map(|user| user.role));
        let stored: StoredStream = match may_subscribe(&facts, role) {
            Ok(stored) => stored.clone(),
            Err(Denied) => return vec![FrameError::unavailable(&sub, &stream).frame()],
        };
        // The caller may know the stream is there now, so a malformed request
        // for it is answered as one.
        if topic.is_some() && !stored.topics.takes_topic() {
            return vec![
                FrameError::invalid(
                    Some(sub),
                    format!("`{stream}` has no topics; subscribe to it without one"),
                )
                .frame(),
            ];
        }
        if filter.is_some() {
            return vec![
                FrameError::invalid(
                    Some(sub),
                    format!("`{stream}` takes no filter; only a stream of row changes does"),
                )
                .frame(),
            ];
        }
        let mut subscription = Subscription::waiting(sub, stream, topic);
        let frames = start(
            &self.connection,
            &mut self.by_attachment,
            self.env.supervisor.as_ref(),
            &mut subscription,
        )
        .await;
        // `check_new` passed above and nothing has been added since, so this
        // cannot fail; if it somehow did, dropping the subscription detaches it.
        let _ = self.subs.insert(subscription);
        frames
    }

    /// What access to `name` is decided by, for `app`.
    async fn facts(&self, app: &MountedApp, name: &str) -> StreamFacts {
        let exposed = app.app.exposes_stream(name);
        if !exposed {
            // Not even read: an unexposed stream does not exist for this app.
            return StreamFacts {
                exposed,
                stored: None,
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
        StreamFacts { exposed, stored }
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
            None => sc_app::stream_registry(),
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
        for subscription in self.subs.iter_mut() {
            let current = supervisor
                .as_ref()
                .and_then(|supervisor| supervisor.by_name(&subscription.stream));
            let follow = match (subscription.attached(), &current) {
                (Some(attached), Some(current)) => !Arc::ptr_eq(attached.running(), current),
                (None, Some(_)) => true,
                _ => false,
            };
            if follow {
                frames.extend(
                    start(
                        &self.connection,
                        &mut self.by_attachment,
                        supervisor.as_ref(),
                        subscription,
                    )
                    .await,
                );
                continue;
            }
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
        frames
    }

    /// Re-read the session and re-ask access for every subscription (§3 rule
    /// 6). `Err` closes the connection.
    async fn recheck(&mut self) -> Result<Vec<ServerFrame>, CloseReason> {
        if let Some(token) = &self.caller.session {
            match self.env.sessions.user_for(token).await {
                Ok(Some(user)) => self.caller.user = Some(user),
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
        let role = caller_role(self.caller.user.as_ref().map(|user| user.role));
        let mut decided: HashMap<String, bool> = HashMap::new();
        let mut frames = Vec::new();
        for name in self.subs.names() {
            let Some(stream) = self.subs.get(&name).map(|s| s.stream.clone()) else {
                continue;
            };
            let allowed = match decided.get(&stream) {
                Some(allowed) => *allowed,
                None => {
                    let facts = self.facts(&app, &stream).await;
                    let allowed = may_subscribe(&facts, role).is_ok();
                    decided.insert(stream, allowed);
                    allowed
                }
            };
            if !allowed && let Some(gone) = self.subs.revoke(&name) {
                self.forget(&gone);
                frames.push(ServerFrame::Revoked { sub: name });
            }
        }
        Ok(frames)
    }

    /// A subscription that has ended: forget its attachment, so anything still
    /// queued for it is dropped. Dropping it (the caller's) detaches it.
    fn forget(&mut self, gone: &Subscription<Attached>) {
        if let Some(attached) = gone.attached() {
            self.by_attachment.remove(&attached.attachment());
        }
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
        }];
    };
    attach(connection, by_attachment, subscription, &running).await
}

/// Attach `subscription` to `running`: `ready`, then the replay.
async fn attach(
    connection: &Connection,
    by_attachment: &mut HashMap<u64, String>,
    subscription: &mut Subscription<Attached>,
    running: &Arc<RunningStream>,
) -> Vec<ServerFrame> {
    let (attached, replay) = connection.attach(running, subscription.topic.clone()).await;
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
        element_type: element_type_json(running),
        replayed: replay.len(),
        can_publish: false,
        status,
        counters,
    });
    frames.extend(replay.into_iter().map(|envelope| ServerFrame::Element {
        sub: subscription.sub.clone(),
        envelope: (*envelope).clone(),
    }));
    frames
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

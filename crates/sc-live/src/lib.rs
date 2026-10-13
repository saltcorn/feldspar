//! Live updates: the server pushing to an application's page while it is open
//! (TODO.md "Live updates", milestones L1–L6; the technical design's §14.10).
//!
//! Everything that moves reaches the browser **through a stream**, and a page
//! holds **one** WebSocket, `GET {mount}/live`, carrying any number of
//! subscriptions to the streams its application exposes. This crate is that
//! socket minus the socket: what is said on it, what one connection may hold,
//! who may subscribe to what, and the hub that fans a running stream's elements
//! out to the subscriptions waiting for them. `sc-server` mounts the route,
//! authenticates the upgrade and drives a connection through these types.
//!
//! | Module | What it holds |
//! | --- | --- |
//! | [`protocol`] | The frames, both directions, and the error codes |
//! | [`subscription`] | One connection's subscriptions and their states |
//! | [`topic`] | Subscriptions bucketed by topic, for the fan-out |
//! | [`access`] | Who may subscribe, as pure functions over looked-up facts |
//! | [`limits`] | The per-connection limits and clocks, as configuration |
//! | [`hub`] | Connections, one fan-out task per running stream, publishing, the write watch, and handing committed writes to `table_changes` streams |
//! | [`rows`] | Which tables a row topic's read rule reads |
//! | [`filter`] | A `table_changes` subscription's filter, in the REST read's vocabulary, evaluated in Rust |
//! | [`changes`] | What each subscriber of a `table_changes` stream is sent: the decision table, the per-element read checks, the counters |
//! | [`doc`] | A collaborative document's CRDT state through `yrs`: apply, merge, read the text, edit |
//! | [`docroom`] | One room per open document: load, apply, relay, persist (debounced and on close) |
//! | [`presence`] | Who is on each topic of a stream with presence: the rooms, the members, and each connection's inbox of changes |
//!
//! ## Where this sits
//!
//! Layer 8, beside `sc-app`: above `sc-stream`, whose
//! running streams it subscribes to and publishes into, `sc-auth`, whose roles
//! it decides by and whose session ends it listens for, `sc-catalog`, whose
//! write seam tells it a row topic's access may have changed, and `sc-api`,
//! whose row layer is where a `table_changes` element's projection, filter
//! vocabulary and read rule come from (one rule, not a copy of it); below
//! `sc-server`. It implements `sc-expr`'s `LiveHost`, which is how the
//! `publish` action and a code body's `live.publish` reach it without naming
//! it.
//!
//! ## The rules every later milestone keeps
//!
//! - **Denial looks like absence.** Unknown, unexposed and forbidden are one
//!   `unavailable` frame ([`FrameError::unavailable`]).
//! - **Access is re-checked while the socket is open.** A subscription is a
//!   standing read; the socket re-reads its session and re-asks [`access`] on
//!   a clock ([`LiveLimits::recheck_interval`]), and a sign-out on this node
//!   closes the session's sockets at once ([`LiveHub`] is a
//!   [`SessionListener`](sc_auth::SessionListener)).
//! - **Nobody blocks the flow.** A connection's queue is bounded; what does
//!   not fit is dropped, counted and reported as `lagged`.

pub mod access;
pub mod changes;
pub mod doc;
pub mod docroom;
pub mod filter;
pub mod hub;
pub mod limits;
pub mod presence;
pub mod protocol;
pub mod rows;
pub mod subscription;
pub mod topic;

pub use access::{
    Caller, Denied, Grant, PublishFacts, Refusal, RowFact, StoredStream, StreamFacts, caller_role,
    may_open, may_publish, may_subscribe,
};
pub use changes::{
    CallerCell, ChangeAccess, ChangeCounters, ChangeCounts, Delivered, Suppressed, deliver_to,
    suppressed_by,
};
pub use doc::{CollabDoc, DocShape, MirrorFormat, RootKind};
pub use docroom::{DocInbox, DocKey, DocNews, DocRefusal, DocRoom, DocRooms, DocSeat, DocTarget};
pub use filter::{LikeRules, RowFilter};
pub use hub::{
    Attached, ChangeWatch, CloseReason, CloseSignal, Connection, ConnectionId, ConnectionInfo,
    Delivery, LiveHub, Seat, Touch, Watch, topic_of,
};
pub use limits::{LiveLimits, RateLimit};
pub use presence::{Member, PresenceEvent, PresenceNews, RoomKey};
pub use protocol::{ClientFrame, ErrorCode, FrameError, Opened, ServerFrame};
pub use rows::read_rule_tables;
pub use subscription::{Phase, Subscription, SubscriptionSet};
pub use topic::TopicIndex;

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
//! | [`hub`] | Connections, and one fan-out task per running stream |
//!
//! ## Where this sits
//!
//! Layer 7, beside `sc-workflow` and `sc-agent`: above `sc-stream`, whose
//! running streams it subscribes to, and `sc-auth`, whose roles it decides by
//! and whose session ends it listens for; below `sc-server`. From L2 it also
//! sits above the ownership evaluator (`sc-expr`), because a row topic is
//! authorised by the row's read rule.
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
pub mod hub;
pub mod limits;
pub mod protocol;
pub mod subscription;
pub mod topic;

pub use access::{Denied, StoredStream, StreamFacts, caller_role, may_subscribe};
pub use hub::{
    Attached, CloseReason, CloseSignal, Connection, ConnectionId, ConnectionInfo, Delivery, LiveHub,
};
pub use limits::LiveLimits;
pub use protocol::{ClientFrame, ErrorCode, FrameError, ServerFrame};
pub use subscription::{Phase, Subscription, SubscriptionSet};
pub use topic::TopicIndex;

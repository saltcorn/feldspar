//! Streams: dataflows as an entity (layer 6; TODO "Streams", which becomes the
//! technical design's §14.3).
//!
//! Everything else Saltcorn holds is **at rest**: a table has rows, a file has
//! bytes, a model has a fit. The one thing that moves is an event, and every
//! event this system knows how to raise, it raises itself — a write, a login, a
//! clock. Nothing could tell it about the world from outside except by calling
//! in over HTTP. A temperature sensor publishing to an MQTT broker, a market
//! feed, a queue of jobs from another system are not rows and they are not
//! requests. They are **dataflows**, and this crate is what makes one an entity
//! an admin can create, observe, expose and trigger on.
//!
//! A stream is a `Model` whose provider has been replaced by a subscription, or
//! a `Trigger` whose event comes from outside, and the shape is the one this
//! tree has built four times: a **provider is code declaring its settings as
//! [`FormField`]s** ([`StreamProvider`]), an **entity is a row that is its own
//! definition** ([`Stream`] and [`store`]), and the admin UI **renders a
//! provider it has never heard of**.
//!
//! ## The three places a flow is genuinely not a fit or a row
//!
//! - **An element is not stored.** There is no `_fd_stream_elements` table and
//!   no retention setting. A stream is a flow, and what makes it durable is a
//!   trigger that writes a row — a thing the admin already knows how to build
//!   and can see, query, back up and give away. The Observe screen's history is
//!   a small in-memory ring, labelled "since this server started". See
//!   [`element`] and [`envelope`].
//! - **Nobody may block the flow.** A broker does not wait for an admin's
//!   browser. [`StreamSink::deliver`] is synchronous and infallible so that no
//!   consumer *can* push back; one that cannot keep up is told it lagged and
//!   loses elements, which is the honest outcome rather than an unbounded queue
//!   with a memory leak in it. See [`provider`].
//! - **A subscription is process-local and long-lived**, where every other
//!   extension point in this tree is a call that returns. [`subscribe`] hands
//!   back a [`Subscription`] whose `Drop` stops the flow. See [`subscription`].
//!
//! ## Where this sits, and the three seams
//!
//! Layer 6 — `sc-model`'s exact placement, for `sc-model`'s exact reason: a
//! module supplies providers (layer 6 is where a module host can reach it) and
//! the rows this crate stores go through the `Catalog` (which fixes it above
//! layer 4). It therefore depends on nothing above layer 4 and knows nothing
//! about triggers, applications or sockets. What it cannot do itself, it
//! declares:
//!
//! | Seam | Declared here | Implemented in | Installed by |
//! | --- | --- | --- | --- |
//! | [`StreamProviderHost`] — a module's providers | `provider` | `sc-module::stream_providers` | `sc-server` at boot and on module change |
//! | [`StreamSink`] — where a provider's element goes | `provider` | [`supervisor`] (the tap) | the supervisor, per connection |
//! | [`StreamConsumer`] — where a delivered element goes | `supervisor` | `sc-server::streams` | `sc-server` at boot |
//! | [`StreamObserver`] — a stream set that changed | `observer` | `sc-server` (mount registry) | `sc-server` at boot |
//!
//! That is what lets the supervisor be tested with a sink that appends to a
//! `Vec` and a provider that reads from a script — see [`testing`], behind the
//! `testing` feature.
//!
//! ## The one limitation, said out loud
//!
//! **One process, one subscription.** Two servers against one database both
//! subscribe, so a stream trigger fires twice. That is real and it is not a bug
//! to be discovered: `sc-bus` does not exist, and until it does a flow is
//! process-local. MQTT's own shared subscriptions (`$share/`) are the escape
//! hatch an admin has today.
//!
//! [`FormField`]: sc_types::FormField
//! [`subscribe`]: StreamProvider::subscribe

pub mod element;
pub mod envelope;
pub mod observer;
pub mod polling;
pub mod provider;
pub mod providers;
pub mod registry;
pub mod running;
pub mod secrets;
pub mod store;
pub mod stream;
pub mod subscription;
pub mod supervisor;
pub mod validate;

#[cfg(feature = "testing")]
#[cfg_attr(docsrs, doc(cfg(feature = "testing")))]
pub mod testing;

pub use element::{ElementField, ElementType, RawPayload, UTF8};
pub use envelope::{Element, Envelope};
pub use observer::StreamObserver;
pub use polling::{
    DEFAULT_INTERVAL_S, INTERVAL_FIELD, MIN_INTERVAL, PollAnswer, PollHost, PollingProvider,
};
pub use provider::{StreamProvider, StreamProviderHost, StreamProviderKind, StreamSink, TopicSpec};
#[cfg(feature = "mqtt")]
pub use providers::mqtt::{MQTT, Mqtt};
pub use providers::{BUILTINS_COMPILED_OUT, MQTT_COMPILED_IN, builtin_providers, builtin_registry};
pub use registry::StreamRegistry;
pub use running::{Counters, ElementFeed, RunningStream, StreamStatus};
pub use secrets::{redact_configuration, redacted_stream, restore_secrets};
pub use store::{
    STREAMS_TABLE, bootstrap_streams, delete_stream, list_streams, load_stream,
    load_stream_by_name, require_stream, save_stream, trigger_referent,
};
pub use stream::{ATTR_ENABLED, Stream, StreamId};
pub use subscription::{Stop, Subscription};
pub use supervisor::{Delivery, StreamConfig, StreamConsumer, StreamSupervisor, backoff_delay};
pub use validate::{check_stream, check_stream_name, validate_stream};

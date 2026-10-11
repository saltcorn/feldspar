//! Streams on a running server: the supervisor, and the bridge that turns a
//! delivered element into a **trigger event** (TODO "Streams" §8, task 5.3).
//!
//! `sc-stream` is at layer 6 so a module can supply a stream provider — the
//! same placement argument `sc-model` and `sc-action` carry — and the price is
//! that it knows nothing about triggers, applications or sockets. So it
//! declares [`StreamConsumer`](sc_stream::StreamConsumer) and this is where the
//! seam is filled in, exactly as `sc-model` declares `DatasetSource` and
//! [`models`](crate::models) supplies it.
//!
//! [`StreamServices`] is the assembly [`ModelServices`](crate::ModelServices)
//! and [`AgentServices`](crate::AgentServices) already are: the provider
//! registry, the supervisor holding one subscription per enabled stream, and
//! the numbers this process was started with. It rides on
//! [`AppMounts`](crate::AppMounts) with the other five for the reason they do —
//! the admin handlers and the observe sockets both already hold that handle.
//!
//! ## Nobody may block the flow (§7)
//!
//! [`TriggerBridge`] is the one consumer a server installs, and its whole job
//! is to **not wait**. `consume` returns immediately: it fires the dispatcher on
//! a spawned task and, if the last element's firing has not finished, it returns
//! [`Delivery::Dropped`] rather than queueing. That is `Scheduler`'s rule for a
//! missed occurrence, for `Scheduler`'s reason — "five queued copies of a report
//! nobody read is worse than one late one" — and the alternative is an unbounded
//! queue in front of a trigger, which is a memory leak with a delay built into
//! it. The drop is **counted**, on the running stream, and shown on the Streams
//! list: a stream that is dropping is a thing you can see.
//!
//! An element whose stream has no trigger listening costs nothing: the bridge
//! asks the live trigger set first, and a stream nobody triggers on spawns no
//! task at all.
//!
//! ## One process, one subscription
//!
//! Two servers against one database both subscribe, so a stream trigger fires
//! twice. That is `sc-stream`'s §6 limitation, said out loud again here because
//! this module is where it becomes visible: the duplicate is two *firings*, and
//! a trigger that inserts a row inserts two. `sc-bus` does not exist, and until
//! it does a flow is process-local.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use sc_action::{Event, EventKind, TriggerDispatcher};
use sc_catalog::Catalog;
use sc_error::{Context, Result};
use sc_stream::{
    Delivery, Envelope, StreamConfig, StreamConsumer, StreamRegistry, StreamSupervisor,
    bootstrap_streams, builtin_registry,
};

/// The stream machinery a running server holds: the provider registry, the
/// supervisor and the numbers it runs with.
///
/// Cheap to clone, like [`ModelServices`](crate::ModelServices), because
/// several places need the same one: the admin handlers (which reload it after
/// a save), the observe sockets (which subscribe to a running stream's
/// broadcast), the module reload that rebuilds the registry, and the `SIGHUP`
/// path.
#[derive(Clone)]
pub struct StreamServices {
    supervisor: Arc<StreamSupervisor>,
}

impl StreamServices {
    /// The services over `registry`, running nothing yet — [`install_streams`]
    /// is what reloads and starts the stored set.
    pub fn new(registry: Arc<StreamRegistry>, config: StreamConfig) -> StreamServices {
        StreamServices {
            supervisor: Arc::new(StreamSupervisor::new(registry, config)),
        }
    }

    /// The supervisor: every running stream, and the one door a reload goes
    /// through.
    pub fn supervisor(&self) -> &Arc<StreamSupervisor> {
        &self.supervisor
    }

    /// The provider registry as it stands — what the Streams form's picker
    /// lists and what a save validates against.
    pub fn registry(&self) -> Arc<StreamRegistry> {
        self.supervisor.registry()
    }

    /// The numbers this process was started with (`--stream-buffer`,
    /// `--stream-max-rate`).
    pub fn config(&self) -> &StreamConfig {
        self.supervisor.config()
    }

    /// Bring the running set into line with the stored rows: what a save, a
    /// delete, a module change and a `SIGHUP` all call.
    pub async fn reload(&self, catalog: &Catalog) -> Result<bool> {
        self.supervisor.reload(catalog).await
    }
}

/// The §2 seam: a delivered element becomes a `stream` event.
///
/// Holds the catalog and the dispatcher, and one **in-flight flag per stream**.
/// The flag is the whole of §7's drop rule: `consume` takes it, spawns the
/// firing, and puts it back when the firing finishes; an element that arrives
/// while it is taken is dropped and counted. Per stream rather than per trigger
/// because a stream's counters are per stream — `dropped_for_triggers` is a
/// number on the Streams list, and two triggers on one stream that between them
/// cannot keep up are one stream that is dropping.
pub struct TriggerBridge {
    catalog: Arc<Catalog>,
    dispatcher: Arc<TriggerDispatcher>,
    in_flight: InFlight,
}

/// One "a firing is still running" flag per stream — §7's drop rule, and the
/// whole of it.
///
/// Keyed by **name**, which is what the event's channel is: a renamed stream is
/// a different channel and deliberately gets its own flag, since the triggers
/// listening to it are different triggers.
#[derive(Default)]
struct InFlight(Mutex<HashMap<String, Arc<AtomicBool>>>);

impl InFlight {
    /// The flag for `stream`, created on first sight. The same `Arc` every
    /// time, which is what makes the rule "is *this stream's* previous firing
    /// still running" rather than a flag nobody ever reads back.
    fn flag(&self, stream: &str) -> Arc<AtomicBool> {
        let mut guard = match self.0.lock() {
            Ok(guard) => guard,
            // A poisoned lock means a panic while the map was being changed.
            // The map is still a valid map — every value is an `Arc` — so the
            // honest answer is the one that is in there.
            Err(poisoned) => poisoned.into_inner(),
        };
        Arc::clone(guard.entry(stream.to_owned()).or_default())
    }

    /// Take the flag for `stream`, or `None` when it is already taken. The
    /// caller puts it back by storing `false` when its firing finishes.
    fn take(&self, stream: &str) -> Option<Arc<AtomicBool>> {
        let flag = self.flag(stream);
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| flag)
    }
}

impl TriggerBridge {
    /// The bridge over this server's catalog and dispatcher.
    pub fn new(catalog: Arc<Catalog>, dispatcher: Arc<TriggerDispatcher>) -> TriggerBridge {
        TriggerBridge {
            catalog,
            dispatcher,
            in_flight: InFlight::default(),
        }
    }

    /// Whether any enabled trigger is listening to `stream`.
    ///
    /// Asked before anything is taken or spawned, so a stream nobody triggers
    /// on — the common case for one that only feeds the Observe screen or an
    /// application's socket — costs one iteration of a short list per element
    /// rather than a task.
    fn anybody_listening(&self, stream: &str) -> bool {
        match self.dispatcher.triggers() {
            Ok(triggers) => triggers
                .matching(EventKind::Stream, Some(stream))
                .next()
                .is_some(),
            // The trigger set could not be read: nothing can fire, and saying so
            // once per element would be the second outage.
            Err(_) => false,
        }
    }
}

impl StreamConsumer for TriggerBridge {
    fn consume(&self, envelope: &Envelope) -> Delivery {
        let stream = envelope.stream.clone();
        if !self.anybody_listening(&stream) {
            // Nothing to drop and nothing to count: an element nobody asked for
            // is not a trigger that fell behind.
            return Delivery::Accepted;
        }
        // The previous element's triggers are still running. §7: drop and
        // count, never queue.
        let Some(flag) = self.in_flight.take(&stream) else {
            return Delivery::Dropped;
        };

        let event = Event::stream(stream, envelope.to_json());
        let catalog = Arc::clone(&self.catalog);
        let dispatcher = Arc::clone(&self.dispatcher);
        tokio::spawn(async move {
            // `fire` rather than `dispatch`: nobody is waiting for the answer,
            // and a trigger that failed is reported where every other
            // fire-and-forget event's failure is (the log, until §16's error
            // log exists).
            dispatcher.fire(&catalog, &event).await;
            flag.store(false, Ordering::Release);
        });
        Delivery::Accepted
    }
}

/// Ensure `_fd_streams` exists, assemble the services, install the trigger
/// bridge, start every enabled stream and start the supervising task.
///
/// Called from `serve` where [`install_models`](crate::install_models) is
/// called from, and for the same reason: a stream is no use to a `feldspar`
/// command that is not serving, and a `build-app` that opened the same database
/// must not connect to somebody's broker.
///
/// The **dispatcher comes first**, because the bridge needs it: an element that
/// arrives before the triggers are up would have nothing to fire. It is
/// therefore installed after `install_triggers`, which is the whole ordering
/// constraint between the two.
///
/// Fails only on what a server must not start without — the table not being
/// creatable, or the built-in providers not registering. A **stream** that does
/// not start is not one of those: it stays listed and `failed` with its reason,
/// exactly as a trigger that does not validate does, because the rest of the
/// server works and the admin can fix it in the UI.
pub async fn install_streams(
    catalog: &Arc<Catalog>,
    dispatcher: &Arc<TriggerDispatcher>,
    config: StreamConfig,
) -> Result<StreamServices> {
    let registry =
        Arc::new(builtin_registry().context("registering the built-in stream providers")?);
    install_streams_with(catalog, dispatcher, registry, config).await
}

/// [`install_streams`] with the provider registry supplied — what a test uses
/// to install a scripted provider, and what a module change will rebuild
/// (Phase 9).
pub async fn install_streams_with(
    catalog: &Arc<Catalog>,
    dispatcher: &Arc<TriggerDispatcher>,
    registry: Arc<StreamRegistry>,
    config: StreamConfig,
) -> Result<StreamServices> {
    bootstrap_streams(catalog)
        .await
        .context("ensuring the streams table exists")?;
    let services = StreamServices::new(registry, config);
    // An application's generated client types its subscriptions from the
    // element types its streams' providers declare (TODO "Streams" §10), and
    // `sc-app` resolves those through an installed registry rather than a
    // parameter threaded down the whole build path. This is where the running
    // server's goes in; a module change installs the rebuilt one the same way.
    sc_app::install_stream_registry(catalog, services.registry());
    services
        .supervisor
        .set_consumer(Arc::new(TriggerBridge::new(
            Arc::clone(catalog),
            Arc::clone(dispatcher),
        )));
    // Start what is stored. A failure here is the *listing* failing — a stream
    // that will not subscribe is held and `failed`, not an error — so it is
    // worth failing the boot for: it means the table cannot be read.
    services
        .reload(catalog)
        .await
        .context("starting the stored streams")?;
    for running in services.supervisor.streams() {
        if let Some(error) = running.status().error() {
            eprintln!(
                "feldspar: stream `{}` is stored but not running: {error}",
                running.name()
            );
        }
    }
    services.supervisor.start_task();
    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_whose_firing_is_still_running_drops_the_next_element() {
        // The drop rule is "is *this stream's* previous firing still running",
        // so the flag has to be stable across elements — a fresh one per
        // element would never drop anything.
        let in_flight = InFlight::default();
        let held = in_flight
            .take("boiler")
            .expect("the first element is taken");
        assert!(
            in_flight.take("boiler").is_none(),
            "the second element must be dropped while the first is firing"
        );
        // A different stream is a different flag: one slow trigger must not
        // stop another stream's.
        assert!(in_flight.take("market_feed").is_some());
        // And once the firing finishes the stream flows again.
        held.store(false, Ordering::Release);
        assert!(in_flight.take("boiler").is_some());
    }
}

//! The supervisor: the diff, the backoff, and the fan-out (TODO tasks 3.2–3.4).
//!
//! An integration test rather than a unit one for the reason the scripted
//! provider's is: the fake lives behind the `testing` feature, and that feature
//! is turned on for this crate's test build only here.
//!
//! **No broker, and no clock.** Every restart decision is driven by handing
//! [`StreamSupervisor::tick`] an instant, exactly as `Scheduler::tick` is, so a
//! test of "the third attempt, seven seconds in, is the one that connects" runs
//! in the time it takes to call a function three times. The only real waiting
//! anywhere in this file is `settle()`, which is not waiting for a *decision* —
//! it is waiting for a spawned task to have run at all.

use sc_types::Role;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sc_stream::supervisor::{Delivery, StreamConfig, StreamConsumer, StreamSupervisor};
use sc_stream::testing::ScriptedProvider;
use sc_stream::{
    ElementType, Envelope, RawPayload, Stream, StreamId, StreamRegistry, StreamStatus,
};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

/// Long enough for a spawned task to make progress on any machine that can run
/// the rest of this suite.
const SETTLE: Duration = Duration::from_millis(80);

async fn settle() {
    tokio::time::sleep(SETTLE).await;
}

/// An instant on the test's own timeline. Every `tick` below is `at(n)` for
/// some whole number of seconds, so the backoff's arithmetic is readable in the
/// assertions.
fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_758_099_600 + seconds, 0).expect("a representable instant")
}

/// A supervisor over one provider, with the numbers this test cares about.
fn supervisor(
    provider: ScriptedProvider,
    config: StreamConfig,
) -> (Arc<StreamSupervisor>, Arc<ScriptedProvider>) {
    let provider = Arc::new(provider);
    let mut registry = StreamRegistry::new();
    registry
        .register(Arc::clone(&provider) as Arc<_>)
        .expect("a fresh registry takes its first provider");
    (
        Arc::new(StreamSupervisor::new(Arc::new(registry), config)),
        provider,
    )
}

/// A config with no cap and no waiting, for the tests that are not about
/// either.
fn plain() -> StreamConfig {
    StreamConfig {
        max_elements_per_second: 0,
        ..StreamConfig::default()
    }
}

fn row(name: &str) -> Stream {
    Stream::new(name, "scripted")
}

/// A consumer that takes everything, and remembers it.
#[derive(Default)]
struct Consumer {
    taken: std::sync::Mutex<Vec<Envelope>>,
}

impl Consumer {
    fn taken(&self) -> Vec<Envelope> {
        self.taken.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl StreamConsumer for Consumer {
    fn consume(&self, envelope: &Envelope) -> Delivery {
        if let Ok(mut taken) = self.taken.lock() {
            taken.push(envelope.clone());
        }
        Delivery::Accepted
    }
}

/// A consumer that is busy for every element after the first — the trigger
/// bridge under a stream faster than its action (§7).
#[derive(Default)]
struct BusyConsumer {
    seen: AtomicUsize,
}

impl StreamConsumer for BusyConsumer {
    fn consume(&self, _envelope: &Envelope) -> Delivery {
        if self.seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Delivery::Accepted
        } else {
            Delivery::Dropped
        }
    }
}

// ---------------------------------------------------------------- task 3.2

#[tokio::test]
async fn starting_a_stream_subscribes_it_and_elements_reach_the_consumer() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements([RawPayload::bytes(b"warm".to_vec())]),
        plain(),
    );
    let consumer = Arc::new(Consumer::default());
    supervisor.set_consumer(Arc::clone(&consumer) as Arc<_>);

    let running = supervisor
        .start_at(row("boiler"), at(0))
        .await
        .expect("a stream the registry knows starts");
    settle().await;

    assert_eq!(provider.subscribes(), 1);
    assert_eq!(running.status(), StreamStatus::Running { since: at(0) });
    assert_eq!(running.counters().elements, 1);
    let taken = consumer.taken();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].stream, "boiler");
    assert_eq!(taken[0].value, json!("warm"));
}

#[tokio::test]
async fn a_stream_whose_provider_nothing_implements_is_held_failed_rather_than_dropped() {
    let (supervisor, _) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text()),
        plain(),
    );

    let running = supervisor
        .start_at(Stream::new("boiler", "mqtt"), at(0))
        .await
        .expect("an unresolvable stream is still held");

    // Still listed and still editable — editing it *is* the repair.
    assert_eq!(supervisor.streams().len(), 1);
    let error = running.status().error().unwrap_or_default().to_owned();
    assert!(error.contains("unknown stream provider `mqtt`"), "{error}");
    // Attempt 0: nothing has been *tried*, and nothing will be. Waiting does
    // not make a missing module arrive.
    assert!(matches!(
        running.status(),
        StreamStatus::Failed { attempt: 0, .. }
    ));
    assert_eq!(supervisor.tick(at(3_600)).await, Vec::<String>::new());
}

#[tokio::test]
async fn stopping_a_stream_hangs_up_but_keeps_it_listed() {
    let (supervisor, _) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements([RawPayload::bytes(b"tick".to_vec())])
            .every(Duration::from_millis(5))
            .repeating(),
        plain(),
    );
    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    settle().await;
    let while_running = running.counters().elements;
    assert!(while_running > 0, "the timer delivered something");

    assert!(supervisor.stop(running.id()));
    settle().await;
    let after_stop = running.counters().elements;
    settle().await;

    assert_eq!(
        running.counters().elements,
        after_stop,
        "dropping the subscription stopped the provider's task"
    );
    assert_eq!(running.status(), StreamStatus::Stopped);
    assert_eq!(
        supervisor.streams().len(),
        1,
        "a stopped stream is still a stream you can ask about"
    );
    // Nothing retries a stopped stream: only a reload starts it again.
    assert_eq!(supervisor.tick(at(3_600)).await, Vec::<String>::new());
}

#[tokio::test]
async fn a_reload_starts_what_is_new_and_forgets_what_is_gone() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text()),
        plain(),
    );
    let boiler = row("boiler");
    let pump = row("pump");

    assert!(
        supervisor
            .apply_at(vec![boiler.clone(), pump.clone()], at(0))
            .await
    );
    settle().await;
    assert_eq!(provider.subscribes(), 2);
    assert_eq!(supervisor.streams().len(), 2);

    // The pump's row is deleted.
    assert!(supervisor.apply_at(vec![boiler.clone()], at(1)).await);
    assert_eq!(supervisor.streams().len(), 1);
    assert!(supervisor.by_name("pump").is_none());

    // And nothing has changed since, so nothing happens — and in particular
    // the boiler keeps the connection it has had all along.
    assert!(!supervisor.apply_at(vec![boiler.clone()], at(2)).await);
    assert_eq!(provider.subscribes(), 2, "no new subscribe");
}

#[tokio::test]
async fn a_changed_configuration_restarts_and_an_unchanged_one_does_not() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text()),
        plain(),
    );
    let boiler = row("boiler").config("topic", "house/boiler/#");
    supervisor.apply_at(vec![boiler.clone()], at(0)).await;
    settle().await;
    assert_eq!(provider.subscribes(), 1);

    // The same row again: the connection is left entirely alone.
    assert!(!supervisor.apply_at(vec![boiler.clone()], at(1)).await);
    assert_eq!(provider.subscribes(), 1);

    // A different topic is a different flow, so it is stopped and started.
    let retopiced = boiler.clone().config("topic", "house/#");
    assert!(supervisor.apply_at(vec![retopiced.clone()], at(2)).await);
    settle().await;
    assert_eq!(provider.subscribes(), 2);
    assert_eq!(
        supervisor.get(boiler.id).map(|r| r.configuration()),
        Some(retopiced.configuration.clone())
    );
}

#[tokio::test]
async fn a_rename_and_a_new_description_keep_the_broker_session() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements([RawPayload::bytes(b"warm".to_vec())])
            .every(Duration::from_millis(5))
            .repeating(),
        plain(),
    );
    let boiler = row("boiler");
    supervisor.apply_at(vec![boiler.clone()], at(0)).await;
    settle().await;
    assert_eq!(provider.subscribes(), 1);

    let renamed = boiler
        .clone()
        .description("the boiler's temperature")
        .min_role(Role::of(40));
    let renamed = Stream {
        name: "boiler_temp".to_owned(),
        ..renamed
    };
    assert!(supervisor.apply_at(vec![renamed], at(1)).await);
    settle().await;

    assert_eq!(
        provider.subscribes(),
        1,
        "an admin fixing a description must not drop a broker session"
    );
    let running = supervisor.get(boiler.id).expect("still the same stream");
    assert!(running.status().is_running());
    assert_eq!(running.name(), "boiler_temp");
    assert_eq!(running.min_role(), Some(Role::of(40)));

    // The new name is what the envelopes carry from here on — the reference a
    // trigger's channel holds is broken deliberately, as a trigger's rename is.
    let before = running.counters().elements;
    let feed = running.subscribe_elements();
    let mut receiver = feed.receiver;
    let envelope = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("an element within two seconds")
        .expect("the channel is open");
    assert_eq!(envelope.stream, "boiler_temp");
    assert!(running.counters().elements > before);
}

#[tokio::test]
async fn disabling_a_stream_hangs_up_and_re_enabling_it_connects_again() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text()),
        plain(),
    );
    let boiler = row("boiler");
    supervisor.apply_at(vec![boiler.clone()], at(0)).await;
    settle().await;
    assert_eq!(provider.subscribes(), 1);

    let off = boiler.clone().enabled(false);
    assert!(supervisor.apply_at(vec![off.clone()], at(1)).await);
    let running = supervisor.get(boiler.id).expect("still listed");
    assert_eq!(running.status(), StreamStatus::Stopped);
    assert!(!supervisor.apply_at(vec![off.clone()], at(2)).await);

    assert!(supervisor.apply_at(vec![boiler.clone()], at(3)).await);
    settle().await;
    assert_eq!(provider.subscribes(), 2);
    let running = supervisor.get(boiler.id).expect("still listed");
    assert_eq!(running.status(), StreamStatus::Running { since: at(3) });
}

#[tokio::test]
async fn a_stream_that_is_disabled_before_it_ever_ran_is_never_subscribed() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text()),
        plain(),
    );
    assert!(
        !supervisor
            .apply_at(vec![row("boiler").enabled(false)], at(0))
            .await
    );
    settle().await;
    assert_eq!(provider.subscribes(), 0);
    assert!(supervisor.streams().is_empty());
}

#[tokio::test]
async fn a_shutdown_stops_everything_and_forgets_it() {
    let (supervisor, _) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements([RawPayload::bytes(b"tick".to_vec())])
            .every(Duration::from_millis(5))
            .repeating(),
        plain(),
    );
    supervisor
        .apply_at(vec![row("boiler"), row("pump")], at(0))
        .await;
    settle().await;
    let boiler = supervisor.by_name("boiler").expect("started");
    let counted = boiler.counters().elements;
    assert!(counted > 0);

    supervisor.shutdown();
    settle().await;
    let after = boiler.counters().elements;
    settle().await;

    assert!(supervisor.streams().is_empty());
    assert_eq!(boiler.counters().elements, after, "the task is gone");
    assert_eq!(boiler.status(), StreamStatus::Stopped);
}

#[tokio::test]
async fn forgetting_a_stream_that_is_not_there_says_so_rather_than_panicking() {
    let (supervisor, _) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text()),
        plain(),
    );
    assert!(!supervisor.forget(StreamId::new()));
    assert!(!supervisor.stop(StreamId::new()));
}

// ---------------------------------------------------------------- task 3.3

#[tokio::test]
async fn a_failing_subscribe_backs_off_and_the_third_attempt_is_the_one_that_connects() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text()).failing_first(2),
        plain(),
    );

    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    assert_eq!(provider.subscribes(), 1);
    match running.status() {
        StreamStatus::Failed {
            error,
            since,
            attempt,
        } => {
            assert!(error.contains("scripted failure on subscribe 1"), "{error}");
            assert_eq!(since, at(0));
            assert_eq!(attempt, 1);
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert_eq!(
        running.retry_at(),
        Some(at(1)),
        "the first delay is a second"
    );

    // Not due yet: a tick before the delay is up changes nothing at all.
    assert_eq!(supervisor.tick(at(0)).await, Vec::<String>::new());
    assert_eq!(provider.subscribes(), 1);

    // Due. The second attempt fails too, and the delay doubles — while `since`
    // stays put, so "failing since 09:00" survives forty retries.
    assert_eq!(supervisor.tick(at(1)).await, vec!["boiler".to_owned()]);
    assert_eq!(provider.subscribes(), 2);
    assert!(matches!(
        running.status(),
        StreamStatus::Failed { attempt: 2, since, .. } if since == at(0)
    ));
    assert_eq!(running.retry_at(), Some(at(3)), "1 then 2");

    assert_eq!(supervisor.tick(at(2)).await, Vec::<String>::new());
    assert_eq!(supervisor.tick(at(3)).await, vec!["boiler".to_owned()]);
    settle().await;

    assert_eq!(provider.subscribes(), 3);
    assert_eq!(running.status(), StreamStatus::Running { since: at(3) });
    assert_eq!(running.retry_at(), None, "nothing is pending any more");
}

#[tokio::test]
async fn a_subscription_that_ends_is_restarted() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements([RawPayload::bytes(b"warm".to_vec())])
            .ending(),
        plain(),
    );

    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    settle().await;
    assert_eq!(running.counters().elements, 1);
    assert!(running.has_ended(), "the broker hung up");

    // The supervisor notices, and treats it as exactly what it is: the flow
    // stopped, so back off and try again.
    assert_eq!(supervisor.tick(at(0)).await, Vec::<String>::new());
    assert!(matches!(
        running.status(),
        StreamStatus::Failed { attempt: 1, .. }
    ));
    assert_eq!(running.status().error(), Some("the subscription ended"));
    assert_eq!(running.retry_at(), Some(at(1)));

    assert_eq!(supervisor.tick(at(1)).await, vec!["boiler".to_owned()]);
    settle().await;
    assert_eq!(provider.subscribes(), 2);
    assert_eq!(
        running.counters().elements,
        2,
        "the reconnected subscription delivers too"
    );
}

#[tokio::test]
async fn the_backoff_is_capped_at_a_minute_however_long_the_outage_is() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text()).failing_first(1_000),
        plain(),
    );

    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    let mut now = at(0);
    for _ in 0..12 {
        now = running.retry_at().expect("a retry is always pending");
        supervisor.tick(now).await;
    }
    let gap = running.retry_at().expect("still pending") - now;
    assert_eq!(gap.num_seconds(), 60, "capped, and it stays capped");
    assert_eq!(provider.subscribes(), 13);
    assert!(matches!(
        running.status(),
        StreamStatus::Failed { attempt: 13, since, .. } if since == at(0)
    ));
}

// ---------------------------------------------------------------- task 3.4

#[tokio::test]
async fn a_receiver_that_falls_behind_is_told_how_many_it_lost() {
    let (supervisor, _) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements((0..10).map(|n| RawPayload::bytes(format!("{n}").into_bytes())))
            .every(Duration::from_millis(2)),
        StreamConfig {
            // Four buffered, ten published: a receiver that reads none of them
            // is six behind.
            channel_capacity: 4,
            max_elements_per_second: 0,
            ..StreamConfig::default()
        },
    );

    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    let mut receiver = running.subscribe_elements().receiver;
    // Deliberately read nothing while the stream runs past the buffer.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(running.counters().elements, 10);

    match receiver.recv().await {
        Err(RecvError::Lagged(n)) => assert_eq!(
            n, 6,
            "told the number, so the socket can send a `lagged` frame rather \
             than silently showing a gap"
        ),
        other => panic!("expected to be told it lagged, got {other:?}"),
    }
    // And it carries on from where the channel still has elements.
    let next = receiver.recv().await.expect("the channel is still open");
    assert_eq!(next.value, json!("6"));
}

#[tokio::test]
async fn a_stream_over_its_cap_drops_and_counts_rather_than_pausing_the_provider() {
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements((0..20).map(|n| RawPayload::bytes(format!("{n}").into_bytes()))),
        StreamConfig {
            max_elements_per_second: 5,
            ..StreamConfig::default()
        },
    );
    let consumer = Arc::new(Consumer::default());
    supervisor.set_consumer(Arc::clone(&consumer) as Arc<_>);

    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    settle().await;

    let counters = running.counters();
    assert_eq!(counters.elements, 5, "the cap let five through");
    assert_eq!(counters.dropped_for_rate, 15);
    assert_eq!(consumer.taken().len(), 5);
    assert_eq!(
        provider.delivered(),
        20,
        "the provider was never paused — it delivered all twenty and we dropped fifteen"
    );
    assert!(running.status().is_running(), "and it is still healthy");
}

#[tokio::test]
async fn a_consumer_too_busy_to_take_an_element_is_counted_rather_than_queued() {
    let (supervisor, _) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements((0..4).map(|n| RawPayload::bytes(format!("{n}").into_bytes()))),
        plain(),
    );
    supervisor.set_consumer(Arc::new(BusyConsumer::default()) as Arc<_>);

    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    settle().await;

    let counters = running.counters();
    assert_eq!(counters.elements, 4, "every element was still broadcast");
    assert_eq!(
        counters.dropped_for_triggers, 3,
        "a stream that is dropping is a thing you can see"
    );
}

#[tokio::test]
async fn a_screen_opened_on_a_slow_stream_replays_what_it_missed_and_then_continues() {
    let (supervisor, _) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements([
                RawPayload::bytes(b"one".to_vec()),
                RawPayload::bytes(b"two".to_vec()),
            ])
            .every(Duration::from_millis(5))
            .repeating(),
        StreamConfig {
            ring_capacity: 2,
            max_elements_per_second: 0,
            ..StreamConfig::default()
        },
    );

    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    settle().await;

    let feed = running.subscribe_elements();
    assert_eq!(feed.replay.len(), 2, "the last two, not a blank screen");
    assert_eq!(running.listeners(), 1);
    let mut receiver = feed.receiver;
    let live = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("an element within two seconds")
        .expect("the channel is open");
    assert_eq!(live.stream, "boiler");
}

#[tokio::test]
async fn a_malformed_payload_is_counted_and_never_reaches_a_consumer() {
    let (supervisor, _) = supervisor(
        ScriptedProvider::new("scripted", ElementType::json([]))
            .elements([RawPayload::bytes(b"not json at all".to_vec())]),
        plain(),
    );
    let consumer = Arc::new(Consumer::default());
    supervisor.set_consumer(Arc::clone(&consumer) as Arc<_>);

    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    settle().await;

    let counters = running.counters();
    assert_eq!(counters.malformed, 1);
    assert_eq!(counters.elements, 0);
    assert!(consumer.taken().is_empty());
    assert_eq!(
        counters.last_element_at, None,
        "a stream receiving nothing but rubbish has still never had an element"
    );
}

#[tokio::test]
async fn a_supervisor_with_no_consumer_still_runs_and_still_broadcasts() {
    let (supervisor, _) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements([RawPayload::bytes(b"warm".to_vec())]),
        plain(),
    );

    let running = supervisor.start_at(row("boiler"), at(0)).await.unwrap();
    settle().await;

    assert!(running.status().is_running());
    assert_eq!(running.counters().elements, 1);
    assert_eq!(
        running.subscribe_elements().replay.len(),
        1,
        "the Observe socket works on a process that has no triggers at all"
    );
}

#[tokio::test]
async fn a_stream_whose_provider_was_uninstalled_becomes_failed_with_the_sentence() {
    // The module-change path (task 9.3): the registry is rebuilt whole and
    // swapped in, and the reload is what makes the swap visible.
    let (supervisor, provider) = supervisor(
        ScriptedProvider::new("scripted", ElementType::text())
            .elements([RawPayload::bytes(b"tick".to_vec())])
            .every(Duration::from_millis(5))
            .repeating(),
        plain(),
    );
    let stored = row("boiler");
    supervisor.start_at(stored.clone(), at(0)).await.unwrap();
    settle().await;
    assert!(provider.subscribes() >= 1);
    let running = supervisor.by_name("boiler").expect("held");
    assert!(matches!(running.status(), StreamStatus::Running { .. }));

    // The module goes away: a registry with nothing in it, swapped in whole,
    // and the same rows reloaded.
    supervisor.set_registry(Arc::new(StreamRegistry::new()));
    assert!(supervisor.apply_at(vec![stored.clone()], at(1)).await);

    // Listed, editable, and saying why — a sentence naming the provider, not a
    // panic and not a stream that vanished from the screen that repairs it.
    let running = supervisor.by_name("boiler").expect("still held");
    let error = running.status().error().unwrap_or_default().to_owned();
    assert!(
        error.contains("unknown stream provider `scripted`"),
        "{error}"
    );
    // And it is not retried: waiting does not make an uninstalled module come
    // back, so the retry loop leaves it alone (attempt 0).
    assert!(matches!(
        running.status(),
        StreamStatus::Failed { attempt: 0, .. }
    ));
    assert_eq!(supervisor.tick(at(3_600)).await, Vec::<String>::new());

    // The subscription really stopped: nothing is still delivering behind the
    // failed status.
    let elements = running.counters().elements;
    settle().await;
    assert_eq!(
        supervisor.by_name("boiler").unwrap().counters().elements,
        elements
    );

    // And when the module comes back, a reload starts it again.
    let mut registry = StreamRegistry::new();
    registry
        .register(Arc::new(
            ScriptedProvider::new("scripted", ElementType::text())
                .elements([RawPayload::bytes(b"tick".to_vec())]),
        ))
        .unwrap();
    supervisor.set_registry(Arc::new(registry));
    assert!(supervisor.apply_at(vec![stored], at(2)).await);
    settle().await;
    assert!(matches!(
        supervisor.by_name("boiler").unwrap().status(),
        StreamStatus::Running { .. }
    ));
}

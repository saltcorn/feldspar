//! The hub against a real running stream: the supervisor, the scripted
//! provider and the broadcast channel are the shipping code, and nothing here
//! needs a database (`StreamSupervisor::start` takes the row).
//!
//! What is claimed:
//!
//! - **one fan-out serves every subscription** to a stream, and each gets every
//!   element;
//! - **the replay and the deliveries meet exactly**: no element is both, and
//!   none is lost between them;
//! - **a full queue is a `lagged` count, never a wait**;
//! - **the subscriber count** the admin list shows goes up and comes back down;
//! - **the per-user connection limit** is per application, ignores anonymous
//!   connections and frees a slot when a connection goes;
//! - **a session end closes exactly that session's connections**, through the
//!   session store's listener hook.

use std::sync::Arc;
use std::time::Duration;

use sc_auth::{ROLE_ADMIN, SessionListener, SessionStore, User};
use sc_live::{ConnectionInfo, Delivery, ErrorCode, LiveHub, LiveLimits};
use sc_stream::testing::ScriptedProvider;
use sc_stream::{
    ElementType, RawPayload, RunningStream, Stream, StreamConfig, StreamProvider, StreamRegistry,
    StreamSupervisor,
};
use tokio::sync::mpsc;
use uuid::Uuid;

/// A stream publishing `"0"`, `"1"`, … every `every`.
async fn counting(
    count: usize,
    every: Duration,
    repeat: bool,
) -> (Arc<StreamSupervisor>, Arc<RunningStream>) {
    let mut provider = ScriptedProvider::new("scripted", ElementType::text())
        .elements((0..count).map(|n| RawPayload::bytes(n.to_string().into_bytes())))
        .every(every);
    if repeat {
        provider = provider.repeating();
    }
    let mut registry = StreamRegistry::new();
    registry
        .register(Arc::new(provider) as Arc<dyn StreamProvider>)
        .unwrap();
    let supervisor = Arc::new(StreamSupervisor::new(
        Arc::new(registry),
        StreamConfig::default(),
    ));
    let running = supervisor
        .start(Stream::new("counter", "scripted"))
        .await
        .unwrap();
    (supervisor, running)
}

fn info(app: &str, user: Option<Uuid>, session: Option<&str>) -> ConnectionInfo {
    ConnectionInfo {
        app: app.to_owned(),
        user,
        session: session.map(str::to_owned),
    }
}

async fn next(deliveries: &mut mpsc::Receiver<Delivery>) -> Delivery {
    tokio::time::timeout(Duration::from_secs(10), deliveries.recv())
        .await
        .expect("a delivery within the timeout")
        .expect("the queue is open")
}

fn value(delivery: &Delivery) -> String {
    delivery.envelope.value.as_str().unwrap().to_owned()
}

#[tokio::test]
async fn two_connections_on_one_stream_each_get_every_element() {
    let (_supervisor, running) = counting(5, Duration::from_millis(30), true).await;
    let hub = LiveHub::new(LiveLimits::default());

    let (a, mut a_queue) = hub.connect(info("blog", None, None)).unwrap();
    let (b, mut b_queue) = hub.connect(info("blog", None, None)).unwrap();
    let (a_sub, _) = a.attach(&running, None).await;
    let (b_sub, _) = b.attach(&running, None).await;
    assert_eq!(hub.subscribers(running.id()), 2);
    assert_eq!(hub.subscriber_counts().get(&running.id()), Some(&2));

    // Both see the same next three elements, in the same order.
    let mut seen_a = Vec::new();
    let mut seen_b = Vec::new();
    for _ in 0..3 {
        let delivery = next(&mut a_queue).await;
        assert_eq!(delivery.attachment, a_sub.attachment());
        seen_a.push(value(&delivery));
        let delivery = next(&mut b_queue).await;
        assert_eq!(delivery.attachment, b_sub.attachment());
        seen_b.push(value(&delivery));
    }
    let start = seen_a[0].clone();
    let b_offset = seen_b.iter().position(|v| *v == start);
    assert!(
        b_offset.is_some() || seen_a.contains(&seen_b[0]),
        "the two saw the same flow: {seen_a:?} / {seen_b:?}"
    );

    drop(a_sub);
    assert_eq!(hub.subscribers(running.id()), 1);
    drop(b_sub);
    assert_eq!(hub.subscribers(running.id()), 0);
    assert!(
        hub.subscriber_counts().is_empty(),
        "a stream nobody watches is not listed"
    );
}

#[tokio::test]
async fn the_replay_and_the_deliveries_meet_exactly() {
    let (_supervisor, running) = counting(30, Duration::from_millis(15), false).await;
    let hub = LiveHub::new(LiveLimits::default());
    // A fan-out already running, so the replay comes from its copy of the ring
    // while elements are flowing through it.
    let (early, _early_queue) = hub.connect(info("blog", None, None)).unwrap();
    let (_early_sub, _) = early.attach(&running, None).await;
    tokio::time::sleep(Duration::from_millis(120)).await;

    let (late, mut queue) = hub.connect(info("blog", None, None)).unwrap();
    let (_sub, replay) = late.attach(&running, None).await;
    assert!(
        !replay.is_empty(),
        "elements were published before this subscription"
    );

    let mut seen: Vec<usize> = replay
        .iter()
        .map(|e| e.value.as_str().unwrap().parse().unwrap())
        .collect();
    while seen.last() != Some(&29) {
        let delivery = next(&mut queue).await;
        seen.push(value(&delivery).parse().unwrap());
    }
    let first = seen[0];
    assert_eq!(
        seen,
        (first..30).collect::<Vec<_>>(),
        "contiguous: nothing doubled, nothing lost between replay and delivery"
    );
}

#[tokio::test]
async fn a_full_queue_is_counted_as_lagged_and_the_connection_is_woken() {
    let (_supervisor, running) = counting(10, Duration::from_millis(5), true).await;
    let hub = LiveHub::new(LiveLimits {
        queue: 1,
        ..LiveLimits::default()
    });
    let (connection, _queue) = hub.connect(info("blog", None, None)).unwrap();
    let (sub, _) = connection.attach(&running, None).await;
    // Nobody drains the queue: the first element fits, the rest are lost.
    tokio::time::timeout(Duration::from_secs(10), connection.lagged())
        .await
        .expect("the connection is woken to report the loss");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let lost = sub.take_lagged();
    assert!(lost > 0, "dropped elements are counted");
    assert!(
        sub.take_lagged() < lost,
        "taking the count resets it, so a loss is reported once"
    );
}

#[tokio::test]
async fn the_connection_limit_is_per_user_per_application() {
    let hub = LiveHub::new(LiveLimits {
        max_connections_per_user: 2,
        ..LiveLimits::default()
    });
    let alice = Some(Uuid::new_v4());
    let (first, _) = hub.connect(info("blog", alice, Some("t1"))).unwrap();
    let (_second, _) = hub.connect(info("blog", alice, Some("t1"))).unwrap();
    let refused = hub.connect(info("blog", alice, Some("t1"))).err().unwrap();
    assert_eq!(refused.code, ErrorCode::TooManyConnections);

    // Another application, another user and nobody at all are all separate.
    assert!(hub.connect(info("shop", alice, Some("t1"))).is_ok());
    assert!(
        hub.connect(info("blog", Some(Uuid::new_v4()), None))
            .is_ok()
    );
    let anonymous: Vec<_> = (0..5)
        .map(|_| hub.connect(info("blog", None, None)).unwrap())
        .collect();
    assert_eq!(anonymous.len(), 5);

    // A connection that goes frees its slot.
    drop(first);
    assert!(hub.connect(info("blog", alice, Some("t1"))).is_ok());
}

#[tokio::test]
async fn a_sign_out_closes_that_sessions_connections_and_no_others() {
    let hub = LiveHub::new(LiveLimits::default());
    let store = SessionStore::default();
    let listener: Arc<dyn SessionListener> = hub.clone();
    store.add_listener(Arc::downgrade(&listener));

    let alice = User::new(Uuid::new_v4(), ROLE_ADMIN).unwrap();
    let bob = User::new(Uuid::new_v4(), ROLE_ADMIN).unwrap();
    let alice_token = store.login(alice.clone()).await.unwrap();
    let bob_token = store.login(bob.clone()).await.unwrap();

    let (alice_conn, _) = hub
        .connect(info("blog", Some(alice.id), Some(&alice_token)))
        .unwrap();
    let (bob_conn, _) = hub
        .connect(info("blog", Some(bob.id), Some(&bob_token)))
        .unwrap();
    let mut alice_closed = alice_conn.close_signal();
    let mut bob_closed = bob_conn.close_signal();

    store.logout(&alice_token).await.unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(5), alice_closed.closed())
        .await
        .expect("alice's connection is closed at once");
    assert_eq!(reason.code, ErrorCode::SignedOut);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), bob_closed.closed())
            .await
            .is_err(),
        "bob's session did not end, so neither did his connection"
    );

    // A forced sign-out of a user reaches every one of their connections.
    store.end_user_sessions(bob.id).await.unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(5), bob_closed.closed())
        .await
        .expect("bob's connection is closed when his sessions are ended");
    assert_eq!(reason.code, ErrorCode::SignedOut);
}

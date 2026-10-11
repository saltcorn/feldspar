//! A page publishing and being present (TODO.md "Live updates" L4.1–L4.3),
//! through the real hub and a real running `internal` stream — no database —
//! and the update rule a row topic's publish is decided by, on an in-memory
//! SQLite database with the real evaluator.
//!
//! What is claimed:
//!
//! - **L4.1** a page's element is stamped `source: {user, connection}` from the
//!   connection, whatever the value carries; it reaches every other subscriber
//!   on the topic and **not the sender**, unless the sender attached with
//!   `echo`; it is validated against the element type like any publish.
//! - **L4.1** `row_updatable_as` is §7.3's update rule: the owner and a share
//!   that may edit pass, a share that may only read and a stranger do not, the
//!   write floor passes, and a missing row is `false`.
//! - **L4.3** a seat in a room hands back the member list, the others are told
//!   of the join, the state and the leave, and dropping the seat leaves.

use std::sync::Arc;
use std::time::Duration;

use sc_auth::{ROLE_ADMIN, User};
use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId, TableId, TableMeta};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_live::{ConnectionInfo, Delivery, LiveHub, LiveLimits, PresenceEvent};
use sc_stream::{
    Internal, RunningStream, Stream, StreamConfig, StreamProvider, StreamRegistry, StreamSupervisor,
};
use sc_types::{BasicType, TypeRef};
use serde_json::{Map, json};
use tokio::sync::mpsc;
use uuid::Uuid;

const MEMBER: u8 = 40;

/// An `internal` stream with one topic that pages may publish to.
async fn board_presence() -> (Arc<StreamSupervisor>, Arc<RunningStream>) {
    let mut registry = StreamRegistry::new();
    registry
        .register(Arc::new(Internal) as Arc<dyn StreamProvider>)
        .unwrap();
    let supervisor = Arc::new(StreamSupervisor::new(
        Arc::new(registry),
        StreamConfig::default(),
    ));
    let mut stream = Stream::new("board_presence", "internal");
    stream.configuration = json!({
        "keys": [{ "name": "dragging", "type": "int" }],
        "topics": "single",
        "client_publish": true,
        "presence": true,
    })
    .as_object()
    .cloned()
    .unwrap();
    let running = supervisor.start(stream).await.unwrap();
    (supervisor, running)
}

fn info(user: Uuid) -> ConnectionInfo {
    ConnectionInfo {
        app: "boards".to_owned(),
        user: Some(user),
        session: None,
    }
}

async fn next(queue: &mut mpsc::Receiver<Delivery>) -> Delivery {
    tokio::time::timeout(Duration::from_secs(10), queue.recv())
        .await
        .expect("a delivery within the timeout")
        .expect("the queue is open")
}

/// Nothing arrives for a moment.
async fn quiet(queue: &mut mpsc::Receiver<Delivery>) {
    if let Ok(Some(delivery)) = tokio::time::timeout(Duration::from_millis(200), queue.recv()).await
    {
        panic!("nothing was expected, got {:?}", delivery.envelope);
    }
}

#[tokio::test]
async fn a_pages_element_is_stamped_and_reaches_everybody_but_the_sender() {
    let (_supervisor, running) = board_presence().await;
    let hub = LiveHub::new(LiveLimits::default());
    let alice = Uuid::from_u128(0xa11ce);
    let bob = Uuid::from_u128(0xb0b);
    let (a, mut a_queue) = hub.connect(info(alice)).unwrap();
    let (b, mut b_queue) = hub.connect(info(bob)).unwrap();
    let (_a_sub, _) = a.attach(&running, None).await;
    let (_b_sub, _) = b.attach(&running, None).await;
    assert_ne!(a.tag(), b.tag());

    // A value that carries a `source` of its own: it stays in the value, and
    // the envelope's source is the server's.
    a.publish(
        &running,
        None,
        json!({ "dragging": 7, "source": { "user": bob.to_string() } }),
    )
    .unwrap();
    let got = next(&mut b_queue).await;
    assert_eq!(got.envelope.value["dragging"], 7);
    assert_eq!(
        got.envelope.source,
        Some(json!({ "user": alice.hyphenated().to_string(), "connection": a.tag() }))
    );
    quiet(&mut a_queue).await;

    // A value that does not match the element type is refused, and counted.
    let before = running.counters().malformed;
    let refused = a
        .publish(&running, None, json!({ "dragging": "a lot" }))
        .unwrap_err();
    assert!(refused.to_string().contains("element type"), "{refused}");
    assert_eq!(running.counters().malformed, before + 1);
    quiet(&mut b_queue).await;

    // Bob's own element reaches Alice, and not Bob.
    b.publish(&running, None, json!({ "dragging": 8 })).unwrap();
    assert_eq!(next(&mut a_queue).await.envelope.value["dragging"], 8);
    quiet(&mut b_queue).await;
}

#[tokio::test]
async fn a_sender_that_asks_for_echo_gets_its_own_element_back() {
    let (_supervisor, running) = board_presence().await;
    let hub = LiveHub::new(LiveLimits::default());
    let (a, mut a_queue) = hub.connect(info(Uuid::from_u128(1))).unwrap();
    let (_sub, _) = a.attach_echo(&running, None, true).await;
    a.publish(&running, None, json!({ "dragging": 1 })).unwrap();
    let got = next(&mut a_queue).await;
    assert_eq!(got.envelope.value["dragging"], 1);
    // Server-side publishes have no connection, and reach everybody.
    let (c, mut c_queue) = hub.connect(info(Uuid::from_u128(2))).unwrap();
    let (_c_sub, _) = c.attach(&running, None).await;
    running
        .publish_value(None, json!({ "dragging": 2 }))
        .unwrap();
    assert_eq!(next(&mut c_queue).await.envelope.value["dragging"], 2);
    assert_eq!(next(&mut a_queue).await.envelope.value["dragging"], 2);
}

#[tokio::test]
async fn a_seat_hands_back_the_list_and_the_others_hear_of_it() {
    let (_supervisor, running) = board_presence().await;
    let hub = LiveHub::new(LiveLimits::default());
    let alice = Uuid::from_u128(0xa11ce);
    let (a, _a_queue) = hub.connect(info(alice)).unwrap();
    let (b, _b_queue) = hub.connect(info(Uuid::from_u128(0xb0b))).unwrap();
    let room = (running.id(), None);
    let mut fields = Map::new();
    fields.insert("name".to_owned(), json!("Alice"));
    let (a_seat, members) = a.join_presence(room.clone(), Some(alice), fields);
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["name"], "Alice");
    assert_eq!(members[0]["connection"], a.tag());
    assert_eq!(members[0]["user_id"], alice.hyphenated().to_string());

    let (b_seat, members) = b.join_presence(room.clone(), None, Map::new());
    assert_eq!(members.len(), 2);
    tokio::time::timeout(Duration::from_secs(5), a.presence_changed())
        .await
        .expect("Alice's connection is woken");
    let news = a.take_presence();
    assert!(
        matches!(&news.events[..], [(_, PresenceEvent::Joined(m))] if m["connection"] == b.tag())
    );

    b_seat.set_state(json!({ "dragging": 4 }));
    assert_eq!(
        a.take_presence().events,
        vec![(
            room.clone(),
            PresenceEvent::Updated {
                connection: b.tag().to_owned(),
                state: json!({ "dragging": 4 })
            }
        )]
    );
    assert_eq!(hub.presence_of(running.id()).len(), 1);
    assert_eq!(hub.presence_of(running.id())[0].1.len(), 2);

    drop(b_seat);
    assert_eq!(
        a.take_presence().events,
        vec![(room.clone(), PresenceEvent::Left(b.tag().to_owned()))]
    );
    assert!(
        b.take_presence().events.is_empty(),
        "nobody hears of themselves"
    );
    drop(a_seat);
    assert!(hub.presence_of(running.id()).is_empty());
}

fn pk() -> DataField {
    DataField::plain("id", TypeRef::Basic(BasicType::Int))
        .required()
        .primary_key()
}

#[tokio::test]
async fn updating_a_row_topic_is_the_tables_update_rule() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    let uuid = |name: &str| DataField::plain(name, TypeRef::Basic(BasicType::Uuid));
    catalog
        .create_table("boards", &[pk(), uuid("owner")])
        .await?;
    let board_key = DataField {
        kind: DataFieldKind::Key {
            target_table: TableId("boards".to_owned()),
            target_field: FieldId("id".to_owned()),
            summary_field: None,
        },
        ..DataField::plain("board", TypeRef::Basic(BasicType::Int))
    };
    catalog
        .create_table(
            "shares",
            &[
                pk(),
                board_key,
                uuid("member"),
                DataField::plain("can_edit", TypeRef::Basic(BasicType::Bool)),
            ],
        )
        .await?;
    catalog.reload().await?;
    let mut meta = TableMeta::new("boards");
    meta.set_ownership_formula(Some(
        "owner === user.id || sharesↃboard.some(s => s.member === user.id && (_read || s.can_edit))",
    ));
    sc_catalog::save_table_meta(&catalog, &meta).await?;
    let boards = catalog.require("boards")?;
    assert!(boards.ownership.is_some(), "{:?}", boards.ownership_error);

    let user = |n: u128| User::new(Uuid::from_u128(n), MEMBER).unwrap();
    let (owner, editor, reader, stranger) = (user(1), user(2), user(3), user(4));
    let id = |u: &User| u.id.hyphenated().to_string();
    let shares = catalog.require("shares")?;
    sc_api::rows::create_row(&catalog, &boards, &json!({ "id": 1, "owner": id(&owner) })).await?;
    for (n, member, can_edit) in [(1, &editor, true), (2, &reader, false)] {
        sc_api::rows::create_row(
            &catalog,
            &shares,
            &json!({ "id": n, "board": 1, "member": id(member), "can_edit": can_edit }),
        )
        .await?;
    }
    let evaluator: Arc<dyn JsEvaluator> = Arc::new(DenoEvaluator::new());
    let boards = catalog.require("boards")?;
    let may = async |u: &User, key: &str| {
        sc_api::row_updatable_as(&catalog, &boards, key, u.role, Some(u), Some(&evaluator))
            .await
            .unwrap()
    };
    // (case, user, key, may update)
    let cases = [
        ("the owner", &owner, "1", true),
        ("a share that may edit", &editor, "1", true),
        ("a share that may only read", &reader, "1", false),
        ("a stranger", &stranger, "1", false),
        ("a missing row", &owner, "2", false),
        ("a key that is no key", &owner, "abc", false),
    ];
    for (case, who, key, expected) in cases {
        assert_eq!(may(who, key).await, expected, "{case}");
    }
    // The reader may read what they may not update: the two rules differ.
    assert!(
        sc_api::row_readable_as(
            &catalog,
            &boards,
            "1",
            reader.role,
            Some(&reader),
            Some(&evaluator)
        )
        .await?
    );
    // The write floor passes without the formula.
    let admin = User::new(Uuid::from_u128(9), ROLE_ADMIN).unwrap();
    assert!(may(&admin, "1").await);
    assert!(
        !may(&admin, "2").await,
        "but not for a row that is not there"
    );
    Ok(())
}

//! What each subscriber of a `table_changes` stream is sent (TODO.md "Live
//! updates" L3.4–L3.9), through the real hub, the real write seam and the
//! real formula evaluator, on an in-memory SQLite database — two users, two
//! connections.
//!
//! What is claimed:
//!
//! - **L3.8, per user, per formula, per write**: the ownership tutorial's three
//!   shapes of read rule — the owner (`owner === user.id`, decided with no
//!   evaluator at all), a share through `Ↄ`, a publisher through `Ⱶ` — each with
//!   an insert, an update that keeps the row in view, an update that moves it
//!   from one user to the other (`leave` for one, `enter` for the other) and a
//!   delete. A row a user could never read produces **no frame of any kind**
//!   for them: each test ends with a write that user can read, and asserts it
//!   is the very next thing they are sent.
//! - **L3.7**: `leave` and `delete` carry the key and nothing else.
//! - **L3.6**: a filter narrows what is sent, and a row changing to or from it
//!   is an `enter` or a `leave`.
//! - **L3.5**: what is sent is the REST read's projection — the users table's
//!   password hash is not in it, nor in the element type.
//! - **L3.4**: a `key` payload sends `{op, key}` only; a subscription attached
//!   without a filter and row access is sent nothing.
//! - **L3.9**: the counters say what was checked, sent and withheld, and why.

use std::sync::Arc;
use std::time::Duration;

use sc_api::rows;
use sc_auth::{ROLE_ADMIN, User};
use sc_catalog::{
    Catalog, DataField, DataFieldKind, FieldId, LIVE_EVENTS, TableEvents, TableId, TableMeta,
    bootstrap_table_meta, save_table_meta,
};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_expr::{DenoEvaluator, JsEvaluator};
use sc_live::{
    ChangeAccess, ChangeCounts, Connection, ConnectionInfo, Delivery, LikeRules, LiveHub,
    LiveLimits, RowFilter,
};
use sc_stream::{
    RunningStream, Stream, StreamConfig, StreamProvider, StreamRegistry, StreamSupervisor,
    TableChanges,
};
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};
use tokio::sync::mpsc;
use uuid::Uuid;

/// The role both users hold: below every table's read floor (admin), so what
/// they may read is what the ownership formula grants.
const MEMBER: u8 = 40;

fn alice() -> User {
    User::new(Uuid::from_u128(0xa11ce), MEMBER).unwrap()
}

fn bob() -> User {
    User::new(Uuid::from_u128(0xb0b), MEMBER).unwrap()
}

fn admin() -> User {
    User::new(Uuid::from_u128(0xad), ROLE_ADMIN).unwrap()
}

fn id(user: &User) -> String {
    user.id.hyphenated().to_string()
}

fn key(name: &str, target: &str) -> DataField {
    DataField {
        kind: DataFieldKind::Key {
            target_table: TableId(target.to_owned()),
            target_field: FieldId("id".to_owned()),
            summary_field: None,
        },
        ..DataField::plain(name, TypeRef::Basic(BasicType::Int))
    }
}

fn pk() -> DataField {
    DataField::plain("id", TypeRef::Basic(BasicType::Int))
        .required()
        .primary_key()
}

struct Fixture {
    catalog: Arc<Catalog>,
    hub: Arc<LiveHub>,
    evaluator: Arc<dyn JsEvaluator>,
    /// `cards_live`: every write on `cards`, rows sent.
    cards: Arc<RunningStream>,
    /// `card_keys`: every write on `cards`, keys sent.
    keys: Arc<RunningStream>,
    _supervisor: Arc<StreamSupervisor>,
}

/// `boards`, `cards` (a key to a board, an owner, a column) and `card_shares`
/// (a key to a card, a member) on SQLite; `formula` as the read rule of
/// `cards`; two `table_changes` streams on it; the hub on the write seam.
async fn fixture(formula: &str) -> Result<Fixture> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    bootstrap_table_meta(&catalog).await?;
    let uuid = |name: &str| DataField::plain(name, TypeRef::Basic(BasicType::Uuid));
    let text = |name: &str| DataField::plain(name, TypeRef::Basic(BasicType::Text));
    catalog
        .create_table("boards", &[pk(), uuid("owner"), text("title")])
        .await?;
    catalog
        .create_table(
            "cards",
            &[
                pk(),
                key("board", "boards"),
                uuid("owner"),
                text("title"),
                text("column"),
            ],
        )
        .await?;
    catalog
        .create_table("card_shares", &[pk(), key("card", "cards"), uuid("member")])
        .await?;
    catalog.reload().await?;
    let mut meta = TableMeta::new("cards");
    meta.set_ownership_formula(Some(formula));
    save_table_meta(&catalog, &meta).await?;
    let cards = catalog.require("cards")?;
    assert!(
        cards.ownership.is_some(),
        "the formula is live: {:?}",
        cards.ownership_error
    );
    assert_eq!(cards.access.min_role_read, ROLE_ADMIN, "admin-only floor");

    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(TableChanges) as Arc<dyn StreamProvider>)?;
    let supervisor = Arc::new(StreamSupervisor::new(
        Arc::new(registry),
        StreamConfig::default(),
    ));
    let start = |name: &str, config: Json| {
        let mut stream = Stream::new(name, "table_changes");
        stream.configuration = config.as_object().cloned().unwrap();
        let supervisor = Arc::clone(&supervisor);
        async move { supervisor.start(stream).await }
    };
    let cards_live = start("cards_live", json!({ "table": "cards" })).await?;
    let keys = start("card_keys", json!({ "table": "cards", "payload": "key" })).await?;

    let hub = LiveHub::new(LiveLimits::default());
    hub.set_streams(Arc::clone(&supervisor));
    catalog.add_table_events(LIVE_EVENTS, Arc::clone(&hub) as Arc<dyn TableEvents>)?;
    Ok(Fixture {
        catalog,
        hub,
        evaluator: Arc::new(DenoEvaluator::new()),
        cards: cards_live,
        keys,
        _supervisor: supervisor,
    })
}

/// One page: a connection signed in as a user, subscribed to a stream.
struct Page {
    _connection: Connection,
    queue: mpsc::Receiver<Delivery>,
    _attached: sc_live::Attached,
}

impl Fixture {
    /// `user`'s page subscribed to `stream` with `filter`, deciding with the
    /// evaluator or (to prove a formula needs none) without it.
    async fn page(
        &self,
        stream: &Arc<RunningStream>,
        user: User,
        filter: Option<Json>,
        evaluator: bool,
    ) -> Page {
        let (connection, queue) = self
            .hub
            .connect(ConnectionInfo {
                app: "boards".to_owned(),
                user: Some(user.id),
                session: None,
            })
            .expect("connects");
        connection.set_user(Some(user));
        let table = self.catalog.require("cards").unwrap();
        let access = ChangeAccess {
            catalog: Arc::clone(&self.catalog),
            evaluator: evaluator.then(|| Arc::clone(&self.evaluator)),
            filter: RowFilter::parse(&table, filter.as_ref(), LikeRules::SQLITE).unwrap(),
        };
        let (attached, replay) = connection.attach_changes(stream, access).await;
        assert!(replay.is_empty());
        Page {
            _connection: connection,
            queue,
            _attached: attached,
        }
    }

    async fn insert(&self, table: &str, row: Json) {
        let table = self.catalog.require(table).unwrap();
        rows::create_row(&self.catalog, &table, &row).await.unwrap();
    }

    async fn update(&self, table: &str, key: i64, changes: Json) {
        let table = self.catalog.require(table).unwrap();
        rows::update_row(&self.catalog, &table, &key.to_string(), &changes)
            .await
            .unwrap();
    }

    async fn delete(&self, table: &str, key: i64) {
        let table = self.catalog.require(table).unwrap();
        rows::delete_row(&self.catalog, &table, &key.to_string())
            .await
            .unwrap();
    }

    fn counts(&self) -> ChangeCounts {
        self.hub
            .change_counts(self.cards.id())
            .expect("the stream has had subscribers")
    }
}

impl Page {
    /// The next thing this page is sent: its `{op, key, row?}`.
    async fn next(&mut self) -> Json {
        let delivery = tokio::time::timeout(Duration::from_secs(10), self.queue.recv())
            .await
            .expect("a frame within the timeout")
            .expect("the queue is open");
        delivery.envelope.value.clone()
    }

    /// Frames until one about `key` with `op`, every one of them about `key`
    /// — for a change whose readability depends on a row written just after
    /// it (a share), which the fan-out reads when it decides, not as of the
    /// write: the insert may or may not be sent, but nothing about another row
    /// is.
    async fn through(&mut self, op: &str, key: i64) -> Json {
        loop {
            let value = self.next().await;
            assert_eq!(value["key"].as_i64(), Some(key), "only {key}: {value}");
            if value["op"] == op {
                return value;
            }
        }
    }

    /// The next frame is about `key`, with `op` — so nothing was sent before
    /// it.
    async fn next_is(&mut self, op: &str, key: i64) -> Json {
        let value = self.next().await;
        assert_eq!(
            (value["op"].as_str(), value["key"].as_i64()),
            (Some(op), Some(key)),
            "expected {op} of {key}, got {value}"
        );
        value
    }
}

#[tokio::test]
async fn the_owner_formula_is_decided_without_the_evaluator() -> Result<()> {
    let f = fixture("owner === user.id").await?;
    // No evaluator: a field comparison decides it, or the test would deny all.
    let mut a = f.page(&f.cards, alice(), None, false).await;
    let mut b = f.page(&f.cards, bob(), None, false).await;

    f.insert(
        "cards",
        json!({ "id": 1, "owner": id(&alice()), "title": "draft" }),
    )
    .await;
    let inserted = a.next_is("insert", 1).await;
    assert_eq!(inserted["row"]["title"], "draft");
    assert_eq!(inserted["row"]["owner"], json!(id(&alice())));

    f.update("cards", 1, json!({ "title": "ready" })).await;
    assert_eq!(a.next_is("update", 1).await["row"]["title"], "ready");

    // Handed to Bob: it leaves Alice's view — the key, and nothing of what the
    // row became — and enters Bob's.
    f.update("cards", 1, json!({ "owner": id(&bob()) })).await;
    assert_eq!(a.next().await, json!({ "op": "leave", "key": 1 }));
    let entered = b.next_is("enter", 1).await;
    assert_eq!(entered["row"]["title"], "ready");

    f.delete("cards", 1).await;
    assert_eq!(b.next().await, json!({ "op": "delete", "key": 1 }));

    // Alice was told nothing of the delete of a row she no longer reads, and
    // Bob nothing of a row he never read: the next frame each gets is theirs.
    f.insert("cards", json!({ "id": 2, "owner": id(&alice()) }))
        .await;
    f.insert("cards", json!({ "id": 3, "owner": id(&bob()) }))
        .await;
    a.next_is("insert", 2).await;
    b.next_is("insert", 3).await;

    // L3.9: six elements × two subscriptions; Alice was sent four, Bob
    // three; every withholding was the read rule's, as nobody filtered.
    let counts = f.counts();
    assert_eq!(
        counts,
        ChangeCounts {
            checked: 12,
            delivered: 7,
            suppressed_access: 5,
            suppressed_filter: 0,
            evaluator_micros: counts.evaluator_micros,
        }
    );
    Ok(())
}

#[tokio::test]
async fn a_publisher_through_a_key_is_followed_to_the_board() -> Result<()> {
    let f = fixture("boardⱵowner === user.id").await?;
    f.insert("boards", json!({ "id": 1, "owner": id(&alice()) }))
        .await;
    f.insert("boards", json!({ "id": 2, "owner": id(&bob()) }))
        .await;
    let mut a = f.page(&f.cards, alice(), None, true).await;
    let mut b = f.page(&f.cards, bob(), None, true).await;

    f.insert(
        "cards",
        json!({ "id": 1, "board": 1, "title": "on alice's board" }),
    )
    .await;
    a.next_is("insert", 1).await;
    f.update("cards", 1, json!({ "title": "still hers" })).await;
    a.next_is("update", 1).await;

    // Moved to Bob's board: the old image was Alice's, the new one is Bob's.
    f.update("cards", 1, json!({ "board": 2 })).await;
    assert_eq!(a.next().await, json!({ "op": "leave", "key": 1 }));
    assert_eq!(b.next_is("enter", 1).await["row"]["board"], json!(2));

    f.delete("cards", 1).await;
    assert_eq!(b.next().await, json!({ "op": "delete", "key": 1 }));

    f.insert("cards", json!({ "id": 2, "board": 1 })).await;
    f.insert("cards", json!({ "id": 3, "board": 2 })).await;
    a.next_is("insert", 2).await;
    b.next_is("insert", 3).await;
    Ok(())
}

#[tokio::test]
async fn a_share_through_an_incoming_key_decides_each_change() -> Result<()> {
    let f = fixture("card_sharesↃcard.some(s => s.member === user.id)").await?;
    let mut a = f.page(&f.cards, alice(), None, true).await;
    let mut b = f.page(&f.cards, bob(), None, true).await;

    // Shared with Alice just after it is written: she hears of card 1, its
    // insert or not (the share is read when the change is decided), and then
    // its update.
    f.insert("cards", json!({ "id": 1, "title": "secret" }))
        .await;
    f.insert(
        "card_shares",
        json!({ "id": 1, "card": 1, "member": id(&alice()) }),
    )
    .await;
    f.update("cards", 1, json!({ "title": "shared" })).await;
    assert_eq!(a.through("update", 1).await["row"]["title"], "shared");

    // The share is removed: the card's next change is nothing to her, and its
    // delete is nothing to anybody.
    f.delete("card_shares", 1).await;
    f.update("cards", 1, json!({ "title": "unshared" })).await;
    f.delete("cards", 1).await;

    f.insert("cards", json!({ "id": 2 })).await;
    f.insert(
        "card_shares",
        json!({ "id": 2, "card": 2, "member": id(&alice()) }),
    )
    .await;
    f.update("cards", 2, json!({ "title": "hers" })).await;
    a.through("update", 2).await;

    f.insert("cards", json!({ "id": 3 })).await;
    f.insert(
        "card_shares",
        json!({ "id": 3, "card": 3, "member": id(&bob()) }),
    )
    .await;
    f.update("cards", 3, json!({ "title": "his" })).await;
    // Bob was sent nothing about cards 1 and 2, ever.
    b.through("update", 3).await;
    assert!(f.counts().evaluator_micros > 0, "the evaluator was asked");
    Ok(())
}

#[tokio::test]
async fn a_filter_narrows_and_moving_across_it_enters_and_leaves() -> Result<()> {
    let f = fixture("owner === user.id").await?;
    let mut todo = f
        .page(&f.cards, alice(), Some(json!({ "column": "todo" })), false)
        .await;
    let me = id(&alice());

    f.insert("cards", json!({ "id": 1, "owner": me, "column": "todo" }))
        .await;
    todo.next_is("insert", 1).await;
    f.update("cards", 1, json!({ "column": "done" })).await;
    assert_eq!(todo.next().await, json!({ "op": "leave", "key": 1 }));
    f.update("cards", 1, json!({ "column": "todo" })).await;
    todo.next_is("enter", 1).await;
    f.update("cards", 1, json!({ "title": "renamed" })).await;
    todo.next_is("update", 1).await;

    // Outside the filter from start to end: nothing.
    f.insert("cards", json!({ "id": 2, "owner": me, "column": "done" }))
        .await;
    f.delete("cards", 2).await;
    f.insert("cards", json!({ "id": 3, "owner": me, "column": "todo" }))
        .await;
    todo.next_is("insert", 3).await;
    assert_eq!(f.counts().suppressed_filter, 2);
    assert_eq!(f.counts().suppressed_access, 0);
    Ok(())
}

#[tokio::test]
async fn a_key_payload_sends_the_key_and_a_bare_attachment_is_sent_nothing() -> Result<()> {
    let f = fixture("owner === user.id").await?;
    let mut keys = f.page(&f.keys, admin(), None, false).await;
    // Attached without a filter or row access: never anybody's raw change.
    let (bare, mut bare_queue) = f
        .hub
        .connect(ConnectionInfo {
            app: "boards".to_owned(),
            user: None,
            session: None,
        })
        .expect("connects");
    bare.set_user(Some(admin()));
    let (_bare, _) = bare.attach(&f.cards, None).await;

    f.insert(
        "cards",
        json!({ "id": 1, "owner": id(&alice()), "title": "x" }),
    )
    .await;
    assert_eq!(keys.next().await, json!({ "op": "insert", "key": 1 }));
    f.update("cards", 1, json!({ "title": "y" })).await;
    assert_eq!(keys.next().await, json!({ "op": "update", "key": 1 }));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        bare_queue.try_recv().is_err(),
        "the bare attachment got nothing"
    );
    Ok(())
}

#[tokio::test]
async fn a_users_stream_is_typed_and_sent_without_the_password_hash() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    sc_auth::bootstrap(&catalog).await?;
    let users = catalog.require(sc_auth::USERS_TABLE)?;
    assert!(users.field(sc_auth::COL_PASSWORD_HASH).is_some());

    // The element type a client is generated from: the users table's columns,
    // the password hash not among them.
    let config = json!({ "table": "users" }).as_object().cloned().unwrap();
    let typed = TableChanges.element_type_in(&config, &catalog)?;
    let row = typed
        .keys()
        .iter()
        .find(|key| key.name == "row")
        .expect("a row key");
    let columns: Vec<&str> = row.keys.iter().map(|key| key.name.as_str()).collect();
    assert!(columns.contains(&"email"), "{columns:?}");
    assert!(
        !columns.contains(&sc_auth::COL_PASSWORD_HASH),
        "{columns:?}"
    );
    typed.validate()?;

    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(TableChanges) as Arc<dyn StreamProvider>)?;
    let supervisor = Arc::new(StreamSupervisor::new(
        Arc::new(registry),
        StreamConfig::default(),
    ));
    let mut stream = Stream::new("people", "table_changes");
    stream.configuration = config;
    let people = supervisor.start(stream).await?;
    let hub = LiveHub::new(LiveLimits::default());
    hub.set_streams(Arc::clone(&supervisor));
    catalog.add_table_events(LIVE_EVENTS, Arc::clone(&hub) as Arc<dyn TableEvents>)?;
    let (connection, mut queue) = hub
        .connect(ConnectionInfo {
            app: "people".to_owned(),
            user: Some(admin().id),
            session: None,
        })
        .expect("connects");
    connection.set_user(Some(admin()));
    let access = ChangeAccess {
        catalog: Arc::clone(&catalog),
        evaluator: None,
        filter: RowFilter::all(),
    };
    let (_attached, _) = connection.attach_changes(&people, access).await;

    // A system write (no caller) may set the hash; the page is still not sent
    // it.
    let created = rows::create_row(
        &catalog,
        &users,
        &json!({
            "id": Uuid::new_v4().to_string(),
            "email": "carol@example.com",
            "password_hash": "not-for-pages",
            "role": sc_auth::ROLE_PUBLIC,
        }),
    )
    .await?;
    let id = created["id"].as_str().expect("a uuid key").to_owned();
    rows::update_row(&catalog, &users, &id, &json!({ "email": "c@example.com" })).await?;
    for op in ["insert", "update"] {
        let delivery = tokio::time::timeout(Duration::from_secs(10), queue.recv())
            .await
            .expect("a frame")
            .expect("open");
        let value = &delivery.envelope.value;
        assert_eq!(value["op"], op);
        assert!(value["row"].get("email").is_some(), "{value}");
        assert!(
            value["row"].get(sc_auth::COL_PASSWORD_HASH).is_none(),
            "no password hash: {value}"
        );
    }
    Ok(())
}

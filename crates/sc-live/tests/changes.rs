//! The live hub as the write seam's `table_changes` listener (TODO.md "Live
//! updates" L3.3), against a real Postgres, writing through the row layer.
//!
//! What is claimed:
//!
//! - **only a subscription makes a write observed**: a `table_changes` stream
//!   that is running but has nobody attached does not make its table observed,
//!   attaching does — for exactly the writes the stream carries — and the last
//!   subscription leaving makes it unobserved again;
//! - **an unobserved update reads no pre-image**: counted at the driver, the
//!   only place a second `SELECT` cannot hide; an observed one reads exactly
//!   one, and its `old` is the row as it was;
//! - **the write hands off and returns**: when the write returns, the change
//!   has reached the stream (its counter moved) and **no** subscriber has been
//!   given it — that is the fan-out's work, on its own task, so a write's
//!   latency does not grow with the number of subscribers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use sc_api::rows;
use sc_catalog::{Catalog, DataField, LIVE_EVENTS, TableEvents, WriteOp};
use sc_db::{
    DatabaseDriver, DbCapabilities, DescribedColumn, PhysicalTable, RowStream, SchemaChange,
    Transaction,
};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_live::{ConnectionInfo, Delivery, LiveHub, LiveLimits};
use sc_query::{SqlDialect, Statement};
use sc_stream::{
    RunningStream, Stream, StreamConfig, StreamProvider, StreamRegistry, StreamSupervisor,
    TableChanges,
};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};
use tokio::sync::mpsc;

/// A real driver that counts the `SELECT`s that read `cards`.
struct Counting {
    inner: Arc<dyn DatabaseDriver>,
    card_reads: Arc<AtomicUsize>,
}

#[async_trait]
impl DatabaseDriver for Counting {
    async fn introspect(&self) -> Result<Vec<PhysicalTable>> {
        self.inner.introspect().await
    }

    async fn query(&self, stmt: &Statement) -> Result<RowStream> {
        if matches!(stmt, Statement::Select(_)) && format!("{stmt:?}").contains("\"cards\"") {
            self.card_reads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.query(stmt).await
    }

    async fn apply_schema(&self, change: &SchemaChange) -> Result<()> {
        self.inner.apply_schema(change).await
    }

    fn render_ddl(&self, change: &SchemaChange) -> Result<String> {
        self.inner.render_ddl(change)
    }

    async fn describe(&self, sql: &str, param_types: &[String]) -> Result<Vec<DescribedColumn>> {
        self.inner.describe(sql, param_types).await
    }

    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        self.inner.begin().await
    }

    fn capabilities(&self) -> DbCapabilities {
        self.inner.capabilities()
    }

    fn dialect(&self) -> &dyn SqlDialect {
        self.inner.dialect()
    }
}

struct Fixture {
    catalog: Arc<Catalog>,
    hub: Arc<LiveHub>,
    card_reads: Arc<AtomicUsize>,
    /// `cards_live`: inserts and updates on `cards`.
    cards: Arc<RunningStream>,
    /// `card_deletes`: deletes on `cards`.
    deletes: Arc<RunningStream>,
    _supervisor: Arc<StreamSupervisor>,
    _db: TestDb,
}

fn changes(name: &str, config: Json) -> Stream {
    let mut stream = Stream::new(name, "table_changes");
    stream.configuration = config.as_object().cloned().unwrap();
    stream
}

/// `cards` and `notes`, the hub installed on the write seam, and three
/// running `table_changes` streams — one of them, on `notes`, never
/// subscribed to.
async fn fixture() -> Result<Fixture> {
    let db = TestDb::new().await?;
    let card_reads = Arc::new(AtomicUsize::new(0));
    let driver = Arc::new(Counting {
        inner: Arc::new(PgDriver::from_pool(db.pool().clone())),
        card_reads: Arc::clone(&card_reads),
    });
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    for name in ["cards", "notes"] {
        catalog
            .create_table(
                name,
                &[
                    DataField::plain("id", TypeRef::Basic(BasicType::Int))
                        .required()
                        .primary_key(),
                    DataField::plain("title", TypeRef::Basic(BasicType::Text)),
                ],
            )
            .await?;
    }

    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(TableChanges) as Arc<dyn StreamProvider>)?;
    let supervisor = Arc::new(StreamSupervisor::new(
        Arc::new(registry),
        StreamConfig::default(),
    ));
    let cards = supervisor
        .start(changes(
            "cards_live",
            json!({ "table": "cards", "ops": ["insert", "update"] }),
        ))
        .await?;
    let deletes = supervisor
        .start(changes(
            "card_deletes",
            json!({ "table": "cards", "ops": ["delete"] }),
        ))
        .await?;
    supervisor
        .start(changes("notes_live", json!({ "table": "notes" })))
        .await?;
    for running in [&cards, &deletes] {
        assert!(running.status().is_running(), "{:?}", running.status());
    }

    let hub = LiveHub::new(LiveLimits::default());
    hub.set_streams(Arc::clone(&supervisor));
    catalog.add_table_events(LIVE_EVENTS, Arc::clone(&hub) as Arc<dyn TableEvents>)?;
    Ok(Fixture {
        catalog,
        hub,
        card_reads,
        cards,
        deletes,
        _supervisor: supervisor,
        _db: db,
    })
}

fn anonymous() -> ConnectionInfo {
    ConnectionInfo {
        app: "boards".to_owned(),
        user: None,
        session: None,
    }
}

async fn next(deliveries: &mut mpsc::Receiver<Delivery>) -> Delivery {
    tokio::time::timeout(Duration::from_secs(10), deliveries.recv())
        .await
        .expect("a delivery within the timeout")
        .expect("the queue is open")
}

#[tokio::test]
async fn only_a_subscription_makes_a_write_observed() -> Result<()> {
    let f = fixture().await?;
    let observed = |table: &str, op: WriteOp| f.catalog.observes_writes(table, op);

    // Three streams are running, and nobody is subscribed: nothing is observed.
    for op in [WriteOp::Insert, WriteOp::Update, WriteOp::Delete] {
        assert!(!observed("cards", op), "{op}");
        assert!(!observed("notes", op), "{op}");
    }

    let (first, _first_queue) = f.hub.connect(anonymous()).expect("connects");
    let (second, _second_queue) = f.hub.connect(anonymous()).expect("connects");
    let (a, _) = first.attach(&f.cards, None).await;
    // Exactly the writes `cards_live` carries, on exactly its table.
    assert!(observed("cards", WriteOp::Insert));
    assert!(observed("cards", WriteOp::Update));
    assert!(!observed("cards", WriteOp::Delete));
    assert!(!observed("notes", WriteOp::Insert));

    let (b, _) = second.attach(&f.cards, None).await;
    let (d, _) = second.attach(&f.deletes, None).await;
    assert!(observed("cards", WriteOp::Delete));
    drop(a);
    assert!(
        observed("cards", WriteOp::Update),
        "the second subscription still watches"
    );
    drop(d);
    assert!(!observed("cards", WriteOp::Delete));
    drop(b);
    for op in [WriteOp::Insert, WriteOp::Update, WriteOp::Delete] {
        assert!(!observed("cards", op), "the last one left: {op}");
    }
    Ok(())
}

#[tokio::test]
async fn an_unobserved_update_reads_no_pre_image_and_an_observed_one_reads_one() -> Result<()> {
    let f = fixture().await?;
    let cards = f.catalog.require("cards")?;
    rows::create_row(&f.catalog, &cards, &json!({ "id": 1, "title": "draft" })).await?;

    f.card_reads.store(0, Ordering::SeqCst);
    rows::update_row(&f.catalog, &cards, "1", &json!({ "title": "unwatched" })).await?;
    assert_eq!(
        f.card_reads.load(Ordering::SeqCst),
        0,
        "nobody is subscribed, so the update fetched no pre-image"
    );

    let (connection, mut queue) = f.hub.connect(anonymous()).expect("connects");
    let (attached, replay) = connection.attach(&f.cards, None).await;
    assert!(replay.is_empty(), "a table-changes stream replays nothing");
    f.card_reads.store(0, Ordering::SeqCst);
    rows::update_row(&f.catalog, &cards, "1", &json!({ "title": "watched" })).await?;
    assert_eq!(f.card_reads.load(Ordering::SeqCst), 1, "one pre-image read");

    let change = next(&mut queue).await;
    assert_eq!(change.attachment, attached.attachment());
    let value = &change.envelope.value;
    assert_eq!(value["op"], "update");
    assert_eq!(value["key"], json!(1));
    assert_eq!(value["old"]["title"], "unwatched");
    assert_eq!(value["new"]["title"], "watched");
    assert_eq!(change.envelope.stream, "cards_live");

    // An insert carries no `old`; a delete is not a write this stream carries.
    rows::create_row(&f.catalog, &cards, &json!({ "id": 2, "title": "new" })).await?;
    let inserted = next(&mut queue).await;
    assert_eq!(inserted.envelope.value["op"], "insert");
    assert_eq!(inserted.envelope.value["old"], Json::Null);
    assert_eq!(inserted.envelope.value["new"]["id"], json!(2));
    rows::delete_row(&f.catalog, &cards, "2").await?;

    drop(attached);
    f.card_reads.store(0, Ordering::SeqCst);
    rows::update_row(
        &f.catalog,
        &cards,
        "1",
        &json!({ "title": "unwatched again" }),
    )
    .await?;
    assert_eq!(
        f.card_reads.load(Ordering::SeqCst),
        0,
        "the last subscriber left, so the pre-image is not read again"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        queue.try_recv().is_err(),
        "nothing after the delete or the unsubscribe"
    );
    Ok(())
}

/// The test runs on one thread, so the fan-out task cannot run until this one
/// awaits something that yields. The write's own path reaches the stream and
/// returns; every one of the many subscribers is served afterwards, by the
/// fan-out.
#[tokio::test(flavor = "current_thread")]
async fn the_write_hands_the_change_off_and_the_fan_out_delivers_it() -> Result<()> {
    let f = fixture().await?;
    let cards = f.catalog.require("cards")?;
    const SUBSCRIBERS: usize = 500;
    let mut held = Vec::new();
    for _ in 0..SUBSCRIBERS {
        let (connection, queue) = f.hub.connect(anonymous()).expect("connects");
        let (attached, _) = connection.attach(&f.cards, None).await;
        held.push((connection, queue, attached));
    }
    let before = f.cards.counters().elements;

    rows::create_row(&f.catalog, &cards, &json!({ "id": 9, "title": "hot" })).await?;

    assert_eq!(
        f.cards.counters().elements,
        before + 1,
        "the write reached the stream before it returned"
    );
    for (_, queue, _) in &mut held {
        assert!(
            queue.try_recv().is_err(),
            "no subscriber was served on the write's path"
        );
    }
    for (_, queue, _) in &mut held {
        assert_eq!(next(queue).await.envelope.value["new"]["id"], json!(9));
    }
    Ok(())
}

//! Document rooms and their persistence (TODO.md "Live updates" L5.2–L5.5), on
//! an in-memory SQLite database, through the real row layer.
//!
//! What is claimed:
//!
//! - **L5.2**: a room loads the stored document (an empty one for a null
//!   field), applies a member's update, relays it to the other members and
//!   not back to the sender, and closes with its last member.
//! - **L5.3**: an update that would take the document past `max_doc_bytes` is
//!   refused `too_large` and not applied; a document over 1 MB opens whole.
//! - **L5.4**: two rooms on one row — two processes — persisting interleaved
//!   edits lose neither, and the mirror is the merged text (and the merged
//!   JSON); the persist is a system update the write seam sees, with the
//!   mirror and never the document bytes; and a room closing with nothing
//!   changed since its last persist writes nothing.
//! - **L5.5**: `live.document` reads the text of either root kind, edits a
//!   `text` root, appends a paragraph to an `xml` root, persists before it
//!   answers, and reaches a member of the open room at once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sc_api::rows;
use sc_catalog::{
    Catalog, DataField, FieldMeta, TableEvents, TableWrite, WriteOp, bootstrap_field_meta,
    bootstrap_table_meta, save_field_meta,
};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_expr::DocRequest;
use sc_live::{
    CollabDoc, DocInbox, DocNews, DocRefusal, DocRooms, DocShape, DocTarget, LiveHub, LiveLimits,
    MirrorFormat, RootKind,
};
use sc_query::Value;
use sc_stream::{
    Document, Stream, StreamConfig, StreamId, StreamProvider, StreamRegistry, StreamSupervisor,
};
use sc_types::{BasicType, COLLAB_DOC, TypeRef};
use serde_json::{Value as Json, json};

/// Counts the updates the write seam is told about on `notes`, and keeps the
/// last row it was handed.
#[derive(Default)]
struct Writes {
    updates: AtomicUsize,
    last: Mutex<Option<Json>>,
}

#[async_trait::async_trait]
impl TableEvents for Writes {
    fn observes(&self, table: &str, op: WriteOp) -> bool {
        table == "notes" && op == WriteOp::Update
    }

    async fn emit(&self, _catalog: &Catalog, write: TableWrite<'_>) -> Result<()> {
        self.updates.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(write.row.clone());
        Ok(())
    }
}

struct Fixture {
    catalog: Arc<Catalog>,
    writes: Arc<Writes>,
}

/// `notes(id, title, body collab_doc, body_text)` with rows 1 and 2.
async fn fixture() -> Result<Fixture> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    catalog
        .create_table(
            "notes",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key(),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
                DataField::plain("body", TypeRef::Basic(BasicType::Bytes)),
                DataField::plain("body_text", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    catalog.reload().await?;
    save_field_meta(
        &catalog,
        &FieldMeta::new("notes", "body").rich_type(COLLAB_DOC),
    )
    .await?;
    let notes = catalog.require("notes")?;
    for (id, title) in [(1, "Plans"), (2, "Minutes")] {
        rows::create_row(&catalog, &notes, &json!({"id": id, "title": title})).await?;
    }
    let writes = Arc::new(Writes::default());
    catalog.add_table_events("test-writes", Arc::clone(&writes) as Arc<dyn TableEvents>)?;
    Ok(Fixture { catalog, writes })
}

fn target(key: &str, kind: RootKind, format: MirrorFormat) -> DocTarget {
    DocTarget {
        stream: StreamId(uuid::Uuid::from_u128(7)),
        stream_name: "note_body".into(),
        table: "notes".into(),
        field: "body".into(),
        key: key.into(),
        shape: DocShape::new("default", kind),
        mirror: Some(("body_text".into(), format)),
    }
}

async fn stored(catalog: &Catalog, key: &str) -> Result<(Option<Vec<u8>>, Json)> {
    let notes = catalog.require("notes")?;
    let body = match rows::read_field(catalog, &notes, "body", key).await? {
        Value::Bytes(b) => Some(b),
        _ => None,
    };
    let mirror = match rows::read_field(catalog, &notes, "body_text", key).await? {
        Value::Text(t) => Json::String(t),
        _ => Json::Null,
    };
    Ok((body, mirror))
}

/// A browser's copy of the document: it starts from `doc_sync` and makes an
/// edit, answering the update it would send.
fn client(sync: &[u8], kind: RootKind) -> CollabDoc {
    CollabDoc::load(DocShape::new("default", kind), Some(sync)).unwrap()
}

#[tokio::test]
async fn a_room_relays_an_edit_to_the_other_members_and_not_back() -> Result<()> {
    let f = fixture().await?;
    let rooms = DocRooms::new(2 * 1024 * 1024, Duration::from_secs(3600));
    let t = target("1", RootKind::Text, MirrorFormat::Text);
    let (alice_inbox, bob_inbox) = (DocInbox::new(), DocInbox::new());
    let (alice, sync) = rooms.join(&f.catalog, &t, Arc::clone(&alice_inbox)).await?;
    let (bob, _) = rooms.join(&f.catalog, &t, Arc::clone(&bob_inbox)).await?;
    assert_eq!(rooms.counts(), (1, 2), "one room, two members");
    // A null field is an empty document.
    assert_eq!(client(&sync, RootKind::Text).text(), "");

    let mine = client(&sync, RootKind::Text);
    let update = mine.edit_text(0, 0, "Hello").unwrap();
    alice.apply(&update).unwrap();
    assert!(
        alice_inbox.is_empty(),
        "the sender is not sent its own edit"
    );
    let news = bob_inbox.take();
    assert_eq!(news, vec![(bob.id(), DocNews::Update(Arc::new(update)))]);
    assert_eq!(bob.text()?, "Hello");

    // Garbage is refused and changes nothing.
    assert!(matches!(
        alice.apply(b"\xff\xfe\xfd"),
        Err(DocRefusal::Invalid(_))
    ));
    assert_eq!(bob.text()?, "Hello");

    // The last to leave closes the room — and persists what changed.
    drop(alice);
    assert_eq!(rooms.counts(), (1, 1));
    drop(bob);
    for _ in 0..100 {
        if rooms.counts() == (0, 0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(rooms.counts(), (0, 0), "the room closed");
    let (body, mirror) = stored(&f.catalog, "1").await?;
    assert_eq!(client(&body.unwrap(), RootKind::Text).text(), "Hello");
    assert_eq!(mirror, json!("Hello"));
    Ok(())
}

#[tokio::test]
async fn two_rooms_persisting_interleaved_edits_lose_neither() -> Result<()> {
    for (kind, format) in [
        (RootKind::Text, MirrorFormat::Text),
        (RootKind::Text, MirrorFormat::Json),
        (RootKind::Xml, MirrorFormat::Text),
        (RootKind::Xml, MirrorFormat::Json),
    ] {
        let f = fixture().await?;
        // Two processes: two registries, each with its own room on row 2.
        let one = DocRooms::new(2 * 1024 * 1024, Duration::from_secs(3600));
        let two = DocRooms::new(2 * 1024 * 1024, Duration::from_secs(3600));
        let t = target("2", kind, format);
        let (a, sync_a) = one.join(&f.catalog, &t, DocInbox::new()).await?;
        let (b, sync_b) = two.join(&f.catalog, &t, DocInbox::new()).await?;
        let (ca, cb) = (client(&sync_a, kind), client(&sync_b, kind));
        let edit = |doc: &CollabDoc, text: &str| match kind {
            RootKind::Text => doc.edit_text(doc.text_len().unwrap(), 0, text).unwrap(),
            RootKind::Xml => doc.append_paragraph(text).unwrap(),
        };

        a.apply(&edit(&ca, "from one")).unwrap();
        assert!(a.persist().await?, "a change is written");
        b.apply(&edit(&cb, "from two")).unwrap();
        assert!(b.persist().await?);
        a.apply(&edit(&ca, " again")).unwrap();
        assert!(a.persist().await?);

        let (body, mirror) = stored(&f.catalog, "2").await?;
        let merged = client(&body.unwrap(), kind);
        let text = merged.text();
        for piece in ["from one", "from two", " again"] {
            assert!(text.contains(piece), "{kind:?}: {piece} in {text:?}");
        }
        assert_eq!(
            mirror,
            json!(merged.mirror(format)),
            "{kind:?}/{format:?}: the mirror is the merged document"
        );
        if format == MirrorFormat::Json {
            assert!(mirror.as_str().unwrap().starts_with('"'), "{mirror}");
        }
        // Room one merged what two persisted when it persisted again.
        assert!(a.text()?.contains("from two"), "{}", a.text()?);

        // Persisting with nothing changed writes nothing.
        let before = f.writes.updates.load(Ordering::SeqCst);
        assert!(!a.persist().await?);
        assert_eq!(f.writes.updates.load(Ordering::SeqCst), before);
        // The persist was an ordinary update to the write seam, carrying the
        // mirror and never the document's bytes.
        let last = f.writes.last.lock().unwrap().clone().unwrap();
        assert!(last.get("body").is_none(), "{last}");
        assert!(last.get("body_text").is_some(), "{last}");
    }
    Ok(())
}

#[tokio::test]
async fn a_room_closing_with_nothing_new_writes_nothing() -> Result<()> {
    let f = fixture().await?;
    let rooms = DocRooms::new(2 * 1024 * 1024, Duration::from_millis(20));
    let t = target("1", RootKind::Text, MirrorFormat::Text);
    let (seat, sync) = rooms.join(&f.catalog, &t, DocInbox::new()).await?;
    seat.apply(&client(&sync, RootKind::Text).edit_text(0, 0, "x").unwrap())
        .unwrap();
    // The debounced persist runs on its own.
    for _ in 0..200 {
        if f.writes.updates.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        f.writes.updates.load(Ordering::SeqCst),
        1,
        "debounced persist"
    );
    drop(seat);
    for _ in 0..100 {
        if rooms.counts() == (0, 0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(rooms.counts(), (0, 0));
    assert_eq!(
        f.writes.updates.load(Ordering::SeqCst),
        1,
        "closing wrote nothing more"
    );
    // A room opened and closed without an edit writes nothing either.
    let (seat, _) = rooms.join(&f.catalog, &t, DocInbox::new()).await?;
    drop(seat);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.writes.updates.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn the_size_limit_refuses_an_edit_and_a_large_document_opens() -> Result<()> {
    let f = fixture().await?;
    // A small limit: an edit past it is refused, saying so, and not applied.
    let small = DocRooms::new(4 * 1024, Duration::from_secs(3600));
    let t = target("1", RootKind::Text, MirrorFormat::Text);
    let (seat, sync) = small.join(&f.catalog, &t, DocInbox::new()).await?;
    let big = client(&sync, RootKind::Text)
        .edit_text(0, 0, &"x".repeat(8 * 1024))
        .unwrap();
    match seat.apply(&big) {
        Err(DocRefusal::TooLarge(why)) => assert!(why.contains("larger than"), "{why}"),
        other => panic!("expected too_large, got {other:?}"),
    }
    assert_eq!(seat.text()?, "");
    let fine = client(&sync, RootKind::Text)
        .edit_text(0, 0, "short")
        .unwrap();
    seat.apply(&fine).unwrap();
    drop(seat);

    // A document over 1 MB, stored, opens whole under the default limit.
    let doc = CollabDoc::empty(DocShape::new("default", RootKind::Text));
    doc.edit_text(0, 0, &"lorem ipsum ".repeat(100_000))
        .unwrap();
    let state = doc.state();
    assert!(state.len() > 1024 * 1024, "{}", state.len());
    let notes = f.catalog.require("notes")?;
    rows::write_document_in(
        &f.catalog,
        &notes,
        "2",
        "body",
        &state,
        None,
        &rows::Executor::Pooled,
    )
    .await?;
    let rooms = DocRooms::new(
        LiveLimits::default().max_doc_bytes,
        Duration::from_secs(3600),
    );
    let (seat, sync) = rooms
        .join(
            &f.catalog,
            &target("2", RootKind::Text, MirrorFormat::Text),
            DocInbox::new(),
        )
        .await?;
    assert!(sync.len() > 1024 * 1024);
    assert_eq!(seat.text()?.len(), 1_200_000);
    Ok(())
}

/// A hub with a running `document` stream on `notes.body`, of `kind`.
async fn hub_with(f: &Fixture, kind: &str) -> Result<(Arc<LiveHub>, Arc<StreamSupervisor>)> {
    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(Document) as Arc<dyn StreamProvider>)?;
    let supervisor = Arc::new(StreamSupervisor::new(
        Arc::new(registry),
        StreamConfig::default(),
    ));
    let mut stream = Stream::new("note_body", "document");
    stream.configuration = json!({
        "table": "notes", "field": "body", "root_kind": kind, "text_mirror": "body_text"
    })
    .as_object()
    .cloned()
    .unwrap();
    supervisor.start(stream).await?;
    let hub = LiveHub::new(LiveLimits::default());
    hub.set_streams(Arc::clone(&supervisor));
    hub.set_catalog(&f.catalog);
    Ok((hub, supervisor))
}

#[tokio::test]
async fn live_document_reads_and_writes_from_the_server() -> Result<()> {
    let f = fixture().await?;
    let (hub, _supervisor) = hub_with(&f, "text").await?;
    // An editor has the document open.
    let running = _supervisor.by_name("note_body").unwrap();
    let t = DocTarget::of(&running, "1")?;
    let inbox = DocInbox::new();
    let (editor, _) = hub.docs().join(&f.catalog, &t, Arc::clone(&inbox)).await?;

    let text = hub
        .document(
            "note_body",
            "1",
            DocRequest::Append {
                text: "Hello".into(),
            },
        )
        .await?;
    assert_eq!(text, "Hello");
    let text = hub
        .document(
            "note_body",
            "1",
            DocRequest::Edit {
                index: 5,
                delete: 0,
                insert: ", world".into(),
            },
        )
        .await?;
    assert_eq!(text, "Hello, world");
    // The editor was sent both edits at once, and has them.
    assert_eq!(inbox.take().len(), 2);
    assert_eq!(editor.text()?, "Hello, world");
    // Persisted before answering.
    assert_eq!(stored(&f.catalog, "1").await?.1, json!("Hello, world"));
    assert_eq!(
        hub.document("note_body", "1", DocRequest::Read).await?,
        "Hello, world"
    );
    // Refusals: past the end, a missing row, a stream that is not a document.
    assert!(
        hub.document(
            "note_body",
            "1",
            DocRequest::Edit {
                index: 99,
                delete: 0,
                insert: "x".into()
            }
        )
        .await
        .is_err()
    );
    let missing = hub
        .document("note_body", "99", DocRequest::Read)
        .await
        .unwrap_err()
        .to_string();
    assert!(missing.contains("99"), "{missing}");
    assert!(hub.document("nope", "1", DocRequest::Read).await.is_err());
    drop(editor);

    // An xml root: a paragraph is appended, and read back as a line.
    let f = fixture().await?;
    let (hub, _supervisor) = hub_with(&f, "xml").await?;
    hub.document(
        "note_body",
        "2",
        DocRequest::Append {
            text: "First".into(),
        },
    )
    .await?;
    let text = hub
        .document(
            "note_body",
            "2",
            DocRequest::Append {
                text: "Second".into(),
            },
        )
        .await?;
    assert_eq!(text, "First\nSecond");
    let err = hub
        .document(
            "note_body",
            "2",
            DocRequest::Edit {
                index: 0,
                delete: 0,
                insert: "x".into(),
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not text"), "{err}");
    Ok(())
}

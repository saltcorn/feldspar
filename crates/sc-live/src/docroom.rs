//! Document rooms: one per open `(document stream, row)` (TODO.md "Live
//! updates" L5.2–L5.4).
//!
//! A [`DocRoom`] holds a document in memory while somebody has it open. It is
//! loaded from the row's `collab_doc` field (an empty document if the field is
//! null) by its first member, applies every authorised update, relays each one
//! to its other members, and **persists** the merged state back into the field
//! — debounced ([`LiveLimits::doc_persist_debounce`], 2 s) and once more when
//! its last member leaves.
//!
//! ## Members and their inboxes
//!
//! A member is a [`DocSeat`], held by one subscription (or briefly by a code
//! body's `live.document`). Its connection's [`DocInbox`] is handed every other
//! member's update — a list behind a lock and a bell, never a wait, like
//! presence's. An inbox past [`DOC_INBOX_CAP`] drops that seat's updates and
//! marks it for a **resync**: the connection sends the whole document again
//! (`doc_sync`), which a Yjs client merges idempotently. A member's copy is
//! patched in order or replaced, never silently behind.
//!
//! ## Persisting
//!
//! One persist at a time per room, in **one transaction**: the stored value is
//! read with `SELECT … FOR UPDATE` (on Postgres; SQLite's write transaction
//! holds the database), merged into the room — so whatever another process
//! persisted meanwhile is kept, and relayed to this room's members — and the
//! merged state is written with the text mirror through the row layer as a
//! system write ([`sc_api::rows::write_document_in`]), so update triggers and
//! `table_changes` streams see it. CRDT merges are idempotent and commutative,
//! which is why two processes persisting one document lose nothing. A persist
//! is **skipped** when nothing changed since the last one, so a room closing
//! after its last persist writes nothing.
//!
//! ## Size
//!
//! An update that would take the document past
//! [`LiveLimits::max_doc_bytes`] is refused (`too_large`) and not applied. The
//! room keeps an upper bound of its encoded size (the state when loaded plus
//! every update since; merges only shrink it) and encodes the document to
//! measure it exactly only when that bound says it might be too big.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use sc_api::rows;
use sc_catalog::{Catalog, SharedTx};
use sc_error::{Error, Result};
use sc_query::{Expr, Projection, Select, Source, Statement, Value};
use sc_stream::{DocSettings, RunningStream, StreamId};
use tokio::sync::{Notify, OnceCell};

use crate::doc::{CollabDoc, DocShape, MirrorFormat, RootKind};

/// How many updates one connection's inbox holds before a seat's are dropped
/// for a resync.
pub const DOC_INBOX_CAP: usize = 1024;

/// A room's key: the stream and the row's key, as a topic spells it.
pub type DocKey = (StreamId, String);

/// What a document stream reads and writes, for one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocTarget {
    /// The stream.
    pub stream: StreamId,
    /// The stream's name, for sentences.
    pub stream_name: String,
    /// The table.
    pub table: String,
    /// The `collab_doc` column.
    pub field: String,
    /// The row's key.
    pub key: String,
    /// The root the stream reads.
    pub shape: DocShape,
    /// The mirror column and its format, if the stream keeps one.
    pub mirror: Option<(String, MirrorFormat)>,
}

impl DocTarget {
    /// The target of a `document` stream's row `key`, from the stream's
    /// settings.
    pub fn of(running: &RunningStream, key: &str) -> Result<DocTarget> {
        let settings = DocSettings::from_config(&running.configuration())?;
        DocTarget::from_settings(running.id(), &running.name(), &settings, key)
    }

    /// The target `settings` describe for row `key`.
    pub fn from_settings(
        stream: StreamId,
        stream_name: &str,
        settings: &DocSettings,
        key: &str,
    ) -> Result<DocTarget> {
        let kind = RootKind::parse(&settings.root_kind).ok_or_else(|| {
            Error::invalid(format!("`{}` is not a root kind", settings.root_kind))
        })?;
        let format = MirrorFormat::parse(&settings.mirror_format).ok_or_else(|| {
            Error::invalid(format!(
                "`{}` is not a mirror format",
                settings.mirror_format
            ))
        })?;
        Ok(DocTarget {
            stream,
            stream_name: stream_name.to_owned(),
            table: settings.table.clone(),
            field: settings.field.clone(),
            key: key.to_owned(),
            shape: DocShape::new(settings.root.clone(), kind),
            mirror: settings.text_mirror.clone().map(|m| (m, format)),
        })
    }

    fn room_key(&self) -> DocKey {
        (self.stream, self.key.clone())
    }
}

/// Why an update was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocRefusal {
    /// It would take the document past the size limit. The sentence says so.
    TooLarge(String),
    /// It is not a Yjs update, or the room could not load its document.
    Invalid(String),
}

/// What a member's connection is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocNews {
    /// Another member's update, to send as `doc_update`.
    Update(Arc<Vec<u8>>),
    /// Updates were dropped: send the whole document (`doc_sync`) instead.
    Resync,
}

/// One connection's document updates waiting to be sent, per seat.
#[derive(Debug, Default)]
pub struct DocInbox {
    pending: Mutex<VecDeque<(u64, DocNews)>>,
    bell: Notify,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl DocInbox {
    /// An empty inbox.
    pub fn new() -> Arc<DocInbox> {
        Arc::new(DocInbox::default())
    }

    fn push(&self, seat: u64, news: DocNews) {
        let mut pending = lock(&self.pending);
        if pending
            .iter()
            .any(|(s, n)| *s == seat && *n == DocNews::Resync)
        {
            return;
        }
        if pending.len() >= DOC_INBOX_CAP || news == DocNews::Resync {
            // The whole document replaces everything queued for this seat.
            pending.retain(|(s, _)| *s != seat);
            pending.push_back((seat, DocNews::Resync));
        } else {
            pending.push_back((seat, news));
        }
        drop(pending);
        self.bell.notify_one();
    }

    /// Everything waiting, oldest first.
    pub fn take(&self) -> Vec<(u64, DocNews)> {
        lock(&self.pending).drain(..).collect()
    }

    /// Wait until something is pushed.
    pub async fn rung(&self) {
        self.bell.notified().await;
    }

    /// Whether anything is waiting.
    pub fn is_empty(&self) -> bool {
        lock(&self.pending).is_empty()
    }
}

/// The hub's open documents.
pub struct DocRooms {
    rooms: Mutex<HashMap<DocKey, Arc<DocRoom>>>,
    next_seat: AtomicU64,
    max_doc_bytes: usize,
    debounce: Duration,
    me: Weak<DocRooms>,
}

impl DocRooms {
    /// No rooms, with these limits.
    pub fn new(max_doc_bytes: usize, debounce: Duration) -> Arc<DocRooms> {
        Arc::new_cyclic(|me| DocRooms {
            rooms: Mutex::new(HashMap::new()),
            next_seat: AtomicU64::new(1),
            max_doc_bytes,
            debounce,
            me: me.clone(),
        })
    }

    /// The room for `target`, opened (and its document loaded) if it was not.
    /// The caller is not a member until it [`join`](DocRoom::join)s.
    async fn room(&self, catalog: &Arc<Catalog>, target: &DocTarget) -> Result<Arc<DocRoom>> {
        let room = {
            let mut rooms = lock(&self.rooms);
            match rooms.get(&target.room_key()) {
                // A configuration change (a new root, a new field) is a new
                // room: the old one closes with its last member.
                Some(room) if room.target == *target => Arc::clone(room),
                _ => {
                    let room = Arc::new(DocRoom {
                        target: target.clone(),
                        catalog: Arc::clone(catalog),
                        doc: OnceCell::new(),
                        members: Mutex::new(HashMap::new()),
                        version: AtomicU64::new(0),
                        persisted: AtomicU64::new(0),
                        size_bound: AtomicU64::new(0),
                        scheduled: AtomicBool::new(false),
                        persisting: tokio::sync::Mutex::new(()),
                        max_doc_bytes: self.max_doc_bytes,
                        debounce: self.debounce,
                        rooms: self.me.clone(),
                    });
                    rooms.insert(target.room_key(), Arc::clone(&room));
                    room
                }
            }
        };
        room.loaded().await?;
        Ok(room)
    }

    /// Join the document `target` names as a member whose connection's inbox
    /// is `inbox`. Answers the seat and the whole document, to send as
    /// `doc_sync`.
    pub async fn join(
        &self,
        catalog: &Arc<Catalog>,
        target: &DocTarget,
        inbox: Arc<DocInbox>,
    ) -> Result<(DocSeat, Vec<u8>)> {
        let room = self.room(catalog, target).await?;
        let id = self.next_seat.fetch_add(1, Ordering::Relaxed);
        lock(&room.members).insert(id, inbox);
        let state = room.state()?;
        Ok((DocSeat { room, id }, state))
    }

    /// Open `target` for a server-side writer (`live.document`) — a member
    /// with nobody to tell.
    pub async fn open(&self, catalog: &Arc<Catalog>, target: &DocTarget) -> Result<DocSeat> {
        Ok(self.join(catalog, target, DocInbox::new()).await?.0)
    }

    /// How many rooms are open, and members in them.
    pub fn counts(&self) -> (usize, usize) {
        let rooms = lock(&self.rooms);
        let members = rooms.values().map(|r| lock(&r.members).len()).sum();
        (rooms.len(), members)
    }

    /// The room open for `key`, if one is.
    pub fn get(&self, key: &DocKey) -> Option<Arc<DocRoom>> {
        lock(&self.rooms).get(key).cloned()
    }

    /// Forget `room` if it is still the one under its key and nobody joined it
    /// while it was closing.
    fn forget_if_empty(&self, room: &Arc<DocRoom>) {
        let mut rooms = lock(&self.rooms);
        let key = room.target.room_key();
        if rooms.get(&key).is_some_and(|r| Arc::ptr_eq(r, room)) && lock(&room.members).is_empty() {
            rooms.remove(&key);
        }
    }
}

/// One open document.
pub struct DocRoom {
    target: DocTarget,
    catalog: Arc<Catalog>,
    doc: OnceCell<Mutex<CollabDoc>>,
    members: Mutex<HashMap<u64, Arc<DocInbox>>>,
    /// Updates applied since the room opened.
    version: AtomicU64,
    /// `version` at the last persist.
    persisted: AtomicU64,
    /// An upper bound of the encoded document's size.
    size_bound: AtomicU64,
    /// Whether a debounced persist is waiting to run.
    scheduled: AtomicBool,
    /// One persist at a time.
    persisting: tokio::sync::Mutex<()>,
    max_doc_bytes: usize,
    debounce: Duration,
    rooms: Weak<DocRooms>,
}

impl std::fmt::Debug for DocRoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocRoom")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl DocRoom {
    /// What the room is of.
    pub fn target(&self) -> &DocTarget {
        &self.target
    }

    /// How many members it has.
    pub fn members(&self) -> usize {
        lock(&self.members).len()
    }

    /// The document, loaded from the row on first use.
    async fn loaded(&self) -> Result<&Mutex<CollabDoc>> {
        self.doc
            .get_or_try_init(|| async {
                let stored = self.read_stored().await?;
                let doc = CollabDoc::load(self.target.shape.clone(), stored.as_deref())?;
                let size = stored.as_ref().map_or(0, Vec::len);
                self.size_bound.store(size as u64, Ordering::Relaxed);
                Ok::<_, Error>(Mutex::new(doc))
            })
            .await
    }

    fn doc(&self) -> Result<MutexGuard<'_, CollabDoc>> {
        self.doc
            .get()
            .map(lock)
            .ok_or_else(|| Error::msg("the document room has not loaded its document"))
    }

    /// The row's stored document, `None` when the field is null. A row that
    /// is not there is an error: there is nothing to edit.
    async fn read_stored(&self) -> Result<Option<Vec<u8>>> {
        let table = self.catalog.require(&self.target.table)?;
        match rows::read_field(&self.catalog, &table, &self.target.field, &self.target.key).await? {
            Value::Bytes(bytes) => Ok(Some(bytes)),
            Value::Null => Ok(None),
            other => Err(Error::invalid(format!(
                "`{}.{}` holds {}, not a document",
                self.target.table,
                self.target.field,
                other.kind()
            ))),
        }
    }

    /// The whole document as one update — `doc_sync`.
    pub fn state(&self) -> Result<Vec<u8>> {
        Ok(self.doc()?.state())
    }

    /// The document's plain text.
    pub fn text(&self) -> Result<String> {
        Ok(self.doc()?.text())
    }

    /// Apply a member's update, relay it to the other members and schedule a
    /// persist.
    fn apply_from(&self, seat: u64, update: &[u8]) -> std::result::Result<(), DocRefusal> {
        {
            let doc = self.doc().map_err(|e| DocRefusal::Invalid(e.to_string()))?;
            self.check_size(&doc, update.len())?;
            doc.apply(update)
                .map_err(|e| DocRefusal::Invalid(e.to_string()))?;
        }
        self.applied(Some(seat), Arc::new(update.to_vec()));
        Ok(())
    }

    /// Refuse an update that would take the document past the limit.
    fn check_size(&self, doc: &CollabDoc, adding: usize) -> std::result::Result<(), DocRefusal> {
        let limit = self.max_doc_bytes as u64;
        let bound = self.size_bound.load(Ordering::Relaxed) + adding as u64;
        if bound <= limit {
            return Ok(());
        }
        // The bound says it might be too big: measure.
        let exact = doc.state().len() as u64;
        self.size_bound.store(exact, Ordering::Relaxed);
        if exact + adding as u64 > limit {
            return Err(DocRefusal::TooLarge(format!(
                "this edit would make the document larger than the {} the stream `{}` allows, \
                 so it was not applied; the document is {} now",
                human_bytes(limit),
                self.target.stream_name,
                human_bytes(exact)
            )));
        }
        Ok(())
    }

    /// An update was applied: count it, relay it, schedule a persist.
    fn applied(&self, from: Option<u64>, update: Arc<Vec<u8>>) {
        self.size_bound
            .fetch_add(update.len() as u64, Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::AcqRel);
        self.relay(from, &update);
        self.schedule();
    }

    /// Hand `update` to every member but `from`.
    fn relay(&self, from: Option<u64>, update: &Arc<Vec<u8>>) {
        for (seat, inbox) in lock(&self.members).iter() {
            if Some(*seat) != from {
                inbox.push(*seat, DocNews::Update(Arc::clone(update)));
            }
        }
    }

    /// Persist after the debounce, unless one is already waiting.
    fn schedule(&self) {
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(room) = self.self_arc() else {
            self.scheduled.store(false, Ordering::Release);
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.scheduled.store(false, Ordering::Release);
            return;
        };
        handle.spawn(async move {
            tokio::time::sleep(room.debounce).await;
            room.scheduled.store(false, Ordering::Release);
            if let Err(e) = room.persist().await {
                sc_log::log_warn!(
                    "persisting the document `{}` of stream `{}` failed: {}",
                    room.target.key,
                    room.target.stream_name,
                    sc_error::format_chain(&e)
                );
            }
        });
    }

    /// This room as an `Arc`, from the registry that holds it.
    fn self_arc(&self) -> Option<Arc<DocRoom>> {
        let rooms = self.rooms.upgrade()?;
        let room = rooms.get(&self.target.room_key())?;
        std::ptr::eq(room.as_ref(), self).then_some(room)
    }

    /// Persist now, if anything changed since the last persist. Answers
    /// whether it wrote.
    pub async fn persist(&self) -> Result<bool> {
        let _one = self.persisting.lock().await;
        if self.doc.get().is_none() {
            return Ok(false);
        }
        let version = self.version.load(Ordering::Acquire);
        if version == self.persisted.load(Ordering::Acquire) {
            return Ok(false);
        }
        let table = self.catalog.require(&self.target.table)?;
        let pk = rows::single_pk(&table)?;
        let tx = SharedTx::begin(&self.catalog, &table.database)?;
        let outcome = async {
            let filter = rows::pk_filter(&table, &pk, &self.target.key)?;
            let select = Select::from(Source::table(table.name.clone()))
                .columns(vec![Projection::expr(Expr::col(self.target.field.clone()))])
                .filter(filter)
                .for_update();
            let found = tx.run(None, &Statement::from(select)).await?;
            let row = found.first().ok_or_else(|| {
                Error::not_found(format!(
                    "the row `{}` of `{}` holding this document is gone",
                    self.target.key, self.target.table
                ))
            })?;
            let stored = match row.get(&self.target.field) {
                Some(Value::Bytes(bytes)) => Some(bytes.clone()),
                _ => None,
            };
            // Merge what is stored into the room: another process's edits
            // are kept, and this room's members are told about them.
            let (state, mirror, news) = {
                let doc = self.doc()?;
                let before = doc.state_vector();
                if let Some(bytes) = stored.as_deref().filter(|b| !b.is_empty()) {
                    doc.apply(bytes)?;
                }
                let news = (doc.state_vector() != before)
                    .then(|| doc.diff(&before))
                    .transpose()?;
                let mirror = self
                    .target
                    .mirror
                    .as_ref()
                    .map(|(column, format)| (column.clone(), doc.mirror(*format)));
                (doc.state(), mirror, news)
            };
            if let Some(news) = news {
                self.relay(None, &Arc::new(news));
            }
            self.size_bound.store(state.len() as u64, Ordering::Relaxed);
            rows::write_document_in(
                &self.catalog,
                &table,
                &self.target.key,
                &self.target.field,
                &state,
                mirror.as_ref().map(|(c, t)| (c.as_str(), t.as_str())),
                &rows::Executor::Transaction(tx.clone()),
            )
            .await?;
            Ok::<_, Error>(())
        }
        .await;
        match outcome {
            Ok(()) => {
                tx.commit().await?;
                self.persisted.store(version, Ordering::Release);
                Ok(true)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(e)
            }
        }
    }

    /// A member left: close the room — persisting first — when it was the last.
    fn leave(self: &Arc<Self>, seat: u64) {
        let empty = {
            let mut members = lock(&self.members);
            members.remove(&seat);
            members.is_empty()
        };
        if !empty {
            return;
        }
        let room = Arc::clone(self);
        let close = async move {
            if let Err(e) = room.persist().await {
                sc_log::log_warn!(
                    "persisting the document `{}` of stream `{}` as its room closed failed: {}",
                    room.target.key,
                    room.target.stream_name,
                    sc_error::format_chain(&e)
                );
            }
            if let Some(rooms) = room.rooms.upgrade() {
                rooms.forget_if_empty(&room);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(close);
            }
            Err(_) => {
                if let Some(rooms) = self.rooms.upgrade() {
                    rooms.forget_if_empty(self);
                }
            }
        }
    }
}

/// A membership of a document room. Dropping it leaves; the last to leave
/// closes the room, persisting what changed.
pub struct DocSeat {
    room: Arc<DocRoom>,
    id: u64,
}

impl std::fmt::Debug for DocSeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocSeat")
            .field("id", &self.id)
            .field("room", &self.room.target)
            .finish()
    }
}

impl DocSeat {
    /// The seat's id: what its inbox entries are addressed to.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The room.
    pub fn room(&self) -> &Arc<DocRoom> {
        &self.room
    }

    /// Apply this member's update and relay it to the others.
    pub fn apply(&self, update: &[u8]) -> std::result::Result<(), DocRefusal> {
        self.room.apply_from(self.id, update)
    }

    /// The whole document — `doc_sync` again.
    pub fn state(&self) -> Result<Vec<u8>> {
        self.room.state()
    }

    /// The document's plain text.
    pub fn text(&self) -> Result<String> {
        self.room.text()
    }

    /// Edit the document on the server (`live.document`): `edit` makes the
    /// change and answers it as an update, which every member is sent.
    pub fn edit(&self, edit: impl FnOnce(&CollabDoc) -> Result<Vec<u8>>) -> Result<()> {
        let update = {
            let doc = self.room.doc()?;
            // A document already at its limit takes no more from the server
            // either. (One edit may still pass it: undo is not a CRDT
            // operation, so the check comes first.)
            if let Err(DocRefusal::TooLarge(why) | DocRefusal::Invalid(why)) =
                self.room.check_size(&doc, 0)
            {
                return Err(Error::invalid(why));
            }
            edit(&doc)?
        };
        self.room.applied(Some(self.id), Arc::new(update));
        Ok(())
    }

    /// Persist now, if anything changed.
    pub async fn persist(&self) -> Result<bool> {
        self.room.persist().await
    }
}

impl Drop for DocSeat {
    fn drop(&mut self) {
        self.room.leave(self.id);
    }
}

/// `n` bytes as a person reads it.
fn human_bytes(n: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{} KB", n / KB)
    } else {
        format!("{n} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inbox_past_its_cap_resyncs_that_seat_only() {
        let inbox = DocInbox::new();
        let update = Arc::new(vec![1u8]);
        inbox.push(2, DocNews::Update(Arc::clone(&update)));
        for _ in 0..DOC_INBOX_CAP + 5 {
            inbox.push(1, DocNews::Update(Arc::clone(&update)));
        }
        inbox.push(2, DocNews::Update(Arc::clone(&update)));
        let taken = inbox.take();
        let ones: Vec<_> = taken.iter().filter(|(s, _)| *s == 1).collect();
        assert_eq!(ones, vec![&(1, DocNews::Resync)], "seat 1 is sent whole");
        let twos = taken.iter().filter(|(s, _)| *s == 2).count();
        assert!(twos >= 1, "seat 2 keeps its own updates");
        assert!(inbox.is_empty());
    }

    #[test]
    fn sizes_read_as_people_say_them() {
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(human_bytes(2048), "2 KB");
        assert_eq!(human_bytes(12), "12 bytes");
    }
}

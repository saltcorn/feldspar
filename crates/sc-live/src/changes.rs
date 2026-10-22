//! What each subscriber of a `table_changes` stream is sent (TODO.md "Live
//! updates" §3 rule 5, L3.7–L3.9).
//!
//! An element on the stream is the change as the write saw it: `{op, key, row,
//! old}`, both images of the row. A subscriber is sent **what their own read
//! would have seen change**, decided per element and per subscriber in the
//! stream's fan-out task, off the write's path:
//!
//! | The old image was… | The new image is… | They are sent |
//! |---|---|---|
//! | — (an insert) | readable and matching | `insert` with the row |
//! | readable and matching | — (a delete) | `delete` with the key only |
//! | readable and matching | readable and matching | `update` with the row |
//! | not | readable and matching | `enter` with the row |
//! | readable and matching | not | `leave` with the key only |
//! | not | not | nothing at all |
//!
//! [`deliver_to`] is that table, as a pure function. `leave` is what makes a
//! card disappear from board 7 when it moves to board 8 — and when it stops
//! being readable — and a `leave` or a `delete` carries no row, so nobody
//! learns what a row they can no longer read became. Nobody learns that a row
//! they could never read changed at all: they are sent nothing, not even a
//! frame saying so.
//!
//! ## Who may read an image
//!
//! [`sc_api::RowImage`]: the role floor, then the table's ownership formula —
//! `owner === user.id` as a field comparison, anything else reified in the
//! evaluator with its Ⱶ/Ↄ values prefetched once per image. Two economies keep
//! a thousand subscribers from costing a thousand evaluations:
//!
//! - **Verdicts are per distinct user**, not per connection: three tabs of
//!   Alice's ask once, and a filter is matched once per distinct filter.
//! - **Readability is asked only of an image that matches.** An image the
//!   subscriber's filter does not match is not sent whatever the verdict, so
//!   the filter (cheap, in Rust) is asked first and the evaluator only when it
//!   matters.
//!
//! ## What is sent
//!
//! `{op, key, row?}`, where `row` is the new image in **the REST read's
//! projection** ([`sc_api::api_row`]) — the users table's password hash is
//! not in it, because no API returns it — and is absent for `leave`, `delete`
//! and a stream whose payload is only the key.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use sc_api::RowImage;
use sc_auth::{ROLE_PUBLIC, User};
use sc_catalog::{Catalog, WriteOp};
use sc_expr::JsEvaluator;
use sc_stream::providers::table_changes::{KEY_KEY, KEY_OLD, KEY_OP, KEY_ROW};
use sc_stream::{Envelope, Payload};
use serde::Serialize;
use serde_json::{Map, Value as Json};
use uuid::Uuid;

use crate::filter::RowFilter;

/// What one subscriber is sent about one change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Delivered {
    /// A row they can see was inserted: the row.
    Insert,
    /// A row they could see, and still can, changed: the row.
    Update,
    /// A row came into their view — it changed to match, or became readable:
    /// the row.
    Enter,
    /// A row left their view — it changed not to match, or stopped being
    /// readable: the key only.
    Leave,
    /// A row they could see was deleted: the key only.
    Delete,
    /// Nothing: they never see this row change.
    Nothing,
}

impl Delivered {
    /// The `op` a page is sent, or `None` for nothing.
    pub fn op(self) -> Option<&'static str> {
        match self {
            Delivered::Insert => Some("insert"),
            Delivered::Update => Some("update"),
            Delivered::Enter => Some("enter"),
            Delivered::Leave => Some("leave"),
            Delivered::Delete => Some("delete"),
            Delivered::Nothing => None,
        }
    }

    /// Whether the row goes with it. Never for `leave` or `delete`: the
    /// subscriber may no longer be entitled to it.
    pub fn carries_row(self) -> bool {
        matches!(
            self,
            Delivered::Insert | Delivered::Update | Delivered::Enter
        )
    }
}

/// The decision table (see the module docs): what a subscriber is sent, from
/// which images the change has (`old` absent for an insert, `new` absent for a
/// delete) and whether each was readable by them and matched their filter.
///
/// The row that goes with an `insert`, `update` or `enter` is `new`; the
/// images are taken so the function says which one, and so an insert cannot be
/// mistaken for an update by a caller passing the wrong flags.
pub fn deliver_to(
    old: Option<&Json>,
    new: Option<&Json>,
    readable_old: bool,
    readable_new: bool,
    matches_old: bool,
    matches_new: bool,
) -> Delivered {
    let saw_old = old.is_some() && readable_old && matches_old;
    let sees_new = new.is_some() && readable_new && matches_new;
    match (old.is_some(), new.is_some()) {
        (false, true) if sees_new => Delivered::Insert,
        (true, false) if saw_old => Delivered::Delete,
        (true, true) => match (saw_old, sees_new) {
            (true, true) => Delivered::Update,
            (false, true) => Delivered::Enter,
            (true, false) => Delivered::Leave,
            (false, false) => Delivered::Nothing,
        },
        _ => Delivered::Nothing,
    }
}

/// Why a subscriber was sent nothing: an image matched their filter but they
/// may not read it (`Access`), or no image they may read matched (`Filter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppressed {
    /// Withheld by the read rule.
    Access,
    /// Not in the subscriber's filter.
    Filter,
}

/// Why `deliver_to` said nothing, with the same arguments: `Access` when the
/// filter matched an image the subscriber may not read, `Filter` otherwise.
pub fn suppressed_by(
    old: Option<&Json>,
    new: Option<&Json>,
    readable_old: bool,
    readable_new: bool,
    matches_old: bool,
    matches_new: bool,
) -> Suppressed {
    let withheld = (old.is_some() && matches_old && !readable_old)
        || (new.is_some() && matches_new && !readable_new);
    if withheld {
        Suppressed::Access
    } else {
        Suppressed::Filter
    }
}

/// A `table_changes` stream's per-element work, counted since this server
/// started (L3.9): what the admin's Streams list shows beside the stream's own
/// counters.
#[derive(Debug, Default)]
pub struct ChangeCounters {
    checked: AtomicU64,
    delivered: AtomicU64,
    suppressed_access: AtomicU64,
    suppressed_filter: AtomicU64,
    evaluator_micros: AtomicU64,
}

/// [`ChangeCounters`] read at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ChangeCounts {
    /// Element × subscription pairs decided.
    pub checked: u64,
    /// Of those, how many were sent something.
    pub delivered: u64,
    /// Sent nothing because an image they matched was not theirs to read.
    pub suppressed_access: u64,
    /// Sent nothing because no image they may read matched their filter.
    pub suppressed_filter: u64,
    /// Time spent deciding readability (the evaluator and its prefetches), in
    /// microseconds.
    pub evaluator_micros: u64,
}

impl ChangeCounters {
    /// The counts now.
    pub fn snapshot(&self) -> ChangeCounts {
        ChangeCounts {
            checked: self.checked.load(Ordering::Relaxed),
            delivered: self.delivered.load(Ordering::Relaxed),
            suppressed_access: self.suppressed_access.load(Ordering::Relaxed),
            suppressed_filter: self.suppressed_filter.load(Ordering::Relaxed),
            evaluator_micros: self.evaluator_micros.load(Ordering::Relaxed),
        }
    }
}

/// Who a connection is right now, shared with its subscriptions' fan-outs so
/// a re-read session (a role lowered, a field changed) reaches every verdict
/// made after it — and, for a visitor who is not signed in, the public id of
/// their guest session (TODO-payments P2.8), which an ownership formula reads
/// as `session.id`.
#[derive(Debug, Clone, Default)]
pub struct CallerCell(Arc<RwLock<Option<User>>>, Arc<RwLock<Option<String>>>);

impl CallerCell {
    /// A cell holding `user` (or nobody).
    pub fn new(user: Option<User>) -> CallerCell {
        CallerCell(Arc::new(RwLock::new(user)), Arc::default())
    }

    /// Replace the guest session the connection's caller has.
    pub fn set_guest_session(&self, session: Option<String>) {
        match self.1.write() {
            Ok(mut guard) => *guard = session,
            Err(poisoned) => *poisoned.into_inner() = session,
        }
    }

    /// The public id of the caller's guest session, if they have one.
    pub fn guest_session(&self) -> Option<String> {
        match self.1.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Replace who the connection is.
    pub fn set(&self, user: Option<User>) {
        match self.0.write() {
            Ok(mut guard) => *guard = user,
            Err(poisoned) => *poisoned.into_inner() = user,
        }
    }

    /// Who the connection is.
    pub fn get(&self) -> Option<User> {
        match self.0.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// What a subscription to a `table_changes` stream carries into the fan-out:
/// where rows are read and formulas evaluated, and its filter.
#[derive(Clone)]
pub struct ChangeAccess {
    /// The catalog the table and its read rule are looked up in.
    pub catalog: Arc<Catalog>,
    /// The evaluator an ownership formula is run in — the REST read's own.
    pub evaluator: Option<Arc<dyn JsEvaluator>>,
    /// The subscription's filter.
    pub filter: RowFilter,
}

impl std::fmt::Debug for ChangeAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeAccess")
            .field("filter", &self.filter)
            .finish_non_exhaustive()
    }
}

/// One subscription as the fan-out decides for it.
pub(crate) struct Subscriber<'a> {
    pub access: &'a ChangeAccess,
    pub caller: &'a CallerCell,
}

/// A change read off the stream's element.
struct Change<'e> {
    op: WriteOp,
    key: Json,
    new: Option<&'e Json>,
    old: Option<&'e Json>,
}

impl<'e> Change<'e> {
    fn read(value: &'e Json) -> Option<Change<'e>> {
        let op = match value.get(KEY_OP)?.as_str()? {
            "insert" => WriteOp::Insert,
            "update" => WriteOp::Update,
            "delete" => WriteOp::Delete,
            _ => return None,
        };
        let image = |key: &str| value.get(key).filter(|row| row.is_object());
        Some(Change {
            op,
            key: value.get(KEY_KEY).cloned().unwrap_or(Json::Null),
            new: image(KEY_ROW),
            old: image(KEY_OLD),
        })
    }
}

/// A caller as verdicts are shared: their id (none for anonymous), role and
/// guest session.
type Who = (Option<Uuid>, u8, Option<String>);

/// What is known of one caller's verdicts on the old and the new image — each
/// asked only once its image matched a filter.
type Known = (Option<bool>, Option<bool>);

/// Decide, for every subscriber, what `element` of the `table_changes` stream
/// on `table` sends them, and return those verdicts with the envelope each is
/// sent (`None` for nothing). Counted in `counters`.
pub(crate) async fn decide(
    table: &str,
    payload: Payload,
    element: &Envelope,
    subscribers: &[Subscriber<'_>],
    counters: &ChangeCounters,
) -> Vec<Option<Arc<Envelope>>> {
    let mut out = vec![None; subscribers.len()];
    let Some(first) = subscribers.first() else {
        return out;
    };
    let Some(change) = Change::read(&element.value) else {
        return out;
    };
    let catalog = &first.access.catalog;
    let table = match catalog.get(table) {
        Ok(Some(table)) => table,
        // A table dropped under a running stream: nobody is sent a row of a
        // table that no longer has a read rule.
        _ => return out,
    };
    let mut old_image = change.old.map(|row| RowImage::new(&table, row));
    let mut new_image = change.new.map(|row| RowImage::new(&table, row));

    // The filter, once per distinct filter.
    let mut matched: HashMap<&str, (bool, bool)> = HashMap::new();
    // Readability, once per distinct caller and image, asked lazily.
    let mut readable: HashMap<Who, Known> = HashMap::new();
    let mut envelopes: HashMap<Delivered, Arc<Envelope>> = HashMap::new();
    let mut evaluating = std::time::Duration::ZERO;

    for (slot, subscriber) in out.iter_mut().zip(subscribers) {
        let filter = &subscriber.access.filter;
        let (matches_old, matches_new) = *matched.entry(filter.key()).or_insert_with(|| {
            (
                old_image
                    .as_ref()
                    .is_some_and(|image| filter.matches(image.values())),
                new_image
                    .as_ref()
                    .is_some_and(|image| filter.matches(image.values())),
            )
        });
        let user = subscriber.caller.get();
        let role = user.as_ref().map_or(ROLE_PUBLIC, |user| user.role);
        let guest = subscriber.caller.guest_session();
        // The verdict is asked inside the caller's origin, so that an
        // ownership formula's `session.id` is their guest session's.
        let origin = sc_catalog::Origin {
            app: None,
            session: guest.clone(),
        };
        let who = (user.as_ref().map(|user| user.id), role, guest);
        let known = readable.entry(who).or_insert((None, None));
        let started = Instant::now();
        if matches_old
            && known.0.is_none()
            && let Some(image) = old_image.as_mut()
        {
            let asked = verdict(image, subscriber.access, role, user.as_ref());
            known.0 = Some(sc_catalog::with_origin(origin.clone(), asked).await);
        }
        if matches_new
            && known.1.is_none()
            && let Some(image) = new_image.as_mut()
        {
            let asked = verdict(image, subscriber.access, role, user.as_ref());
            known.1 = Some(sc_catalog::with_origin(origin, asked).await);
        }
        evaluating += started.elapsed();
        let (readable_old, readable_new) = (known.0.unwrap_or(false), known.1.unwrap_or(false));

        let mut delivered = deliver_to(
            change.old,
            change.new,
            readable_old,
            readable_new,
            matches_old,
            matches_new,
        );
        // An update whose old image was not read (it raced a subscription)
        // has no "before" to compare with: what the subscriber may see now
        // comes into view.
        if change.op == WriteOp::Update && delivered == Delivered::Insert {
            delivered = Delivered::Enter;
        }
        counters.checked.fetch_add(1, Ordering::Relaxed);
        if delivered == Delivered::Nothing {
            let counter = match suppressed_by(
                change.old,
                change.new,
                readable_old,
                readable_new,
                matches_old,
                matches_new,
            ) {
                Suppressed::Access => &counters.suppressed_access,
                Suppressed::Filter => &counters.suppressed_filter,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        counters.delivered.fetch_add(1, Ordering::Relaxed);
        let envelope = envelopes.entry(delivered).or_insert_with(|| {
            Arc::new(Envelope {
                stream: element.stream.clone(),
                topic: None,
                value: sent(delivered, &change.key, change.new, payload, &table),
                received_at: element.received_at,
                source: None,
            })
        });
        *slot = Some(Arc::clone(envelope));
    }
    counters
        .evaluator_micros
        .fetch_add(evaluating.as_micros() as u64, Ordering::Relaxed);
    out
}

/// One caller's verdict on one image; anything that went wrong denies, and is
/// said once in the log.
async fn verdict(
    image: &mut RowImage<'_>,
    access: &ChangeAccess,
    role: u8,
    user: Option<&User>,
) -> bool {
    match image
        .readable(&access.catalog, role, user, access.evaluator.as_ref())
        .await
    {
        Ok(verdict) => verdict,
        Err(e) => {
            sc_log::log_warn!(
                "a live table change was withheld: {}",
                sc_error::format_chain(&e)
            );
            false
        }
    }
}

/// What a page is sent: `{op, key}`, and the row in the REST projection when
/// the verdict carries one and the stream sends rows.
fn sent(
    delivered: Delivered,
    key: &Json,
    new: Option<&Json>,
    payload: Payload,
    table: &sc_catalog::Table,
) -> Json {
    let mut value = Map::new();
    value.insert(
        KEY_OP.to_owned(),
        Json::from(delivered.op().unwrap_or_default()),
    );
    value.insert(KEY_KEY.to_owned(), key.clone());
    if payload == Payload::Row
        && delivered.carries_row()
        && let Some(row) = new
    {
        value.insert(KEY_ROW.to_owned(), sc_api::api_row(table, row));
    }
    Json::Object(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every combination of images and flags, against the table in the module
    /// docs.
    #[test]
    fn the_full_truth_table() {
        let old = json!({ "id": 1, "board": 7 });
        let new = json!({ "id": 1, "board": 8 });
        let bools = [false, true];
        for &ro in &bools {
            for &rn in &bools {
                for &mo in &bools {
                    for &mn in &bools {
                        let saw = ro && mo;
                        let sees = rn && mn;
                        // An insert: only the new image.
                        assert_eq!(
                            deliver_to(None, Some(&new), ro, rn, mo, mn),
                            if sees {
                                Delivered::Insert
                            } else {
                                Delivered::Nothing
                            },
                            "insert ro={ro} rn={rn} mo={mo} mn={mn}"
                        );
                        // A delete: only the old image.
                        assert_eq!(
                            deliver_to(Some(&old), None, ro, rn, mo, mn),
                            if saw {
                                Delivered::Delete
                            } else {
                                Delivered::Nothing
                            },
                            "delete ro={ro} rn={rn} mo={mo} mn={mn}"
                        );
                        // An update: both.
                        let expected = match (saw, sees) {
                            (true, true) => Delivered::Update,
                            (false, true) => Delivered::Enter,
                            (true, false) => Delivered::Leave,
                            (false, false) => Delivered::Nothing,
                        };
                        assert_eq!(
                            deliver_to(Some(&old), Some(&new), ro, rn, mo, mn),
                            expected,
                            "update ro={ro} rn={rn} mo={mo} mn={mn}"
                        );
                        // No images is nothing, whatever the flags.
                        assert_eq!(deliver_to(None, None, ro, rn, mo, mn), Delivered::Nothing);
                    }
                }
            }
        }
    }

    #[test]
    fn leave_and_delete_carry_no_row_and_nothing_has_no_op() {
        for delivered in [Delivered::Leave, Delivered::Delete, Delivered::Nothing] {
            assert!(!delivered.carries_row(), "{delivered:?}");
        }
        for delivered in [Delivered::Insert, Delivered::Update, Delivered::Enter] {
            assert!(delivered.carries_row(), "{delivered:?}");
        }
        assert_eq!(Delivered::Nothing.op(), None);
        assert_eq!(Delivered::Leave.op(), Some("leave"));
    }

    #[test]
    fn what_is_sent_is_op_and_key_and_the_projected_row_only_when_it_carries_one() {
        let table = crate::filter::tests::table(
            "users",
            &[
                ("id", sc_types::BasicType::Int),
                ("email", sc_types::BasicType::Text),
                ("password_hash", sc_types::BasicType::Text),
            ],
        );
        let row = json!({ "id": 1, "email": "a@b.c", "password_hash": "x", "joined": 3 });
        let key = json!(1);
        assert_eq!(
            sent(Delivered::Enter, &key, Some(&row), Payload::Row, &table),
            json!({ "op": "enter", "key": 1, "row": { "id": 1, "email": "a@b.c" } }),
            "the REST projection: no password hash, nothing the table does not declare"
        );
        for delivered in [Delivered::Leave, Delivered::Delete] {
            assert_eq!(
                sent(delivered, &key, Some(&row), Payload::Row, &table),
                json!({ "op": delivered.op().unwrap(), "key": 1 })
            );
        }
        assert_eq!(
            sent(Delivered::Update, &key, Some(&row), Payload::Key, &table),
            json!({ "op": "update", "key": 1 }),
            "a key payload never carries the row"
        );
    }

    #[test]
    fn a_suppression_is_access_only_when_the_filter_matched_an_unreadable_image() {
        let row = json!({ "id": 1 });
        // Matched, not readable: access.
        assert_eq!(
            suppressed_by(None, Some(&row), false, false, false, true),
            Suppressed::Access
        );
        // Readable, not matched: filter.
        assert_eq!(
            suppressed_by(None, Some(&row), false, true, false, false),
            Suppressed::Filter
        );
        // Neither matched: filter, whatever the verdicts.
        assert_eq!(
            suppressed_by(Some(&row), Some(&row), false, false, false, false),
            Suppressed::Filter
        );
        // An update whose old image matched and was withheld: access.
        assert_eq!(
            suppressed_by(Some(&row), Some(&row), false, true, true, false),
            Suppressed::Access
        );
    }
}

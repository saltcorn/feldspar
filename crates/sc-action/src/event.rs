//! The event model (design §10.2): what happened, to what, and who caused it.
//!
//! An event is deliberately **separate from the trigger that listens for it**.
//! One insert on `books` is one event; it may fire three triggers or none, and
//! nothing about the event changes either way. That split is what lets the row
//! layer emit events without knowing whether anything is listening, and it is
//! what a workflow engine will later listen to with a different body.
//!
//! ## The caller is data, not a type
//!
//! An event carries its caller as a role plus a JSON object of the user's fields
//! — not a `sc_auth::User`. Two reasons: that JSON object is *exactly* what the
//! formula language binds `user` to (§7.3), so an `only_if` evaluation needs no
//! conversion; and it keeps this crate off `sc-auth`, which in turn keeps actions
//! implementable from a guest language, where a Rust `User` cannot travel.
//!
//! ## Cascades are bounded and named
//!
//! An action may write a row, and that write is an event, which may fire another
//! trigger. That is a feature (denormalising into a second table is the archetype)
//! so it is bounded rather than forbidden: an event carries the **chain** of
//! trigger names that led to it, and [`Event::firing`] refuses to descend past
//! [`MAX_DEPTH`], naming the whole chain. A depth counter alone would have caught
//! the loop but left the admin to find it; the chain *is* the diagnosis.

use std::sync::Arc;

use sc_catalog::{CallerContext, CallingApp, Changeset, Origin};
use sc_error::{Error, ErrorKind, Result};
use serde_json::{Map, Value as Json};

/// The least-restrictive role: everyone, including anonymous callers.
///
/// Mirrors `sc_auth::ROLE_PUBLIC` (and the copy `sc-files` keeps for the same
/// reason): the 1–100 role scale is a domain constant, and duplicating one `u8`
/// is cheaper than a dependency that would drag a Rust-only `User` type into a
/// crate whose extension point must be implementable from a guest language.
pub const ROLE_PUBLIC: u8 = 100;

/// How deep a cascade of trigger-firing-a-trigger may go before it is refused.
///
/// Five is enough for any deliberate chain and short enough that a runaway loop
/// is reported while the admin can still read it.
pub const MAX_DEPTH: usize = 5;

/// The kind of occurrence a trigger listens for (design §10.2).
///
/// Round-trips through a lowercase string ([`as_str`](EventKind::as_str) /
/// [`parse`](EventKind::parse)) because that is how it is stored (a column in
/// `_fd_triggers`) and how the admin SPA posts it. There is no `serde` derive:
/// every crossing is one of those two, and a second spelling of the same mapping
/// is a second thing to keep in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// A row was inserted into the channel's table.
    Insert,
    /// A row of the channel's table was updated.
    Update,
    /// A row was deleted from the channel's table.
    Delete,
    /// No intrinsic event: the trigger runs only when something asks it to (the
    /// admin UI's Run button, an application's API).
    None,
    /// A user authenticated successfully.
    Login,
    /// The server finished starting up.
    Startup,
    /// An error was reported — application *or* system (§16's `ErrorKind`), which
    /// the payload distinguishes.
    Error,
    /// Every five minutes.
    Often,
    /// Once an hour, at a configured minute past it.
    Hourly,
    /// Once a day, at a configured time.
    Daily,
    /// Once a week, on a configured day and time.
    Weekly,
    /// An **element arrived on a stream** (TODO "Streams" §8). The channel is
    /// the stream's name and the payload is the element's envelope.
    ///
    /// The first event kind whose occurrence comes from *outside* this server:
    /// every other one is raised by something Saltcorn did — a write, a login,
    /// a clock, an error. That is why it carries no row: a broker's element is
    /// not a row of anything, and a trigger that wants one writes it.
    Stream,
}

/// Every event kind, in the order the admin UI lists them.
///
/// The single enumeration of the set: [`as_str`](EventKind::as_str),
/// [`parse`](EventKind::parse) and the admin picker all derive from it, so a new
/// kind is one line here plus one match arm.
pub const EVENT_KINDS: [EventKind; 12] = [
    EventKind::Insert,
    EventKind::Update,
    EventKind::Delete,
    EventKind::None,
    EventKind::Login,
    EventKind::Startup,
    EventKind::Error,
    EventKind::Often,
    EventKind::Hourly,
    EventKind::Daily,
    EventKind::Weekly,
    EventKind::Stream,
];

impl EventKind {
    /// The stored, posted and displayed spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Insert => "insert",
            EventKind::Update => "update",
            EventKind::Delete => "delete",
            EventKind::None => "none",
            EventKind::Login => "login",
            EventKind::Startup => "startup",
            EventKind::Error => "error",
            EventKind::Often => "often",
            EventKind::Hourly => "hourly",
            EventKind::Daily => "daily",
            EventKind::Weekly => "weekly",
            EventKind::Stream => "stream",
        }
    }

    /// Parse the stored spelling. An unrecognised one is a configuration error
    /// naming it and the alternatives — a trigger whose event nothing implements
    /// must not silently become a trigger that never fires.
    pub fn parse(s: &str) -> Result<EventKind> {
        EVENT_KINDS
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| {
                Error::config(format!(
                    "unknown trigger event `{s}`; the events are {}",
                    EVENT_KINDS
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// Whether this event is about a row of a table — the kinds whose channel is
    /// a table name, which carry a row and may have an `only_if` formula.
    pub fn is_table_event(self) -> bool {
        matches!(
            self,
            EventKind::Insert | EventKind::Update | EventKind::Delete
        )
    }

    /// Whether this event's `channel` names something — a table for a table
    /// event, a **stream** for a stream one — and so must be present and must
    /// match for the trigger to fire.
    ///
    /// Separate from [`is_table_event`](EventKind::is_table_event) because the
    /// two questions have different answers for exactly one kind, and every
    /// place that conflated them would be a place a stream trigger fired for
    /// the wrong stream: `is_table_event` asks "does this carry a row and can
    /// its formula range over one", which a stream event cannot, while this
    /// asks "is the channel load-bearing", which it is.
    pub fn has_channel(self) -> bool {
        self.is_table_event() || self == EventKind::Stream
    }

    /// Whether this event fires on a schedule (the Phase 8 scheduler's set).
    pub fn is_periodic(self) -> bool {
        matches!(
            self,
            EventKind::Often | EventKind::Hourly | EventKind::Daily | EventKind::Weekly
        )
    }
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One occurrence: its kind, what it happened to, and who caused it.
///
/// Built fluently — `Event::new(EventKind::Insert).on("books").row(json)` — so
/// the fields that do not apply to a kind are simply not set rather than filled
/// with placeholders: a startup event has no channel, no row and no user, and
/// that is exactly how it reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// What happened.
    pub kind: EventKind,
    /// What it happened to: the table name for a table event, `None` otherwise.
    pub channel: Option<String>,
    /// The row the event is about: the inserted/updated/deleted row.
    pub row: Option<Json>,
    /// The row as it was *before* an update; `None` for every other kind.
    pub old_row: Option<Json>,
    /// Free-form detail: a directly-run trigger's posted body, an error event's
    /// `{kind, message, …}`. Always present (`Json::Null` when there is none) so
    /// a formula reading `payload` never has to guess.
    pub payload: Json,
    /// The caller's role — [`ROLE_PUBLIC`] when nobody is logged in.
    pub role: u8,
    /// The caller's fields, or `None` when nobody is logged in. This is what the
    /// formula language binds `user` to.
    pub user: Option<Json>,
    /// The trigger names that led here, outermost first. Empty for an event
    /// raised by a request rather than by another trigger's action.
    pub chain: Vec<String>,
    /// The application the event's request came through (TODO-payments P1.3):
    /// its id, its base URL as that request reached it, and its native apps'
    /// URL scheme. `None` for an event no application's request raised.
    pub app: Option<CallingApp>,
    /// The caller's **guest session** public id (TODO-payments P1.5), what a
    /// formula sees as `session.id`. `None` for a signed-in caller and for
    /// anybody with no guest session.
    pub session: Option<String>,
    /// The changeset of the write that raised it (§14.12), which the writes
    /// of the triggers it fires join: the user's edit and its trigger's
    /// follow-up are one action. `None` for an event no changeset led to.
    pub changeset: Option<Arc<Changeset>>,
}

impl Event {
    /// An event of `kind` with nothing else set: no channel, no row, a null
    /// payload, an anonymous caller and an empty chain.
    pub fn new(kind: EventKind) -> Event {
        Event {
            kind,
            channel: None,
            row: None,
            old_row: None,
            payload: Json::Null,
            role: ROLE_PUBLIC,
            user: None,
            chain: Vec::new(),
            app: None,
            session: None,
            changeset: None,
        }
    }

    /// The **`login`** event: a caller authenticated successfully.
    ///
    /// The user *is* the event — there is no row and no payload — so this is
    /// [`caller`](Event::caller) under a name that says which occurrence it is.
    /// Fired for an admin login and for an application's own, because both are
    /// "someone signed in" and a trigger that records logins must see both.
    pub fn login(role: u8, user: Json) -> Event {
        Event::new(EventKind::Login).caller(role, Some(user))
    }

    /// The **`startup`** event: the server finished coming up. No caller, no
    /// channel, nothing to carry — it happened, once.
    pub fn startup() -> Event {
        Event::new(EventKind::Startup)
    }

    /// The **`error`** event: an [`Error`] became a response (§16).
    ///
    /// The payload is written out here rather than assembled at the call site
    /// because it is a **wire contract**: an error trigger's formulas read
    /// `payload.kind` and `payload.message`, and a `fetch` action posts the whole
    /// object to whatever is on the other end. It must not change silently.
    ///
    /// - `kind` — `"application"` or `"system"`, the existing [`ErrorKind`]
    ///   split: the first is a caller's mistake, the second is the server's, and
    ///   an alerting trigger almost always wants only the second.
    /// - `message` — what the caller was told.
    /// - `method`/`path` — where it happened, which is the difference between an
    ///   alert someone can act on and one they cannot.
    ///
    /// The caller (role and user) is set by [`caller`](Event::caller) on top, as
    /// for every other event.
    pub fn error(kind: ErrorKind, message: impl Into<String>, method: &str, path: &str) -> Event {
        let kind = match kind {
            ErrorKind::Application => "application",
            ErrorKind::System => "system",
        };
        Event::new(EventKind::Error).payload(serde_json::json!({
            "kind": kind,
            "message": message.into(),
            "method": method,
            "path": path,
        }))
    }

    /// The **`stream`** event: an element arrived on the stream named `stream`
    /// (TODO "Streams" §8).
    ///
    /// The payload is written out here rather than assembled at the call site
    /// for [`error`](Event::error)'s reason, and it is the same kind of
    /// promise: the envelope is a **wire contract** (§4), read by a trigger's
    /// `only_if` as `payload.value.temperature`, posted whole by a `fetch`
    /// action, and typed into an application's generated client. It must not
    /// change silently.
    ///
    /// - `stream` — the stream's name, which is the event's channel: a trigger
    ///   listens to one stream, never to all of them.
    /// - `envelope` — `{stream, value, received_at, source?}`, exactly as
    ///   `sc_stream::Envelope::to_json` writes it. This crate does not depend
    ///   on `sc-stream` (§2) and takes the JSON, which is what a module-supplied
    ///   or test-supplied element can hand over too.
    ///
    /// There is **no row**: a stream element is not a row of anything, so
    /// `row` and `old` are out of scope and `only_if` reads `payload`.
    pub fn stream(stream: impl Into<String>, envelope: Json) -> Event {
        Event::new(EventKind::Stream).on(stream).payload(envelope)
    }

    /// Set the channel — the table name, for a table event.
    pub fn on(mut self, channel: impl Into<String>) -> Event {
        self.channel = Some(channel.into());
        self
    }

    /// Set the row the event is about.
    pub fn row(mut self, row: Json) -> Event {
        self.row = Some(row);
        self
    }

    /// Set the pre-update row.
    pub fn old_row(mut self, old: Json) -> Event {
        self.old_row = Some(old);
        self
    }

    /// Set the payload.
    pub fn payload(mut self, payload: Json) -> Event {
        self.payload = payload;
        self
    }

    /// Set the caller: their role, and their fields when logged in.
    pub fn caller(mut self, role: u8, user: Option<Json>) -> Event {
        self.role = role;
        self.user = user;
        self
    }

    /// Set everything about who caused this event from the caller context of
    /// the write or the run that raised it: the role, the user, the chain, the
    /// application, the guest session and the changeset.
    pub fn caused_by(mut self, caller: &CallerContext) -> Event {
        self.role = caller.role;
        self.user = caller.user.clone();
        self.chain = caller.chain.clone();
        self.app = caller.app.clone();
        self.session = caller.session.clone();
        self.changeset = caller.changeset.clone();
        self
    }

    /// Set where the event came from: the application and the guest session.
    pub fn from_origin(mut self, origin: Origin) -> Event {
        self.app = origin.app;
        self.session = origin.session;
        self
    }

    /// Where the event came from, to run its trigger's action inside.
    pub fn origin(&self) -> Origin {
        Origin {
            app: self.app.clone(),
            session: self.session.clone(),
        }
    }

    /// Set the chain of triggers this event descends from — what
    /// [`firing`](Event::firing) handed the action that caused it.
    pub fn chained(mut self, chain: Vec<String>) -> Event {
        self.chain = chain;
        self
    }

    /// Set the changeset the event belongs to.
    pub fn in_changeset(mut self, changeset: Option<Arc<Changeset>>) -> Event {
        self.changeset = changeset;
        self
    }

    /// How deep in a cascade this event is: 0 for one raised by a request.
    pub fn depth(&self) -> usize {
        self.chain.len()
    }

    /// Record that `trigger` is firing for this event, returning the chain any
    /// event *its action* causes must carry.
    ///
    /// This is the one boundary the cascade limit is enforced at: an
    /// [`Err`](Result) here means the trigger does not run, and it names the
    /// whole chain, because "trigger depth exceeded" without the path is a
    /// diagnosis the admin has to redo by hand.
    pub fn firing(&self, trigger: &str) -> Result<Vec<String>> {
        if self.depth() >= MAX_DEPTH {
            let mut chain = self.chain.clone();
            chain.push(trigger.to_owned());
            return Err(Error::invalid(format!(
                "trigger `{trigger}` was not run: it is {} triggers deep, past the \
                 limit of {MAX_DEPTH} — the chain is {}",
                chain.len(),
                chain.join(" → ")
            )));
        }
        let mut chain = self.chain.clone();
        chain.push(trigger.to_owned());
        Ok(chain)
    }

    /// The channel, or a configuration error naming the event that requires one.
    ///
    /// A table event with no channel is a broken trigger, not an event about
    /// every table; validation refuses it on save (Phase 2) and this is the
    /// belt for anything that reaches dispatch anyway.
    pub fn require_channel(&self) -> Result<&str> {
        self.channel.as_deref().ok_or_else(|| {
            let what = match self.kind {
                EventKind::Stream => "stream",
                _ => "table",
            };
            Error::config(format!("a `{}` event must name a {what}", self.kind))
        })
    }

    /// The channel **when it names a table**, which is every kind but
    /// [`Stream`](EventKind::Stream) — whose channel names a stream.
    ///
    /// What every caller that is about to look a channel up in the catalog
    /// wants. A stream's name is not a table's, and resolving it as one would
    /// fail with "no table named `boiler`", which is a confusing answer to a
    /// question nobody asked: the admin named a stream, and it exists.
    ///
    /// Deliberately **not** `is_table_event()`: a `none` trigger run against a
    /// row of its table (the row button, §13.4) carries both a channel and a
    /// row, and its templates range over that table exactly as an `update`
    /// trigger's do. The one kind whose channel is not a table is the one kind
    /// this excludes.
    pub fn table_channel(&self) -> Option<&str> {
        self.channel
            .as_deref()
            .filter(|_| self.kind != EventKind::Stream)
    }

    /// The row's fields as a JSON object, or an empty one — what a formula's
    /// ambient `row` binds to (decision 7). A non-object row (never produced by
    /// the row layer) reads as empty rather than as a type error at evaluation
    /// time.
    pub fn row_object(&self) -> Map<String, Json> {
        object_or_empty(self.row.as_ref())
    }

    /// The pre-update row's fields, as [`row_object`](Event::row_object) — empty
    /// on every kind but `update`, so `old.x` is null there rather than an error.
    pub fn old_row_object(&self) -> Map<String, Json> {
        object_or_empty(self.old_row.as_ref())
    }
}

/// A JSON object's entries, or none for anything else (including absence).
fn object_or_empty(value: Option<&Json>) -> Map<String, Json> {
    match value {
        Some(Json::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_kind_round_trips_through_its_string() {
        for kind in EVENT_KINDS {
            assert_eq!(EventKind::parse(kind.as_str()).unwrap(), kind);
            // The spelling is the stored one: lowercase, no separators to get
            // wrong.
            assert_eq!(kind.as_str().to_lowercase(), kind.as_str());
        }
        // The set is enumerated once; a kind missing from EVENT_KINDS would make
        // this count wrong and its `parse` fail above.
        assert_eq!(EVENT_KINDS.len(), 12);
    }

    #[test]
    fn an_unknown_kind_is_refused_by_name_with_the_alternatives() {
        let err = EventKind::parse("insert_validate").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("insert_validate"), "{msg}");
        assert!(msg.contains("insert"), "{msg}");
        assert!(msg.contains("weekly"), "{msg}");
    }

    #[test]
    fn table_and_periodic_kinds_are_classified() {
        assert!(EventKind::Insert.is_table_event());
        assert!(EventKind::Update.is_table_event());
        assert!(EventKind::Delete.is_table_event());
        for kind in EVENT_KINDS.iter().filter(|k| !k.is_table_event()) {
            assert!(!kind.is_table_event(), "{kind}");
        }
        // A stream event's channel is load-bearing but it is not a table
        // event: it carries no row, so `row`/`old` are out of scope and its
        // `only_if` reads `payload` (§8).
        assert!(!EventKind::Stream.is_table_event());
        assert!(EventKind::Stream.has_channel());
        assert!(EventKind::Insert.has_channel());
        assert!(!EventKind::Login.has_channel());
        assert!(EventKind::Often.is_periodic());
        assert!(EventKind::Weekly.is_periodic());
        assert!(!EventKind::None.is_periodic());
        assert!(!EventKind::Insert.is_periodic());
    }

    #[test]
    fn a_stream_event_carries_the_envelope_as_its_payload_and_the_stream_as_its_channel() {
        let envelope = json!({
            "stream": "boiler",
            "value": { "temperature": 31.2 },
            "received_at": "2026-09-17T09:00:00.000Z",
            "source": { "topic": "house/boiler/temp", "qos": 0, "retain": false },
        });
        let ev = Event::stream("boiler", envelope.clone());
        assert_eq!(ev.kind, EventKind::Stream);
        assert_eq!(ev.require_channel().unwrap(), "boiler");
        // The payload is the envelope, field for field — the wire contract.
        assert_eq!(ev.payload, envelope);
        assert_eq!(ev.payload["value"]["temperature"], json!(31.2));
        // No row, and none pretended: `row`/`old` read as empty, which is what
        // keeps them out of scope in `trigger_shape`.
        assert!(ev.row.is_none() && ev.old_row.is_none());
        assert!(ev.row_object().is_empty());
    }

    #[test]
    fn a_stream_event_with_no_channel_is_refused_naming_a_stream_not_a_table() {
        let msg = Event::new(EventKind::Stream)
            .require_channel()
            .unwrap_err()
            .to_string();
        assert!(msg.contains("stream"), "{msg}");
        assert!(!msg.contains("table"), "{msg}");
    }

    #[test]
    fn a_fresh_event_is_anonymous_with_nothing_set() {
        let ev = Event::new(EventKind::Startup);
        assert_eq!(ev.role, ROLE_PUBLIC);
        assert!(ev.user.is_none() && ev.channel.is_none() && ev.row.is_none());
        assert_eq!(ev.payload, Json::Null);
        assert_eq!(ev.depth(), 0);
        // A startup event has no table, and says so rather than guessing.
        assert!(ev.require_channel().is_err());
    }

    #[test]
    fn a_table_event_carries_its_row_user_and_channel() {
        let ev = Event::new(EventKind::Update)
            .on("books")
            .row(json!({ "id": 1, "title": "new" }))
            .old_row(json!({ "id": 1, "title": "old" }))
            .caller(1, Some(json!({ "email": "a@b.c" })));
        assert_eq!(ev.require_channel().unwrap(), "books");
        assert_eq!(ev.row_object()["title"], json!("new"));
        assert_eq!(ev.old_row_object()["title"], json!("old"));
        assert_eq!(ev.role, 1);
        assert_eq!(ev.user.unwrap()["email"], json!("a@b.c"));
    }

    #[test]
    fn a_missing_or_non_object_row_reads_as_an_empty_object() {
        // `old.x` on an insert must be null, not an evaluation error — which the
        // empty object is what delivers (decision 7).
        let insert = Event::new(EventKind::Insert).row(json!({ "id": 1 }));
        assert!(insert.old_row_object().is_empty());
        let odd = Event::new(EventKind::Insert).row(json!([1, 2, 3]));
        assert!(odd.row_object().is_empty());
    }

    #[test]
    fn firing_extends_the_chain_and_the_limit_names_it() {
        // A request-raised event is at depth 0; each firing adds one name.
        let ev = Event::new(EventKind::Insert).on("books");
        let chain = ev.firing("audit").unwrap();
        assert_eq!(chain, vec!["audit".to_owned()]);

        // Descend to the limit: still allowed at MAX_DEPTH - 1.
        let deep = Event::new(EventKind::Insert).on("books").chained(vec![
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
        ]);
        assert_eq!(deep.depth(), MAX_DEPTH - 1);
        assert_eq!(deep.firing("e").unwrap().len(), MAX_DEPTH);

        // One deeper is refused, and the message is the whole path, not just a
        // number.
        let too_deep = deep.clone().chained(
            ["a", "b", "c", "d", "e"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        );
        let err = too_deep.firing("f").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("a → b → c → d → e → f"), "{msg}");
        assert!(msg.contains("`f`"), "{msg}");
        // An over-deep cascade is the app builder's misconfiguration, not a bug
        // in Saltcorn (§16's split).
        assert_eq!(err.kind(), sc_error::ErrorKind::Application);
    }
}

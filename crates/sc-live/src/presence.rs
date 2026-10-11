//! Presence: who else has a topic open (TODO.md "Live updates" §3 rule 9,
//! L4.3).
//!
//! A **room** is one `(stream, topic)` of a stream whose settings switch
//! `presence` on, and its members are the **connections** subscribed to it —
//! one member per connection however many of its subscriptions are on the
//! topic, so two components of one page are one avatar. A member is
//!
//! ```json
//! { "connection": "…", "user_id": "…" | null, …presence_fields, "state": … | null }
//! ```
//!
//! — the fields the admin chose to show, read from `users` when the member
//! joins, and the state the page last set (a cursor, what it is dragging).
//! Nothing else about the user is in it.
//!
//! ## Only members see members
//!
//! The only way into a room is a subscription, and a subscription is let in
//! only by the topic's read check ([`crate::may_subscribe`]); it leaves the
//! room when it is unsubscribed, closed or `revoked` ([`Seat`] is held by it
//! and leaves on drop). So the member list is shown only to callers who passed
//! the check, and someone who fails it is never in the list either.
//!
//! ## Never a wait, never a wrong list
//!
//! A change to a room is pushed to each other member's [`PresenceInbox`] —
//! a short list behind a lock, and a bell — and the member's own connection
//! turns it into `presence_diff` frames on its own task. The pusher never
//! waits on a socket. An inbox that fills up (a connection that has stopped
//! reading) drops that room's diffs and marks the room for a **resync**: the
//! connection is handed the whole list again as a `presence` frame, so a list
//! is either patched in order or rebuilt, never silently wrong.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use sc_stream::StreamId;
use serde_json::{Map, Value as Json};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::hub::{ConnectionId, lock};

/// One room: a topic of a stream. `None` is a `single` stream's one topic.
pub type RoomKey = (StreamId, Option<String>);

/// How many presence changes a connection may have waiting before a room's
/// changes are dropped for it and the room is sent whole instead.
pub const INBOX_CAP: usize = 1024;

/// One member, as everybody else in the room sees them.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    /// The connection's opaque id ([`Connection::tag`](crate::Connection::tag)).
    pub connection: String,
    /// The signed-in user, or `None` for an anonymous page.
    pub user_id: Option<Uuid>,
    /// The `users` columns the stream's settings show, read at join time.
    pub fields: Map<String, Json>,
    /// The page's state, `null` until it sets one.
    pub state: Json,
}

impl Member {
    /// The member as the wire carries it. The three fixed keys win over a
    /// shown field of the same name, so a `users` column called `state` cannot
    /// pass for the page's state.
    pub fn to_json(&self) -> Json {
        let mut out = self.fields.clone();
        out.insert("connection".to_owned(), Json::from(self.connection.clone()));
        out.insert(
            "user_id".to_owned(),
            self.user_id
                .map_or(Json::Null, |id| Json::from(id.hyphenated().to_string())),
        );
        out.insert("state".to_owned(), self.state.clone());
        Json::Object(out)
    }
}

/// One change to a room, as a connection is handed it.
#[derive(Debug, Clone, PartialEq)]
pub enum PresenceEvent {
    /// A member arrived (the member, as [`Member::to_json`]).
    Joined(Json),
    /// The member with this connection id left.
    Left(String),
    /// The member with this connection id set its state.
    Updated {
        /// The member's connection id.
        connection: String,
        /// Its state now.
        state: Json,
    },
}

/// What a connection has been handed and not yet sent.
#[derive(Debug, Default)]
pub struct PresenceInbox {
    pending: Mutex<InboxState>,
    bell: Notify,
}

#[derive(Debug, Default)]
struct InboxState {
    events: Vec<(RoomKey, PresenceEvent)>,
    resync: BTreeSet<RoomKey>,
}

/// What a connection takes out of its inbox: the changes, oldest first, and
/// the rooms whose changes were dropped and which it sends whole instead.
#[derive(Debug, Default)]
pub struct PresenceNews {
    /// The changes, oldest first.
    pub events: Vec<(RoomKey, PresenceEvent)>,
    /// The rooms to send whole.
    pub resync: Vec<RoomKey>,
}

impl PresenceInbox {
    fn push(&self, room: &RoomKey, event: PresenceEvent) {
        let mut state = lock(&self.pending);
        if state.resync.contains(room) {
            return;
        }
        if state.events.len() >= INBOX_CAP {
            // This room is sent whole when the connection catches up; its
            // patches so far would only be applied to a list that is replaced.
            state.events.retain(|(key, _)| key != room);
            state.resync.insert(room.clone());
        } else {
            state.events.push((room.clone(), event));
        }
        drop(state);
        self.bell.notify_one();
    }

    /// Forget everything about `room`: the connection is (re)joining it and
    /// is about to be handed the list as it stands.
    fn forget(&self, room: &RoomKey) {
        let mut state = lock(&self.pending);
        state.events.retain(|(key, _)| key != room);
        state.resync.remove(room);
    }

    /// Take what is waiting.
    pub fn take(&self) -> PresenceNews {
        let mut state = lock(&self.pending);
        PresenceNews {
            events: std::mem::take(&mut state.events),
            resync: std::mem::take(&mut state.resync).into_iter().collect(),
        }
    }

    /// Resolves when something was pushed.
    pub async fn rang(&self) {
        self.bell.notified().await;
    }
}

struct Seated {
    member: Member,
    /// How many of the connection's subscriptions are on this topic.
    seats: usize,
    inbox: Arc<PresenceInbox>,
}

/// Every room in the process with somebody in it.
#[derive(Default)]
pub(crate) struct Rooms {
    rooms: HashMap<RoomKey, BTreeMap<ConnectionId, Seated>>,
}

impl Rooms {
    /// Seat `connection` in `room` — or count one more of its subscriptions if
    /// it is already there — and answer with the members as they now stand.
    pub(crate) fn join(
        &mut self,
        room: &RoomKey,
        connection: ConnectionId,
        member: Member,
        inbox: &Arc<PresenceInbox>,
    ) -> Vec<Json> {
        let seated = self.rooms.entry(room.clone()).or_default();
        // Whatever was queued about this room is older than the list it is
        // about to be handed.
        inbox.forget(room);
        match seated.get_mut(&connection) {
            Some(already) => already.seats += 1,
            None => {
                let joined = member.to_json();
                for other in seated.values() {
                    other
                        .inbox
                        .push(room, PresenceEvent::Joined(joined.clone()));
                }
                seated.insert(
                    connection,
                    Seated {
                        member,
                        seats: 1,
                        inbox: Arc::clone(inbox),
                    },
                );
            }
        }
        Self::listed(seated)
    }

    /// One of `connection`'s subscriptions left `room`; the last one out takes
    /// the member with it.
    pub(crate) fn leave(&mut self, room: &RoomKey, connection: ConnectionId) {
        let Some(seated) = self.rooms.get_mut(room) else {
            return;
        };
        let Some(entry) = seated.get_mut(&connection) else {
            return;
        };
        entry.seats = entry.seats.saturating_sub(1);
        if entry.seats > 0 {
            return;
        }
        if let Some(gone) = seated.remove(&connection) {
            for other in seated.values() {
                other
                    .inbox
                    .push(room, PresenceEvent::Left(gone.member.connection.clone()));
            }
        }
        if seated.is_empty() {
            self.rooms.remove(room);
        }
    }

    /// Set `connection`'s state in `room`, telling everybody else.
    pub(crate) fn set_state(&mut self, room: &RoomKey, connection: ConnectionId, state: Json) {
        let Some(seated) = self.rooms.get_mut(room) else {
            return;
        };
        let Some(entry) = seated.get_mut(&connection) else {
            return;
        };
        if entry.member.state == state {
            return;
        }
        entry.member.state = state.clone();
        let tag = entry.member.connection.clone();
        for (id, other) in seated.iter() {
            if *id != connection {
                other.inbox.push(
                    room,
                    PresenceEvent::Updated {
                        connection: tag.clone(),
                        state: state.clone(),
                    },
                );
            }
        }
    }

    /// The members of `room` as they stand.
    pub(crate) fn members(&self, room: &RoomKey) -> Vec<Json> {
        self.rooms.get(room).map(Self::listed).unwrap_or_default()
    }

    /// Every room of `stream`, by topic, with its members.
    pub(crate) fn of_stream(&self, stream: StreamId) -> Vec<(Option<String>, Vec<Json>)> {
        let mut found: Vec<(Option<String>, Vec<Json>)> = self
            .rooms
            .iter()
            .filter(|((id, _), _)| *id == stream)
            .map(|((_, topic), seated)| (topic.clone(), Self::listed(seated)))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    }

    fn listed(seated: &BTreeMap<ConnectionId, Seated>) -> Vec<Json> {
        seated.values().map(|s| s.member.to_json()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn room(topic: &str) -> RoomKey {
        (StreamId(Uuid::from_u128(7)), Some(topic.to_owned()))
    }

    fn member(tag: &str) -> Member {
        Member {
            connection: tag.to_owned(),
            user_id: None,
            fields: Map::new(),
            state: Json::Null,
        }
    }

    #[test]
    fn a_member_is_the_shown_fields_and_the_three_fixed_keys() {
        let mut fields = Map::new();
        fields.insert("name".to_owned(), json!("Alice"));
        fields.insert("state".to_owned(), json!("a column called state"));
        let alice = Member {
            connection: "c1".to_owned(),
            user_id: Some(Uuid::from_u128(0xa11ce)),
            fields,
            state: json!({ "dragging": 3 }),
        };
        assert_eq!(
            alice.to_json(),
            json!({
                "connection": "c1",
                "user_id": Uuid::from_u128(0xa11ce).hyphenated().to_string(),
                "name": "Alice",
                "state": { "dragging": 3 },
            })
        );
    }

    #[test]
    fn joins_leaves_and_states_reach_the_others_and_not_oneself() {
        let mut rooms = Rooms::default();
        let a = Arc::new(PresenceInbox::default());
        let b = Arc::new(PresenceInbox::default());
        let board = room("1");

        let listed = rooms.join(&board, 1, member("ca"), &a);
        assert_eq!(listed.len(), 1);
        let listed = rooms.join(&board, 2, member("cb"), &b);
        assert_eq!(listed.len(), 2, "the joiner is handed both");
        let news = a.take();
        assert_eq!(news.events.len(), 1);
        assert!(matches!(&news.events[0].1, PresenceEvent::Joined(m) if m["connection"] == "cb"));
        assert!(b.take().events.is_empty(), "nobody is told of themselves");

        rooms.set_state(&board, 2, json!({ "dragging": 9 }));
        // Setting the same state again is not news.
        rooms.set_state(&board, 2, json!({ "dragging": 9 }));
        let news = a.take();
        assert_eq!(
            news.events,
            vec![(
                board.clone(),
                PresenceEvent::Updated {
                    connection: "cb".to_owned(),
                    state: json!({ "dragging": 9 })
                }
            )]
        );
        assert!(b.take().events.is_empty());

        rooms.leave(&board, 2);
        assert_eq!(
            a.take().events,
            vec![(board.clone(), PresenceEvent::Left("cb".to_owned()))]
        );
        rooms.leave(&board, 1);
        assert!(rooms.members(&board).is_empty());
        assert!(rooms.rooms.is_empty(), "an empty room is dropped");
    }

    #[test]
    fn two_subscriptions_of_one_connection_are_one_member() {
        let mut rooms = Rooms::default();
        let a = Arc::new(PresenceInbox::default());
        let b = Arc::new(PresenceInbox::default());
        let board = room("1");
        rooms.join(&board, 2, member("cb"), &b);
        rooms.join(&board, 1, member("ca"), &a);
        rooms.join(&board, 1, member("ca"), &a);
        assert_eq!(rooms.members(&board).len(), 2);
        assert_eq!(b.take().events.len(), 1, "one join, not two");
        rooms.leave(&board, 1);
        assert!(b.take().events.is_empty(), "still there on its other one");
        rooms.leave(&board, 1);
        assert_eq!(
            b.take().events,
            vec![(board, PresenceEvent::Left("ca".to_owned()))]
        );
    }

    #[test]
    fn a_full_inbox_rebuilds_the_room_rather_than_patching_it_wrongly() {
        let mut rooms = Rooms::default();
        let watcher = Arc::new(PresenceInbox::default());
        let busy = Arc::new(PresenceInbox::default());
        let board = room("1");
        let quiet = room("2");
        rooms.join(&board, 1, member("watcher"), &watcher);
        rooms.join(&quiet, 1, member("watcher"), &watcher);
        rooms.join(&quiet, 3, member("other"), &busy);
        rooms.join(&board, 2, member("busy"), &busy);
        for n in 0..(INBOX_CAP + 10) {
            rooms.set_state(&board, 2, json!(n));
        }
        let news = watcher.take();
        assert_eq!(news.resync, vec![board.clone()]);
        assert!(
            news.events.iter().all(|(key, _)| *key != board),
            "nothing patched about a room that is rebuilt"
        );
        assert!(
            news.events
                .iter()
                .any(|(key, e)| *key == quiet && matches!(e, PresenceEvent::Joined(_))),
            "another room's news survives"
        );
        // Rejoining forgets the resync: the joiner is handed the list.
        rooms.set_state(&board, 2, json!("again"));
        rooms.join(&board, 1, member("watcher"), &watcher);
        assert!(watcher.take().resync.is_empty());
    }

    #[test]
    fn the_admin_sees_every_room_of_a_stream_by_topic() {
        let mut rooms = Rooms::default();
        let inbox = Arc::new(PresenceInbox::default());
        rooms.join(&room("2"), 1, member("c1"), &inbox);
        rooms.join(&room("1"), 2, member("c2"), &inbox);
        let other = (StreamId(Uuid::from_u128(8)), None);
        rooms.join(&other, 3, member("c3"), &inbox);
        let listed = rooms.of_stream(StreamId(Uuid::from_u128(7)));
        let topics: Vec<Option<String>> = listed.iter().map(|(t, _)| t.clone()).collect();
        assert_eq!(topics, vec![Some("1".to_owned()), Some("2".to_owned())]);
    }
}

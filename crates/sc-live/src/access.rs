//! Who may subscribe to what (TODO.md "Live updates" §3), as **pure
//! functions** over facts the socket has already looked up.
//!
//! Pure so the rules can be tested as a table of cases, which is how an
//! authorisation rule should be read and how a reviewer can check it is the
//! rule the design states — and so the socket, the re-check and (from L3) the
//! fan-out all ask the same function rather than each holding a copy of it.
//!
//! The rules decided here:
//!
//! - **Exposure is the application's decision** (rule 3). A stream the app does
//!   not list does not exist for that app, and neither does a row topic of a
//!   table outside the app's table subset.
//! - **The stream's `min_role` is a floor on subscribing at all**, and a stream
//!   with none is admin-only: one an admin has not thought about stays closed.
//!   Topic checks come on top of it and can only narrow.
//! - **A `user` topic is the subscriber's own.** They name no topic, they get
//!   their own user id, and an anonymous caller — who has none — is refused.
//! - **A `row` topic needs read access to its row**, decided by the §7.3 rule
//!   (role floor OR ownership formula). The verdict is a *fact* here, looked up
//!   by the socket through the application's **REST read** of that one row —
//!   the same function, so a subscription can never see a row the REST API
//!   would not return. A row that does not exist and a row the caller may not
//!   read are the same fact: the read returned nothing.
//! - **Denial looks like absence** (rule 4). Unknown, unexposed, forbidden, a
//!   missing row and an unreadable row all come back as the one [`Denied`],
//!   which the socket turns into the one `unavailable` frame.
//! - **A malformed request is told so — but only once access has passed.** A
//!   topic for a stream that takes none, or none for a row stream, is
//!   [`Refusal::Invalid`]: by then the caller has been allowed to know the
//!   stream is there, so saying their frame was wrong tells them nothing new.
//! - **A per-element stream is subscribed without a topic** (L3): who may see
//!   which change is decided element by element in the fan-out
//!   ([`crate::changes`]), so here it needs only the stream-level rules — and
//!   its table must be in the application's subset, like a row topic's (rule
//!   3). Naming a topic for it is malformed: its subscriber narrows it with a
//!   filter.

use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC};
use sc_stream::TopicSpec;
use uuid::Uuid;

/// Who is asking: the role they hold and, when signed in, who they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    /// Their role — the public role for an anonymous caller.
    pub role: u8,
    /// Their user id, or `None` for an anonymous caller.
    pub user: Option<Uuid>,
}

impl Caller {
    /// A caller with this role and user.
    pub fn new(role: Option<u8>, user: Option<Uuid>) -> Caller {
        Caller {
            role: caller_role(role),
            user,
        }
    }
}

/// What the socket knows about a stream when someone asks to subscribe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFacts {
    /// Whether the application lists the stream in its `streams`.
    pub exposed: bool,
    /// The stored stream, if there is one by that name.
    pub stored: Option<StoredStream>,
    /// For a `row` stream asked about one row: what the caller's REST read of
    /// it found. For a per-element stream, [`RowFact::TableNotExposed`] when
    /// its table is outside the application's subset. [`RowFact::NotAsked`]
    /// otherwise.
    pub row: RowFact,
}

/// The parts of a stored stream access is decided by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredStream {
    /// Its `min_role`; `None` is admin-only.
    pub min_role: Option<u8>,
    /// How its elements are split into topics.
    pub topics: TopicSpec,
}

/// What the caller's read of a row topic's row found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowFact {
    /// Nobody looked: the stream has no row topics, or no topic was named.
    NotAsked,
    /// The row's table is not in the application's table subset — it does not
    /// exist for this app, whatever the caller's role (rule 3).
    TableNotExposed,
    /// The caller's REST read of the row returned it.
    Readable,
    /// It returned nothing: the row does not exist, or the caller may not read
    /// it. One fact, on purpose (rule 4).
    Unreadable,
}

/// The one refusal of access. It carries nothing, so no caller can tell anybody
/// which rule refused them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denied;

/// Why a subscription was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Access: the one `unavailable` answer.
    Denied,
    /// The frame does not fit the stream — said only once the caller may know
    /// the stream is there.
    Invalid(String),
}

impl From<Denied> for Refusal {
    fn from(_: Denied) -> Refusal {
        Refusal::Denied
    }
}

/// What a subscription that passed is granted: the stream as stored, and the
/// topic it is routed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The stream, as access was decided on it.
    pub stored: StoredStream,
    /// The topic the subscription is on: none for a `single` stream, the
    /// caller's own id for a `user` one, the row's key for a `row` one.
    pub topic: Option<String>,
}

/// The role a caller holds: their own, or the public role for nobody.
pub fn caller_role(role: Option<u8>) -> u8 {
    role.unwrap_or(ROLE_PUBLIC)
}

/// The stream-level rules alone: exposed, stored, and the caller meets its
/// floor. What every subscription passes before anything about its topic is
/// looked up — so a caller below the floor never causes a row to be read.
pub fn may_open(facts: &StreamFacts, caller: &Caller) -> Result<StoredStream, Denied> {
    if !facts.exposed {
        return Err(Denied);
    }
    let stored = facts.stored.as_ref().ok_or(Denied)?;
    let floor = stored.min_role.unwrap_or(ROLE_ADMIN);
    // A lower number is more privilege: admin is 1, public is 100.
    if caller.role > floor {
        return Err(Denied);
    }
    Ok(stored.clone())
}

/// Whether `caller` may subscribe to the stream `facts` describes, naming
/// `topic` (or not), and if so on which topic.
pub fn may_subscribe(
    facts: &StreamFacts,
    caller: &Caller,
    topic: Option<&str>,
) -> Result<Grant, Refusal> {
    let stored = may_open(facts, caller)?;
    let topic = match (&stored.topics, topic) {
        (TopicSpec::Single, None) => None,
        (TopicSpec::Single, Some(_)) => {
            return Err(Refusal::Invalid(
                "this stream has no topics; subscribe to it without one".to_owned(),
            ));
        }
        // The subscriber's own topic, never one they name — and nobody's for
        // an anonymous caller, who is refused as if the stream were not there.
        (TopicSpec::User, None) => {
            let user = caller.user.ok_or(Denied)?;
            Some(user.hyphenated().to_string())
        }
        (TopicSpec::User, Some(_)) => {
            // Checked after the anonymous refusal, so an anonymous caller is
            // told nothing more by naming a topic than by not naming one.
            caller.user.ok_or(Denied)?;
            return Err(Refusal::Invalid(
                "this stream's topic is your own; subscribe to it without one".to_owned(),
            ));
        }
        (TopicSpec::Row { .. }, None) => {
            return Err(Refusal::Invalid(
                "this stream has a topic per row; name the row's key as the topic".to_owned(),
            ));
        }
        (TopicSpec::Row { .. }, Some(key)) => match facts.row {
            RowFact::Readable => Some(key.to_owned()),
            RowFact::Unreadable | RowFact::TableNotExposed | RowFact::NotAsked => {
                return Err(Refusal::Denied);
            }
        },
        // Its table must exist for this app — checked before the frame's shape,
        // so naming a topic tells a caller nothing about a table they cannot
        // see.
        (TopicSpec::PerElementRow { .. }, _) if facts.row == RowFact::TableNotExposed => {
            return Err(Refusal::Denied);
        }
        (TopicSpec::PerElementRow { .. }, None) => None,
        (TopicSpec::PerElementRow { .. }, Some(_)) => {
            return Err(Refusal::Invalid(
                "this stream carries row changes, which are narrowed with a filter, not a \
                 topic; subscribe without one"
                    .to_owned(),
            ));
        }
    };
    Ok(Grant { stored, topic })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDITOR: u8 = 80;
    const MEMBER: u8 = 90;

    fn facts(exposed: bool, stored: Option<(Option<u8>, TopicSpec)>) -> StreamFacts {
        StreamFacts {
            exposed,
            stored: stored.map(|(min_role, topics)| StoredStream { min_role, topics }),
            row: RowFact::NotAsked,
        }
    }

    fn with_row(mut facts: StreamFacts, row: RowFact) -> StreamFacts {
        facts.row = row;
        facts
    }

    fn alice() -> Uuid {
        Uuid::from_u128(0xa11ce)
    }

    fn signed_in(role: u8) -> Caller {
        Caller {
            role,
            user: Some(alice()),
        }
    }

    fn anonymous() -> Caller {
        Caller::new(None, None)
    }

    fn boards() -> TopicSpec {
        TopicSpec::Row {
            table: "boards".to_owned(),
        }
    }

    fn cards() -> TopicSpec {
        TopicSpec::PerElementRow {
            table: "cards".to_owned(),
        }
    }

    /// What `decide` answers: the granted topic, or `Err(None)` for denied and
    /// `Err(Some(()))` for invalid.
    type Decision = Result<Option<String>, Option<()>>;

    /// `Ok(topic)`, `Err(None)` for denied, `Err(Some(()))` for invalid.
    fn decide(facts: &StreamFacts, caller: &Caller, topic: Option<&str>) -> Decision {
        match may_subscribe(facts, caller, topic) {
            Ok(grant) => Ok(grant.topic),
            Err(Refusal::Denied) => Err(None),
            Err(Refusal::Invalid(_)) => Err(Some(())),
        }
    }

    #[test]
    fn the_stream_rules_as_a_table() {
        let single = TopicSpec::Single;
        // (case, facts, caller, allowed)
        let cases: Vec<(&str, StreamFacts, Caller, bool)> = vec![
            (
                "exposed, at the floor",
                facts(true, Some((Some(MEMBER), single.clone()))),
                signed_in(MEMBER),
                true,
            ),
            (
                "exposed, above the floor",
                facts(true, Some((Some(MEMBER), single.clone()))),
                signed_in(EDITOR),
                true,
            ),
            (
                "exposed, admin",
                facts(true, Some((Some(MEMBER), single.clone()))),
                signed_in(ROLE_ADMIN),
                true,
            ),
            (
                "below the floor",
                facts(true, Some((Some(EDITOR), single.clone()))),
                signed_in(MEMBER),
                false,
            ),
            (
                "anonymous, floor above public",
                facts(true, Some((Some(MEMBER), single.clone()))),
                anonymous(),
                false,
            ),
            (
                "anonymous, public floor",
                facts(true, Some((Some(ROLE_PUBLIC), single.clone()))),
                anonymous(),
                true,
            ),
            (
                "no min_role is admin-only",
                facts(true, Some((None, single.clone()))),
                signed_in(EDITOR),
                false,
            ),
            (
                "no min_role, admin",
                facts(true, Some((None, single.clone()))),
                signed_in(ROLE_ADMIN),
                true,
            ),
            (
                "not exposed, would be allowed",
                facts(false, Some((Some(ROLE_PUBLIC), single.clone()))),
                signed_in(ROLE_ADMIN),
                false,
            ),
            (
                "exposed, but no such stream",
                facts(true, None),
                signed_in(ROLE_ADMIN),
                false,
            ),
            (
                "per-element, at the floor",
                facts(true, Some((Some(MEMBER), cards()))),
                signed_in(MEMBER),
                true,
            ),
            (
                "per-element, below the floor",
                facts(true, Some((Some(EDITOR), cards()))),
                signed_in(MEMBER),
                false,
            ),
            (
                "per-element, anonymous at a public floor",
                facts(true, Some((Some(ROLE_PUBLIC), cards()))),
                anonymous(),
                true,
            ),
            (
                "per-element, table outside the app",
                with_row(
                    facts(true, Some((Some(ROLE_PUBLIC), cards()))),
                    RowFact::TableNotExposed,
                ),
                signed_in(ROLE_ADMIN),
                false,
            ),
        ];
        for (case, facts, caller, allowed) in cases {
            assert_eq!(
                may_subscribe(&facts, &caller, None).is_ok(),
                allowed,
                "{case}: role {}",
                caller.role
            );
        }
    }

    #[test]
    fn a_user_topic_is_the_subscribers_own_and_nobody_elses() {
        let job = facts(true, Some((Some(MEMBER), TopicSpec::User)));
        assert_eq!(
            decide(&job, &signed_in(MEMBER), None),
            Ok(Some(alice().hyphenated().to_string())),
            "their own id, never named"
        );
        // Anonymous has no topic, and is told the stream is not there — the
        // same answer whether or not they named one.
        let public_job = facts(true, Some((Some(ROLE_PUBLIC), TopicSpec::User)));
        assert_eq!(decide(&public_job, &anonymous(), None), Err(None));
        assert_eq!(decide(&public_job, &anonymous(), Some("x")), Err(None));
        // Naming a topic — anybody's, your own included — is a malformed frame.
        assert_eq!(
            decide(&job, &signed_in(MEMBER), Some(&alice().to_string())),
            Err(Some(()))
        );
        assert_eq!(decide(&job, &signed_in(MEMBER), Some("bob")), Err(Some(())));
        // The floor still applies first.
        assert_eq!(decide(&job, &signed_in(ROLE_PUBLIC), None), Err(None));
    }

    #[test]
    fn a_row_topic_needs_the_rest_read_of_its_row() {
        let presence = facts(true, Some((Some(MEMBER), boards())));
        let caller = signed_in(MEMBER);
        // (case, row fact, topic, outcome)
        let cases: Vec<(&str, RowFact, Option<&str>, Decision)> = vec![
            (
                "readable",
                RowFact::Readable,
                Some("7"),
                Ok(Some("7".to_owned())),
            ),
            ("unreadable", RowFact::Unreadable, Some("7"), Err(None)),
            (
                "table outside the app",
                RowFact::TableNotExposed,
                Some("7"),
                Err(None),
            ),
            ("never looked up", RowFact::NotAsked, Some("7"), Err(None)),
            ("no topic", RowFact::NotAsked, None, Err(Some(()))),
        ];
        for (case, row, topic, outcome) in cases {
            assert_eq!(
                decide(&with_row(presence.clone(), row), &caller, topic),
                outcome,
                "{case}"
            );
        }
        // Below the floor nothing about the row matters, readable or not.
        assert_eq!(
            decide(
                &with_row(presence, RowFact::Readable),
                &signed_in(ROLE_PUBLIC),
                Some("7")
            ),
            Err(None)
        );
    }

    #[test]
    fn a_topic_on_a_single_stream_is_invalid_only_once_access_passed() {
        let boiler = facts(true, Some((Some(EDITOR), TopicSpec::Single)));
        assert_eq!(
            decide(&boiler, &signed_in(EDITOR), Some("7")),
            Err(Some(()))
        );
        // Below the floor it is the one refusal, not a hint the stream exists.
        assert_eq!(decide(&boiler, &signed_in(MEMBER), Some("7")), Err(None));
    }

    #[test]
    fn a_change_stream_takes_no_topic_and_its_table_must_be_the_apps() {
        let live = facts(true, Some((Some(MEMBER), cards())));
        assert_eq!(decide(&live, &signed_in(MEMBER), None), Ok(None));
        // A topic is malformed — but only once access passed…
        assert_eq!(decide(&live, &signed_in(MEMBER), Some("7")), Err(Some(())));
        assert_eq!(decide(&live, &signed_in(ROLE_PUBLIC), Some("7")), Err(None));
        // …and a table outside the app is absent, topic or not.
        let outside = with_row(live, RowFact::TableNotExposed);
        assert_eq!(decide(&outside, &signed_in(MEMBER), None), Err(None));
        assert_eq!(decide(&outside, &signed_in(MEMBER), Some("7")), Err(None));
    }

    #[test]
    fn every_refusal_of_access_is_the_same_refusal() {
        let cases = [
            (facts(false, None), signed_in(ROLE_ADMIN), None),
            (facts(true, None), signed_in(ROLE_ADMIN), None),
            (
                facts(true, Some((Some(EDITOR), TopicSpec::Single))),
                signed_in(MEMBER),
                None,
            ),
            (
                facts(true, Some((Some(ROLE_PUBLIC), TopicSpec::User))),
                anonymous(),
                None,
            ),
            (
                with_row(
                    facts(true, Some((Some(MEMBER), boards()))),
                    RowFact::Unreadable,
                ),
                signed_in(MEMBER),
                Some("7"),
            ),
            (
                with_row(
                    facts(true, Some((Some(MEMBER), boards()))),
                    RowFact::TableNotExposed,
                ),
                signed_in(MEMBER),
                Some("7"),
            ),
            (
                with_row(
                    facts(true, Some((Some(MEMBER), cards()))),
                    RowFact::TableNotExposed,
                ),
                signed_in(MEMBER),
                None,
            ),
        ];
        for (facts, caller, topic) in &cases {
            assert_eq!(may_subscribe(facts, caller, *topic), Err(Refusal::Denied));
        }
    }
}

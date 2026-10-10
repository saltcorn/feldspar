//! Who may subscribe to what (TODO.md "Live updates" §3), as **pure
//! functions** over facts the socket has already looked up.
//!
//! Pure so the rules can be tested as a table of cases, which is how an
//! authorisation rule should be read and how a reviewer can check it is the
//! rule the design states — and so the socket, the re-check and (from L2) the
//! fan-out all ask the same function rather than each holding a copy of it.
//!
//! The rules this milestone decides:
//!
//! - **Exposure is the application's decision** (rule 3). A stream the app does
//!   not list does not exist for that app.
//! - **The stream's `min_role` is a floor on subscribing at all**, and a stream
//!   with none is admin-only: one an admin has not thought about stays closed.
//! - **Denial looks like absence** (rule 4). Unknown, unexposed and forbidden
//!   all come back as the one [`Denied`], which the socket turns into the one
//!   `unavailable` frame.
//! - **Topics other than `Single` are refused until their milestone.** The
//!   per-user, per-row and per-element checks arrive with L2 and L3; until
//!   then a stream that declares them is closed rather than open to its
//!   `min_role`, because the narrowing they promise is exactly what would be
//!   missing.

use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC};
use sc_stream::TopicSpec;

/// What the socket knows about a stream when someone asks to subscribe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFacts {
    /// Whether the application lists the stream in its `streams`.
    pub exposed: bool,
    /// The stored stream, if there is one by that name.
    pub stored: Option<StoredStream>,
}

/// The parts of a stored stream access is decided by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredStream {
    /// Its `min_role`; `None` is admin-only.
    pub min_role: Option<u8>,
    /// How its elements are split into topics.
    pub topics: TopicSpec,
}

/// The one refusal. It carries nothing, so no caller can tell anybody which
/// rule refused them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denied;

/// The role a caller holds: their own, or the public role for nobody.
pub fn caller_role(role: Option<u8>) -> u8 {
    role.unwrap_or(ROLE_PUBLIC)
}

/// Whether a caller holding `role` may subscribe to the stream `facts`
/// describes, naming `topic` (or not).
///
/// A `topic` given for a stream that takes none is not a denial — the caller
/// has been allowed to know the stream is there, and is told their frame was
/// wrong — so it is the socket's to report as `invalid` *after* this passes.
pub fn may_subscribe(facts: &StreamFacts, role: u8) -> Result<&StoredStream, Denied> {
    if !facts.exposed {
        return Err(Denied);
    }
    let stored = facts.stored.as_ref().ok_or(Denied)?;
    let floor = stored.min_role.unwrap_or(ROLE_ADMIN);
    // A lower number is more privilege: admin is 1, public is 100.
    if role > floor {
        return Err(Denied);
    }
    match stored.topics {
        TopicSpec::Single => Ok(stored),
        TopicSpec::User | TopicSpec::Row { .. } | TopicSpec::PerElementRow { .. } => Err(Denied),
    }
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
        }
    }

    #[test]
    fn the_rules_as_a_table() {
        let single = TopicSpec::Single;
        let row = TopicSpec::Row {
            table: "boards".to_owned(),
        };
        // (case, facts, role, allowed)
        let cases: Vec<(&str, StreamFacts, u8, bool)> = vec![
            (
                "exposed, at the floor",
                facts(true, Some((Some(MEMBER), single.clone()))),
                MEMBER,
                true,
            ),
            (
                "exposed, above the floor",
                facts(true, Some((Some(MEMBER), single.clone()))),
                EDITOR,
                true,
            ),
            (
                "exposed, admin",
                facts(true, Some((Some(MEMBER), single.clone()))),
                ROLE_ADMIN,
                true,
            ),
            (
                "below the floor",
                facts(true, Some((Some(EDITOR), single.clone()))),
                MEMBER,
                false,
            ),
            (
                "anonymous, floor above public",
                facts(true, Some((Some(MEMBER), single.clone()))),
                caller_role(None),
                false,
            ),
            (
                "anonymous, public floor",
                facts(true, Some((Some(ROLE_PUBLIC), single.clone()))),
                caller_role(None),
                true,
            ),
            (
                "no min_role is admin-only",
                facts(true, Some((None, single.clone()))),
                EDITOR,
                false,
            ),
            (
                "no min_role, admin",
                facts(true, Some((None, single.clone()))),
                ROLE_ADMIN,
                true,
            ),
            (
                "not exposed, would be allowed",
                facts(false, Some((Some(ROLE_PUBLIC), single.clone()))),
                ROLE_ADMIN,
                false,
            ),
            (
                "exposed, but no such stream",
                facts(true, None),
                ROLE_ADMIN,
                false,
            ),
            (
                "topics before their milestone",
                facts(true, Some((Some(ROLE_PUBLIC), row))),
                ROLE_ADMIN,
                false,
            ),
            (
                "user topics before their milestone",
                facts(true, Some((Some(ROLE_PUBLIC), TopicSpec::User))),
                ROLE_ADMIN,
                false,
            ),
        ];
        for (case, facts, role, allowed) in cases {
            assert_eq!(
                may_subscribe(&facts, role).is_ok(),
                allowed,
                "{case}: role {role}"
            );
        }
    }

    #[test]
    fn every_refusal_is_the_same_refusal() {
        let cases = [
            (facts(false, None), ROLE_ADMIN),
            (facts(true, None), ROLE_ADMIN),
            (facts(true, Some((Some(EDITOR), TopicSpec::Single))), MEMBER),
        ];
        for (facts, role) in &cases {
            assert_eq!(may_subscribe(facts, *role), Err(Denied));
        }
    }
}

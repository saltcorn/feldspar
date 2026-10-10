//! One connection's subscriptions, and the transitions each may make (TODO.md
//! "Live updates" §4).
//!
//! ```text
//!               subscribe (authorised)
//!   (none) ───────────────────────────► Waiting ──── stream starts ───► Active
//!     ▲                                    ▲  ◄──── stream restarts ────┘ │
//!     │                                    │      (a fresh `ready`)       │
//!     └──── unsubscribe / revoked / close ─┴──────────────────────────────┘
//! ```
//!
//! - **Waiting**: authorised, but the stream is not running on this server
//!   (disabled, or failing to start). The client has had `ready` with a
//!   `stopped` status and no element type; it moves to Active by itself when
//!   the stream starts, with a second `ready`.
//! - **Active**: attached to the running stream's fan-out, receiving elements.
//!
//! [`SubscriptionSet`] holds a connection's subscriptions by the client's own
//! name for each (`sub`) and enforces the two rules that do not depend on any
//! stream: a name is used once at a time, and there are at most
//! [`max_subscriptions`](crate::LiveLimits::max_subscriptions) of them.
//! Generic over what an attachment is, so it is tested here as data; the
//! socket attaches a [`Attached`](crate::Attached) from the hub.

use std::collections::BTreeMap;

use crate::protocol::{ErrorCode, FrameError};

/// Where a subscription is.
#[derive(Debug)]
pub enum Phase<A> {
    /// Authorised; the stream is not running here yet.
    Waiting,
    /// Attached to the running stream.
    Active(A),
}

/// One subscription on a connection.
#[derive(Debug)]
pub struct Subscription<A> {
    /// The client's name for it.
    pub sub: String,
    /// The stream's name.
    pub stream: String,
    /// The topic, for a stream with topics.
    pub topic: Option<String>,
    /// Where it is.
    pub phase: Phase<A>,
    /// The last status frame sent for it, so an unchanged one is not sent
    /// again.
    pub last_status: Option<serde_json::Value>,
}

impl<A> Subscription<A> {
    /// A subscription that is authorised and waiting for its stream.
    pub fn waiting(
        sub: impl Into<String>,
        stream: impl Into<String>,
        topic: Option<String>,
    ) -> Subscription<A> {
        Subscription {
            sub: sub.into(),
            stream: stream.into(),
            topic,
            phase: Phase::Waiting,
            last_status: None,
        }
    }

    /// The attachment, when it is active.
    pub fn attached(&self) -> Option<&A> {
        match &self.phase {
            Phase::Active(attached) => Some(attached),
            Phase::Waiting => None,
        }
    }

    /// Attach it, returning the attachment it replaces (a restart).
    pub fn activate(&mut self, attached: A) -> Option<A> {
        match std::mem::replace(&mut self.phase, Phase::Active(attached)) {
            Phase::Active(previous) => Some(previous),
            Phase::Waiting => None,
        }
    }

    /// Detach it: the stream stopped being something it can follow.
    pub fn deactivate(&mut self) -> Option<A> {
        match std::mem::replace(&mut self.phase, Phase::Waiting) {
            Phase::Active(previous) => Some(previous),
            Phase::Waiting => None,
        }
    }
}

/// A connection's subscriptions, by name.
#[derive(Debug)]
pub struct SubscriptionSet<A> {
    subs: BTreeMap<String, Subscription<A>>,
    max: usize,
}

impl<A> SubscriptionSet<A> {
    /// An empty set holding at most `max`.
    pub fn new(max: usize) -> SubscriptionSet<A> {
        SubscriptionSet {
            subs: BTreeMap::new(),
            max,
        }
    }

    /// Whether `sub` may be opened: its name is not in use, and there is room.
    ///
    /// Asked **before** anything about the stream is looked up, so the answer
    /// cannot depend on whether the stream exists.
    pub fn check_new(&self, sub: &str) -> Result<(), FrameError> {
        if sub.is_empty() {
            return Err(FrameError::invalid(
                Some(sub.to_owned()),
                "a subscription needs a non-empty `sub`",
            ));
        }
        if self.subs.contains_key(sub) {
            return Err(FrameError::invalid(
                Some(sub.to_owned()),
                format!("`{sub}` is already subscribed on this connection; unsubscribe it first"),
            ));
        }
        if self.subs.len() >= self.max {
            return Err(FrameError::new(
                Some(sub.to_owned()),
                ErrorCode::TooManySubscriptions,
                format!(
                    "this connection already holds {} subscriptions, which is as many as it may",
                    self.max
                ),
            ));
        }
        Ok(())
    }

    /// Add an authorised subscription. Fails as [`check_new`](Self::check_new)
    /// does, so a caller that skipped it still cannot break the rules.
    pub fn insert(&mut self, subscription: Subscription<A>) -> Result<(), FrameError> {
        self.check_new(&subscription.sub)?;
        self.subs.insert(subscription.sub.clone(), subscription);
        Ok(())
    }

    /// End `sub` at the client's request. An unknown name is `invalid`: the
    /// client has lost track of its own subscriptions, and saying so is kinder
    /// than silence.
    pub fn remove(&mut self, sub: &str) -> Result<Subscription<A>, FrameError> {
        self.subs.remove(sub).ok_or_else(|| {
            FrameError::invalid(
                Some(sub.to_owned()),
                format!("`{sub}` is not subscribed on this connection"),
            )
        })
    }

    /// End `sub` because access was re-checked and refused. `None` if it was
    /// already gone.
    pub fn revoke(&mut self, sub: &str) -> Option<Subscription<A>> {
        self.subs.remove(sub)
    }

    /// The subscription named `sub`.
    pub fn get(&self, sub: &str) -> Option<&Subscription<A>> {
        self.subs.get(sub)
    }

    /// The subscription named `sub`, to change.
    pub fn get_mut(&mut self, sub: &str) -> Option<&mut Subscription<A>> {
        self.subs.get_mut(sub)
    }

    /// Every subscription, in name order.
    pub fn iter(&self) -> impl Iterator<Item = &Subscription<A>> {
        self.subs.values()
    }

    /// Every subscription, to change.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Subscription<A>> {
        self.subs.values_mut()
    }

    /// The names, in order — for a pass that may remove some.
    pub fn names(&self) -> Vec<String> {
        self.subs.keys().cloned().collect()
    }

    /// How many there are.
    pub fn len(&self) -> usize {
        self.subs.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }

    /// End every subscription, as a closing connection does.
    pub fn clear(&mut self) {
        self.subs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_used_once_at_a_time() {
        let mut set: SubscriptionSet<u32> = SubscriptionSet::new(8);
        set.insert(Subscription::waiting("s1", "boiler", None))
            .unwrap();
        let again = set
            .insert(Subscription::waiting("s1", "meter", None))
            .unwrap_err();
        assert_eq!(again.code, ErrorCode::Invalid);
        assert_eq!(again.sub.as_deref(), Some("s1"));
        // Free again once it is gone.
        set.remove("s1").unwrap();
        set.insert(Subscription::waiting("s1", "meter", None))
            .unwrap();
        assert_eq!(set.get("s1").unwrap().stream, "meter");
    }

    #[test]
    fn the_limit_is_its_own_code_and_the_set_is_unchanged() {
        let mut set: SubscriptionSet<u32> = SubscriptionSet::new(2);
        set.insert(Subscription::waiting("a", "x", None)).unwrap();
        set.insert(Subscription::waiting("b", "x", None)).unwrap();
        let error = set.check_new("c").unwrap_err();
        assert_eq!(error.code, ErrorCode::TooManySubscriptions);
        assert_eq!(set.len(), 2);
        // A duplicate is reported as a duplicate even when the set is full.
        assert_eq!(set.check_new("a").unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn an_unknown_name_cannot_be_unsubscribed() {
        let mut set: SubscriptionSet<u32> = SubscriptionSet::new(2);
        let error = set.remove("ghost").unwrap_err();
        assert_eq!(error.code, ErrorCode::Invalid);
        assert!(set.revoke("ghost").is_none());
        assert_eq!(set.check_new("").unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn waiting_becomes_active_and_a_restart_replaces_the_attachment() {
        let mut sub: Subscription<u32> = Subscription::waiting("s1", "boiler", None);
        assert!(sub.attached().is_none());
        assert_eq!(sub.activate(1), None);
        assert_eq!(sub.attached(), Some(&1));
        assert_eq!(sub.activate(2), Some(1), "a restart hands back the old one");
        assert_eq!(sub.deactivate(), Some(2));
        assert!(matches!(sub.phase, Phase::Waiting));
    }
}

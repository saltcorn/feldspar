//! The cached trigger set: what an event is matched against.
//!
//! GOALS: every created entity except users, runs and files is cached in memory,
//! and firing an event must be a lookup rather than a query — a table write that
//! did a `SELECT` on `_fd_triggers` first would put the cost of the *feature* on
//! every write that does not use it.
//!
//! **Loading validates**, and a trigger that fails is dropped from the live set
//! with its reason kept ([`TriggerIssue`]). That is the fail-closed reading, the
//! same one an invalid ownership formula gets: a trigger whose table was dropped
//! or whose action a removed plugin provided must not fire on a guess, and the
//! admin must be able to see why it is not firing — which is what the issues are
//! for (the admin UI surfaces them; the trigger stays editable, so it stays
//! fixable).

use sc_catalog::Catalog;
use sc_error::{Error, Result};

use crate::event::{Event, EventKind};
use crate::registry::ActionRegistry;
use crate::store::list_triggers;
use crate::trigger::Trigger;
use crate::validate::validate_trigger;

/// Why one stored trigger is not in the live set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerIssue {
    /// The trigger's name, as stored.
    pub trigger: String,
    /// What is wrong with it, in the words the validator used.
    pub problem: String,
}

/// The triggers that can fire, plus the ones that cannot and why.
#[derive(Debug, Clone, Default)]
pub struct Triggers {
    /// Valid, ordered by name (the store's order).
    triggers: Vec<Trigger>,
    /// Dropped, with reasons.
    issues: Vec<TriggerIssue>,
}

impl Triggers {
    /// An empty set — a catalog with no `_fd_triggers` table, and the starting
    /// point for a test.
    pub fn empty() -> Triggers {
        Triggers::default()
    }

    /// A set assembled from triggers that are already known good — how a test
    /// pins matching without a database, and nothing else: the production path is
    /// [`load`](Triggers::load), which is where validation happens.
    #[cfg(test)]
    pub(crate) fn of(triggers: Vec<Trigger>) -> Triggers {
        Triggers {
            triggers,
            issues: Vec::new(),
        }
    }

    /// Load and validate every stored trigger.
    ///
    /// A catalog with no `_fd_triggers` table yields an empty set rather than an
    /// error: that table's absence *means* "no triggers have ever been defined",
    /// which is a legitimate state for a database Saltcorn has just met. A
    /// database error while reading a table that does exist stays an error.
    pub async fn load(catalog: &Catalog, registry: &ActionRegistry) -> Result<Triggers> {
        if catalog.get(crate::TRIGGERS_TABLE)?.is_none() {
            return Ok(Triggers::empty());
        }
        let mut out = Triggers::default();
        for trigger in list_triggers(catalog).await? {
            match validate_trigger(catalog, registry, &trigger).await {
                Ok(()) => out.triggers.push(trigger),
                Err(e) => out.issues.push(TriggerIssue {
                    trigger: trigger.name.clone(),
                    problem: e.to_string(),
                }),
            }
        }
        Ok(out)
    }

    /// Reload in place, so a live handle picks up a save or a delete.
    pub async fn reload(&mut self, catalog: &Catalog, registry: &ActionRegistry) -> Result<()> {
        *self = Triggers::load(catalog, registry).await?;
        Ok(())
    }

    /// The triggers that fire for `event`: the enabled ones whose event matches
    /// and — where the kind has a channel — whose channel is the event's table
    /// or stream.
    ///
    /// A disabled trigger is skipped here rather than at load, so it stays in the
    /// set (and in the admin's list) while not firing.
    pub fn for_event(&self, event: &Event) -> impl Iterator<Item = &Trigger> {
        self.matching(event.kind, event.channel.as_deref())
    }

    /// [`for_event`](Triggers::for_event) by kind and channel, for callers that
    /// have not built an [`Event`] yet (the scheduler asks "what is due?").
    pub fn matching(
        &self,
        kind: EventKind,
        channel: Option<&str>,
    ) -> impl Iterator<Item = &Trigger> {
        self.triggers.iter().filter(move |t| {
            t.is_enabled()
                && t.when.listens_for(kind)
                // A table event must agree on the table and a stream event on
                // the stream; nothing else has a channel, and validation has
                // already ensured neither side invents one. `has_channel`
                // rather than `is_table_event`, because a stream trigger that
                // fired for every stream would be the loudest possible bug.
                && (!kind.has_channel() || t.channel.as_deref() == channel)
        })
    }

    /// The trigger named `name`, if it is in the live set.
    pub fn by_name(&self, name: &str) -> Option<&Trigger> {
        self.triggers.iter().find(|t| t.name == name)
    }

    /// The trigger named `name`, or an error that distinguishes the two ways it
    /// can be missing — never defined, or defined but **not usable**, in which
    /// case the reason is the one thing the caller needs to hear.
    pub fn require(&self, name: &str) -> Result<&Trigger> {
        if let Some(trigger) = self.by_name(name) {
            return Ok(trigger);
        }
        match self.issues.iter().find(|i| i.trigger == name) {
            Some(issue) => Err(Error::invalid(format!(
                "trigger `{name}` is not usable: {}",
                issue.problem
            ))),
            None => Err(Error::not_found(format!("no trigger named `{name}`"))),
        }
    }

    /// Every trigger that can fire, ordered by name.
    pub fn all(&self) -> &[Trigger] {
        &self.triggers
    }

    /// The stored triggers that were dropped, and why — what the admin UI shows
    /// as a warning beside the trigger list.
    pub fn issues(&self) -> &[TriggerIssue] {
        &self.issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;

    /// A set assembled directly, to test matching without a database (loading is
    /// integration-tested, where a catalog exists).
    fn triggers(list: Vec<Trigger>) -> Triggers {
        Triggers::of(list)
    }

    #[test]
    fn a_table_event_matches_only_its_own_channel() {
        let set = triggers(vec![
            Trigger::new("books_insert", EventKind::Insert, "a").on("books"),
            Trigger::new("authors_insert", EventKind::Insert, "a").on("authors"),
            Trigger::new("books_update", EventKind::Update, "a").on("books"),
        ]);
        let names: Vec<&str> = set
            .matching(EventKind::Insert, Some("books"))
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, vec!["books_insert"]);
        assert_eq!(set.matching(EventKind::Insert, Some("nothing")).count(), 0);
        assert_eq!(set.matching(EventKind::Delete, Some("books")).count(), 0);
    }

    #[test]
    fn a_stream_trigger_fires_only_for_its_own_stream() {
        // A stream event's channel is load-bearing exactly as a table event's
        // is: a trigger on `boiler` must not see `market_feed`'s elements.
        let set = triggers(vec![
            Trigger::new("boiler_log", EventKind::Stream, "a").on("boiler"),
            Trigger::new("feed_log", EventKind::Stream, "a").on("market_feed"),
        ]);
        let fired: Vec<&str> = set
            .matching(EventKind::Stream, Some("boiler"))
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(fired, vec!["boiler_log"]);
        assert_eq!(set.matching(EventKind::Stream, Some("nothing")).count(), 0);
        assert_eq!(set.matching(EventKind::Stream, None).count(), 0);
    }

    #[test]
    fn a_channel_less_event_matches_on_kind_alone() {
        let set = triggers(vec![
            Trigger::new("on_login", EventKind::Login, "a"),
            Trigger::new("on_startup", EventKind::Startup, "a"),
        ]);
        assert_eq!(set.matching(EventKind::Login, None).count(), 1);
        // The event carries no channel, and the trigger has none — a `Some`
        // channel on a channel-less kind cannot happen (validation refuses it),
        // and if it did it would not silently filter the match out.
        assert_eq!(set.matching(EventKind::Login, Some("books")).count(), 1);
    }

    #[test]
    fn a_disabled_trigger_stays_in_the_set_but_does_not_fire() {
        let mut off = Trigger::new("off", EventKind::Login, "a");
        off.set_enabled(false);
        let set = triggers(vec![off, Trigger::new("on", EventKind::Login, "a")]);
        let names: Vec<&str> = set
            .matching(EventKind::Login, None)
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, vec!["on"]);
        // Still listed and still resolvable by name — that is how it gets
        // re-enabled.
        assert_eq!(set.all().len(), 2);
        assert!(set.by_name("off").is_some());
    }

    #[test]
    fn an_unusable_trigger_is_named_with_its_reason_not_reported_as_missing() {
        let set = Triggers {
            triggers: vec![Trigger::new("good", EventKind::None, "a")],
            issues: vec![TriggerIssue {
                trigger: "broken".into(),
                problem: "trigger `broken`: no table named `gone`".into(),
            }],
        };
        assert!(set.require("good").is_ok());
        let err = set.require("broken").err().unwrap().to_string();
        assert!(
            err.contains("not usable") && err.contains("no table named"),
            "{err}"
        );
        let err = set.require("never_existed").err().unwrap();
        assert!(err.to_string().contains("no trigger named"), "{err}");
        // The two are different errors, because they call for different fixes.
        assert!(matches!(err.repr(), sc_error::Repr::NotFound(_)));
    }
}

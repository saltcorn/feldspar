//! The [`Stream`]: a provider, its configuration, who may observe it, and
//! whether it is running (TODO §5, task 2.1).
//!
//! Pure data, like a `Trigger`, an `Agent` or a `Model` — the row it is stored
//! as lives in [`store`](crate::store), the validation it must pass in
//! `validate` (task 2.2), and the subscription that makes it *flow* in the
//! supervisor (Phase 3). A stream knows nothing about how it is observed, which
//! is what lets the same record be started at boot, reloaded on a `SIGHUP`,
//! rendered in a form and exposed to an application without reshaping anything.
//!
//! **There is no `element_type` field**, and that is the one thing worth
//! reading this module for. The element type is a pure function of `provider` +
//! `configuration` ([`StreamProvider::element_type`](crate::StreamProvider::element_type)),
//! so storing a copy would be a second answer that drifts the day a provider's
//! declaration changes — and the drift would be invisible, because the stored
//! copy is what the Observe screen and the generated client would be reading
//! while the live decoder used the other one. It is computed on read and cached
//! on the running stream instead.

use sc_types::Attrs;
use sc_types::Role;
use serde_json::Value as Json;
use uuid::Uuid;

/// The attribute holding whether a stream is subscribed to — sparse (§9's
/// rule), so an untouched stream carries no key at all.
pub const ATTR_ENABLED: &str = "enabled";

/// Identifies a stream: the UUID primary key of its `_fd_streams` row (§5).
///
/// `Ord` as well as `Eq`, so the supervisor can key a `BTreeMap` on it: the
/// running set is walked on every reload and printed in every status listing,
/// and a hash order would make "which stream did it start first" a different
/// answer on every run.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct StreamId(pub Uuid);

impl StreamId {
    /// Mint an id for a new stream.
    pub fn new() -> StreamId {
        StreamId(Uuid::new_v4())
    }
}

impl Default for StreamId {
    fn default() -> Self {
        StreamId::new()
    }
}

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A dataflow an admin has created: which provider observes it, with what
/// settings, and who may watch it.
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    /// Stable identity: the UUID of its `_fd_streams` row.
    pub id: StreamId,
    /// The unique, human-facing name.
    ///
    /// What a trigger's `channel` names, what an application's `StreamRef`
    /// names, and what the observe socket's path segment *is* — so it is a
    /// legal identifier (checked in task 2.2), and renaming one breaks those
    /// references deliberately rather than silently, exactly as renaming a
    /// trigger does.
    pub name: String,
    /// Human-readable description (§9 requires one on every metadata row; the
    /// empty string means "none given").
    pub description: String,
    /// The registered [`StreamProvider`](crate::StreamProvider) name.
    pub provider: String,
    /// The provider's settings, keyed by its
    /// [`config_spec`](crate::StreamProvider::config_spec) field names.
    ///
    /// Secrets are stored **as given**: redaction happens on the way out to a
    /// form ([`redact_attrs`](sc_types::redact_attrs)) and the untouched value
    /// is restored on save ([`merge_secrets`](sc_types::merge_secrets)), which
    /// is task 2.3.
    pub configuration: Attrs,
    /// The role floor for **observing** this stream through an application, or
    /// `None` for admin-only.
    ///
    /// `None` is the restrictive end, which is the trigger rule and it is here
    /// for the trigger reason: a flow nobody has thought about the access of is
    /// not public. A stream can carry a broker's whole topic tree.
    pub min_role: Option<Role>,
    /// Sparse per-stream values (§9): [`ATTR_ENABLED`], and per-provider
    /// bookkeeping.
    pub attributes: Attrs,
}

impl Stream {
    /// A **new** stream with a fresh id, observed by `provider`.
    pub fn new(name: impl Into<String>, provider: impl Into<String>) -> Stream {
        Stream::with_id(StreamId::new(), name, provider)
    }

    /// Reconstruct an existing stream, which already has an id — what
    /// [`load_stream`](crate::load_stream) and an update path use.
    pub fn with_id(id: StreamId, name: impl Into<String>, provider: impl Into<String>) -> Stream {
        Stream {
            id,
            name: name.into(),
            description: String::new(),
            provider: provider.into(),
            configuration: Attrs::new(),
            min_role: None,
            attributes: Attrs::new(),
        }
    }

    /// Set the description.
    pub fn description(mut self, description: impl Into<String>) -> Stream {
        self.description = description.into();
        self
    }

    /// Set one configuration value, returning `self` for chaining.
    pub fn config(mut self, key: impl Into<String>, value: impl Into<Json>) -> Stream {
        self.configuration.insert(key.into(), value.into());
        self
    }

    /// Set the role floor for observing this stream through an application.
    pub fn min_role(mut self, role: Role) -> Stream {
        self.min_role = Some(role);
        self
    }

    /// Set one attribute, returning `self` for chaining.
    pub fn attribute(mut self, key: impl Into<String>, value: impl Into<Json>) -> Stream {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Whether the supervisor subscribes to it. Absent means **enabled**: a
    /// stream is created to flow, and the attribute exists so an admin can
    /// switch one off — hanging up on the broker without losing the
    /// configuration — rather than having to delete it.
    pub fn is_enabled(&self) -> bool {
        self.attributes
            .get(ATTR_ENABLED)
            .and_then(Json::as_bool)
            .unwrap_or(true)
    }

    /// Enable or disable the stream. Enabling **removes** the key rather than
    /// storing `true`, so an untouched stream carries no residue (§9's sparse
    /// rule, as a `Trigger`'s accessor does).
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled {
            self.attributes.remove(ATTR_ENABLED);
        } else {
            self.attributes
                .insert(ATTR_ENABLED.into(), Json::Bool(false));
        }
    }

    /// Set the enabled flag, returning `self` for chaining.
    pub fn enabled(mut self, enabled: bool) -> Stream {
        self.set_enabled(enabled);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_stream_is_enabled_and_carries_no_residue_for_saying_so() {
        let stream = Stream::new("boiler", "mqtt");
        assert!(stream.is_enabled());
        assert!(stream.attributes.is_empty());

        let mut stream = stream;
        stream.set_enabled(false);
        assert!(!stream.is_enabled());
        assert_eq!(
            stream.attributes.get(ATTR_ENABLED),
            Some(&Json::Bool(false))
        );

        stream.set_enabled(true);
        assert!(stream.is_enabled());
        assert!(!stream.attributes.contains_key(ATTR_ENABLED));
    }

    #[test]
    fn a_stream_with_no_min_role_is_admin_only_rather_than_public() {
        let stream = Stream::new("boiler", "mqtt");
        assert_eq!(stream.min_role, None);
        assert_eq!(stream.min_role(Role::of(40)).min_role, Some(Role::of(40)));
    }

    #[test]
    fn the_builders_fill_in_what_a_form_would() {
        let stream = Stream::new("boiler", "mqtt")
            .description("the boiler's temperature")
            .config("topic", "house/boiler/#")
            .attribute("last_topic", "house/boiler/temp")
            .enabled(false);
        assert_eq!(stream.description, "the boiler's temperature");
        assert_eq!(stream.configuration["topic"], Json::from("house/boiler/#"));
        assert_eq!(
            stream.attributes["last_topic"],
            Json::from("house/boiler/temp")
        );
        assert!(!stream.is_enabled());
    }
}

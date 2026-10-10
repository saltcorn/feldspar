//! What a stream provider *is*, and where the elements it produces go (TODO §3,
//! task 1.4).
//!
//! A provider is code that can observe something: `mqtt`, a module's polled
//! feed. It is an extension point of exactly the shape an `Action`, an
//! `AgentTrait` and a `ModelProvider` already are — a trait object, registered
//! by name, declaring its settings as [`FormField`]s so the admin UI renders a
//! provider it has never heard of — and the three things worth reading this
//! module for are the three places it deliberately differs.
//!
//! ## `element_type` takes the configuration
//!
//! Exactly as `ModelProvider::outcome` does, and for the same reason: MQTT with
//! `payload = json` and four declared keys is a different element type from the
//! same provider with `payload = text`, and making those two providers would be
//! making four. It is fallible, because a configuration can be incoherent
//! (`json` with a key of no type) and the admin should hear about it while
//! looking at the form rather than at 3am.
//!
//! ## `subscribe` returns rather than blocks
//!
//! It hands back a [`Subscription`] whose `Drop` stops the flow — see that
//! module for why that shape, and not a `&mut self` loop, is what makes the
//! supervisor's restart path three lines.
//!
//! ## A provider says what its topics are
//!
//! [`StreamProvider::topic_spec`] — again a function of the configuration, as
//! `element_type` is — says how a stream's elements are split into **topics**
//! (TODO.md "Live updates" §2) and so how access to them is decided: one topic
//! under the stream's `min_role` ([`TopicSpec::Single`], every provider there
//! was before live updates), one per user, one per row of a table, or one per
//! subscription filter with every element checked against the subscriber. The
//! default is `Single`, so a provider that has never heard of topics — MQTT, a
//! module's polled feed — is exactly what it was.
//!
//! ## Nobody may block the flow
//!
//! [`StreamSink::deliver`] is **synchronous and infallible**. That is not an
//! oversight, it is §7 written into a type: a broker does not wait for an
//! admin's browser, and a sink that could return an error or an await point
//! would let one slow consumer push back on a provider — which for a stream
//! means either an unbounded queue or a dropped broker session. A consumer that
//! cannot keep up is *the consumer's* problem, and the sink `sc-server`
//! installs publishes onto a broadcast channel and returns.

use std::sync::Arc;

use async_trait::async_trait;
use sc_error::Result;
use sc_types::{Attrs, FormField};
use serde::{Deserialize, Serialize};

use crate::element::ElementType;
use crate::envelope::Element;
use crate::subscription::Subscription;

/// Where a delivered element goes — the first of `sc-stream`'s three seams (§2).
///
/// Declared here, implemented in `sc-server::streams` (which broadcasts it to
/// the observe sockets and fires the trigger dispatcher), and in tests by
/// something that appends to a `Vec`. That is what lets the supervisor be
/// tested without a broker, a socket or a trigger.
pub trait StreamSink: Send + Sync {
    /// Take one element. **Returns immediately and cannot fail** — see the
    /// module docs.
    fn deliver(&self, element: Element);

    /// Report a payload that could not be decoded against the stream's element
    /// type.
    ///
    /// Counted and warned at most once a minute per stream, never delivered
    /// (§11): a broker with one misbehaving publisher must not be able to fill
    /// the log or the trigger queue. The default does nothing, so a sink that
    /// only cares about elements — a test's `Vec`, an observer — need not say
    /// so.
    fn malformed(&self, reason: &str) {
        let _ = reason;
    }
}

/// How a stream's elements are split into **topics**, and so how access to
/// them is decided (TODO.md "Live updates" §2).
///
/// Declared by the provider as a function of its configuration
/// ([`StreamProvider::topic_spec`]), the way the element type is. The stream's
/// `min_role` is a floor on subscribing at all whatever this says; a topic or
/// element check comes **on top of** it and can only narrow access.
///
/// Serialised as `{"kind": "single"}`, `{"kind": "row", "table": "boards"}`
/// and so on — what the Streams list and the client generator read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TopicSpec {
    /// One topic. Access is the stream's `min_role`. Every provider there was
    /// before live updates — MQTT, a module's — is this.
    #[default]
    Single,
    /// The topic is a user id. A subscriber gets exactly their own topic and
    /// never names it; an anonymous caller is refused.
    User,
    /// The topic is the primary key of a row of `table`. Subscribing needs
    /// **read** access to that row; publishing from a client needs **update**
    /// access.
    Row {
        /// The table whose rows the topics are.
        table: String,
    },
    /// One topic per subscription filter, and **every element** is checked
    /// against the subscriber's read access to the row it carries.
    PerElementRow {
        /// The table whose rows the elements carry.
        table: String,
    },
}

impl TopicSpec {
    /// Whether a subscriber names a topic when subscribing. `Single` has one
    /// topic and `User`'s is the subscriber's own, so neither takes one;
    /// `Row` is nothing without one.
    pub fn takes_topic(&self) -> bool {
        matches!(self, TopicSpec::Row { .. })
    }
}

/// One stream provider the picker offers, and everything about it that can be
/// described without running Rust code.
///
/// `sc-model`'s `ModelProviderKind`'s twin, and for the same reason: it is what
/// crosses the module seam, and it is what the admin UI lists.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamProviderKind {
    /// The name it is registered and stored under — `mqtt`.
    pub name: String,
    /// What the picker calls it.
    pub label: String,
    /// One line for the picker.
    pub description: String,
    /// The package supplying it, or `None` for a built-in.
    pub module: Option<String>,
    /// The settings an admin fills in.
    pub config_spec: Vec<FormField>,
}

impl StreamProviderKind {
    /// A built-in provider's kind: no module, no settings.
    pub fn new(
        name: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> StreamProviderKind {
        StreamProviderKind {
            name: name.into(),
            label: label.into(),
            description: description.into(),
            module: None,
            config_spec: Vec::new(),
        }
    }

    /// The settings it takes.
    pub fn config(mut self, spec: Vec<FormField>) -> StreamProviderKind {
        self.config_spec = spec;
        self
    }

    /// The module that supplies it.
    pub fn module(mut self, module: impl Into<String>) -> StreamProviderKind {
        self.module = Some(module.into());
        self
    }

    /// Where this provider comes from, as a phrase an error message can use:
    /// "the built-in providers", or "the module `@saltcorn/rss`".
    pub fn source(&self) -> String {
        match &self.module {
            Some(module) => format!("the module `{module}`"),
            None => "the built-in providers".to_owned(),
        }
    }
}

/// Code that can observe a dataflow (§3).
///
/// Object-safe and dynamically dispatched, for the reason an `Action` is: which
/// provider a stream uses is decided at runtime from a stored name, and the set
/// is meant to grow from outside this crate.
#[async_trait]
pub trait StreamProvider: Send + Sync {
    /// The name it is registered and stored under. Stable: it is what a saved
    /// stream references.
    fn name(&self) -> &str;

    /// What the picker calls it. Defaults to the name, which is right for a
    /// provider whose name is already a word an admin knows (`mqtt`).
    fn label(&self) -> &str {
        self.name()
    }

    /// One line for the picker.
    fn description(&self) -> &str;

    /// The settings an admin fills in, as data.
    ///
    /// Rendered by the same spec-driven `<Form>` the Model form, the Trigger
    /// form and the agent trait form use, with secrets redacted by
    /// `redact_attrs` on the way out and restored by `merge_secrets` on save
    /// (task 2.3).
    fn config_spec(&self) -> Vec<FormField>;

    /// The element type **as a function of the configuration** (GOALS, §4).
    ///
    /// Fallible: a configuration can be incoherent in ways a
    /// [`FormField`] cannot express — `payload = json` with no declared keys —
    /// and the admin hears about it on save rather than on the first element.
    fn element_type(&self, config: &Attrs) -> Result<ElementType>;

    /// How this stream's elements are split into topics, for `config` (see
    /// [`TopicSpec`]).
    ///
    /// Defaults to [`TopicSpec::Single`]: one topic, under the stream's
    /// `min_role`. Only an in-process provider whose elements are about a user
    /// or a row — Saltcorn's own `internal`, `table_changes` and `document` —
    /// answers anything else, because only Saltcorn can vouch for which user or
    /// row an element is about.
    fn topic_spec(&self, config: &Attrs) -> Result<TopicSpec> {
        let _ = config;
        Ok(TopicSpec::Single)
    }

    /// The element type for `config`, **asked for** rather than answered from
    /// what is already known.
    ///
    /// The asynchronous door in front of
    /// [`element_type`](StreamProvider::element_type), and it exists for one
    /// kind of provider: a module's, whose declaration is a JavaScript function
    /// on a Deno worker and therefore a call across a seam, where
    /// `element_type` is a synchronous method the Streams form, the client
    /// generator and the Streams list all call. Such a provider answers the
    /// synchronous one from what it last resolved and fills that in here; see
    /// `PollingProvider`.
    ///
    /// Everything compiled in — MQTT, a scripted provider — computes its type
    /// from the configuration and nothing else, so the default is exactly the
    /// synchronous answer and costs nothing.
    ///
    /// Called by `validate_stream` before it validates and by the supervisor
    /// before it starts, which between them are every path on which a
    /// configuration becomes live.
    async fn resolve_element_type(&self, config: &Attrs) -> Result<ElementType> {
        self.element_type(config)
    }

    /// Check a configuration beyond what [`config_spec`](StreamProvider::config_spec)
    /// can express — the part only this provider knows: that a topic filter's
    /// wildcards are where MQTT allows them, that a port is a port.
    ///
    /// Runs where the generic check runs: on save, in front of the admin. The
    /// default asks for the element type, which is the check every provider has
    /// whether or not it has another.
    fn validate(&self, config: &Attrs) -> Result<()> {
        self.element_type(config)?.validate()
    }

    /// Everything this provider is, as the picker and the module seam see it.
    fn kind(&self) -> StreamProviderKind {
        StreamProviderKind {
            name: self.name().to_owned(),
            label: self.label().to_owned(),
            description: self.description().to_owned(),
            module: None,
            config_spec: self.config_spec(),
        }
    }

    /// Start observing. Elements go to `sink` until the returned
    /// [`Subscription`] is dropped.
    ///
    /// Returns as soon as the flow is *established* — it does not run the flow.
    /// An error here means "this could not be started", and the supervisor
    /// retries it with backoff (§6); it is emphatically not where a provider
    /// reports a bad payload, which is [`StreamSink::malformed`]'s job.
    ///
    /// `stream` is the stream's **current name**, and it is here because a
    /// provider needs it for two things a configuration cannot supply. It is
    /// what a log line has to say to be worth reading — "the connection to
    /// `boiler` ended" rather than "a connection ended" — and, for MQTT, it is
    /// the default `client_id`: a stable one, because a random id per reconnect
    /// leaves the broker holding a session per attempt (§11). It is not part of
    /// the configuration, because a rename must not restart a flow (§6) and a
    /// setting that changed does.
    async fn subscribe(
        &self,
        stream: &str,
        config: &Attrs,
        sink: Arc<dyn StreamSink>,
    ) -> Result<Subscription>;
}

/// A source of stream providers that are not compiled in — the second half of
/// §2's first seam.
///
/// Declared here and implemented in `sc-module` (Phase 9), where a module's
/// `streamproviders` export becomes a set of these.
///
/// **It hands back code, not a declaration**, and that is the one place this
/// differs from `ModelProviderHost`. A model provider's seam is a call that
/// returns a fit, so the registry can wrap a declaration in a
/// `HostProvider` and route `fit` back over it. A subscription is not a call
/// that returns, and there is no channel from a Deno worker back into the host
/// (§12) — so what a module supplies is wrapped in `sc-stream`'s own
/// `PollingProvider`, and the host is what knows to do that. Phase 9 writes
/// that wrapper; this trait is what it plugs into.
pub trait StreamProviderHost: Send + Sync {
    /// The providers this host supplies, as declarations — what the picker
    /// lists.
    fn providers(&self) -> Vec<StreamProviderKind>;

    /// Code for one of them.
    ///
    /// Fallible so that one unusable provider — a declaration this host cannot
    /// route, a kind it does not recognise — is reported by name rather than
    /// silently missing from the picker.
    fn provider(&self, kind: &StreamProviderKind) -> Result<Arc<dyn StreamProvider>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_topic_spec_is_tagged_json_and_reads_back() {
        let cases = [
            (TopicSpec::Single, json!({ "kind": "single" })),
            (TopicSpec::User, json!({ "kind": "user" })),
            (
                TopicSpec::Row {
                    table: "boards".to_owned(),
                },
                json!({ "kind": "row", "table": "boards" }),
            ),
            (
                TopicSpec::PerElementRow {
                    table: "cards".to_owned(),
                },
                json!({ "kind": "per_element_row", "table": "cards" }),
            ),
        ];
        for (spec, wire) in cases {
            assert_eq!(serde_json::to_value(&spec).unwrap(), wire);
            assert_eq!(serde_json::from_value::<TopicSpec>(wire).unwrap(), spec);
        }
        assert_eq!(TopicSpec::default(), TopicSpec::Single);
        assert!(!TopicSpec::Single.takes_topic());
        assert!(!TopicSpec::User.takes_topic());
        assert!(
            TopicSpec::Row {
                table: "boards".to_owned()
            }
            .takes_topic()
        );
    }
}

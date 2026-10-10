//! [`Element`] and [`Envelope`]: what arrives, and what everybody downstream
//! reads (TODO §4, task 1.3).
//!
//! **The envelope is a wire contract**, in the sense `Event::error`'s payload
//! already is. A trigger's `only_if` reads it (`payload.value.temperature > 30`),
//! an application's generated client is typed from it, and the admin's Observe
//! socket sends it frame by frame. So it must not change silently, and the test
//! at the bottom of this file asserts it field by field rather than round-trip:
//! a round trip passes just as happily when both halves are wrong together.
//!
//! ```json
//! { "stream": "boiler", "value": { "temperature": 31.2 },
//!   "topic": "7",                                   // only on a stream with topics
//!   "received_at": "2026-09-17T09:00:00.000Z",
//!   "source": { "topic": "house/boiler/temp", "qos": 0, "retain": false } }
//! ```
//!
//! ## Why there are two types
//!
//! A provider knows the element and, sometimes, something about where it came
//! from; it does **not** know what the admin called the stream or when this
//! server saw it. So a provider delivers an [`Element`] and the running stream
//! stamps it into an [`Envelope`]. That split is what stops a provider from
//! being able to lie about either of the two fields a consumer trusts.
//!
//! ## The fields, and what each is *not*
//!
//! - **`topic`** is the sub-channel of the stream the element was published
//!   on (TODO.md "Live updates" §2), present only for a stream whose provider
//!   declares topics ([`TopicSpec`](crate::TopicSpec) other than `Single`). It
//!   is what the live socket routes on and what decides who may receive the
//!   element. Absent — not null — for every stream there was before topics,
//!   so an envelope a trigger or a generated client already reads is
//!   unchanged. It is **not** MQTT's topic, which stays in `source`: a broker's
//!   routing key is that provider's metadata, and a live topic is Saltcorn's.
//! - **`value`** is the element itself, shaped by the stream's
//!   [`ElementType`] — an object for `Json`, a string for
//!   `Text`, base64 for `Binary`.
//! - **`source`** is the provider's own metadata, free JSON, absent when a
//!   provider has none. MQTT's topic lives here rather than beside `value`
//!   because "which topic" is a fact about *that provider*, and a formula that
//!   reads it has already accepted that it is talking to MQTT.
//! - **`received_at`** is when *this server* saw the element. It is not a claim
//!   about when it was produced: a provider that knows the producer's timestamp
//!   puts that in `source`, where it is the provider's word and reads like it.

use chrono::{DateTime, SecondsFormat, Utc};
use sc_error::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::element::{ElementType, RawPayload};

/// What a provider delivers to a [`StreamSink`](crate::StreamSink): the decoded
/// element, and whatever the provider knows about where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    /// The element, already shaped by the stream's
    /// [`ElementType`].
    pub value: Json,
    /// The provider's own metadata, or `None` when it has none.
    pub source: Option<Json>,
    /// The topic it was published on, for a stream whose provider declares
    /// topics; `None` on a `Single` stream (see the module docs).
    pub topic: Option<String>,
}

impl Element {
    /// An element with no provider metadata.
    pub fn new(value: impl Into<Json>) -> Element {
        Element {
            value: value.into(),
            source: None,
            topic: None,
        }
    }

    /// Publish it on `topic` — for a provider whose [`TopicSpec`](crate::TopicSpec)
    /// is not `Single`.
    pub fn topic(mut self, topic: impl Into<String>) -> Element {
        self.topic = Some(topic.into());
        self
    }

    /// Attach the provider's metadata — MQTT's `{topic, qos, retain}`.
    pub fn source(mut self, source: impl Into<Json>) -> Element {
        self.source = Some(source.into());
        self
    }

    /// Decode a raw payload against `ty` into an element.
    ///
    /// The path every provider takes, so that "what counts as malformed" is
    /// decided once (see [`ElementType::decode`]) rather than once per broker.
    pub fn decode(ty: &ElementType, raw: RawPayload) -> Result<Element> {
        Ok(Element::new(ty.decode(raw)?))
    }

    /// Stamp this element with the stream that produced it and the moment this
    /// server saw it.
    ///
    /// The clock is a parameter, not `Utc::now()`, for `Scheduler::tick`'s
    /// reason: a test that asserts a wire contract cannot assert a field it
    /// cannot predict.
    pub fn into_envelope(self, stream: impl Into<String>, received_at: DateTime<Utc>) -> Envelope {
        Envelope {
            stream: stream.into(),
            topic: self.topic,
            value: self.value,
            received_at,
            source: self.source,
        }
    }
}

/// One element of a stream, as everything downstream sees it (§4).
///
/// A wire contract: the trigger payload, the admin Observe socket's `element`
/// frame and an application's subscription all carry exactly this JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// The stream's name — what a trigger's channel and a socket path name.
    pub stream: String,
    /// The topic it was published on. Absent — not null — on a stream with no
    /// topics, which is every stream there was before topics existed, so their
    /// envelopes are byte-for-byte what they were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The element itself, shaped by the stream's
    /// [`ElementType`].
    pub value: Json,
    /// When *this server* saw it.
    #[serde(with = "rfc3339")]
    pub received_at: DateTime<Utc>,
    /// The provider's own metadata. Absent — not null — when there is none, so
    /// a consumer's `if (envelope.source)` reads the same in every language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Json>,
}

impl Envelope {
    /// The envelope as JSON: the payload of a `stream` event, and the body of
    /// an `element` frame.
    ///
    /// Infallible, unlike a general `to_value`: every field is already JSON or
    /// a string, so there is nothing here that can fail to serialise and no
    /// reason to make every caller handle an error that cannot happen.
    pub fn to_json(&self) -> Json {
        let mut out = json!({
            "stream": self.stream,
            "value": self.value,
            "received_at": self.received_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        if let Some(object) = out.as_object_mut() {
            if let Some(topic) = &self.topic {
                object.insert("topic".to_owned(), Json::String(topic.clone()));
            }
            if let Some(source) = &self.source {
                object.insert("source".to_owned(), source.clone());
            }
        }
        out
    }
}

/// `received_at` on the wire: RFC 3339, UTC, milliseconds, `Z`.
///
/// Fixed width on purpose. chrono's own encoding prints as many fractional
/// digits as the value happens to need, so a consumer would see
/// `…T09:00:00Z` from one element and `…T09:00:00.123456789Z` from the next —
/// which every hand-written parser at the far end gets wrong exactly once.
///
/// `pub(crate)` rather than private because the same fixed width is wanted
/// wherever else this crate puts an instant on a wire — a running stream's
/// `since` and `last_element_at` are read by the same consumers, from the same
/// screen, and two encodings of a timestamp in one payload is exactly the trap
/// this module exists to close.
pub(crate) mod rfc3339 {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(at: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&at.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let text = String::deserialize(d)?;
        DateTime::parse_from_rfc3339(&text)
            .map(|at| at.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }

    /// The same encoding for an instant that may not have happened — a stream
    /// that has never seen an element. `null`, not the epoch, because "never"
    /// and "1970" are different answers and only one of them is true.
    pub mod option {
        use chrono::{DateTime, Utc};
        use serde::{Deserialize, Deserializer, Serializer};

        pub fn serialize<S: Serializer>(
            at: &Option<DateTime<Utc>>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match at {
                Some(at) => super::serialize(at, s),
                None => s.serialize_none(),
            }
        }

        #[allow(dead_code)]
        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<DateTime<Utc>>, D::Error> {
            let text = Option::<String>::deserialize(d)?;
            match text {
                Some(text) => DateTime::parse_from_rfc3339(&text)
                    .map(|at| Some(at.with_timezone(&Utc)))
                    .map_err(serde::de::Error::custom),
                None => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::ElementField;
    use sc_types::BasicType;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn the_envelope_is_the_json_the_design_writes_down() {
        let ty = ElementType::json([ElementField::new("temperature", BasicType::Float).required()]);
        let envelope = Element::decode(&ty, RawPayload::bytes(br#"{"temperature":31.2}"#.to_vec()))
            .unwrap()
            .source(json!({ "topic": "house/boiler/temp", "qos": 0, "retain": false }))
            .into_envelope("boiler", at("2026-09-17T09:00:00Z"));

        // Field by field, against §4's example. This is a wire contract: a
        // trigger's `only_if`, an application's generated client and the
        // Observe screen all read these four names.
        let wire = envelope.to_json();
        assert_eq!(wire["stream"], json!("boiler"));
        assert_eq!(wire["value"], json!({ "temperature": 31.2 }));
        assert_eq!(wire["received_at"], json!("2026-09-17T09:00:00.000Z"));
        assert_eq!(
            wire["source"],
            json!({ "topic": "house/boiler/temp", "qos": 0, "retain": false })
        );
        assert_eq!(wire.as_object().unwrap().len(), 4);
        // And serde's rendering is the same JSON, so the socket frame and the
        // trigger payload cannot drift apart.
        assert_eq!(serde_json::to_value(&envelope).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<Envelope>(wire).unwrap(),
            envelope,
            "the envelope reads back as itself"
        );
    }

    #[test]
    fn a_provider_with_no_metadata_leaves_source_out_rather_than_null() {
        let envelope = Element::decode(&ElementType::text(), RawPayload::bytes(b"hi".to_vec()))
            .unwrap()
            .into_envelope("chat", at("2026-09-17T09:00:00Z"));
        let wire = envelope.to_json();
        assert_eq!(wire["value"], json!("hi"));
        assert!(
            !wire.as_object().unwrap().contains_key("source"),
            "absent, not null: {wire}"
        );
        assert_eq!(serde_json::to_value(&envelope).unwrap(), wire);
    }

    #[test]
    fn a_topic_is_carried_when_there_is_one_and_absent_when_there_is_not() {
        let envelope = Element::new(json!({ "progress": 0.5 }))
            .topic("7")
            .into_envelope("job_status", at("2026-09-17T09:00:00Z"));
        let wire = envelope.to_json();
        assert_eq!(wire["topic"], json!("7"));
        assert_eq!(wire.as_object().unwrap().len(), 4, "{wire}");
        assert_eq!(serde_json::to_value(&envelope).unwrap(), wire);
        assert_eq!(serde_json::from_value::<Envelope>(wire).unwrap(), envelope);

        // A `Single` stream's envelope has no `topic` key at all — the wire
        // contract every consumer written before topics reads.
        let single = Element::new(json!(1)).into_envelope("boiler", at("2026-09-17T09:00:00Z"));
        assert!(!single.to_json().as_object().unwrap().contains_key("topic"));
    }

    #[test]
    fn received_at_is_fixed_width_milliseconds_in_utc() {
        let envelope =
            Element::new(json!(1)).into_envelope("s", at("2026-09-17T11:00:00.123456789+02:00"));
        assert_eq!(
            envelope.to_json()["received_at"],
            json!("2026-09-17T09:00:00.123Z")
        );
    }

    #[test]
    fn a_binary_element_carries_base64_in_value() {
        let envelope = Element::decode(
            &ElementType::Binary,
            RawPayload::bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        )
        .unwrap()
        .into_envelope("bytes", at("2026-09-17T09:00:00Z"));
        assert_eq!(envelope.to_json()["value"], json!("3q2+7w=="));
    }
}

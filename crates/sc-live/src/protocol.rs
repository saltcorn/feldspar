//! The live socket's frames (TODO.md "Live updates" §4): JSON text, tagged on
//! `type`, in both directions.
//!
//! ```text
//! client → server
//!   {"type":"subscribe",   "sub":"s1", "stream":"boiler", "topic"?: "7", "filter"?: {...}}
//!   {"type":"unsubscribe", "sub":"s1"}
//!   {"type":"publish",     "sub":"s2", "value": {...}}
//!   {"type":"presence",    "sub":"s2", "state": {...}}
//!   {"type":"doc_update",  "sub":"s3", "update": "<base64>"}
//!   {"type":"ping"}
//!
//! server → client
//!   {"type":"ready",   "sub":"s1", "stream":"boiler", "element_type":…, "replayed":n,
//!                      "can_publish":false, "status":…, "counters":…}
//!   {"type":"element", "sub":"s1", "envelope":{…}}
//!   {"type":"lagged",  "sub":"s1", "dropped":n}
//!   {"type":"status",  "sub":"s1", "status":…, "counters":…}
//!   {"type":"revoked", "sub":"s1"}
//!   {"type":"error",   "sub"?:"s1", "code":"unavailable", "message":"…"}
//!   {"type":"pong"}
//! ```
//!
//! `sub` is the **client's** name for a subscription: it picks it, and every
//! frame about that subscription carries it back. One socket carries any
//! number of them, which is the point of the socket (§4).
//!
//! ## A frame the server does not understand is an error frame, not a hang-up
//!
//! [`ClientFrame::parse`] turns anything that is not a frame it knows —
//! malformed JSON, a missing `type`, a `type` from a newer client, a known
//! `type` with a field of the wrong shape — into a [`FrameError`] the socket
//! answers with `{"type":"error","code":"invalid"}`, carrying the frame's `sub`
//! back when it had one. Closing the connection instead would take every other
//! subscription on it down with the one bad frame, and a client a version
//! ahead of the server would be unable to use the server at all.
//!
//! The frames for publishing, presence and documents are declared here now,
//! because the protocol is one contract and a client written against it should
//! not have to learn it in instalments. What the server does with them arrives
//! with the milestones that give them meaning (L4, L5).

use sc_stream::Envelope;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value as Json;

/// A frame a client sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Start a subscription named `sub` to `stream`.
    Subscribe {
        /// The client's name for the subscription.
        sub: String,
        /// The stream's name, as the application exposes it.
        stream: String,
        /// The topic, for a stream whose topics are rows (`TopicSpec::Row`).
        /// A string or a number on the wire — a row's key is often an
        /// integer, and `"7"` and `7` name the same topic.
        #[serde(
            default,
            deserialize_with = "topic_name",
            skip_serializing_if = "Option::is_none"
        )]
        topic: Option<String>,
        /// A filter in the REST read's vocabulary, for a stream of row
        /// changes (L3).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Json>,
    },
    /// End the subscription named `sub`.
    Unsubscribe {
        /// The client's name for the subscription.
        sub: String,
    },
    /// Publish `value` on the subscription's topic (L4: an `internal` stream
    /// with `client_publish` on).
    Publish {
        /// The subscription whose stream and topic it is published to.
        sub: String,
        /// The element.
        value: Json,
    },
    /// Set this connection's presence state on the subscription's topic (L4).
    Presence {
        /// The subscription whose topic it is.
        sub: String,
        /// The state — a cursor, a selection — at most 2 KB.
        state: Json,
    },
    /// A document update, base64 (L5).
    DocUpdate {
        /// The subscription whose document it is.
        sub: String,
        /// The Yjs update, base64.
        update: String,
    },
    /// Are you there? Answered with `pong`.
    Ping,
}

/// Every `type` a client may send — what an unknown one is told it is not.
const CLIENT_TYPES: [&str; 6] = [
    "subscribe",
    "unsubscribe",
    "publish",
    "presence",
    "doc_update",
    "ping",
];

/// A topic given as a string or as a number, read as a string: `"7"` and `7`
/// are the same topic.
fn topic_name<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<Json>::deserialize(d)? {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(text)) => Ok(Some(text)),
        Some(Json::Number(number)) => Ok(Some(number.to_string())),
        Some(other) => Err(serde::de::Error::custom(format!(
            "a topic is a string or a number, not {other}"
        ))),
    }
}

impl ClientFrame {
    /// Read one text frame.
    ///
    /// Never fails with anything but a [`FrameError`] the socket can answer
    /// with — see the module docs.
    pub fn parse(text: &str) -> Result<ClientFrame, FrameError> {
        let value: Json = serde_json::from_str(text)
            .map_err(|e| FrameError::invalid(None, format!("a frame is a JSON object: {e}")))?;
        let Json::Object(object) = &value else {
            return Err(FrameError::invalid(None, "a frame is a JSON object"));
        };
        // The subscription the frame is about, if it names one, so the error
        // can say which of the socket's subscriptions it concerns.
        let sub = object.get("sub").and_then(Json::as_str).map(str::to_owned);
        let Some(kind) = object.get("type").and_then(Json::as_str).map(str::to_owned) else {
            return Err(FrameError::invalid(sub, "a frame needs a string `type`"));
        };
        if !CLIENT_TYPES.contains(&kind.as_str()) {
            return Err(FrameError::invalid(
                sub,
                format!(
                    "`{kind}` is not a frame this server knows; it knows {}",
                    CLIENT_TYPES.join(", ")
                ),
            ));
        }
        serde_json::from_value(value)
            .map_err(|e| FrameError::invalid(sub, format!("a `{kind}` frame: {e}")))
    }

    /// The subscription this frame is about, if it is about one.
    pub fn sub(&self) -> Option<&str> {
        match self {
            ClientFrame::Subscribe { sub, .. }
            | ClientFrame::Unsubscribe { sub }
            | ClientFrame::Publish { sub, .. }
            | ClientFrame::Presence { sub, .. }
            | ClientFrame::DocUpdate { sub, .. } => Some(sub),
            ClientFrame::Ping => None,
        }
    }
}

/// Why a subscription, a frame or a connection was refused — the `code` of an
/// `error` frame.
///
/// One code per thing a client can **act** on differently. In particular there
/// is one [`Unavailable`](ErrorCode::Unavailable) for an unknown stream, an
/// unexposed one, one above the caller's role, a missing row and a forbidden
/// row: §3 rule 4, denial looks like absence, so subscribing is never a way to
/// learn that something exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// There is nothing here this caller may subscribe to or publish on.
    Unavailable,
    /// The frame is not one the server can act on: malformed, an unknown
    /// `type`, a `sub` already in use or never opened, a topic for a stream
    /// that has none.
    Invalid,
    /// Too many publishes or presence updates on this connection (L4).
    RateLimited,
    /// This connection already holds as many subscriptions as it may.
    TooManySubscriptions,
    /// This user already holds as many connections to this application as
    /// they may. The connection is closed after this frame.
    TooManyConnections,
    /// The frame was larger than the server accepts. It was not read.
    TooLarge,
    /// Nothing arrived on the connection for too long — not even a pong. The
    /// connection is closed after this frame.
    Idle,
    /// The connection's session ended: a sign-out, a forced sign-out, an
    /// expiry. The connection is closed after this frame.
    SignedOut,
}

impl ErrorCode {
    /// The code as it is spelled on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::Invalid => "invalid",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::TooManySubscriptions => "too_many_subscriptions",
            ErrorCode::TooManyConnections => "too_many_connections",
            ErrorCode::TooLarge => "too_large",
            ErrorCode::Idle => "idle",
            ErrorCode::SignedOut => "signed_out",
        }
    }
}

/// A frame the server answers with an `error` frame rather than acting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    /// The subscription it concerns, when the frame named one.
    pub sub: Option<String>,
    /// What kind of refusal it is.
    pub code: ErrorCode,
    /// A sentence for the developer reading the frames.
    pub message: String,
}

impl FrameError {
    /// A refusal with `code`.
    pub fn new(sub: Option<String>, code: ErrorCode, message: impl Into<String>) -> FrameError {
        FrameError {
            sub,
            code,
            message: message.into(),
        }
    }

    /// An [`Invalid`](ErrorCode::Invalid) frame.
    pub fn invalid(sub: Option<String>, message: impl Into<String>) -> FrameError {
        FrameError::new(sub, ErrorCode::Invalid, message)
    }

    /// The one [`Unavailable`](ErrorCode::Unavailable) answer, worded the same
    /// whatever the reason was — the wording is part of not telling.
    pub fn unavailable(sub: &str, stream: &str) -> FrameError {
        FrameError::new(
            Some(sub.to_owned()),
            ErrorCode::Unavailable,
            format!("there is no stream `{stream}` you can subscribe to here"),
        )
    }

    /// The `error` frame that reports it.
    pub fn frame(&self) -> ServerFrame {
        ServerFrame::Error {
            sub: self.sub.clone(),
            code: self.code,
            message: self.message.clone(),
        }
    }
}

/// A frame the server sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// The subscription is live. Elements follow, the first `replayed` of
    /// them history. A second `ready` on the same `sub` means the stream was
    /// restarted (its configuration changed) and the elements that follow are
    /// the new flow's.
    Ready {
        /// The subscription.
        sub: String,
        /// The stream's name.
        stream: String,
        /// What an element's `value` is, or `null` while the stream is not
        /// running here.
        element_type: Json,
        /// How many of the elements that follow are history.
        replayed: usize,
        /// Whether this caller may publish on this subscription (L4).
        can_publish: bool,
        /// The stream's status, as the admin's Observe screen shows it.
        status: Json,
        /// The stream's counters.
        counters: Json,
    },
    /// One element, in the §14.3 envelope.
    Element {
        /// The subscription it arrived on.
        sub: String,
        /// The envelope.
        envelope: Envelope,
    },
    /// This subscription fell behind and lost `dropped` elements. Told, never
    /// hidden: a gap the client was told about is a gap it can resync over.
    Lagged {
        /// The subscription.
        sub: String,
        /// How many elements it lost.
        dropped: u64,
    },
    /// The stream connected, failed or stopped.
    Status {
        /// The subscription.
        sub: String,
        /// The stream's status.
        status: Json,
        /// The stream's counters.
        counters: Json,
    },
    /// Access was re-checked and refused: the subscription is gone. Worded as
    /// nothing more, for the reason an `unavailable` is.
    Revoked {
        /// The subscription that ended.
        sub: String,
    },
    /// A refusal. With a `sub`, it concerns that subscription only; without
    /// one, the frame it answers named none, or it concerns the connection.
    Error {
        /// The subscription it concerns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub: Option<String>,
        /// What kind of refusal.
        code: ErrorCode,
        /// A sentence.
        message: String,
    },
    /// The answer to `ping`.
    Pong,
}

impl ServerFrame {
    /// The frame as the text it is sent as.
    ///
    /// Infallible in practice — every field is already JSON or a string — and
    /// a frame that somehow cannot be written becomes an `error` frame saying
    /// so rather than a panic on a socket's task.
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| {
            format!(
                r#"{{"type":"error","code":"invalid","message":{}}}"#,
                Json::String(format!("the server could not write a frame: {e}"))
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(value: Json) -> Result<ClientFrame, FrameError> {
        ClientFrame::parse(&value.to_string())
    }

    #[test]
    fn every_client_frame_reads_and_writes_back_as_itself() {
        let frames = [
            ClientFrame::Subscribe {
                sub: "s1".to_owned(),
                stream: "boiler".to_owned(),
                topic: None,
                filter: None,
            },
            ClientFrame::Subscribe {
                sub: "s2".to_owned(),
                stream: "cards".to_owned(),
                topic: Some("7".to_owned()),
                filter: Some(json!({ "board": "eq.7" })),
            },
            ClientFrame::Unsubscribe {
                sub: "s1".to_owned(),
            },
            ClientFrame::Publish {
                sub: "s2".to_owned(),
                value: json!({ "dragging": 3 }),
            },
            ClientFrame::Presence {
                sub: "s2".to_owned(),
                state: json!({ "cursor": 12 }),
            },
            ClientFrame::DocUpdate {
                sub: "s3".to_owned(),
                update: "AAEC".to_owned(),
            },
            ClientFrame::Ping,
        ];
        for frame in frames {
            let text = serde_json::to_string(&frame).unwrap();
            assert_eq!(ClientFrame::parse(&text).unwrap(), frame, "{text}");
        }
    }

    #[test]
    fn the_wire_spelling_is_the_one_the_design_writes_down() {
        assert_eq!(
            frame(json!({ "type": "subscribe", "sub": "s1", "stream": "boiler" })).unwrap(),
            ClientFrame::Subscribe {
                sub: "s1".to_owned(),
                stream: "boiler".to_owned(),
                topic: None,
                filter: None,
            }
        );
        assert_eq!(frame(json!({ "type": "ping" })).unwrap(), ClientFrame::Ping);
        // A row's key is often an integer; `7` and `"7"` are one topic.
        let ClientFrame::Subscribe { topic, .. } =
            frame(json!({ "type": "subscribe", "sub": "s", "stream": "b", "topic": 7 })).unwrap()
        else {
            panic!("a subscribe frame");
        };
        assert_eq!(topic.as_deref(), Some("7"));
    }

    #[test]
    fn an_unknown_frame_type_is_an_invalid_error_and_not_a_hang_up() {
        let error = frame(json!({ "type": "teleport", "sub": "s9" })).unwrap_err();
        assert_eq!(error.code, ErrorCode::Invalid);
        assert_eq!(
            error.sub.as_deref(),
            Some("s9"),
            "the frame's sub comes back"
        );
        assert!(error.message.contains("teleport"), "{}", error.message);
        assert!(error.message.contains("subscribe"), "{}", error.message);

        // And the error is itself a frame the client can read.
        let wire: Json = serde_json::from_str(&error.frame().to_text()).unwrap();
        assert_eq!(
            wire,
            json!({ "type": "error", "sub": "s9", "code": "invalid", "message": error.message })
        );
    }

    #[test]
    fn anything_else_that_is_not_a_frame_is_invalid_too() {
        for text in [
            "not json",
            "[1, 2]",
            r#"{"sub": "s1"}"#,
            r#"{"type": 3}"#,
            // A known type with a field of the wrong shape.
            r#"{"type": "subscribe", "sub": "s1"}"#,
            r#"{"type": "subscribe", "sub": "s1", "stream": "b", "topic": {"x": 1}}"#,
            r#"{"type": "unsubscribe"}"#,
        ] {
            let error = ClientFrame::parse(text).unwrap_err();
            assert_eq!(error.code, ErrorCode::Invalid, "{text}: {}", error.message);
        }
    }

    #[test]
    fn server_frames_are_tagged_and_read_back() {
        let envelope: Envelope = serde_json::from_value(json!({
            "stream": "boiler",
            "value": { "temperature": 31.2 },
            "received_at": "2026-09-17T09:00:00.000Z",
        }))
        .unwrap();
        let frames = [
            (
                ServerFrame::Ready {
                    sub: "s1".to_owned(),
                    stream: "boiler".to_owned(),
                    element_type: json!({ "kind": "text" }),
                    replayed: 2,
                    can_publish: false,
                    status: json!({ "status": "running" }),
                    counters: json!({}),
                },
                "ready",
            ),
            (
                ServerFrame::Element {
                    sub: "s1".to_owned(),
                    envelope,
                },
                "element",
            ),
            (
                ServerFrame::Lagged {
                    sub: "s1".to_owned(),
                    dropped: 4,
                },
                "lagged",
            ),
            (
                ServerFrame::Status {
                    sub: "s1".to_owned(),
                    status: json!({ "status": "stopped" }),
                    counters: json!({}),
                },
                "status",
            ),
            (
                ServerFrame::Revoked {
                    sub: "s1".to_owned(),
                },
                "revoked",
            ),
            (FrameError::unavailable("s1", "boiler").frame(), "error"),
            (ServerFrame::Pong, "pong"),
        ];
        for (frame, kind) in frames {
            let text = frame.to_text();
            let wire: Json = serde_json::from_str(&text).unwrap();
            assert_eq!(wire["type"], json!(kind), "{text}");
            assert_eq!(serde_json::from_str::<ServerFrame>(&text).unwrap(), frame);
        }
    }

    #[test]
    fn an_element_frame_carries_the_envelope_unchanged() {
        let envelope: Envelope = serde_json::from_value(json!({
            "stream": "boiler",
            "value": { "temperature": 31.2 },
            "received_at": "2026-09-17T09:00:00.000Z",
            "source": { "topic": "house/boiler" },
        }))
        .unwrap();
        let wire: Json = serde_json::from_str(
            &ServerFrame::Element {
                sub: "s1".to_owned(),
                envelope: envelope.clone(),
            }
            .to_text(),
        )
        .unwrap();
        assert_eq!(wire["envelope"], envelope.to_json());
    }

    #[test]
    fn denial_is_worded_the_same_whatever_was_denied() {
        // The message names only what the caller already said; nothing about
        // why distinguishes a missing stream from a forbidden one.
        let missing = FrameError::unavailable("s1", "boiler");
        let forbidden = FrameError::unavailable("s1", "boiler");
        assert_eq!(missing, forbidden);
        assert_eq!(missing.code.as_str(), "unavailable");
    }
}

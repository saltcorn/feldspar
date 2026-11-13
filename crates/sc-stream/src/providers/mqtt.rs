//! The built-in MQTT stream provider (TODO §11, tasks 4.2–4.4).
//!
//! GOALS asks for one built-in stream provider and names it: MQTT. It is the
//! protocol the sensor on the wall already speaks — a broker, a topic filter, a
//! payload of whatever the publisher felt like sending — and it is the reason
//! [`ElementType`] takes the configuration rather than being a property of the
//! provider: the same broker and the same topic are a stream of objects with
//! four declared keys, a stream of text or a stream of bytes depending on one
//! setting the admin picks.
//!
//! ## What is this provider's, and what is the supervisor's
//!
//! **Connecting is this provider's. Reconnecting is not.** [`subscribe`] returns
//! only once the broker has acknowledged the subscription, so a wrong host, a
//! refused password or a filter the broker rejects is an `Err` the admin can
//! read in the stream's status — and once the flow is running, *anything* that
//! ends it (a dropped socket, a `Disconnect` from the broker, a TLS error) ends
//! the subscription's task. The supervisor then sees a subscription that has
//! ended and restarts it with capped exponential backoff (§6).
//!
//! That is a deliberate choice against rumqttc's own reconnection, which its
//! event loop will do on the next `poll` if you let it. Two reasons. Retrying
//! properly — backoff, a cap, an attempt count, a status an admin can see — is
//! written once in the supervisor for every provider, and a provider that
//! reconnected quietly underneath it would make "this stream is running" a
//! claim nobody had checked. And with `clean_session` the subscription itself
//! does not survive a reconnect: a client that comes back without re-sending
//! its `SUBSCRIBE` is connected, healthy and receiving nothing, which is the
//! worst of the available outcomes.
//!
//! ## A payload that does not match the declaration is counted, not delivered
//!
//! One misbehaving publisher on a wildcard filter must not be able to fill the
//! log or the trigger queue (§11), and on a topic like `house/#` it is not even
//! misbehaviour — a filter that matches four sensors and one heartbeat string
//! is an ordinary thing to write. So a payload that will not decode against the
//! element type is reported to [`StreamSink::malformed`] (which counts it, and
//! the count is on the Streams list) and **warned at most once a minute per
//! stream**, with the number suppressed since the last warning, so the log says
//! "and 40 more" rather than saying it 40 times. See [`MalformedThrottle`].
//!
//! ## One TLS stack, and no panic on the way to it
//!
//! `mqtts://` is the same **aws-lc-rs** rustls the HTTPS listener and `reqwest`
//! use (§16), installed as the process-wide provider here as well as in
//! `sc-server` because a stream can be the first thing in a process to want
//! TLS. The root store is built here rather than taken from rumqttc's
//! `TlsConfiguration::default()`, which panics when the platform certificates
//! cannot be read — and a panic inside the supervisor's task would take every
//! other stream's supervision with it, to report a configuration error.
//!
//! [`subscribe`]: StreamProvider::subscribe

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, MqttOptions, Packet, QoS,
    SubscribeReasonCode, TlsConfiguration, Transport,
};
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::element::{ElementField, ElementType, RawPayload, UTF8};
use crate::envelope::Element;
use crate::provider::{StreamProvider, StreamSink};
use crate::subscription::Subscription;

/// The name this provider is registered and stored under.
pub const MQTT: &str = "mqtt";

/// The broker's hostname or address.
pub const CFG_HOST: &str = "host";
/// The broker's port — 1883 plain, 8883 over TLS.
pub const CFG_PORT: &str = "port";
/// Whether to connect over TLS.
pub const CFG_USE_TLS: &str = "use_tls";
/// The MQTT client id, or blank for a stable `feldspar-{stream name}`.
pub const CFG_CLIENT_ID: &str = "client_id";
/// The username, when the broker wants one.
pub const CFG_USERNAME: &str = "username";
/// The password. A [`secret`](FormField::secret).
pub const CFG_PASSWORD: &str = "password";
/// The topic filter to subscribe to; wildcards allowed.
pub const CFG_TOPIC: &str = "topic";
/// The quality of service to subscribe at: 0, 1 or 2.
pub const CFG_QOS: &str = "qos";
/// Whether to start a clean session rather than resume the broker's.
pub const CFG_CLEAN_SESSION: &str = "clean_session";
/// What a payload *is*: `json`, `text` or `binary`. What
/// [`element_type`](StreamProvider::element_type) reads.
pub const CFG_PAYLOAD: &str = "payload";
/// The declared keys of a `json` payload — the repeating group.
pub const CFG_KEYS: &str = "keys";
/// The encoding of a `text` payload.
pub const CFG_ENCODING: &str = "encoding";

/// `payload = json`: an object with declared keys.
pub const PAYLOAD_JSON: &str = "json";
/// `payload = text`: characters in a named encoding.
pub const PAYLOAD_TEXT: &str = "text";
/// `payload = binary`: bytes, base64 in the envelope.
pub const PAYLOAD_BINARY: &str = "binary";

/// The default port for a plain connection, and the form's default.
pub const PORT_PLAIN: i64 = 1883;
/// The port a broker listening for TLS conventionally uses — what the label
/// tells the admin to type when they tick `use_tls`.
pub const PORT_TLS: i64 = 8883;

/// How long [`subscribe`](StreamProvider::subscribe) waits for the broker to
/// acknowledge the subscription before calling it a failure.
///
/// Bounded because the supervisor `await`s it: a host that accepts a TCP
/// connection and then says nothing — a firewall, a broker mid-restart, the
/// wrong port entirely — must not hold the supervisor's task for ever. Ten
/// seconds is long enough for a TLS handshake over a slow link and short enough
/// that the backoff, which is the thing that is *meant* to do the waiting, gets
/// to do it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The keep-alive the client asks the broker for.
///
/// It is what makes a connection that has silently gone away *look* gone away:
/// without it a stream behind a NAT that dropped the mapping stays "running"
/// and delivers nothing until something else notices.
const KEEP_ALIVE: Duration = Duration::from_secs(30);

/// The capacity of rumqttc's request channel. Small on purpose: the only
/// requests this provider makes are one `SUBSCRIBE` and the acknowledgements
/// the event loop generates for itself.
const REQUEST_CAPACITY: usize = 10;

/// At most one malformed-payload warning per stream per minute (§11).
const WARN_EVERY: Duration = Duration::from_secs(60);

/// The built-in MQTT provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mqtt;

#[async_trait]
impl StreamProvider for Mqtt {
    fn name(&self) -> &str {
        MQTT
    }

    fn label(&self) -> &str {
        // Its own name is the word the admin already knows, and expanding the
        // acronym in a picker helps nobody who has an MQTT broker.
        "MQTT"
    }

    fn description(&self) -> &str {
        "Subscribe to a topic filter on an MQTT broker"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_HOST, BasicType::Text)
                .label("Broker host")
                .required(),
            FormField::new(CFG_PORT, BasicType::Int)
                .label("Port (1883 plain, 8883 over TLS)")
                .default_value(PORT_PLAIN),
            FormField::new(CFG_USE_TLS, BasicType::Bool)
                .label("Connect over TLS")
                // Off by default, and the port is not moved for you: a broker
                // that speaks TLS on 1883 exists, and silently rewriting a
                // setting the admin typed is worse than a refused connection
                // they can read.
                .default_value(false),
            FormField::new(CFG_CLIENT_ID, BasicType::Text)
                .label("Client id (blank for feldspar-{stream name})"),
            FormField::new(CFG_USERNAME, BasicType::Text).label("Username"),
            FormField::new(CFG_PASSWORD, BasicType::Text)
                .label("Password")
                // Redacted wherever the row is serialised and restored on a
                // save that did not retype it (task 2.3).
                .secret(),
            FormField::new(CFG_TOPIC, BasicType::Text)
                .label("Topic filter (`+` for one level, `#` for the rest)")
                .required(),
            FormField::new(CFG_QOS, BasicType::Int)
                .label("Quality of service")
                .options([0, 1, 2])
                .default_value(0),
            FormField::new(CFG_CLEAN_SESSION, BasicType::Bool)
                .label("Start a clean session")
                // True by default, because the supervisor reconnects with the
                // same client id and a *resumed* session would hand back
                // everything published while the stream was down — which for a
                // sensor is a burst of readings all stamped with the moment the
                // connection came back (§4: `received_at` is when this server
                // saw it). A stream is a flow; replay is out of scope.
                .default_value(true),
            FormField::new(CFG_PAYLOAD, BasicType::Text)
                .label("Payload")
                .options([PAYLOAD_JSON, PAYLOAD_TEXT, PAYLOAD_BINARY])
                .default_value(PAYLOAD_JSON)
                .required(),
            // The repeating group §11 asks for, as a JSON value: a list of
            // `{ name, type, required }`. `FormField` has no repeating-group
            // kind — the vocabulary is one control per setting — and a
            // `Json` field with an array default is what every other
            // list-shaped setting in the tree is (an agent trait's allowed
            // fields, a coding trait's checks). The Streams form renders it as
            // a key table (task 7.2); the declaration stays inert data.
            FormField::new(CFG_KEYS, BasicType::Json)
                .label("Declared keys of a json payload")
                .default_value(Json::Array(Vec::new())),
            FormField::new(CFG_ENCODING, BasicType::Text)
                .label("Encoding of a text payload")
                .default_value(UTF8),
        ]
    }

    fn element_type(&self, config: &Attrs) -> Result<ElementType> {
        match payload_kind(config)? {
            PAYLOAD_JSON => Ok(ElementType::Json {
                keys: declared_keys(config)?,
            }),
            PAYLOAD_TEXT => Ok(ElementType::Text {
                encoding: encoding(config),
            }),
            _ => Ok(ElementType::Binary),
        }
    }

    fn validate(&self, config: &Attrs) -> Result<()> {
        // The element type first: it is the one every provider has, and for
        // this one it carries the `payload = json` with no declared keys
        // refusal, which is the mistake an admin actually makes.
        self.element_type(config)?.validate()?;
        // Then the three settings only MQTT can judge. Read in the order the
        // form lists them, so an admin fixing them works down the page.
        let _ = host(config)?;
        let _ = port(config)?;
        check_topic_filter(&topic(config)?)?;
        let _ = qos(config)?;
        Ok(())
    }

    async fn subscribe(
        &self,
        stream: &str,
        config: &Attrs,
        sink: Arc<dyn StreamSink>,
    ) -> Result<Subscription> {
        let element_type = self.element_type(config)?;
        element_type.validate()?;
        let settings = Settings::read(stream, config)?;
        let mut publishes = Publishes {
            stream: stream.to_owned(),
            element_type,
            sink,
            throttle: MalformedThrottle::default(),
        };

        let (client, mut eventloop) = AsyncClient::new(settings.options()?, REQUEST_CAPACITY);
        client
            .subscribe(settings.topic.clone(), settings.qos)
            .await
            .map_err(|e| {
                Error::invalid(format!(
                    "the topic filter `{}` was refused before it was sent: {e}",
                    settings.topic
                ))
            })?;

        // Poll until the broker has acknowledged the subscription, so that
        // "this stream is running" means the broker agreed to send us
        // something. Anything that arrives in the meantime — a retained
        // message can beat the `SUBACK` in practice — is delivered rather than
        // dropped on the floor.
        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        loop {
            let event = tokio::time::timeout_at(deadline, eventloop.poll())
                .await
                .map_err(|_| {
                    Error::msg(format!(
                        "the broker at {} did not acknowledge a subscription to `{}` within {}s",
                        settings.address(),
                        settings.topic,
                        CONNECT_TIMEOUT.as_secs()
                    ))
                })?
                .map_err(|e| settings.connection_failed(e))?;
            match event {
                Event::Incoming(Packet::SubAck(ack)) => {
                    if ack
                        .return_codes
                        .iter()
                        .any(|code| matches!(code, SubscribeReasonCode::Failure))
                    {
                        return Err(Error::invalid(format!(
                            "the broker at {} refused a subscription to `{}`; a broker refuses a \
                             filter it will not serve, usually because this user is not \
                             authorised for that topic",
                            settings.address(),
                            settings.topic
                        )));
                    }
                    break;
                }
                other => {
                    // Anything else is handled as it will be once we are
                    // running — except a disconnect, which before the
                    // acknowledgement is a failure to start rather than a
                    // connection to restart, and says so with the broker's
                    // name instead of waiting out the timeout.
                    if !publishes.handle(other) {
                        return Err(Error::msg(format!(
                            "the broker at {} disconnected before acknowledging a subscription                              to `{}`",
                            settings.address(),
                            settings.topic
                        )));
                    }
                }
            }
        }

        let address = settings.address();
        Ok(Subscription::spawn(move |mut stop| async move {
            // The client is what owns the request channel the event loop reads;
            // dropping it here would end the loop with `RequestsDone` on the
            // first poll. It is also how a future `unsubscribe` would be sent.
            let _client = client;
            loop {
                tokio::select! {
                    _ = stop.stopped() => break,
                    event = eventloop.poll() => match event {
                        Ok(event) => {
                            if !publishes.handle(event) {
                                eprintln!(
                                    "feldspar: stream `{}`: the broker at {address} sent a \
                                     disconnect; reconnecting",
                                    publishes.stream
                                );
                                break;
                            }
                        }
                        Err(e) => {
                            // Ending the task is how this provider reports a
                            // connection that is over: the supervisor sees a
                            // subscription that has ended and restarts it with
                            // backoff, re-sending the `SUBSCRIBE` that a
                            // rumqttc-internal reconnect would have lost.
                            eprintln!(
                                "feldspar: stream `{}`: the MQTT connection to {address} ended: \
                                 {e}",
                                publishes.stream
                            );
                            break;
                        }
                    }
                }
            }
        }))
    }
}

/// The settings of one connection, read and checked once (§11).
///
/// A struct rather than a bag of lookups at the point of use, because
/// `subscribe` has to be able to fail *before* it opens a socket: everything
/// here is the part of a configuration that can be wrong.
struct Settings {
    host: String,
    port: u16,
    use_tls: bool,
    client_id: String,
    username: Option<String>,
    password: sc_error::Secret<String>,
    topic: String,
    qos: QoS,
    clean_session: bool,
}

impl Settings {
    /// Read and check every setting of a connection for `stream`.
    fn read(stream: &str, config: &Attrs) -> Result<Settings> {
        let topic = topic(config)?;
        check_topic_filter(&topic)?;
        let client_id = match text(config, CFG_CLIENT_ID) {
            Some(id) => id,
            // A **stable** default, not a random one: a fresh id per reconnect
            // leaves the broker holding a session per attempt, and a stream
            // that flaps for a day becomes a broker nobody can administer
            // (§11). The stream's name is an identifier (`check_stream_name`),
            // so it needs no escaping here.
            None => format!("feldspar-{stream}"),
        };
        Ok(Settings {
            host: host(config)?,
            port: port(config)?,
            use_tls: flag(config, CFG_USE_TLS, false),
            client_id,
            username: text(config, CFG_USERNAME),
            password: text(config, CFG_PASSWORD).unwrap_or_default().into(),
            topic,
            qos: qos(config)?,
            clean_session: flag(config, CFG_CLEAN_SESSION, true),
        })
    }

    /// The broker, as an error message names it.
    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The client options these settings describe.
    fn options(&self) -> Result<MqttOptions> {
        let mut options = MqttOptions::new(&self.client_id, &self.host, self.port);
        options.set_keep_alive(KEEP_ALIVE);
        options.set_clean_session(self.clean_session);
        if let Some(username) = &self.username {
            options.set_credentials(username, self.password.expose());
        }
        if self.use_tls {
            options.set_transport(tls_transport()?);
        }
        Ok(options)
    }

    /// A connection error, as the admin reads it in the stream's status.
    ///
    /// The two authentication codes get a sentence of their own, because "the
    /// connection was refused, return code `BadUserNamePassword`" names a
    /// packet field and not the two settings on the form.
    fn connection_failed(&self, e: ConnectionError) -> Error {
        let address = self.address();
        match e {
            ConnectionError::ConnectionRefused(
                ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized,
            ) => Error::invalid(format!(
                "the broker at {address} refused these credentials; check `{CFG_USERNAME}` and \
                 `{CFG_PASSWORD}`"
            )),
            other => Error::msg(format!(
                "could not connect to the MQTT broker at {address}: {other}"
            )),
        }
    }
}

/// What a decoded publish does next: count it or hand it on, and say whether
/// the connection is still alive.
struct Publishes {
    stream: String,
    element_type: ElementType,
    sink: Arc<dyn StreamSink>,
    throttle: MalformedThrottle,
}

impl Publishes {
    /// Handle one event from the broker. `false` means the connection is over.
    fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Incoming(Packet::Publish(publish)) => {
                let qos = qos_number(publish.qos);
                match element_from_publish(
                    &self.element_type,
                    &publish.topic,
                    qos,
                    publish.retain,
                    publish.payload.to_vec(),
                ) {
                    Ok(element) => self.sink.deliver(element),
                    Err(e) => self.refuse(&publish.topic, &sc_error::format_chain(&e)),
                }
                true
            }
            // The broker said goodbye. Not an error, and still the end of the
            // flow: the supervisor restarts it.
            Event::Incoming(Packet::Disconnect) => false,
            // Acknowledgements, pings, the outgoing half of our own traffic.
            _ => true,
        }
    }

    /// Count a payload that would not decode, and warn about it at most once a
    /// minute (§11, task 4.4).
    fn refuse(&mut self, topic: &str, reason: &str) {
        self.sink.malformed(reason);
        if let Some(suppressed) = self.throttle.note(Instant::now()) {
            let and_more = match suppressed {
                0 => String::new(),
                1 => " (and 1 more since the last warning)".to_owned(),
                n => format!(" (and {n} more since the last warning)"),
            };
            eprintln!(
                "feldspar: stream `{}`: a payload on `{topic}` does not match the declared \
                 element type and was not delivered: {reason}{and_more}",
                self.stream
            );
        }
    }
}

/// One warning a minute, and how many were held back (§11).
///
/// A publisher sending the wrong shape at 50 Hz is one configuration mistake,
/// not 50 log lines a second — and a *count* is what tells the admin which of
/// the two it is: one stray heartbeat on a wildcard filter, or every element of
/// the stream.
///
/// The clock is a parameter for `Scheduler::tick`'s reason: a throttle whose
/// test has to sleep for a minute is a test nobody runs.
#[derive(Debug, Default)]
struct MalformedThrottle {
    last_warned: Option<Instant>,
    suppressed: u64,
}

impl MalformedThrottle {
    /// Whether to warn now, and how many have been suppressed since the last
    /// warning. `None` while the current minute is still running.
    fn note(&mut self, now: Instant) -> Option<u64> {
        match self.last_warned {
            Some(last) if now.saturating_duration_since(last) < WARN_EVERY => {
                self.suppressed = self.suppressed.saturating_add(1);
                None
            }
            _ => {
                self.last_warned = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
        }
    }
}

/// One publish, decoded against the stream's element type and stamped with
/// MQTT's own metadata (§4).
///
/// `topic`, `qos` and `retain` go in `source` rather than beside `value`
/// because "which topic" is a fact about *this provider*: a formula that reads
/// `payload.source.topic` has already accepted that it is talking to MQTT,
/// which is the honest position for it to be in.
///
/// A free function over plain data rather than a method over rumqttc's
/// `Publish`, so the whole of "what arrives becomes what a trigger reads" is
/// testable without a broker.
fn element_from_publish(
    element_type: &ElementType,
    topic: &str,
    qos: u8,
    retain: bool,
    payload: Vec<u8>,
) -> Result<Element> {
    Ok(Element::decode(element_type, RawPayload::Bytes(payload))?
        .source(json!({ "topic": topic, "qos": qos, "retain": retain })))
}

/// The TLS transport an `mqtts://` connection uses: this binary's one rustls,
/// and the platform's root store.
///
/// See the module docs for why this is here rather than
/// `TlsConfiguration::default()`.
fn tls_transport() -> Result<Transport> {
    // Idempotent, and `sc-server` does the same at boot: a stream can be the
    // first thing in a process to want TLS (a `feldspar` run with no
    // certificate and no outbound HTTPS yet), and rustls refuses to guess
    // between the two providers that reach this binary.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let native = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    let mut added = 0usize;
    for certificate in native.certs {
        if roots.add(certificate).is_ok() {
            added += 1;
        }
    }
    if added == 0 {
        let why = native
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Error::config(format!(
            "no platform root certificates could be read, so a TLS connection to an MQTT broker \
             cannot verify it{}{why}",
            if why.is_empty() { "" } else { ": " }
        )));
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Transport::tls_with_config(TlsConfiguration::Rustls(
        Arc::new(config),
    )))
}

// --- Reading the settings ----------------------------------------------------

/// A text setting, trimmed, or `None` when it is absent or blank.
///
/// Blank is `None` on purpose: a form submits every control, so "not set" and
/// "set to nothing" arrive identically, and there is no MQTT setting where an
/// empty string is a value.
fn text(config: &Attrs, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A boolean setting, defaulted.
fn flag(config: &Attrs, key: &str, default: bool) -> bool {
    config.get(key).and_then(Json::as_bool).unwrap_or(default)
}

/// The broker's host. Required: there is nothing sensible to default it to, and
/// `localhost` would be a stream that quietly watches the wrong machine.
fn host(config: &Attrs) -> Result<String> {
    text(config, CFG_HOST)
        .ok_or_else(|| Error::invalid(format!("`{CFG_HOST}`: no broker is named")))
}

/// The broker's port, defaulted to 1883 and refused outside the port range.
fn port(config: &Attrs) -> Result<u16> {
    let port = match config.get(CFG_PORT) {
        None | Some(Json::Null) => PORT_PLAIN,
        Some(value) => value.as_i64().ok_or_else(|| {
            Error::invalid(format!("`{CFG_PORT}`: `{value}` is not a whole number"))
        })?,
    };
    u16::try_from(port)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| Error::invalid(format!("`{CFG_PORT}`: {port} is not a port (1–65535)")))
}

/// The topic filter. Required, for `host`'s reason.
fn topic(config: &Attrs) -> Result<String> {
    text(config, CFG_TOPIC)
        .ok_or_else(|| Error::invalid(format!("`{CFG_TOPIC}`: no topic filter is given")))
}

/// The quality of service, defaulted to 0 and refused outside 0–2.
fn qos(config: &Attrs) -> Result<QoS> {
    let qos = match config.get(CFG_QOS) {
        None | Some(Json::Null) => 0,
        Some(value) => value
            .as_i64()
            .ok_or_else(|| Error::invalid(format!("`{CFG_QOS}`: `{value}` is not 0, 1 or 2")))?,
    };
    match qos {
        0 => Ok(QoS::AtMostOnce),
        1 => Ok(QoS::AtLeastOnce),
        2 => Ok(QoS::ExactlyOnce),
        other => Err(Error::invalid(format!(
            "`{CFG_QOS}`: {other} is not a quality of service; MQTT has 0 (at most once), 1 (at \
             least once) and 2 (exactly once)"
        ))),
    }
}

/// A quality of service as the number the envelope's `source` carries.
fn qos_number(qos: QoS) -> u8 {
    match qos {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
    }
}

/// Which of the three payload kinds this configuration declares.
///
/// Returns the stable name rather than an enum of its own: there are three of
/// them, they are the values of one picker, and
/// [`element_type`](StreamProvider::element_type) is the only caller.
fn payload_kind(config: &Attrs) -> Result<&'static str> {
    match text(config, CFG_PAYLOAD)
        .unwrap_or_else(|| PAYLOAD_JSON.to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        PAYLOAD_JSON => Ok(PAYLOAD_JSON),
        PAYLOAD_TEXT => Ok(PAYLOAD_TEXT),
        PAYLOAD_BINARY => Ok(PAYLOAD_BINARY),
        other => Err(Error::invalid(format!(
            "`{CFG_PAYLOAD}`: `{other}` is not a payload kind; it is one of `{PAYLOAD_JSON}`, \
             `{PAYLOAD_TEXT}` or `{PAYLOAD_BINARY}`"
        ))),
    }
}

/// The declared keys of a `json` payload — §11's repeating group, as the data
/// it is stored as.
///
/// An absent or empty list is **not** refused here: it is
/// [`ElementType::validate`]'s refusal, so "a json stream must declare a key"
/// is one sentence written once rather than one per provider.
fn declared_keys(config: &Attrs) -> Result<Vec<ElementField>> {
    match config.get(CFG_KEYS) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value::<Vec<ElementField>>(value.clone()).map_err(|e| {
            Error::invalid(format!(
                "`{CFG_KEYS}`: each declared key is an object with a `name` and a `type` (one of \
                 `bool`, `int`, `float`, `decimal`, `text`, `json`, `uuid`, `date`, `time`, \
                 `timestamp`), and optionally `required`; {e}"
            ))
        }),
    }
}

/// The declared encoding of a `text` payload, defaulted to `utf8`.
///
/// Anything else is refused by [`ElementType::validate`] rather than here, for
/// [`declared_keys`]' reason.
fn encoding(config: &Attrs) -> String {
    text(config, CFG_ENCODING).unwrap_or_else(|| UTF8.to_owned())
}

/// Refuse a topic filter whose wildcards are not where MQTT allows them
/// (task 4.2).
///
/// A broker would refuse it too — with a `SUBACK` failure code and no reason —
/// so checking it on save is the difference between a sentence on the form and
/// a stream that is `failed` with "the broker refused a subscription".
///
/// The two rules are MQTT 3.1.1 §4.7.1: `+` and `#` each occupy a whole level,
/// and `#` can only be the last one. Everything else is allowed, including the
/// leading `$share/…` of a shared subscription — which §6 names as the escape
/// hatch for two servers subscribing to one stream, so it must not be refused
/// here.
fn check_topic_filter(topic: &str) -> Result<()> {
    if topic.is_empty() {
        return Err(Error::invalid(format!(
            "`{CFG_TOPIC}`: a topic filter cannot be empty"
        )));
    }
    if topic.contains('\0') {
        return Err(Error::invalid(format!(
            "`{CFG_TOPIC}`: a topic filter cannot contain a null character"
        )));
    }
    let levels: Vec<&str> = topic.split('/').collect();
    for (i, level) in levels.iter().enumerate() {
        if level.contains('#') {
            if *level != "#" {
                return Err(Error::invalid(format!(
                    "`{CFG_TOPIC}`: in `{topic}`, `#` is a level of its own — `{}/#`, not \
                     `{level}`",
                    levels[..i].join("/")
                )));
            }
            if i + 1 != levels.len() {
                return Err(Error::invalid(format!(
                    "`{CFG_TOPIC}`: in `{topic}`, `#` matches every level after it, so it can \
                     only be the last one; `+` is the wildcard for a single level"
                )));
            }
        } else if level.contains('+') && *level != "+" {
            return Err(Error::invalid(format!(
                "`{CFG_TOPIC}`: in `{topic}`, `+` is a level of its own — `a/+/b`, not `{level}`"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "test code: casts over test data whose range the test chose"
)]
mod tests {
    use super::*;
    use sc_types::validate_attrs;
    use serde_json::json;

    /// A configuration as the form submits one.
    fn config(value: Json) -> Attrs {
        match value {
            Json::Object(map) => map,
            other => panic!("a configuration is an object, got {other}"),
        }
    }

    fn boiler() -> Attrs {
        config(json!({
            "host": "broker.example",
            "topic": "house/+/temp",
            "payload": "json",
            "keys": [
                { "name": "temperature", "type": "float", "required": true },
                { "name": "unit", "type": "text" },
            ],
        }))
    }

    // --- The declaration ----------------------------------------------------

    #[test]
    fn the_declared_settings_are_the_ones_section_11_names() {
        let spec = Mqtt.config_spec();
        let names: Vec<&str> = spec.iter().map(FormField::name).collect();
        assert_eq!(
            names,
            vec![
                CFG_HOST,
                CFG_PORT,
                CFG_USE_TLS,
                CFG_CLIENT_ID,
                CFG_USERNAME,
                CFG_PASSWORD,
                CFG_TOPIC,
                CFG_QOS,
                CFG_CLEAN_SESSION,
                CFG_PAYLOAD,
                CFG_KEYS,
                CFG_ENCODING,
            ]
        );
        // The password is a secret *on the declaration*, which is what makes
        // `redact_attrs` and `merge_secrets` cover it without either of them
        // knowing what MQTT is (task 2.3).
        let password = spec
            .iter()
            .find(|f| f.name() == CFG_PASSWORD)
            .expect("a password setting");
        assert!(password.secret);
        // The payload is a picker over exactly three values, because the
        // element type is a function of it.
        let payload = spec
            .iter()
            .find(|f| f.name() == CFG_PAYLOAD)
            .expect("a payload setting");
        assert_eq!(
            payload.static_options(),
            [
                json!(PAYLOAD_JSON),
                json!(PAYLOAD_TEXT),
                json!(PAYLOAD_BINARY)
            ]
        );
    }

    #[test]
    fn a_configuration_of_every_setting_validates_against_the_declaration() {
        // The generic check `check_stream` runs, over a fully filled-in form:
        // what this asserts is that the spec and the settings this module reads
        // are the same set of names, which is the mistake a rename makes.
        let full = config(json!({
            "host": "broker.example",
            "port": 8883,
            "use_tls": true,
            "client_id": "feldspar-boiler",
            "username": "sensors",
            "password": "hunter2",
            "topic": "house/boiler/#",
            "qos": 1,
            "clean_session": true,
            "payload": "text",
            "keys": [],
            "encoding": "utf8",
        }));
        validate_attrs(&Mqtt.config_spec(), &full).expect("the settings validate");
        Mqtt.validate(&full).expect("and MQTT agrees");
    }

    // --- The element type over each payload setting --------------------------

    #[test]
    fn a_json_payload_declares_its_keys() {
        let ty = Mqtt.element_type(&boiler()).unwrap();
        assert_eq!(
            ty,
            ElementType::json([
                ElementField::new("temperature", BasicType::Float).required(),
                ElementField::new("unit", BasicType::Text),
            ])
        );
        ty.validate().unwrap();
    }

    #[test]
    fn the_payload_setting_is_what_the_element_type_is_a_function_of() {
        // The same broker and the same filter, three element types (§3).
        let mut text_stream = boiler();
        text_stream.insert(CFG_PAYLOAD.to_owned(), json!("text"));
        assert_eq!(
            Mqtt.element_type(&text_stream).unwrap(),
            ElementType::text()
        );

        let mut encoded = text_stream.clone();
        encoded.insert(CFG_ENCODING.to_owned(), json!("latin1"));
        assert_eq!(
            Mqtt.element_type(&encoded).unwrap(),
            ElementType::Text {
                encoding: "latin1".to_owned()
            }
        );
        // Declared, and refused where every other unimplementable declaration
        // is refused (§4).
        let err = Mqtt.validate(&encoded).unwrap_err().to_string();
        assert!(err.contains("latin1") && err.contains("utf8"), "{err}");

        let mut binary = boiler();
        binary.insert(CFG_PAYLOAD.to_owned(), json!("binary"));
        assert_eq!(Mqtt.element_type(&binary).unwrap(), ElementType::Binary);

        // A payload kind nothing implements, named.
        let mut wrong = boiler();
        wrong.insert(CFG_PAYLOAD.to_owned(), json!("protobuf"));
        let err = Mqtt.element_type(&wrong).unwrap_err().to_string();
        assert!(err.contains("protobuf") && err.contains("`json`"), "{err}");
    }

    #[test]
    fn a_default_configuration_is_a_json_stream_of_no_keys_which_is_refused() {
        // The form's defaults with nothing else filled in: the element type
        // resolves (so the admin API can show it) and the *validation* is what
        // refuses it, naming what is missing.
        let bare = config(json!({ "host": "broker.example", "topic": "house/boiler/temp" }));
        assert_eq!(
            Mqtt.element_type(&bare).unwrap(),
            ElementType::Json { keys: Vec::new() }
        );
        let err = Mqtt.validate(&bare).unwrap_err().to_string();
        assert!(err.contains("at least one key"), "{err}");
    }

    #[test]
    fn a_declared_key_that_is_not_a_key_is_refused_naming_the_setting() {
        let mut broken = boiler();
        broken.insert(CFG_KEYS.to_owned(), json!(["temperature"]));
        let err = Mqtt.element_type(&broken).unwrap_err().to_string();
        assert!(err.contains(CFG_KEYS) && err.contains("`name`"), "{err}");

        let mut wrong_type = boiler();
        wrong_type.insert(
            CFG_KEYS.to_owned(),
            json!([{ "name": "where", "type": "hologram" }]),
        );
        // An unknown type name survives the parse (it is `BasicType::Other`)
        // and is refused by the element type, naming both. (Not `geometry`,
        // which has been a known type — GeoJSON on the wire — since the
        // analytics milestones.)
        let err = Mqtt.validate(&wrong_type).unwrap_err().to_string();
        assert!(err.contains("`where`") && err.contains("hologram"), "{err}");
    }

    // --- The settings validation --------------------------------------------

    #[test]
    fn a_port_that_is_not_a_port_is_refused() {
        let mut stream = boiler();
        stream.insert(CFG_PORT.to_owned(), json!(0));
        let err = Mqtt.validate(&stream).unwrap_err().to_string();
        assert!(err.contains(CFG_PORT) && err.contains("65535"), "{err}");

        stream.insert(CFG_PORT.to_owned(), json!(70_000));
        let err = Mqtt.validate(&stream).unwrap_err().to_string();
        assert!(err.contains("70000"), "{err}");

        stream.insert(CFG_PORT.to_owned(), json!(8883));
        Mqtt.validate(&stream).unwrap();
        // Absent is the plain default rather than an error.
        stream.remove(CFG_PORT);
        assert_eq!(port(&stream).unwrap(), 1883);
    }

    #[test]
    fn a_quality_of_service_mqtt_does_not_have_is_refused() {
        let mut stream = boiler();
        stream.insert(CFG_QOS.to_owned(), json!(3));
        let err = Mqtt.validate(&stream).unwrap_err().to_string();
        assert!(
            err.contains(CFG_QOS) && err.contains("exactly once"),
            "{err}"
        );
        for (n, expected) in [
            (0, QoS::AtMostOnce),
            (1, QoS::AtLeastOnce),
            (2, QoS::ExactlyOnce),
        ] {
            stream.insert(CFG_QOS.to_owned(), json!(n));
            assert_eq!(qos(&stream).unwrap(), expected);
            assert_eq!(qos_number(expected), n as u8);
        }
    }

    #[test]
    fn a_missing_host_or_topic_is_refused_naming_the_setting() {
        let mut stream = boiler();
        stream.insert(CFG_HOST.to_owned(), json!("   "));
        let err = Mqtt.validate(&stream).unwrap_err().to_string();
        assert!(err.contains(CFG_HOST) && err.contains("broker"), "{err}");

        let mut stream = boiler();
        stream.remove(CFG_TOPIC);
        let err = Mqtt.validate(&stream).unwrap_err().to_string();
        assert!(err.contains(CFG_TOPIC), "{err}");
    }

    #[test]
    fn a_wildcard_where_mqtt_does_not_allow_one_is_refused() {
        // Every filter a broker would take.
        for good in [
            "house/boiler/temp",
            "house/+/temp",
            "house/#",
            "#",
            "+",
            "+/+/temp",
            "$share/feldspar/house/#",
            "house/",
        ] {
            check_topic_filter(good).unwrap_or_else(|e| panic!("`{good}` should pass: {e}"));
        }
        // `#` is a level of its own.
        let err = check_topic_filter("house#").unwrap_err().to_string();
        assert!(err.contains("level of its own"), "{err}");
        // and only the last one.
        let err = check_topic_filter("house/#/temp").unwrap_err().to_string();
        assert!(err.contains("only be the last one"), "{err}");
        // `+` is a level of its own too.
        let err = check_topic_filter("house/bo+ler/temp")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`a/+/b`"), "{err}");

        let err = check_topic_filter("").unwrap_err().to_string();
        assert!(err.contains("cannot be empty"), "{err}");
    }

    // --- The decoder, over all three payload kinds (task 4.4) ---------------

    #[test]
    fn a_json_publish_becomes_an_element_with_mqtt_metadata_in_its_source() {
        let ty = Mqtt.element_type(&boiler()).unwrap();
        let element = element_from_publish(
            &ty,
            "house/boiler/temp",
            0,
            false,
            br#"{"temperature": 31.2, "unit": "C"}"#.to_vec(),
        )
        .unwrap();
        assert_eq!(element.value, json!({ "temperature": 31.2, "unit": "C" }));
        assert_eq!(
            element.source,
            Some(json!({ "topic": "house/boiler/temp", "qos": 0, "retain": false }))
        );
        // The stream's name and the moment it arrived are stamped on later, by
        // the running stream — a provider cannot lie about either (§4).
        let envelope = element.into_envelope("boiler", chrono::Utc::now());
        assert_eq!(envelope.stream, "boiler");
        assert_eq!(envelope.value["temperature"], json!(31.2));
    }

    #[test]
    fn a_text_publish_is_decoded_and_a_binary_one_is_base64() {
        let element = element_from_publish(
            &ElementType::text(),
            "house/notice",
            1,
            true,
            "héllo".as_bytes().to_vec(),
        )
        .unwrap();
        assert_eq!(element.value, json!("héllo"));
        assert_eq!(
            element.source,
            Some(json!({ "topic": "house/notice", "qos": 1, "retain": true }))
        );

        let element = element_from_publish(
            &ElementType::Binary,
            "house/frame",
            2,
            false,
            vec![0xde, 0xad, 0xbe, 0xef],
        )
        .unwrap();
        assert_eq!(element.value, json!("3q2+7w=="));
    }

    #[test]
    fn a_payload_that_does_not_match_the_declaration_is_an_error_not_an_element() {
        let ty = Mqtt.element_type(&boiler()).unwrap();
        // The wildcard-filter case: a heartbeat string on `house/+/temp`.
        let err = element_from_publish(&ty, "house/heartbeat/temp", 0, false, b"ok".to_vec())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not JSON"), "{err}");
        // A declared key of the wrong type.
        let err = element_from_publish(
            &ty,
            "house/boiler/temp",
            0,
            false,
            br#"{"temperature": "warm"}"#.to_vec(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`temperature`"), "{err}");
        // Bytes that are not UTF-8, declared as text.
        let err = element_from_publish(&ElementType::text(), "t", 0, false, vec![0x80])
            .unwrap_err()
            .to_string();
        assert!(err.contains("UTF-8"), "{err}");
    }

    // --- The warning throttle (task 4.4) ------------------------------------

    #[test]
    fn a_malformed_payload_is_warned_about_at_most_once_a_minute_and_counts_the_rest() {
        let mut throttle = MalformedThrottle::default();
        let t0 = Instant::now();
        // The first one warns, with nothing held back.
        assert_eq!(throttle.note(t0), Some(0));
        // A publisher at 50 Hz for the rest of the minute: nothing is logged.
        for i in 1..=50 {
            assert_eq!(throttle.note(t0 + Duration::from_millis(i * 20)), None);
        }
        assert_eq!(throttle.note(t0 + Duration::from_secs(59)), None);
        // The next minute warns once, and says how many it did not.
        assert_eq!(throttle.note(t0 + Duration::from_secs(60)), Some(51));
        // And the count starts again, so the next line is about the next
        // minute rather than the whole history.
        assert_eq!(throttle.note(t0 + Duration::from_secs(61)), None);
        assert_eq!(throttle.note(t0 + Duration::from_secs(121)), Some(1));
    }

    // --- Connecting ---------------------------------------------------------

    #[test]
    fn the_client_id_defaults_to_a_stable_one_per_stream() {
        let settings = Settings::read("boiler", &boiler()).unwrap();
        assert_eq!(settings.client_id, "feldspar-boiler");
        assert_eq!(settings.address(), "broker.example:1883");
        assert!(settings.clean_session, "the default is a clean session");
        assert!(settings.username.is_none());

        let mut named = boiler();
        named.insert(CFG_CLIENT_ID.to_owned(), json!("upstairs-1"));
        assert_eq!(
            Settings::read("boiler", &named).unwrap().client_id,
            "upstairs-1"
        );
        // A blank one is not a client id, it is an unfilled control.
        named.insert(CFG_CLIENT_ID.to_owned(), json!("  "));
        assert_eq!(
            Settings::read("boiler", &named).unwrap().client_id,
            "feldspar-boiler"
        );
    }

    #[test]
    fn credentials_and_tls_reach_the_options() {
        let mut stream = boiler();
        stream.insert(CFG_USERNAME.to_owned(), json!("sensors"));
        stream.insert(CFG_PASSWORD.to_owned(), json!("hunter2"));
        stream.insert(CFG_PORT.to_owned(), json!(8883));
        let settings = Settings::read("boiler", &stream).unwrap();
        let options = settings.options().unwrap();
        let login = options.credentials().expect("credentials were set");
        assert_eq!(login.username, "sensors");
        assert_eq!(login.password, "hunter2");
        assert_eq!(
            options.broker_address(),
            ("broker.example".to_owned(), 8883)
        );
        assert_eq!(options.keep_alive(), KEEP_ALIVE);
        assert!(matches!(options.transport(), Transport::Tcp));

        stream.insert(CFG_USE_TLS.to_owned(), json!(true));
        let with_tls = Settings::read("boiler", &stream)
            .unwrap()
            .options()
            .unwrap();
        assert!(matches!(with_tls.transport(), Transport::Tls(_)));
    }

    /// A sink that would notice if anything reached it. Local rather than
    /// `testing::Collector`, so these unit tests do not depend on a feature.
    #[derive(Debug, Default)]
    struct Nothing(std::sync::atomic::AtomicUsize);

    impl StreamSink for Nothing {
        fn deliver(&self, _element: Element) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_broker_that_is_not_there_is_an_error_rather_than_a_silent_stream() {
        // Port 1 on the loopback: nothing listens, and the refusal is
        // immediate, so this test needs neither a broker nor the timeout. The
        // point of it is that the failure is `subscribe`'s return value —
        // which is what the supervisor backs off on and what the admin reads
        // in the status — rather than a stream that is "running" and quiet.
        let mut stream = boiler();
        stream.insert(CFG_HOST.to_owned(), json!("127.0.0.1"));
        stream.insert(CFG_PORT.to_owned(), json!(1));
        let sink = Arc::new(Nothing::default());
        let err = Mqtt
            .subscribe("boiler", &stream, Arc::clone(&sink) as Arc<dyn StreamSink>)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("127.0.0.1:1"), "{err}");
        assert_eq!(sink.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}

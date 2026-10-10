//! The Observe socket: watching a stream's elements arrive, live (TODO
//! "Streams" §9, task 6.3).
//!
//! **Why a WebSocket at all**, when everything else in the admin API is a typed
//! request/response pair (§13.1): a stream is the one entity here that is not
//! at rest. There is no "the elements" to `GET` — an element is not stored
//! (§4) — so the only honest shape for "show me this stream" is a connection
//! that is handed each element as it arrives. That is the same split the
//! file-store IDE's language server (§12.1) and the admin chat (§11.4) made,
//! and it is authenticated the same way: admin-only, through the session
//! cookie, **decided before the upgrade**, because a browser cannot read the
//! body of a failed handshake and an auth refusal is the one thing a caller
//! must be able to tell from a network fault.
//!
//! ## The protocol
//!
//! Nothing comes *in*. The client opens the socket and reads; a Pause button is
//! client-side, because pausing the server would mean either dropping the
//! elements or queueing them, and §7 says which of those a stream does. What
//! goes out, as JSON text frames:
//!
//! - `{"type":"ready","stream":"boiler","element_type":{…},"status":{…},
//!    "counters":{…},"replayed":n}` — once, first. `element_type` is null for a
//!   stream whose provider could not be resolved, which is a stream the screen
//!   can still show the status of.
//! - `{"type":"element","envelope":{…}}` — per element, the §4 envelope
//!   unchanged. It is a wire contract: the same JSON a trigger's `only_if`
//!   reads and an application's generated client is typed from.
//! - `{"type":"lagged","dropped":n}` — this socket fell behind the broadcast
//!   channel and lost `n` elements. **Told, never hidden** (§7): a consumer
//!   that cannot keep up is the consumer's problem, and a screen showing a
//!   silent gap is worse than one showing a gap it named.
//! - `{"type":"status","status":{…},"counters":{…}}` — the stream reconnected,
//!   failed, or stopped. Polled rather than pushed, because the supervisor has
//!   no observer seam per stream and a second one would be a channel to keep in
//!   step with the first for a frame that arrives once a minute.
//!
//! ## The replay is small, and says so
//!
//! `ready` is followed by up to [`RunningStream::subscribe_elements`]' ring —
//! the last hundred envelopes **this process** saw — so a screen opened on a
//! stream that publishes twice an hour is not blank. `replayed` says how many
//! of the elements that follow are history, and the screen labels them "since
//! this server started", which is the whole truth: there is no
//! `_fd_stream_elements` and there is not going to be one (§4).
//!
//! The replay and the subscription are taken **together, under one lock**
//! (`subscribe_elements`), so the tail is neither gapped nor doubled.
//!
//! ## An application's page uses the live socket instead
//!
//! This socket is the **admin's**: one stream per connection, every topic, no
//! per-subscriber checks beyond "is an admin". An application's page holds the
//! multiplexed live socket, `{mount}/live` ([`crate::live`]), which replaced the
//! per-stream app socket this module used to serve too (TODO.md "Live updates"
//! L1.7). The two share how a running stream's status, counters and element
//! type are written ([`status_parts`], [`element_type_json`]), so the Observe
//! screen and a page cannot describe one stream two ways.
//!
//! ## What closes the socket
//!
//! Only things that make observation impossible: this server has no streams
//! installed, there is no stream with that id, or it is not running here. Each
//! arrives as a close frame with a reason rather than a status, for the reason
//! the language server's do — the handshake has already succeeded by then. A
//! stream that *fails* does not close anything: it is a `status` frame on an
//! open socket, because the supervisor is going to reconnect and the screen
//! should be there when it does.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use sc_stream::{RunningStream, StreamId, StreamSupervisor};
use serde_json::{Value as Json, json};
use tokio::sync::broadcast::error::RecvError;

/// The route the admin SPA opens an Observe socket on.
///
/// Beside the typed endpoint set rather than in it, and mounted as a real axum
/// route in `router.rs`: an upgrade cannot survive the fallback's `Bytes` body,
/// because it has to be extracted before the body is touched.
pub const STREAM_OBSERVE_ROUTE: &str = "/api/streams/{id}/observe";

/// How often the socket re-reads the running stream's status and counters.
///
/// A poll rather than a push, and a slow one: what it is watching for is a
/// reconnection or a failure, which are events measured in seconds at best, and
/// a socket that wakes ten times a second to find nothing changed is a cost
/// every open Observe screen pays for nothing.
const STATUS_INTERVAL: Duration = Duration::from_secs(2);

/// A close frame's reason is capped by the protocol; the reasons sent here are
/// written to fit.
const MAX_CLOSE_REASON: usize = 120;

/// Serve one Observe socket for the stream with `id`.
///
/// The caller has already established that the request is an admin's — that is
/// the one refusal answered with an HTTP status, because a browser cannot read
/// the body of a failed handshake. What is decided here is whether there is
/// anything to observe, and both of those refusals are close frames.
pub(crate) async fn stream_observe_upgrade(
    ws: WebSocketUpgrade,
    supervisor: Option<&Arc<StreamSupervisor>>,
    id: StreamId,
) -> axum::response::Response {
    let Some(supervisor) = supervisor else {
        let reason = "this server has no stream support installed, so there is nothing to observe";
        return ws.on_upgrade(move |socket| refuse(socket, reason.to_owned()));
    };
    let Some(running) = supervisor.get(id) else {
        // Not running *here*. A disabled stream and one this process has not
        // reloaded yet are the same answer, and it is the true one: a
        // subscription is process-local (§6).
        let reason = format!(
            "stream {id} is not running on this server, so there are no elements to observe"
        );
        return ws.on_upgrade(move |socket| refuse(socket, reason));
    };
    ws.on_upgrade(move |socket| observe(socket, running))
}

/// Close a socket, saying why.
async fn refuse(mut socket: WebSocket, reason: String) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: close_code::POLICY,
            reason: fit_close_reason(reason).into(),
        })))
        .await;
}

/// A reason cut to what a close frame can carry, on a character boundary.
pub(crate) fn fit_close_reason(reason: String) -> String {
    if reason.len() <= MAX_CLOSE_REASON {
        return reason;
    }
    let mut cut = MAX_CLOSE_REASON;
    while cut > 0 && !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = reason;
    text.truncate(cut);
    text
}

/// The `ready` frame: what this stream is, and how much of what follows is
/// history.
pub(crate) fn ready_frame(running: &RunningStream, replayed: usize) -> Json {
    let (status, counters) = status_parts(running);
    json!({
        "type": "ready",
        "stream": running.name(),
        "element_type": element_type_json(running),
        "status": status,
        "counters": counters,
        "replayed": replayed,
    })
}

/// The `status` frame — sent when either half of it has moved.
fn status_frame(running: &RunningStream) -> Json {
    let (status, counters) = status_parts(running);
    json!({
        "type": "status",
        "status": status,
        "counters": counters,
    })
}

/// A running stream's element type as a frame carries it: `null` for a stream
/// whose provider could not be resolved. Shared with the live socket's
/// `ready`.
pub(crate) fn element_type_json(running: &RunningStream) -> Json {
    running
        .element_type()
        .and_then(|ty| serde_json::to_value(ty).ok())
        .unwrap_or(Json::Null)
}

/// A running stream's status and counters as a frame carries them. Shared with
/// the live socket's `ready` and `status`.
pub(crate) fn status_parts(running: &RunningStream) -> (Json, Json) {
    (
        serde_json::to_value(running.status()).unwrap_or(Json::Null),
        running.counters().to_json(),
    )
}

/// Read the stream until the socket goes away.
///
/// The client sends nothing, so the receiving half is watched only for the
/// close: a socket whose reader is never polled would leave a browser that
/// navigated away holding a subscription until the next element arrived, which
/// on a quiet stream is never.
pub(crate) async fn observe(socket: WebSocket, running: Arc<RunningStream>) {
    use futures::{SinkExt, StreamExt};

    let (mut sender, mut receiver) = socket.split();

    // Replay and subscription in one step, under the ring's lock: taking them
    // separately would either lose every element published in between or show
    // some of them twice.
    let feed = running.subscribe_elements();
    let replay = feed.replay;
    let mut elements = feed.receiver;

    if sender
        .send(Message::text(
            ready_frame(&running, replay.len()).to_string(),
        ))
        .await
        .is_err()
    {
        return;
    }
    for envelope in replay {
        let frame = json!({ "type": "element", "envelope": envelope.to_json() });
        if sender.send(Message::text(frame.to_string())).await.is_err() {
            return;
        }
    }

    let mut last = status_frame(&running);
    let mut ticker = tokio::time::interval(STATUS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is immediate and would repeat what `ready` just said.
    ticker.tick().await;

    loop {
        let frame = tokio::select! {
            received = elements.recv() => match received {
                Ok(envelope) => json!({ "type": "element", "envelope": envelope.to_json() }),
                // §7: a consumer that cannot keep up is told, never shown a
                // silent gap. The receiver is still good — the next `recv`
                // resumes at the oldest element still buffered.
                Err(RecvError::Lagged(dropped)) => json!({
                    "type": "lagged",
                    "dropped": dropped,
                }),
                // Unreachable while this socket holds the `Arc` the sender
                // lives on, which is the whole connection — a reload that
                // stops or replaces the stream drops the *supervisor's*
                // handle, not this one, and the socket goes on reporting
                // `stopped` until the browser leaves. Handled rather than
                // unwrapped because "the channel closed" must not become a
                // busy loop if that ever stops being true.
                Err(RecvError::Closed) => break,
            },
            _ = ticker.tick() => {
                let current = status_frame(&running);
                if current == last {
                    continue;
                }
                last = current.clone();
                current
            }
            // The browser went away, or sent something. Nothing is read from
            // this socket, so anything but a close is ignored and only the
            // close ends the loop.
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => continue,
            },
        };
        if sender.send(Message::text(frame.to_string())).await.is_err() {
            break;
        }
    }
    let _ = sender.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_close_reason_is_cut_on_a_character_boundary() {
        let reason = "é".repeat(200);
        let cut = fit_close_reason(reason);
        assert!(cut.len() <= MAX_CLOSE_REASON);
        // Still valid UTF-8 as a `String`, which is the whole point: cutting
        // mid-character would produce a frame a browser refuses.
        assert!(cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_short_reason_is_left_alone() {
        assert_eq!(
            fit_close_reason("stream 1 is not running on this server".to_owned()),
            "stream 1 is not running on this server"
        );
    }
}

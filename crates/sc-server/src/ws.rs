//! Every WebSocket this server accepts is upgraded here, with the largest
//! message it will take from the peer (security review 2026-11 F-9,
//! TODO-type-hardening H5.1).
//!
//! Left alone, axum takes tungstenite's defaults: 64 MiB a message and 16 MiB
//! a frame, buffered whole before the handler sees any of it. One socket per
//! open tab is a lot of memory a client can ask the server to hold, so each
//! endpoint names what it is for instead — [`MessageLimit`] — and
//! `WebSocketUpgrade::on_upgrade` is a disallowed method everywhere but
//! [`upgrade`] (`clippy.toml`), so a new socket cannot forget to.
//!
//! The frame limit is the message limit: a browser sends a message as one
//! frame, so a smaller frame limit would be the real message limit under
//! another name. A message over the limit ends the connection — the
//! transport's answer, before any of it reaches the handler.

use std::future::Future;

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::response::Response;

/// The largest message a socket accepts from its peer, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MessageLimit(usize);

impl MessageLimit {
    /// At most `n` bytes a message.
    pub(crate) const fn bytes(n: usize) -> MessageLimit {
        MessageLimit(n)
    }

    /// At most `n` MiB a message.
    pub(crate) const fn mib(n: usize) -> MessageLimit {
        MessageLimit(n.saturating_mul(1024 * 1024))
    }

    /// The limit, in bytes.
    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

/// A socket that is only ever closed with a reason: whatever the peer says is
/// not read, so it may say very little.
pub(crate) const REFUSAL: MessageLimit = MessageLimit::bytes(4 * 1024);

/// A socket the server talks on and the peer only listens to (a stream's
/// Observe socket, a fit's progress): room for a ping or a close.
pub(crate) const LISTEN_ONLY: MessageLimit = MessageLimit::bytes(64 * 1024);

/// Upgrade `ws`, holding the peer to `limit` a message, and serve the socket
/// with `callback` — the one place a socket is accepted.
#[allow(
    clippy::disallowed_methods,
    reason = "the replacement clippy.toml names for WebSocketUpgrade::on_upgrade: the limits are set here"
)]
pub(crate) fn upgrade<F, Fut>(ws: WebSocketUpgrade, limit: MessageLimit, callback: F) -> Response
where
    F: FnOnce(WebSocket) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    ws.max_message_size(limit.get())
        .max_frame_size(limit.get())
        .on_upgrade(callback)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: the server's own listener task"
)]
mod tests {
    use super::*;

    use axum::extract::ws::Message;
    use futures::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    /// A server with one socket at `/echo` that answers each text message
    /// with its length, holding the peer to `limit`.
    async fn echo_server(limit: MessageLimit) -> String {
        let app = axum::Router::new().route(
            "/echo",
            axum::routing::get(move |ws: WebSocketUpgrade| async move {
                upgrade(ws, limit, |mut socket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        if let Message::Text(text) = message {
                            let reply = Message::Text(text.len().to_string().into());
                            if socket.send(reply).await.is_err() {
                                return;
                            }
                        }
                    }
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("ws://{addr}/echo")
    }

    #[tokio::test]
    async fn a_message_at_the_limit_is_read_and_one_past_it_ends_the_socket() {
        let url = echo_server(MessageLimit::bytes(1024)).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .unwrap();

        socket
            .send(ClientMessage::text("x".repeat(1024)))
            .await
            .unwrap();
        let reply = socket.next().await.unwrap().unwrap();
        assert_eq!(reply.into_text().unwrap().as_str(), "1024");

        socket
            .send(ClientMessage::text("x".repeat(1025)))
            .await
            .unwrap();
        // The server does not read it: the socket ends, with a close or a
        // reset, and nothing is answered.
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("the server ends the socket");
        assert!(
            !matches!(next, Some(Ok(ClientMessage::Text(_)))),
            "an oversized message was answered: {next:?}"
        );

        // The server keeps serving: a new socket works.
        let (mut again, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .unwrap();
        again.send(ClientMessage::text("hello")).await.unwrap();
        let reply = again.next().await.unwrap().unwrap();
        assert_eq!(reply.into_text().unwrap().as_str(), "5");
    }

    #[test]
    fn limits_are_in_bytes() {
        assert_eq!(MessageLimit::mib(16).get(), 16 * 1024 * 1024);
        assert!(REFUSAL.get() < LISTEN_ONLY.get());
    }
}

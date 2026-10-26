//! A stub HTTP server that replays a recorded response — the whole of how the
//! adapters are tested (TODO Phase 1, decision 7).
//!
//! **No test may require an API key or spend a token.** A provider adapter's job
//! is to turn a particular wire format into [`LlmDelta`](sc_llm::LlmDelta)s, and
//! that is exactly what a recorded SSE body exercises: the vendor adds nothing
//! to the test except cost, flakiness and a credential in CI. What the vendor
//! *would* catch — a wire format that changed — is caught by the recorded body
//! being wrong, which is a thing to notice when it happens rather than on every
//! run.
//!
//! Deliberately a raw `TcpListener` rather than an HTTP framework: the server
//! under test is not being tested here, the client is, and this needs to be able
//! to send responses no framework would let it — a body cut off mid-event, a
//! `Content-Length` that lies.

#![allow(dead_code)]

use std::net::SocketAddr;

use sc_error::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the stub sends back, once, to the first request it receives.
pub struct Reply {
    /// The status line's code.
    pub status: u16,
    /// The `Content-Type` header.
    pub content_type: &'static str,
    /// The body.
    pub body: String,
    /// Whether to announce a `Content-Length` **larger** than the body and then
    /// close — a truncated response, which is what a provider dying mid-answer
    /// looks like to a client.
    pub truncate: bool,
}

impl Reply {
    /// An SSE body, served with `200`.
    pub fn sse(body: impl Into<String>) -> Reply {
        Reply {
            status: 200,
            content_type: "text/event-stream",
            body: body.into(),
            truncate: false,
        }
    }

    /// A JSON body, served with `200` — a non-streaming endpoint's answer.
    pub fn json(body: impl Into<String>) -> Reply {
        Reply {
            status: 200,
            content_type: "application/json",
            body: body.into(),
            truncate: false,
        }
    }

    /// A JSON error body, served with `status` — what a rejected key looks like.
    pub fn error(status: u16, body: impl Into<String>) -> Reply {
        Reply {
            status,
            content_type: "application/json",
            body: body.into(),
            truncate: false,
        }
    }

    /// A body of another type, served with `200` — an audio file.
    pub fn typed(content_type: &'static str, body: impl Into<String>) -> Reply {
        Reply {
            status: 200,
            content_type,
            body: body.into(),
            truncate: false,
        }
    }

    /// Promise more bytes than are sent, then close.
    pub fn truncated(self) -> Reply {
        Reply {
            truncate: true,
            ..self
        }
    }
}

/// Start a stub serving `reply`, and return its base URL together with a handle
/// on **what was asked for**.
///
/// Capturing the request is what makes it possible to assert on the shape of the
/// body an adapter builds — the merged tool results, the system prompt, the tool
/// schemas — none of which a reply-only stub can see. That matters because those
/// are exactly the mistakes a vendor would reject and a stub would not.
pub async fn serve_capturing(reply: Reply) -> Result<(String, Requests)> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let base = serve_inner(reply, Some(tx)).await?;
    Ok((base, Requests { rx }))
}

/// The requests a [`serve_capturing`] stub has received.
pub struct Requests {
    rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl Requests {
    /// The next request's **body**, parsed as JSON.
    ///
    /// "Nothing was sent" is an error rather than an empty value: a test that
    /// reached here is asserting on a request, and no request at all is the
    /// failure it is looking for.
    pub async fn next_body(&mut self) -> Result<serde_json::Value> {
        Ok(self.next_request().await?.1)
    }

    /// The next request's **path** and body, the body parsed as JSON.
    pub async fn next_request(&mut self) -> Result<(String, serde_json::Value)> {
        let raw = self
            .rx
            .recv()
            .await
            .ok_or_else(|| Error::msg("the client sent no request"))?;
        let path = raw.split_whitespace().nth(1).unwrap_or_default().to_owned();
        let body = raw
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        let body = serde_json::from_str(body)
            .map_err(|e| Error::msg(format!("request body was not JSON: {e}\n{raw}")))?;
        Ok((path, body))
    }
}

/// Start a stub serving `reply` to every connection, and return its base URL.
///
/// The listener is leaked into a background task on purpose: it lives as long as
/// the test runtime, so a client that reconnects (a retry, a second request in
/// one test) is served rather than refused.
pub async fn serve(reply: Reply) -> Result<String> {
    serve_inner(reply, None).await
}

async fn serve_inner(
    reply: Reply,
    captured: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Result<String> {
    let addr: SocketAddr = "127.0.0.1:0"
        .parse()
        .map_err(|e| Error::msg(format!("the loopback address did not parse: {e}")))?;
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| Error::msg(format!("binding a stub provider port: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Error::msg(format!("reading the stub's address: {e}")))?;

    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        if reply.status == 200 { "OK" } else { "Error" },
        reply.content_type,
        if reply.truncate {
            reply.body.len() + 1024
        } else {
            reply.body.len()
        },
    );

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let head = head.clone();
            let body = reply.body.clone();
            let captured = captured.clone();
            tokio::spawn(async move {
                // What was asked for does not change what this replays, but a
                // capturing stub hands it to the test. One read is enough for
                // the request sizes here — the point is to consume enough that
                // the client is not blocked writing it.
                let mut buf = vec![0u8; 65536];
                let read = socket.read(&mut buf).await.unwrap_or(0);
                if let Some(captured) = captured {
                    let _ = captured.send(String::from_utf8_lossy(&buf[..read]).into_owned());
                }
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    Ok(format!("http://{addr}"))
}

/// One SSE `data:` line per event, as both vendors frame them.
pub fn sse_events(events: &[serde_json::Value]) -> String {
    events
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<Vec<_>>()
        .join("")
}

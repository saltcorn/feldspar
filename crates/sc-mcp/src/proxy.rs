//! Forwarding a client's MCP traffic to a stored server (design §12.1, "The
//! installation's other MCP servers, through a proxy").
//!
//! The IDE's Claude Code sessions reach the servers offered to them through
//! `sc-server`'s proxy route, which hands each request here. The client never
//! holds the server's credential: the stored headers are added on the way out,
//! so their values stay in this process. The request goes through the same
//! guard and timeout as an agent's call, so a definition that does not allow
//! the private network does not become a way to reach it.
//!
//! Streamable HTTP only: one request per message, so a body and the
//! `Mcp-Session-Id` header can be passed through unchanged. An SSE server's
//! event stream names the URL to post to, which a proxy would have to rewrite.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Url};
use sc_error::{Error, Result};

use crate::def::{McpServerDef, McpTransport};
use crate::guard;

/// The request headers a client's own are passed on with: what streamable HTTP
/// needs and nothing about the client. Its `Authorization` is the session's
/// and is never forwarded.
const FORWARDED_REQUEST_HEADERS: [&str; 5] = [
    "content-type",
    "accept",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// The response headers passed back to the client.
pub const FORWARDED_RESPONSE_HEADERS: [&str; 2] = ["content-type", "mcp-session-id"];

/// Send one client request to `def`, with `method`, the allowed part of the
/// client's `headers`, the stored headers, and `body`. The answer is returned
/// unread, so an event stream reaches the client as it arrives.
///
/// A non-success status is an answer, not an error: the client reads it.
pub async fn forward(
    def: &McpServerDef,
    method: Method,
    headers: &HeaderMap,
    body: impl Into<reqwest::Body>,
) -> Result<reqwest::Response> {
    if def.transport != McpTransport::StreamableHttp {
        return Err(Error::invalid(format!(
            "MCP server `{}` uses SSE, which the proxy does not forward",
            def.name
        )));
    }
    let url = Url::parse(def.url.trim()).map_err(|e| {
        Error::invalid(format!(
            "MCP server `{}`: `{}` is not a URL: {e}",
            def.name, def.url
        ))
    })?;
    guard::check_url(def, &url)?;
    let client = guard::client(def)?;

    let mut outgoing = HeaderMap::new();
    for name in FORWARDED_REQUEST_HEADERS {
        for value in headers.get_all(name) {
            outgoing.append(HeaderName::from_static(name), value.clone());
        }
    }
    for (name, value) in &def.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            Error::invalid(format!(
                "MCP server `{}`: bad header name `{name}`",
                def.name
            ))
        })?;
        let mut value = HeaderValue::from_str(value).map_err(|_| {
            Error::invalid(format!(
                "MCP server `{}`: the value of header `{name}` is not one a header can hold",
                def.name
            ))
        })?;
        value.set_sensitive(true);
        outgoing.insert(name, value);
    }

    client
        .request(method, url)?
        .headers(outgoing)
        .timeout(def.timeout())
        .body(body)
        .send()
        .await
        .map_err(|e| {
            // The resolver's refusal of a private address arrives as a
            // connect error; its sentence is the one worth reading.
            let mut reason = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(inner) = source {
                reason = inner.to_string();
                source = inner.source();
            }
            Error::msg(format!(
                "MCP server `{}` could not be reached: {reason}",
                def.name
            ))
        })
}

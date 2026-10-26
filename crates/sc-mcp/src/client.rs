//! The MCP client (TODO-agent-skills AS4.2): `initialize`, `tools/list` and
//! `tools/call` against a [`McpServerDef`], over either transport.
//!
//! **A session per operation.** Listing tools or calling one opens a session
//! (`initialize`, then `notifications/initialized`), makes its request and
//! closes it. Nothing waits between a run's tool calls holding a connection, a
//! server restart between two calls costs nothing, and the definition read at
//! the call is the one used. Listing is what is worth keeping, and is cached
//! (`cache.rs`).
//!
//! **Streamable HTTP** (MCP 2025-06-18): each message is a POST to the URL,
//! answered with JSON or with an event stream that carries the answer (and
//! perhaps notifications first). The server's `Mcp-Session-Id` goes back on
//! every later request, with `MCP-Protocol-Version`, and the session is ended
//! with a DELETE.
//!
//! **HTTP+SSE** (MCP 2024-11-05): a GET of the URL opens an event stream whose
//! first `endpoint` event names where messages are POSTed; the answers arrive
//! on the stream. The endpoint must be on the URL's own origin.
//!
//! Every request carries the definition's headers, is held to its timeout, and
//! goes only where the guard admits.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use reqwest::Url;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use sc_error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use tokio::sync::{Mutex, oneshot};

use crate::def::{McpServerDef, McpTransport};
use crate::guard;
use crate::sse;
use crate::tools::{McpTool, McpToolResult};

/// The protocol revision asked for over streamable HTTP.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The revision the HTTP+SSE transport belongs to.
const SSE_PROTOCOL_VERSION: &str = "2024-11-05";

/// Every revision this client speaks.
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// How many pages of `tools/list` are read before giving up on a server that
/// keeps answering with a cursor.
const MAX_LIST_PAGES: usize = 20;

/// The longest error body quoted back.
const MAX_QUOTED: usize = 500;

/// What a server said about itself when the session opened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerInfo {
    /// Its `serverInfo.name`.
    pub name: String,
    /// Its `serverInfo.version`.
    pub version: String,
    /// The protocol revision agreed.
    pub protocol_version: String,
    /// Its `instructions`, if it gave any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Whether it says its tool list can change (`tools.listChanged`).
    pub tools_list_changed: bool,
}

/// What a connection test found: the server, and its tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerReport {
    /// The server.
    pub server: McpServerInfo,
    /// Every tool it offers.
    pub tools: Vec<McpTool>,
}

/// Open a session, list the tools, close it: the admin screen's *Test*. Also
/// refreshes the cached listing.
pub async fn test_connection(def: &McpServerDef) -> Result<McpServerReport> {
    let session = Session::open(def).await?;
    let tools = session.list_tools().await;
    session.close().await;
    let tools = tools?;
    crate::cache::store(def, &tools, session.info.tools_list_changed);
    Ok(McpServerReport {
        server: session.info.clone(),
        tools,
    })
}

/// Every tool `def` offers, read now.
pub async fn list_tools(def: &McpServerDef) -> Result<Vec<McpTool>> {
    Ok(test_connection(def).await?.tools)
}

/// Every tool `def` offers: the cached listing while it is fresh
/// (`cache.rs`), else read now.
pub async fn tools(def: &McpServerDef) -> Result<Arc<Vec<McpTool>>> {
    if let Some(tools) = crate::cache::fresh(def) {
        return Ok(tools);
    }
    list_tools(def).await.map(Arc::new)
}

/// Call `tool` with `arguments` on `def`, held to the definition's timeout.
///
/// A result the server marks `isError` is an `Ok`, for the model to read; an
/// `Err` is a call that did not happen: no such tool, a refused credential, a
/// server that is down or too slow.
pub async fn call_tool(def: &McpServerDef, tool: &str, arguments: Json) -> Result<McpToolResult> {
    let session = Session::open(def).await?;
    let result = session
        .request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )
        .await;
    session.close().await;
    // The server said its tools changed while this call ran.
    if session.list_changed.load(Ordering::Relaxed) {
        crate::cache::forget(def.id);
    }
    Ok(McpToolResult::from_result(&result?))
}

/// One open session.
struct Session<'a> {
    def: &'a McpServerDef,
    client: reqwest::Client,
    headers: HeaderMap,
    next_id: AtomicU64,
    /// Set when a `notifications/tools/list_changed` arrives.
    list_changed: Arc<AtomicBool>,
    link: Link,
    info: McpServerInfo,
}

/// How messages reach the server.
enum Link {
    Http {
        url: Url,
        session_id: Mutex<Option<String>>,
        protocol: Mutex<Option<String>>,
    },
    Sse {
        post_url: Url,
        pending: Pending,
        reader: tokio::task::JoinHandle<()>,
    },
}

/// The answers an SSE session is waiting for, by request id.
type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Json>>>>;

impl<'a> Session<'a> {
    /// Connect and initialise.
    async fn open(def: &'a McpServerDef) -> Result<Session<'a>> {
        let url = Url::parse(def.url.trim()).map_err(|e| {
            Error::invalid(format!(
                "MCP server `{}`: `{}` is not a URL: {e}",
                def.name, def.url
            ))
        })?;
        guard::check_url(def, &url)?;
        let client = guard::client(def)?;
        let mut headers = HeaderMap::new();
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
            headers.insert(name, value);
        }
        let list_changed = Arc::new(AtomicBool::new(false));
        let (link, asked) = match def.transport {
            McpTransport::StreamableHttp => (
                Link::Http {
                    url,
                    session_id: Mutex::new(None),
                    protocol: Mutex::new(None),
                },
                PROTOCOL_VERSION,
            ),
            McpTransport::Sse => (
                open_sse(def, &client, &headers, url, list_changed.clone()).await?,
                SSE_PROTOCOL_VERSION,
            ),
        };
        let mut session = Session {
            def,
            client,
            headers,
            next_id: AtomicU64::new(1),
            list_changed,
            link,
            info: McpServerInfo {
                name: String::new(),
                version: String::new(),
                protocol_version: String::new(),
                instructions: None,
                tools_list_changed: false,
            },
        };
        let opened = session
            .request(
                "initialize",
                json!({
                    "protocolVersion": asked,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "saltcorn-feldspar",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await;
        let opened = match opened {
            Ok(opened) => opened,
            Err(e) => {
                session.close().await;
                return Err(e);
            }
        };
        let version = opened
            .get("protocolVersion")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned();
        if !SUPPORTED_VERSIONS.contains(&version.as_str()) {
            session.close().await;
            return Err(Error::msg(format!(
                "MCP server `{}` speaks protocol `{version}`; this client speaks {}",
                def.name,
                SUPPORTED_VERSIONS.join(", ")
            )));
        }
        if let Link::Http { protocol, .. } = &session.link {
            *protocol.lock().await = Some(version.clone());
        }
        let text = |pointer: &str| {
            opened
                .pointer(pointer)
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        session.info = McpServerInfo {
            name: text("/serverInfo/name"),
            version: text("/serverInfo/version"),
            protocol_version: version,
            instructions: opened
                .get("instructions")
                .and_then(Json::as_str)
                .map(str::to_owned),
            tools_list_changed: opened
                .pointer("/capabilities/tools/listChanged")
                .and_then(Json::as_bool)
                .unwrap_or(false),
        };
        if opened.pointer("/capabilities/tools").is_none() {
            session.close().await;
            return Err(Error::msg(format!(
                "MCP server `{}` offers no tools",
                def.name
            )));
        }
        session.notify("notifications/initialized").await?;
        Ok(session)
    }

    /// Every tool, following the cursor.
    async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = self.request("tools/list", params).await?;
            tools.extend(
                page.get("tools")
                    .and_then(Json::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(McpTool::from_listing),
            );
            cursor = page
                .get("nextCursor")
                .and_then(Json::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        Err(Error::msg(format!(
            "MCP server `{}` listed more than {MAX_LIST_PAGES} pages of tools",
            self.def.name
        )))
    }

    /// Send a request and wait for its result, within the timeout.
    async fn request(&self, method: &str, params: Json) -> Result<Json> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let timeout = self.def.timeout();
        let answer = tokio::time::timeout(timeout, self.exchange(id, &message))
            .await
            .map_err(|_| {
                Error::msg(format!(
                    "MCP server `{}` did not answer `{method}` within {} seconds",
                    self.def.name,
                    timeout.as_secs()
                ))
            })??;
        if let Some(error) = answer.get("error") {
            return Err(Error::msg(format!(
                "MCP server `{}` refused `{method}`: {} (code {})",
                self.def.name,
                error
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("no message"),
                error.get("code").and_then(Json::as_i64).unwrap_or(0)
            )));
        }
        answer.get("result").cloned().ok_or_else(|| {
            Error::msg(format!(
                "MCP server `{}` answered `{method}` with neither a result nor an error",
                self.def.name
            ))
        })
    }

    /// Send a notification. Nothing comes back.
    async fn notify(&self, method: &str) -> Result<()> {
        let message = json!({ "jsonrpc": "2.0", "method": method });
        match &self.link {
            Link::Http { url, .. } => {
                let response = self.post(url, &message).await?;
                if !response.status().is_success() {
                    return Err(self.refused(method, response).await);
                }
                Ok(())
            }
            Link::Sse { post_url, .. } => {
                let response = self.post(post_url, &message).await?;
                if !response.status().is_success() {
                    return Err(self.refused(method, response).await);
                }
                Ok(())
            }
        }
    }

    /// One request's whole exchange: the answer message with id `id`.
    async fn exchange(&self, id: u64, message: &Json) -> Result<Json> {
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        match &self.link {
            Link::Http {
                url, session_id, ..
            } => {
                let response = self.post(url, message).await?;
                let status = response.status();
                if status == reqwest::StatusCode::NOT_FOUND && session_id.lock().await.is_some() {
                    return Err(Error::msg(format!(
                        "MCP server `{}` ended the session during `{method}`",
                        self.def.name
                    )));
                }
                if !status.is_success() {
                    return Err(self.refused(&method, response).await);
                }
                if let Some(sid) = response
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                {
                    *session_id.lock().await = Some(sid.to_owned());
                }
                let is_stream = response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|t| t.starts_with("text/event-stream"));
                if is_stream {
                    self.answer_from_stream(id, &method, response).await
                } else {
                    let body = response
                        .bytes()
                        .await
                        .map_err(|e| self.failed(&method, &e))?;
                    let answer: Json = serde_json::from_slice(&body).map_err(|e| {
                        self.failed(&method, &format!("the answer is not JSON: {e}"))
                    })?;
                    Ok(answer)
                }
            }
            Link::Sse {
                post_url, pending, ..
            } => {
                let (tx, rx) = oneshot::channel();
                lock(pending).insert(id, tx);
                let response = self.post(post_url, message).await;
                let response = match response {
                    Ok(response) => response,
                    Err(e) => {
                        lock(pending).remove(&id);
                        return Err(e);
                    }
                };
                if !response.status().is_success() {
                    lock(pending).remove(&id);
                    return Err(self.refused(&method, response).await);
                }
                rx.await.map_err(|_| {
                    Error::msg(format!(
                        "MCP server `{}` closed its event stream before answering `{method}`",
                        self.def.name
                    ))
                })
            }
        }
    }

    /// Read a streamed answer until the message answering `id` arrives.
    async fn answer_from_stream(
        &self,
        id: u64,
        method: &str,
        response: reqwest::Response,
    ) -> Result<Json> {
        let mut parser = sse::Parser::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| self.failed(method, &e))?;
            for event in parser.feed(&chunk) {
                if let Some(answer) = self.read_event(id, &event) {
                    return Ok(answer);
                }
            }
        }
        if let Some(event) = parser.finish()
            && let Some(answer) = self.read_event(id, &event)
        {
            return Ok(answer);
        }
        Err(self.failed(method, &"the event stream ended without an answer"))
    }

    /// The answer to `id`, if `event` is it; notes a tool-list change.
    fn read_event(&self, id: u64, event: &sse::Event) -> Option<Json> {
        if event.event != "message" {
            return None;
        }
        let message: Json = serde_json::from_str(&event.data).ok()?;
        note_list_changed(&message, &self.list_changed);
        (message.get("id").and_then(Json::as_u64) == Some(id)).then_some(message)
    }

    /// POST `message` to `url` with the session's headers.
    async fn post(&self, url: &Url, message: &Json) -> Result<reqwest::Response> {
        guard::check_url(self.def, url)?;
        let mut request = self
            .client
            .post(url.clone())
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .json(message);
        if let Link::Http {
            session_id,
            protocol,
            ..
        } = &self.link
        {
            if let Some(sid) = session_id.lock().await.as_deref() {
                request = request.header("Mcp-Session-Id", sid);
            }
            if let Some(version) = protocol.lock().await.as_deref() {
                request = request.header("MCP-Protocol-Version", version);
            }
        }
        let method = message["method"].as_str().unwrap_or_default();
        request.send().await.map_err(|e| self.failed(method, &e))
    }

    /// End the session: a DELETE for streamable HTTP when the server gave a
    /// session id, and closing the stream for SSE. Best effort.
    async fn close(&self) {
        match &self.link {
            Link::Http {
                url, session_id, ..
            } => {
                let Some(sid) = session_id.lock().await.clone() else {
                    return;
                };
                let request = self
                    .client
                    .delete(url.clone())
                    .headers(self.headers.clone())
                    .header("Mcp-Session-Id", sid)
                    .timeout(Duration::from_secs(5));
                let _ = request.send().await;
            }
            Link::Sse { reader, .. } => reader.abort(),
        }
    }

    /// The error for a non-2xx answer, quoting (the start of) its body.
    async fn refused(&self, method: &str, response: reqwest::Response) -> Error {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let body: String = body.chars().take(MAX_QUOTED).collect();
        let hint = match status.as_u16() {
            401 | 403 => "; check the credential in its headers",
            _ => "",
        };
        Error::msg(format!(
            "MCP server `{}` refused `{method}`: {status} {body}{hint}",
            self.def.name
        ))
    }

    /// The error for a request that failed on the way.
    fn failed(&self, method: &str, e: &dyn std::fmt::Display) -> Error {
        Error::msg(format!(
            "MCP server `{}` could not be asked `{method}`: {e}",
            self.def.name
        ))
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Link::Sse { reader, .. } = &self.link {
            reader.abort();
        }
    }
}

/// Open an HTTP+SSE session's event stream, and wait for the endpoint it names.
async fn open_sse(
    def: &McpServerDef,
    client: &reqwest::Client,
    headers: &HeaderMap,
    url: Url,
    list_changed: Arc<AtomicBool>,
) -> Result<Link> {
    let failed = |e: &dyn std::fmt::Display| {
        Error::msg(format!(
            "MCP server `{}`: its event stream could not be opened: {e}",
            def.name
        ))
    };
    let response = tokio::time::timeout(
        def.timeout(),
        client
            .get(url.clone())
            .headers(headers.clone())
            .header(ACCEPT, "text/event-stream")
            .send(),
    )
    .await
    .map_err(|_| failed(&"it did not answer in time"))?
    .map_err(|e| failed(&e))?;
    if !response.status().is_success() {
        let status = response.status();
        let body: String = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(MAX_QUOTED)
            .collect();
        return Err(failed(&format!("{status} {body}")));
    }
    let pending: Pending = Arc::default();
    let (endpoint_tx, endpoint_rx) = oneshot::channel::<String>();
    let reader = {
        let pending = pending.clone();
        tokio::spawn(async move {
            let mut endpoint_tx = Some(endpoint_tx);
            let mut parser = sse::Parser::default();
            let mut stream = response.bytes_stream();
            while let Some(Ok(chunk)) = stream.next().await {
                for event in parser.feed(&chunk) {
                    if event.event == "endpoint" {
                        if let Some(tx) = endpoint_tx.take() {
                            let _ = tx.send(event.data.trim().to_owned());
                        }
                        continue;
                    }
                    let Ok(message) = serde_json::from_str::<Json>(&event.data) else {
                        continue;
                    };
                    note_list_changed(&message, &list_changed);
                    if let Some(id) = message.get("id").and_then(Json::as_u64)
                        && (message.get("result").is_some() || message.get("error").is_some())
                        && let Some(tx) = lock(&pending).remove(&id)
                    {
                        let _ = tx.send(message);
                    }
                }
            }
            // The stream ended: every waiting request learns so.
            lock(&pending).clear();
        })
    };
    let endpoint = tokio::time::timeout(def.timeout(), endpoint_rx)
        .await
        .map_err(|_| failed(&"it named no endpoint in time"))
        .and_then(|r| r.map_err(|_| failed(&"it closed before naming an endpoint")));
    let endpoint = match endpoint {
        Ok(endpoint) => endpoint,
        Err(e) => {
            reader.abort();
            return Err(e);
        }
    };
    let post_url = url.join(&endpoint).map_err(|e| {
        reader.abort();
        failed(&format!("its endpoint `{endpoint}` is not a URL: {e}"))
    })?;
    if post_url.origin() != url.origin() {
        reader.abort();
        return Err(Error::auth(format!(
            "MCP server `{}` named an endpoint on another origin ({post_url}), which is refused",
            def.name
        )));
    }
    Ok(Link::Sse {
        post_url,
        pending,
        reader,
    })
}

/// Note a `notifications/tools/list_changed` message.
fn note_list_changed(message: &Json, flag: &AtomicBool) {
    if message.get("method").and_then(Json::as_str) == Some("notifications/tools/list_changed") {
        flag.store(true, Ordering::Relaxed);
    }
}

/// A lock that survives a poisoned mutex: the map is a set of waiting
/// requests, and a panic elsewhere has already failed theirs.
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

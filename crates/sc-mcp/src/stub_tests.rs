//! The client against stub servers (TODO-agent-skills AS4.6): what this
//! server's own MCP endpoint never does — answer with an event stream, hand out
//! a session id, page its tool list, announce a change, speak HTTP+SSE — is
//! exercised here. `sc-server`'s tests run the client against the real thing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::def::{McpServerDef, McpTransport};

/// One request as the stub read it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Json,
}

/// Read one HTTP request off `socket`.
async fn read_request(socket: &mut TcpStream) -> Option<Seen> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let path = first.next()?.to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = serde_json::from_slice(&buf[head_end..]).unwrap_or(Json::Null);
    Some(Seen {
        method,
        path,
        headers,
        body,
    })
}

async fn respond(socket: &mut TcpStream, status: &str, headers: &str, body: &str) {
    let reply = format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(reply.as_bytes()).await;
    let _ = socket.flush().await;
}

fn rpc_result(id: &Json, result: Json) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn initialized(version: &str, list_changed: bool) -> Json {
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": list_changed } },
        "serverInfo": { "name": "stub", "version": "1.2.3" },
        "instructions": "Be brief.",
    })
}

/// A streamable HTTP server that answers in event streams, keeps a session,
/// pages its tools and announces a change during a call.
async fn streamable_stub() -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else {
                    return;
                };
                log.lock().unwrap().push(request.clone());
                let stream = "Content-Type: text/event-stream\r\n";
                if request.method == "DELETE" {
                    return respond(&mut socket, "200 OK", "", "").await;
                }
                let id = request.body["id"].clone();
                let method = request.body["method"].as_str().unwrap_or_default();
                if method != "initialize"
                    && request.headers.get("mcp-session-id").map(String::as_str) != Some("s-1")
                {
                    return respond(&mut socket, "400 Bad Request", "", "no session").await;
                }
                match method {
                    "initialize" => {
                        let body = format!(
                            ": hello\n\nevent: message\ndata: {}\n\n",
                            rpc_result(&id, initialized("2025-06-18", true))
                        );
                        let headers = format!("{stream}Mcp-Session-Id: s-1\r\n");
                        respond(&mut socket, "200 OK", &headers, &body).await;
                    }
                    "notifications/initialized" => {
                        respond(&mut socket, "202 Accepted", "", "").await;
                    }
                    "tools/list" => {
                        let page = if request.body["params"]["cursor"] == "2" {
                            json!({ "tools": [{ "name": "second", "inputSchema": {"type": "object"} }] })
                        } else {
                            json!({
                                "tools": [{ "name": "first", "description": "One",
                                            "annotations": { "readOnlyHint": true } }],
                                "nextCursor": "2",
                            })
                        };
                        let body = rpc_result(&id, page);
                        respond(
                            &mut socket,
                            "200 OK",
                            "Content-Type: application/json\r\n",
                            &body,
                        )
                        .await;
                    }
                    "tools/call" => {
                        let changed = json!({ "jsonrpc": "2.0",
                                              "method": "notifications/tools/list_changed" });
                        let result = rpc_result(
                            &id,
                            json!({ "content": [{ "type": "text",
                                                  "text": format!("echo {}", request.body["params"]["arguments"]["say"]) }] }),
                        );
                        let body = format!("data: {changed}\n\ndata: {result}\n\n");
                        respond(&mut socket, "200 OK", stream, &body).await;
                    }
                    _ => respond(&mut socket, "404 Not Found", "", "").await,
                }
            });
        }
    });
    (format!("http://{addr}/mcp"), seen)
}

#[tokio::test]
async fn streamable_http_with_event_streams_a_session_and_a_paged_list() {
    let (url, seen) = streamable_stub().await;
    let def = McpServerDef::new("stub", &url)
        .private_network(true)
        .header("X-Api-Key", "k-1");

    let report = crate::test_connection(&def).await.unwrap();
    assert_eq!(report.server.name, "stub");
    assert_eq!(report.server.version, "1.2.3");
    assert_eq!(report.server.protocol_version, "2025-06-18");
    assert_eq!(report.server.instructions.as_deref(), Some("Be brief."));
    assert!(report.server.tools_list_changed);
    let names: Vec<&str> = report.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["first", "second"]);
    assert!(report.tools[0].read_only && !report.tools[1].read_only);

    {
        let seen = seen.lock().unwrap();
        // Every request carried the header; every one after `initialize`
        // carried the session and the protocol; the session was ended.
        assert!(
            seen.iter()
                .all(|r| r.headers.get("x-api-key").map(String::as_str) == Some("k-1"))
        );
        let after: Vec<&Seen> = seen.iter().filter(|r| r.method == "POST").skip(1).collect();
        assert!(after.iter().all(
            |r| r.headers.get("mcp-protocol-version").map(String::as_str) == Some("2025-06-18")
        ));
        assert_eq!(seen.last().unwrap().method, "DELETE");
        assert!(seen.iter().all(|r| r.path == "/mcp"));
    }

    // The listing is cached until the server announces a change.
    assert!(crate::cache::fresh(&def).is_some());
    let result = crate::call_tool(&def, "first", json!({ "say": "hi" }))
        .await
        .unwrap();
    assert_eq!(result.text, "echo \"hi\"");
    assert!(!result.is_error);
    assert!(
        crate::cache::fresh(&def).is_none(),
        "list_changed forgets it"
    );
}

/// An HTTP+SSE server: a GET opens the stream, which names `/messages`, and
/// each POST there is answered on the stream.
async fn sse_stub(endpoint: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stream_tx: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>> = Arc::default();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let stream_tx = stream_tx.clone();
            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else {
                    return;
                };
                if request.method == "GET" {
                    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<String>();
                    *stream_tx.lock().unwrap() = Some(events_tx);
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let first = format!("event: endpoint\ndata: {endpoint}\n\n");
                    let _ = socket.write_all(first.as_bytes()).await;
                    let _ = socket.flush().await;
                    while let Some(event) = events_rx.recv().await {
                        let _ = socket.write_all(event.as_bytes()).await;
                        let _ = socket.flush().await;
                    }
                    return;
                }
                respond(&mut socket, "202 Accepted", "", "").await;
                let id = request.body["id"].clone();
                let answer = match request.body["method"].as_str().unwrap_or_default() {
                    "initialize" => Some(rpc_result(&id, initialized("2024-11-05", false))),
                    "tools/list" => Some(rpc_result(
                        &id,
                        json!({ "tools": [{ "name": "legacy_tool" }] }),
                    )),
                    "tools/call" => Some(rpc_result(
                        &id,
                        json!({ "content": [{ "type": "text", "text": "done" }], "isError": true }),
                    )),
                    _ => None,
                };
                if let Some(answer) = answer
                    && let Some(events) = stream_tx.lock().unwrap().as_ref()
                {
                    let _ = events.send(format!("event: message\ndata: {answer}\n\n"));
                }
            });
        }
    });
    format!("http://{addr}/sse")
}

#[tokio::test]
async fn the_sse_transport_posts_where_the_stream_says_and_reads_answers_from_it() {
    let url = sse_stub("/messages?session=1").await;
    let def = McpServerDef::new("legacy", &url)
        .private_network(true)
        .transport(McpTransport::Sse);
    let report = crate::test_connection(&def).await.unwrap();
    assert_eq!(report.server.protocol_version, "2024-11-05");
    assert_eq!(report.tools[0].name, "legacy_tool");
    // A result the server marks an error is a result, for the model to read.
    let result = crate::call_tool(&def, "legacy_tool", json!({}))
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(result.text, "done");
}

#[tokio::test]
async fn an_endpoint_on_another_origin_is_refused() {
    let url = sse_stub("http://169.254.169.254/steal").await;
    let def = McpServerDef::new("legacy", &url)
        .private_network(true)
        .transport(McpTransport::Sse);
    let err = crate::test_connection(&def).await.unwrap_err().to_string();
    assert!(err.contains("another origin"), "{err}");
}

#[tokio::test]
async fn a_private_address_is_refused_before_anything_is_sent_and_a_slow_server_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(Mutex::new(0usize));
    let count = accepted.clone();
    tokio::spawn(async move {
        // Accept and say nothing.
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            *count.lock().unwrap() += 1;
            held.push(socket);
        }
    });
    let mut def = McpServerDef::new("quiet", format!("http://{addr}/mcp"));
    let err = crate::test_connection(&def).await.unwrap_err().to_string();
    assert!(err.contains("private or reserved"), "{err}");
    assert_eq!(*accepted.lock().unwrap(), 0, "nothing was sent");

    def.allow_private_network = true;
    def.timeout_secs = 1;
    let err = crate::test_connection(&def).await.unwrap_err().to_string();
    assert!(
        err.contains("did not answer `initialize` within 1 seconds"),
        "{err}"
    );
}

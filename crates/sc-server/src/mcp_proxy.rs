//! The installation's MCP servers, for the IDE's Claude Code sessions (design
//! §12.1, "The installation's other MCP servers, through a proxy").
//!
//! `/ide/mcp-proxy/{server}` forwards a session's MCP traffic to a stored
//! server ([`sc_mcp::proxy::forward`]), which adds the server's headers on the
//! way out. The session's MCP config names this route, not the server, so the
//! credential in those headers never reaches a file or a process the admin's
//! shell can read.
//!
//! Only an open terminal's session token opens it, from loopback only: the
//! sessions are this server's children, and nothing else has a reason to be
//! here. A table token is refused, whatever it may do on `/mcp`. Only a server
//! whose definition offers it to the terminal is forwarded to.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::mcp::{MAX_BODY_BYTES, is_loopback, terminal_caller};
use crate::router::AppState;

/// The route a session's MCP config names for each offered server.
pub const MCP_PROXY_ROUTE: &str = "/ide/mcp-proxy/{server}";

/// The URL of `server`'s proxy, under `origin` (`http://127.0.0.1:<port>`).
pub(crate) fn proxy_url(origin: &str, server: &str) -> String {
    format!("{origin}/ide/mcp-proxy/{server}")
}

/// Forward one request to the offered server `server`.
pub(crate) async fn mcp_proxy(
    State(state): State<AppState>,
    Path(server): Path<String>,
    request: Request,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .copied();
    let method = request.method().clone();
    let headers = request.headers().clone();
    let Some(catalog) = state.apps.catalog().cloned() else {
        return answer(StatusCode::NOT_FOUND, "this server has no catalog");
    };
    let Some(grant) = crate::security::bearer_token(&headers)
        .and_then(|presented| state.terminal_tokens.grant(&presented))
    else {
        return answer(
            StatusCode::UNAUTHORIZED,
            "this route takes an open IDE terminal's session token and nothing else",
        );
    };
    if headers.contains_key(header::ORIGIN) {
        return answer(
            StatusCode::FORBIDDEN,
            "this route does not serve browser requests",
        );
    }
    if !is_loopback(peer) {
        return answer(
            StatusCode::FORBIDDEN,
            "a terminal's session token is accepted only from the server's own machine",
        );
    }
    if let Err(e) = terminal_caller(&catalog, &grant).await {
        return answer(StatusCode::UNAUTHORIZED, &e.to_string());
    }
    let def = match sc_mcp::load_mcp_server_by_name(&catalog, &server).await {
        Ok(Some(def)) if def.offered_to_terminal() => def,
        Ok(_) => {
            return answer(
                StatusCode::NOT_FOUND,
                &format!("no MCP server named `{server}` is offered to the IDE's terminal"),
            );
        }
        Err(e) => return answer(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return answer(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("a request body here is at most {MAX_BODY_BYTES} bytes"),
            );
        }
    };

    let upstream = match sc_mcp::proxy::forward(&def, method, &headers, body).await {
        Ok(upstream) => upstream,
        Err(e) => return answer(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    let mut response = Response::builder().status(upstream.status().as_u16());
    for name in sc_mcp::proxy::FORWARDED_RESPONSE_HEADERS {
        for value in upstream.headers().get_all(name) {
            response = response.header(name, value.clone());
        }
    }
    // Streamed, so an answer that is an event stream arrives as it is written.
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|e| answer(StatusCode::BAD_GATEWAY, &e.to_string()))
}

/// A refusal, as JSON with the sentence in it.
fn answer(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

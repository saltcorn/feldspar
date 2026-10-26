//! `sc-mcp` — the MCP client: remote Model Context Protocol servers this one
//! calls (TODO-agent-skills AS4).
//!
//! The server is itself an MCP server (`sc-server` `mcp.rs`); this is the other
//! direction, for the `mcp_server` agent trait (AS9.1), which offers a remote
//! server's tools to an agent. The server makes the calls itself rather than
//! handing the URL to a vendor's hosted MCP tool, so the same trait works with
//! every LLM backend, runs as the caller, and is ledgered like any other tool.
//!
//! ## What is here
//!
//! - The **definition** ([`def`]): [`McpServerDef`], a name, URL, transport
//!   and headers whose values are secrets, with [`merge_headers`] for the
//!   sentinel a form sends back.
//! - **Storage** ([`storage`]): `_fd_mcp_servers`, as `_fd_llm_providers` is.
//! - The **client** ([`client`]): `initialize`, `tools/list` and `tools/call`
//!   over streamable HTTP or HTTP+SSE, held to the definition's timeout,
//!   results turned into text and images ([`tools`]).
//! - The **guard** (`guard.rs`): public addresses only unless the definition
//!   allows the private network, checked as names resolve; no redirects.
//! - The **cache** ([`cache`]): each server's tool list, kept while fresh.
//!
//! Layer 6, beside `sc-llm`: it needs a `Catalog` for its table and nothing
//! about agents.

pub mod cache;
pub mod client;
pub mod def;
mod guard;
mod sse;
pub mod storage;
#[cfg(test)]
mod stub_tests;
pub mod tools;

pub use client::{
    McpServerInfo, McpServerReport, PROTOCOL_VERSION, call_tool, list_tools, test_connection, tools,
};
pub use def::{
    DEFAULT_TIMEOUT_SECS, MAX_TIMEOUT_SECS, McpServerDef, McpServerDefId, McpTransport,
    merge_headers, validate_server,
};
pub use storage::{
    MCP_SERVERS_TABLE, bootstrap_mcp_servers, delete_mcp_server, list_mcp_servers, load_mcp_server,
    load_mcp_server_by_name, require_mcp_server, save_mcp_server,
};
pub use tools::{McpImage, McpTool, McpToolResult};

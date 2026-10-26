//! [`McpServerDef`]: the stored definition of a remote MCP server this one
//! calls (TODO-agent-skills AS4.1).
//!
//! A name, a URL, a transport and the headers each request carries, which is
//! where a server's credential goes (`Authorization: Bearer …`, `X-Api-Key`).
//! A header's **value** is a secret, kept exactly as an LLM provider's API key
//! is: stored in the primary database, never returned by the admin API (each
//! value reads back as [`SECRET_SENTINEL`]), and kept when a save hands the
//! sentinel back ([`merge_headers`]).

use std::collections::BTreeMap;

use sc_error::{Error, Result};
use sc_types::{Attrs, SECRET_SENTINEL};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The longest name a server may have: its tools are offered to a model as
/// `<server>__<tool>`, and a tool name is at most 64 characters.
pub const MAX_NAME_CHARS: usize = 24;

/// How long a call may take when the definition does not say.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// The longest a definition may let a call take.
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// Headers the client sets itself, which a definition may not.
const RESERVED_HEADERS: [&str; 7] = [
    "accept",
    "content-type",
    "content-length",
    "host",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// Identifies a stored definition. A UUID, per §9.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct McpServerDefId(pub Uuid);

impl McpServerDefId {
    /// A fresh random id, for a definition that has never been saved.
    pub fn new() -> McpServerDefId {
        McpServerDefId(Uuid::new_v4())
    }
}

impl Default for McpServerDefId {
    fn default() -> McpServerDefId {
        McpServerDefId::new()
    }
}

/// How the server is spoken to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// MCP 2025-06-18's *streamable HTTP*: every message a POST to the URL,
    /// answered with JSON or a short event stream.
    #[default]
    StreamableHttp,
    /// MCP 2024-11-05's *HTTP+SSE*: an event stream from a GET of the URL,
    /// which names the endpoint messages are POSTed to.
    Sse,
}

impl McpTransport {
    /// The transport's name, as stored and as the API spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            McpTransport::StreamableHttp => "streamable_http",
            McpTransport::Sse => "sse",
        }
    }

    /// The transport called `name`; blank is streamable HTTP.
    pub fn parse(name: &str) -> Result<McpTransport> {
        match name.trim() {
            "" | "streamable_http" => Ok(McpTransport::StreamableHttp),
            "sse" => Ok(McpTransport::Sse),
            other => Err(Error::invalid(format!(
                "unknown MCP transport `{other}`; the transports are streamable_http and sse"
            ))),
        }
    }
}

/// The stored definition of one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerDef {
    /// The row's identity.
    pub id: McpServerDefId,
    /// What a trait names it by, and the prefix of its tools. Unique.
    pub name: String,
    /// A human description; empty when none was given.
    pub description: String,
    /// The server's endpoint: the URL POSTed to (streamable HTTP) or the event
    /// stream (SSE).
    pub url: String,
    /// How it is spoken to.
    pub transport: McpTransport,
    /// Headers every request carries, by name. The values are secrets.
    pub headers: BTreeMap<String, String>,
    /// Whether the server may be on a loopback or private address, as the
    /// `http` trait's own setting (`guard.rs`). Off, a URL that is or resolves
    /// to one is refused.
    pub allow_private_network: bool,
    /// How long one call may take, in seconds; 0 is
    /// [`DEFAULT_TIMEOUT_SECS`].
    pub timeout_secs: u64,
    /// Sparse per-server values (§9).
    pub attributes: Attrs,
}

impl McpServerDef {
    /// A streamable HTTP server called `name` at `url`, with no headers.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> McpServerDef {
        McpServerDef {
            id: McpServerDefId::new(),
            name: name.into(),
            description: String::new(),
            url: url.into(),
            transport: McpTransport::StreamableHttp,
            headers: BTreeMap::new(),
            allow_private_network: false,
            timeout_secs: 0,
            attributes: Attrs::new(),
        }
    }

    /// Add a header, returning `self` for chaining.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> McpServerDef {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Set the transport, returning `self` for chaining.
    pub fn transport(mut self, transport: McpTransport) -> McpServerDef {
        self.transport = transport;
        self
    }

    /// Allow a private address, returning `self` for chaining.
    pub fn private_network(mut self, allow: bool) -> McpServerDef {
        self.allow_private_network = allow;
        self
    }

    /// How long one call may take.
    pub fn timeout(&self) -> std::time::Duration {
        let secs = match self.timeout_secs {
            0 => DEFAULT_TIMEOUT_SECS,
            n => n.min(MAX_TIMEOUT_SECS),
        };
        std::time::Duration::from_secs(secs)
    }

    /// The definition with every header value replaced by the sentinel — what
    /// the admin API returns.
    pub fn redacted(&self) -> McpServerDef {
        McpServerDef {
            headers: self
                .headers
                .keys()
                .map(|k| (k.clone(), SECRET_SENTINEL.to_owned()))
                .collect(),
            ..self.clone()
        }
    }
}

/// The headers a save submits, with each value that is the sentinel replaced
/// by the stored value of the same header (compared without case). A sentinel
/// with nothing stored behind it is refused: it would send the mask itself.
pub fn merge_headers(
    stored: &BTreeMap<String, String>,
    submitted: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    submitted
        .iter()
        .map(|(name, value)| {
            if value != SECRET_SENTINEL {
                return Ok((name.clone(), value.clone()));
            }
            stored
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| (name.clone(), v.clone()))
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "header `{name}` has no stored value to keep; type its value"
                    ))
                })
        })
        .collect()
}

/// Check what a save can check without a network: a name usable as a tool
/// prefix, an http(s) URL with no credentials in it, headers that are headers
/// and not ones the client sets, and a timeout in range.
pub fn validate_server(def: &McpServerDef) -> Result<()> {
    let name = def.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("an MCP server needs a name"));
    }
    if name.chars().count() > MAX_NAME_CHARS
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        || name.contains("__")
    {
        return Err(Error::invalid(format!(
            "MCP server name `{name}` must be at most {MAX_NAME_CHARS} lowercase letters, \
             digits, `-` and `_`, without `__`: its tools are offered as `{name}__<tool>`"
        )));
    }
    let url = reqwest::Url::parse(def.url.trim()).map_err(|e| {
        Error::invalid(format!(
            "MCP server `{name}`: `{}` is not a URL: {e}",
            def.url
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::invalid(format!(
            "MCP server `{name}`: only http and https URLs can be used, not `{}:`",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::invalid(format!(
            "MCP server `{name}`: put a credential in a header, not in the URL"
        )));
    }
    for (header, value) in &def.headers {
        if header.is_empty()
            || !header
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        {
            return Err(Error::invalid(format!(
                "MCP server `{name}`: `{header}` is not a header name"
            )));
        }
        if RESERVED_HEADERS.contains(&header.to_ascii_lowercase().as_str()) {
            return Err(Error::invalid(format!(
                "MCP server `{name}`: header `{header}` is set by the client itself"
            )));
        }
        if value == SECRET_SENTINEL {
            return Err(Error::invalid(format!(
                "MCP server `{name}`: header `{header}` has no value; type it"
            )));
        }
        if reqwest::header::HeaderValue::from_str(value).is_err() {
            return Err(Error::invalid(format!(
                "MCP server `{name}`: the value of header `{header}` is not one a header can hold"
            )));
        }
    }
    if def.timeout_secs > MAX_TIMEOUT_SECS {
        return Err(Error::invalid(format!(
            "MCP server `{name}`: a call may take at most {MAX_TIMEOUT_SECS} seconds"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_value_never_leaves_and_survives_a_save_that_did_not_touch_it() {
        let stored = McpServerDef::new("docs", "https://mcp.example.com/mcp")
            .header("Authorization", "Bearer s3cret");
        let redacted = stored.redacted();
        assert_eq!(redacted.headers["Authorization"], SECRET_SENTINEL);
        // The form sends the sentinel back, with the name in another case.
        let mut submitted = BTreeMap::new();
        submitted.insert("authorization".to_owned(), SECRET_SENTINEL.to_owned());
        submitted.insert("X-Team".to_owned(), "blue".to_owned());
        let merged = merge_headers(&stored.headers, &submitted).unwrap();
        assert_eq!(merged["authorization"], "Bearer s3cret");
        assert_eq!(merged["X-Team"], "blue");
        // A new header that is the sentinel has nothing to keep.
        let mut fresh = BTreeMap::new();
        fresh.insert("X-Key".to_owned(), SECRET_SENTINEL.to_owned());
        assert!(merge_headers(&stored.headers, &fresh).is_err());
    }

    #[test]
    fn what_cannot_work_is_refused_on_save() {
        let ok = McpServerDef::new("docs", "https://mcp.example.com/mcp").header("X-Key", "k");
        validate_server(&ok).unwrap();
        for (def, needle) in [
            (McpServerDef::new("", "https://x/"), "needs a name"),
            (McpServerDef::new("Docs", "https://x/"), "lowercase"),
            (McpServerDef::new("a__b", "https://x/"), "without `__`"),
            (McpServerDef::new("docs", "ftp://x/"), "http and https"),
            (McpServerDef::new("docs", "not a url"), "not a URL"),
            (
                McpServerDef::new("docs", "https://u:p@x/"),
                "credential in a header",
            ),
            (
                McpServerDef::new("docs", "https://x/").header("Mcp-Session-Id", "1"),
                "set by the client",
            ),
            (
                McpServerDef::new("docs", "https://x/").header("bad header", "1"),
                "not a header name",
            ),
            (
                McpServerDef::new("docs", "https://x/").header("X-Key", "a\nb"),
                "not one a header can hold",
            ),
        ] {
            let err = validate_server(&def).unwrap_err().to_string();
            assert!(err.contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn the_timeout_defaults_and_transports_parse() {
        let def = McpServerDef::new("docs", "https://x/");
        assert_eq!(def.timeout().as_secs(), DEFAULT_TIMEOUT_SECS);
        assert_eq!(
            McpTransport::parse("").unwrap(),
            McpTransport::StreamableHttp
        );
        assert_eq!(McpTransport::parse("sse").unwrap(), McpTransport::Sse);
        assert!(McpTransport::parse("stdio").is_err());
    }
}

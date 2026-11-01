//! Where an MCP server may be (TODO-agent-skills AS4.4): a public address,
//! unless its definition allows the private network.
//!
//! The rule the `http` trait keeps, for the same reason: what a tool call
//! sends is written by a model, and loopback, the private ranges and
//! link-local (a cloud's metadata endpoint) are where a server's own secrets
//! live. The rule itself is `sc-net`'s (TODO-code-quality Q4.3): an address
//! literal is refused before anything is sent, and a name is checked as it
//! resolves ([`sc_net::PublicClient`]), so the address checked is the address
//! connected to. Redirects are not followed at all.

use reqwest::{Method, RequestBuilder, Url};
use sc_error::{Error, Result};

use crate::def::McpServerDef;

/// Check `url` for `def`: http(s), and a public address literal unless the
/// definition allows the private network.
pub(crate) fn check_url(def: &McpServerDef, url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::invalid(format!(
            "MCP server `{}`: only http and https URLs can be used, not `{}:`",
            def.name,
            url.scheme()
        )));
    }
    if !def.allow_private_network
        && let Some(ip) = sc_net::private_literal(url)
    {
        return Err(Error::auth(format!(
            "MCP server `{}` is at `{ip}`, a private or reserved address; allow the private \
             network on its definition to reach it",
            def.name
        )));
    }
    Ok(())
}

/// The client a session sends with: one that only connects to a public
/// address ([`sc_net::PublicClient`]), or — when the definition allows the
/// private network — one that connects where it is told.
#[derive(Clone)]
pub(crate) enum McpClient {
    Public(sc_net::PublicClient),
    Open(reqwest::Client),
}

impl McpClient {
    /// A request to `url` (refused for a private literal by the public client).
    pub(crate) fn request(&self, method: Method, url: Url) -> Result<RequestBuilder> {
        match self {
            McpClient::Public(client) => client.request(method, url),
            McpClient::Open(client) => Ok(client.request(method, url)),
        }
    }
}

/// A client for `def`: no redirects, and when the private network is not
/// allowed, public addresses only.
pub(crate) fn client(def: &McpServerDef) -> Result<McpClient> {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15));
    if def.allow_private_network {
        return builder
            .build()
            .map(McpClient::Open)
            .map_err(|e| Error::msg(format!("building the MCP client: {e}")));
    }
    sc_net::PublicClient::new(
        builder,
        &format!(
            "; allow the private network on MCP server `{}` to reach it",
            def.name
        ),
        sc_net::Redirects::None,
    )
    .map(McpClient::Public)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_private_literal_is_refused_unless_allowed() {
        let def = McpServerDef::new("local", "http://127.0.0.1:3000/mcp");
        let url = Url::parse(&def.url).unwrap();
        let err = check_url(&def, &url).unwrap_err().to_string();
        assert!(err.contains("private or reserved"), "{err}");
        assert!(check_url(&def.clone().private_network(true), &url).is_ok());
        let metadata = Url::parse("http://169.254.169.254/").unwrap();
        assert!(check_url(&def, &metadata).is_err());
        let public = Url::parse("https://mcp.example.com/mcp").unwrap();
        assert!(check_url(&def, &public).is_ok());
        // The client a public-only definition sends with refuses the literal
        // too, whoever forgets to check it first.
        let err = client(&def)
            .unwrap()
            .request(Method::GET, url.clone())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("allow the private network"), "{err}");
        assert!(
            client(&def.clone().private_network(true))
                .unwrap()
                .request(Method::GET, url)
                .is_ok()
        );
    }
}

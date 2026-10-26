//! Where an MCP server may be (TODO-agent-skills AS4.4): a public address,
//! unless its definition allows the private network.
//!
//! The rule the `http` trait keeps (`sc-core-traits` `http/guard.rs`), for the
//! same reason: what a tool call sends is written by a model, and loopback,
//! the private ranges and link-local (a cloud's metadata endpoint) are where a
//! server's own secrets live. An address literal is refused before anything is
//! sent; a name is checked as it resolves ([`PublicResolver`]), so the address
//! checked is the address connected to. Redirects are not followed at all.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sc_email::is_public;
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
        && let Some(ip) = literal_ip(url)
        && !is_public(ip)
    {
        return Err(Error::auth(format!(
            "MCP server `{}` is at `{ip}`, a private or reserved address; allow the private \
             network on its definition to reach it",
            def.name
        )));
    }
    Ok(())
}

/// A client for `def`: no redirects, and when the private network is not
/// allowed, a resolver that answers with public addresses only.
pub(crate) fn client(def: &McpServerDef) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15));
    if !def.allow_private_network {
        builder = builder.dns_resolver(Arc::new(PublicResolver {
            server: def.name.clone(),
        }));
    }
    builder
        .build()
        .map_err(|e| Error::msg(format!("building the MCP client: {e}")))
}

fn literal_ip(url: &Url) -> Option<IpAddr> {
    url.host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// A resolver that answers with public addresses only.
struct PublicResolver {
    server: String,
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let server = self.server.clone();
        Box::pin(async move {
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = found
                .iter()
                .copied()
                .filter(|addr| is_public(addr.ip()))
                .collect();
            if public.is_empty() {
                let message = if found.is_empty() {
                    format!("`{host}` did not resolve to any address")
                } else {
                    format!(
                        "`{host}` resolves only to private or reserved addresses; allow the \
                         private network on MCP server `{server}` to reach it"
                    )
                };
                return Err(message.into());
            }
            let addrs: Addrs = Box::new(public.into_iter());
            Ok(addrs)
        })
    }
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
    }
}

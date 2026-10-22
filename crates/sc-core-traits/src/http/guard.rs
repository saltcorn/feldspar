//! Where an agent's request may go.
//!
//! The `fetch` action and a code body's `fetch` reach whatever the server can
//! reach, and say why: an administrator wrote the URL. An agent's URL is written
//! by a **model**, and the model may be reading a page that tells it where to go
//! next. So the question this module answers is the one those two can skip:
//! "is this somewhere a model may send the server?" Two rules, both checked on
//! **every hop** of a redirect chain:
//!
//! - **The host allow-list**, when the admin gave one. `example.com` admits
//!   `example.com` and every subdomain of it — the rule the Claude API's own web
//!   fetch uses, and the one an admin means by naming a documentation site.
//! - **Public addresses only**, unless the admin turned that off. Loopback, the
//!   private ranges, link-local (which is where a cloud's metadata endpoint
//!   lives, and its credentials with it), CGNAT, multicast and the reserved
//!   blocks are refused. An address *literal* is checked here, before anything
//!   is sent; a *name* is checked by [`PublicResolver`] as it is resolved, so a
//!   public name that resolves to `127.0.0.1` is refused too, and the address
//!   checked is the address connected to.

use std::net::{IpAddr, SocketAddr};

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sc_error::{Error, Result};

/// The longest URL a model may ask for. Longer than any documentation link, and
/// short enough that a URL is not a channel for a page of the conversation.
pub const MAX_URL_CHARS: usize = 2_000;

/// What an instance of the trait may reach.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostPolicy {
    /// Hosts, lowercased, each admitting itself and its subdomains. Empty
    /// admits every host.
    allowed: Vec<String>,
    /// Whether loopback and private addresses may be reached.
    private_network: bool,
}

impl HostPolicy {
    /// A policy over `allowed` hosts (empty for any), reaching private
    /// addresses only when `private_network` is set.
    pub fn new(allowed: Vec<String>, private_network: bool) -> HostPolicy {
        HostPolicy {
            allowed,
            private_network,
        }
    }

    /// The allow-list, as configured.
    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    /// Whether any host is admitted.
    pub fn is_open(&self) -> bool {
        self.allowed.is_empty()
    }

    /// Whether private addresses may be reached.
    pub fn private_network(&self) -> bool {
        self.private_network
    }

    /// Whether `url` may be requested: a scheme, a host and an address this
    /// policy admits. Called on the URL the model sent and on every redirect.
    pub fn check(&self, url: &Url) -> Result<()> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::invalid(format!(
                "only http and https URLs can be fetched, not `{}:`",
                url.scheme()
            )));
        }
        if url.as_str().len() > MAX_URL_CHARS {
            return Err(Error::invalid(format!(
                "the URL is {} characters long; the most is {MAX_URL_CHARS}",
                url.as_str().len()
            )));
        }
        // A password in a URL is either a mistake or a way to carry something
        // out of the conversation; neither is what reading a page needs.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::invalid(
                "a URL with a user name or password in it cannot be fetched",
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::invalid(format!("`{url}` has no host")))?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if !self.admits_host(&host) {
            return Err(Error::auth(format!(
                "`{host}` is not one of the hosts this agent may fetch from: {}",
                self.allowed.join(", ")
            )));
        }
        if !self.private_network
            && let Some(ip) = literal_ip(url)
            && !is_public(ip)
        {
            return Err(Error::auth(format!(
                "`{ip}` is a private or reserved address, which this agent may not reach"
            )));
        }
        Ok(())
    }

    /// Whether `host` is on the allow-list (or there is none).
    fn admits_host(&self, host: &str) -> bool {
        self.allowed.is_empty()
            || self.allowed.iter().any(|allowed| {
                host == allowed
                    || host
                        .strip_suffix(allowed.as_str())
                        .is_some_and(|rest| rest.ends_with('.'))
            })
    }
}

/// Parse the allow-list setting: host names separated by commas, spaces or
/// lines. An entry with a scheme or a path is refused by name rather than
/// silently trimmed, because an admin who wrote `https://docs.rs/serde` meant
/// something narrower than all of `docs.rs`, and the difference should be
/// theirs to decide.
pub fn parse_allowed_hosts(text: &str) -> Result<Vec<String>> {
    let mut hosts = Vec::new();
    for entry in text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|e| !e.is_empty())
    {
        let host = entry.trim_end_matches('.').to_ascii_lowercase();
        if host.contains("://") || host.contains('/') {
            return Err(Error::invalid(format!(
                "`{entry}` is not a host name: list hosts only, such as `docs.rs` \
                 (each admits its subdomains too)"
            )));
        }
        if let Some(bare) = host.strip_prefix("*.") {
            return Err(Error::invalid(format!(
                "`{entry}`: write `{bare}` instead — a host admits its subdomains already"
            )));
        }
        if !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
        {
            return Err(Error::invalid(format!("`{entry}` is not a host name")));
        }
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    Ok(hosts)
}

/// The address a URL names literally, if it names one.
fn literal_ip(url: &Url) -> Option<IpAddr> {
    // The URL parser has already normalised the host: an IPv4 literal in any
    // of its spellings (`0x7f.1`, `2130706433`) is dotted-quad by now, and an
    // IPv6 one is bracketed.
    let host = url.host_str()?;
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Whether `ip` is an address on the public internet — the one definition,
/// shared with an SMTP override's check (`sc-email`), so "public" means the
/// same thing to an agent's fetch and a trigger's mail.
pub use sc_email::is_public;

/// A resolver that answers with public addresses only.
///
/// Installed on the client a public-only policy sends with, so the address
/// checked is the address the connection is made to: checking a name and then
/// letting the client resolve it again would let a DNS server that answers
/// differently the second time choose where the request goes.
///
/// Resolution itself is the process's (`tokio::net::lookup_host`, which is
/// `getaddrinfo`, which `sc-dns` intercepts in a static build) — this filters
/// the answer and does not second-guess the lookup.
pub struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
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
                        "`{host}` resolves only to private or reserved addresses, \
                         which this agent may not reach"
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

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn an_address_literal_is_refused_before_anything_is_sent() {
        let policy = HostPolicy::default();
        let err = policy
            .check(&url("http://169.254.169.254/latest/meta-data"))
            .unwrap_err();
        assert!(err.to_string().contains("private or reserved"), "{err}");
        assert!(policy.check(&url("http://[::1]:8080/")).is_err());
        // Other spellings of loopback are normalised by the parser first.
        assert!(policy.check(&url("http://2130706433/")).is_err());
        assert!(policy.check(&url("http://0x7f.1/")).is_err());
        // The admin may open the private network, for an intranet wiki.
        let open = HostPolicy::new(Vec::new(), true);
        assert!(open.check(&url("http://127.0.0.1:3000/")).is_ok());
    }

    #[test]
    fn a_host_admits_its_subdomains_and_nothing_that_merely_ends_like_it() {
        let policy = HostPolicy::new(vec!["docs.rs".into(), "github.com".into()], false);
        assert!(policy.check(&url("https://docs.rs/serde")).is_ok());
        assert!(policy.check(&url("https://raw.github.com/x")).is_ok());
        assert!(policy.check(&url("https://DOCS.RS./serde")).is_ok());
        let err = policy.check(&url("https://evildocs.rs/")).unwrap_err();
        assert!(err.to_string().contains("docs.rs, github.com"), "{err}");
    }

    #[test]
    fn schemes_credentials_and_long_urls_are_refused() {
        let policy = HostPolicy::default();
        assert!(policy.check(&url("file:///etc/passwd")).is_err());
        assert!(policy.check(&url("https://user:pw@example.com/")).is_err());
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
        assert!(policy.check(&url(&long)).is_err());
    }

    #[test]
    fn the_allow_list_is_hosts_and_says_so_when_it_is_not() {
        assert_eq!(
            parse_allowed_hosts("docs.rs, GitHub.com\nnpmjs.com docs.rs").unwrap(),
            vec!["docs.rs", "github.com", "npmjs.com"]
        );
        assert!(parse_allowed_hosts("").unwrap().is_empty());
        let err = parse_allowed_hosts("https://docs.rs/serde").unwrap_err();
        assert!(err.to_string().contains("list hosts only"), "{err}");
        let err = parse_allowed_hosts("*.docs.rs").unwrap_err();
        assert!(err.to_string().contains("write `docs.rs`"), "{err}");
    }
}

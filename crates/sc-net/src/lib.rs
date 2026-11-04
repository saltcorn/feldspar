//! Outbound connections to the public internet only (layer 0;
//! code-quality review §4.6, TODO-code-quality Q4.3).
//!
//! Three places send a request to an address something other than an
//! administrator chose: an agent's `http` trait (a model wrote the URL), the
//! `fetch` action with a templated host (a row wrote it), an MCP server's
//! definition, and a `send_email` override's SMTP host (a row again). Each must
//! not be made to knock on doors inside the server's own network — loopback,
//! the private ranges, link-local (a cloud's metadata endpoint, and its
//! credentials) — security review #27. Each had its own copy of the rule and
//! its own resolver; this crate is the one copy.
//!
//! - [`is_public`] is the one definition of "on the public internet".
//! - [`PublicAddr`] is an address that passed it. Only this crate makes one: a
//!   literal through [`PublicAddr::check`], a name through [`resolve_public`]
//!   (the public addresses among its answers) or [`resolve_every_public`] (all
//!   of them, or a refusal), and [`PublicResolver`], which hands `reqwest`
//!   nothing else to connect to.
//! - [`PublicClient`] is an HTTP client that only connects to a `PublicAddr`:
//!   names go through [`PublicResolver`], so the address checked is the
//!   address connected to (a DNS server that answers differently the second
//!   time chooses nothing), and an address **literal** — which `reqwest` never
//!   hands a resolver — is checked on the request and on every redirect it
//!   follows.
//!
//! What may be reached *besides* the public internet — an agent's allow-list,
//! an MCP server or agent whose definition opens the private network — is the
//! caller's policy and stays with the caller; this crate only answers "public
//! or not" and holds the connection to the answer.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sc_error::{Error, Result};

/// Whether `ip` is an address on the public internet.
///
/// Written out rather than taken from `IpAddr::is_global`, which is still
/// unstable; the list is IANA's special-purpose registries, reduced to the
/// blocks that are somewhere other than the internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        // 0.0.0.0/8, "this network".
        || a == 0
        // 100.64.0.0/10, carrier-grade NAT: private in all but name.
        || (a == 100 && (64..128).contains(&b))
        // 192.0.0.0/24, protocol assignments.
        || (a == 192 && b == 0 && c == 0)
        // 198.18.0.0/15, benchmarking.
        || (a == 198 && (b == 18 || b == 19))
        // 240.0.0.0/4, reserved.
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // An IPv4 address carried in IPv6 is judged as the IPv4 address it is —
    // otherwise `::ffff:127.0.0.1` is a way around every rule above.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    // 64:ff9b::/96, NAT64: the last 32 bits are the IPv4 destination.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        let [.., hi, lo] = segments;
        return is_public_v4(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo)));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // fc00::/7, unique local.
        || (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10, link-local.
        || (segments[0] & 0xffc0) == 0xfe80
        // 2001:db8::/32, documentation.
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        // ::/96, the deprecated IPv4-compatible form.
        || segments[..6] == [0, 0, 0, 0, 0, 0])
}

/// An address on the public internet ([`is_public`]): what a connection that
/// must not reach the server's own network is made to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicAddr(IpAddr);

impl PublicAddr {
    /// `ip`, when it is public.
    pub fn check(ip: IpAddr) -> Option<PublicAddr> {
        is_public(ip).then_some(PublicAddr(ip))
    }

    /// The address.
    pub fn ip(self) -> IpAddr {
        self.0
    }
}

#[cfg(feature = "test-listener")]
impl PublicAddr {
    /// A test's own listener (`127.0.0.1`), taken as public so a connection
    /// pinned to it can be tested. Only with the `test-listener` feature,
    /// which only dev-dependencies enable.
    pub fn test_listener(ip: IpAddr) -> PublicAddr {
        PublicAddr(ip)
    }
}

impl fmt::Display for PublicAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Why a host has no [`PublicAddr`]. The caller words it: what is being
/// connected, and what the admin may do about it, are the caller's to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The lookup itself failed.
    NotResolved(String),
    /// It resolved to nothing.
    NoAddress,
    /// It resolves to this private or reserved address (and, for
    /// [`resolve_public`], to nothing public).
    Private(IpAddr),
}

/// The address `host` names when it is an address literal (`127.0.0.1`,
/// `[::1]`), with no lookup.
fn literal(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// What `host` resolves to (a literal is itself).
async fn lookup(host: &str, port: u16) -> std::result::Result<Vec<IpAddr>, Refused> {
    if let Some(ip) = literal(host) {
        return Ok(vec![ip]);
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let found: Vec<IpAddr> = tokio::net::lookup_host((bare, port))
        .await
        .map_err(|e| Refused::NotResolved(e.to_string()))?
        .map(|addr| addr.ip())
        .collect();
    Ok(found)
}

/// The public addresses among those `host` resolves to — refused when there
/// is none. The rule an HTTP connection is held to: it is made to one of
/// these, and the private ones are never offered.
pub async fn resolve_public(
    host: &str,
    port: u16,
) -> std::result::Result<Vec<PublicAddr>, Refused> {
    let found = lookup(host, port).await?;
    let public: Vec<PublicAddr> = found
        .iter()
        .copied()
        .filter_map(PublicAddr::check)
        .collect();
    match (public.is_empty(), found.first()) {
        (false, _) => Ok(public),
        (true, Some(first)) => Err(Refused::Private(*first)),
        (true, None) => Err(Refused::NoAddress),
    }
}

/// The address to connect to when **every** address `host` resolves to is
/// public, and a refusal naming the first that is not otherwise — the rule an
/// SMTP override is held to, where a name with one public and one private
/// address is refused because which of the two a connection reaches is not
/// this code's choice to leave to chance. A literal is checked as it is.
pub async fn resolve_every_public(
    host: &str,
    port: u16,
) -> std::result::Result<PublicAddr, Refused> {
    let found = lookup(host, port).await?;
    if let Some(private) = found.iter().find(|ip| !is_public(**ip)) {
        return Err(Refused::Private(*private));
    }
    found
        .first()
        .copied()
        .and_then(PublicAddr::check)
        .ok_or(Refused::NoAddress)
}

/// The address a URL names literally, if it names one. The URL parser has
/// normalised the host already: an IPv4 literal in any of its spellings
/// (`0x7f.1`, `2130706433`) is dotted-quad by now, and an IPv6 one bracketed.
pub fn literal_ip(url: &Url) -> Option<IpAddr> {
    literal(url.host_str()?)
}

/// The private or reserved address a URL names literally, if it does.
pub fn private_literal(url: &Url) -> Option<IpAddr> {
    literal_ip(url).filter(|ip| !is_public(*ip))
}

/// A `reqwest` resolver that answers with [`PublicAddr`]s only, so the
/// address checked is the address connected to.
///
/// Resolution itself is the process's (`tokio::net::lookup_host`, which is
/// `getaddrinfo`, which `sc-dns` intercepts in a static build) — this filters
/// the answer and does not second-guess the lookup.
#[derive(Clone)]
pub struct PublicResolver {
    /// What a refusal ends with: `, which this agent may not reach`.
    why: Arc<str>,
}

impl PublicResolver {
    /// A resolver whose refusals end with `why` (`", which this agent may not
    /// reach"`).
    pub fn new(why: impl Into<Arc<str>>) -> PublicResolver {
        PublicResolver { why: why.into() }
    }
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let why = Arc::clone(&self.why);
        Box::pin(async move {
            match resolve_public(&host, 0).await {
                Ok(public) => {
                    let addrs: Addrs = Box::new(
                        public
                            .into_iter()
                            .map(|addr| SocketAddr::new(addr.ip(), 0))
                            .collect::<Vec<_>>()
                            .into_iter(),
                    );
                    Ok(addrs)
                }
                Err(Refused::NotResolved(e)) => Err(e.into()),
                Err(Refused::NoAddress) => {
                    Err(format!("`{host}` did not resolve to any address").into())
                }
                Err(Refused::Private(_)) => Err(format!(
                    "`{host}` resolves only to private or reserved addresses{why}"
                )
                .into()),
            }
        })
    }
}

/// What a [`PublicClient`] does with a redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirects {
    /// Follow none: the caller sees the 3xx and decides (an agent's client,
    /// which checks its own allow-list on every hop; an MCP client).
    None,
    /// Follow up to this many, each hop's address literal checked.
    Follow(usize),
}

/// An HTTP client that only connects to a [`PublicAddr`]: names through
/// [`PublicResolver`], literals checked on the request
/// ([`request`](PublicClient::request)) and on every redirect it follows.
#[derive(Clone)]
pub struct PublicClient {
    client: reqwest::Client,
    why: Arc<str>,
}

impl PublicClient {
    /// The client `builder` makes, with the public-only resolver and redirect
    /// rule installed over whatever it set for those two, and no proxy: a
    /// proxy (`HTTPS_PROXY`, `ALL_PROXY`, or one the builder set) resolves the
    /// target name itself, so the resolver would never see it. `why` ends every
    /// refusal (`", which this agent may not reach"`).
    pub fn new(
        builder: reqwest::ClientBuilder,
        why: &str,
        redirects: Redirects,
    ) -> Result<PublicClient> {
        let why: Arc<str> = Arc::from(why);
        let policy = match redirects {
            Redirects::None => reqwest::redirect::Policy::none(),
            Redirects::Follow(max) => {
                let why = Arc::clone(&why);
                reqwest::redirect::Policy::custom(move |attempt| {
                    if attempt.previous().len() >= max {
                        return attempt.error(format!("more than {max} redirects"));
                    }
                    match private_literal(attempt.url()) {
                        Some(ip) => attempt.error(format!(
                            "a redirect to {ip}, a private or reserved address{why}"
                        )),
                        None => attempt.follow(),
                    }
                })
            }
        };
        let client = builder
            .no_proxy()
            .dns_resolver(Arc::new(PublicResolver::new(Arc::clone(&why))))
            .redirect(policy)
            .build()
            .map_err(|e| Error::config(format!("could not build the HTTP client: {e}")))?;
        Ok(PublicClient { client, why })
    }

    /// Refused when `url` names a private or reserved address literally.
    pub fn check(&self, url: &Url) -> Result<()> {
        match private_literal(url) {
            Some(ip) => Err(Error::auth(format!(
                "`{ip}` is a private or reserved address{}",
                self.why
            ))),
            None => Ok(()),
        }
    }

    /// A request to `url`, once [`check`](PublicClient::check) admits it.
    pub fn request(&self, method: reqwest::Method, url: Url) -> Result<reqwest::RequestBuilder> {
        self.check(&url)?;
        Ok(self.client.request(method, url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_loopback_and_metadata_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "240.0.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} should not be public");
            assert!(PublicAddr::check(ip.parse().unwrap()).is_none(), "{ip}");
        }
        for ip in [
            "93.184.215.14",
            "1.1.1.1",
            "2606:4700::1111",
            "64:ff9b::101:101",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip} should be public");
            assert!(PublicAddr::check(ip.parse().unwrap()).is_some(), "{ip}");
        }
    }

    #[test]
    fn a_url_literal_is_found_in_every_spelling() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            private_literal(&url("http://169.254.169.254/latest/meta-data")),
            Some("169.254.169.254".parse().unwrap())
        );
        assert!(private_literal(&url("http://[::1]:8080/")).is_some());
        assert!(private_literal(&url("http://2130706433/")).is_some());
        assert!(private_literal(&url("http://0x7f.1/")).is_some());
        assert!(private_literal(&url("https://1.1.1.1/")).is_none());
        assert!(private_literal(&url("https://example.com/")).is_none());
        assert_eq!(
            literal_ip(&url("https://1.1.1.1/")),
            Some("1.1.1.1".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn every_address_must_be_public_for_the_strict_form() {
        for host in [
            "127.0.0.1",
            "10.0.0.5",
            "[::1]",
            "::ffff:127.0.0.1",
            "localhost",
        ] {
            assert!(
                matches!(
                    resolve_every_public(host, 587).await,
                    Err(Refused::Private(_))
                ),
                "{host}"
            );
            assert!(
                matches!(resolve_public(host, 0).await, Err(Refused::Private(_))),
                "{host}"
            );
        }
        // A public literal is answered as itself, with no lookup.
        assert_eq!(
            resolve_every_public("1.1.1.1", 587).await.unwrap().ip(),
            "1.1.1.1".parse::<IpAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn a_public_client_refuses_a_private_literal_before_sending() {
        let client = PublicClient::new(
            reqwest::Client::builder(),
            ", which nothing may reach",
            Redirects::None,
        )
        .unwrap();
        let err = client
            .request(
                reqwest::Method::GET,
                Url::parse("http://127.0.0.1:9/").unwrap(),
            )
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("private or reserved address, which nothing may reach"),
            "{err}"
        );
        assert!(
            client
                .request(
                    reqwest::Method::GET,
                    Url::parse("https://example.com/").unwrap()
                )
                .is_ok()
        );
    }
}

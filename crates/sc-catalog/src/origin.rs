//! Where this deployment's applications are reachable in a browser.
//!
//! An application is served at `<subdomain>.<base-domain>` (design §13.2), and
//! that URL is a fact about the *deployment*, not about the app: the base
//! domain, the port the server bound, and whether it is behind TLS. Nothing in
//! the data layer needs it — and yet it is here, for the same reason the schema
//! observer and the table-event sink are: it is a process-wide fact that two
//! layers which cannot name each other both need, and the [`Catalog`] is the
//! handle they both already hold.
//!
//! The two layers here are the **server**, which is told the base domain on its
//! command line, and the **project generator** in `sc-app`, which writes an
//! application's `AGENTS.md` and `src/feldspar/README.md` and has to tell a
//! developer — or their coding agent — which URL to open. Threading an origin
//! through `scaffold_app`, `emit_react_runtime`, `emit_app_client`,
//! `build_application` and every one of their callers would put a parameter that
//! is about *documentation* into the signature of everything that builds; every
//! one of those functions already takes a `&Catalog`.
//!
//! It is set once, at boot, by whichever process is doing the work: `feldspar
//! serve` from its own configuration, and the command-line build from the
//! `feldspar.toml` environment it was pointed at. That last part is the point of
//! storing it rather than passing it from the server alone — a `feldspar
//! build-app` that rewrote the generated documentation *without* the URL would
//! be worse than one that never wrote it, because the two builds would disagree.
//!
//! Unset is a normal state, not a misconfiguration: a server with no
//! `--base-domain` serves no applications at all, and the documentation then
//! says what to substitute instead of inventing a hostname.
//!
//! [`Catalog`]: crate::Catalog

/// The origin an application is served on: `https://<subdomain>.<base_domain>`,
/// with the port when it is not the scheme's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicOrigin {
    /// The server's base domain — `example.com`, so the app `blog` is at
    /// `blog.example.com` (§13.2).
    pub base_domain: String,
    /// The port the server is reachable on.
    pub port: u16,
    /// Whether it is served over TLS.
    pub secure: bool,
}

/// A host this server serves, with its port when it has one (`blog.example.com`,
/// `admin.example.com:3000`): code-quality review §4.6, TODO-code-quality Q4.2,
/// security review #1 and #2.
///
/// An absolute link a server hands out — a password reset, an invitation, the
/// OAuth callback and the hand-off back to where a sign-in began, an
/// application's base URL — must point at a host this deployment answers for,
/// or a `Host` header a caller made up (`blog.example.com:@evil.com`) chooses
/// where a token goes. So the host is a type, and only two things make one:
///
/// - [`TrustedHost::configured`]: configuration — the base domain, the admin
///   host, a subdomain under the base domain;
/// - [`TrustedHost::served`]: the router, for a `Host` header it has matched
///   against the hosts it serves (the validated host table).
///
/// Both refuse what is not a plain `host[:port]` ([`is_valid_host`]). Lowercased.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrustedHost(String);

impl TrustedHost {
    /// A host the deployment's configuration names.
    pub fn configured(host: &str) -> sc_error::Result<TrustedHost> {
        TrustedHost::checked(host)
    }

    /// A request's `Host`, once the router has matched it against the hosts
    /// it serves — never a header nothing checked.
    pub fn served(host: &str) -> sc_error::Result<TrustedHost> {
        TrustedHost::checked(host)
    }

    fn checked(host: &str) -> sc_error::Result<TrustedHost> {
        if !is_valid_host(host) {
            return Err(sc_error::Error::invalid(format!(
                "`{host}` is not a host name with an optional port"
            )));
        }
        Ok(TrustedHost(host.to_ascii_lowercase()))
    }

    /// `host[:port]`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The port, when one is given.
    pub fn port(&self) -> Option<&str> {
        match self.0.rsplit_once(':') {
            Some((name, port)) if !name.ends_with(':') && !port.contains(']') => Some(port),
            _ => None,
        }
    }

    /// This host on `other`'s port (or on none, when `other` has none) — the
    /// admin UI's host as a request on another served host reached it.
    pub fn on_port_of(&self, other: &TrustedHost) -> TrustedHost {
        let name = match self.port() {
            Some(port) => &self.0[..self.0.len() - port.len() - 1],
            None => self.0.as_str(),
        };
        match other.port() {
            Some(port) => TrustedHost(format!("{name}:{port}")),
            None => TrustedHost(name.to_owned()),
        }
    }
}

impl std::fmt::Display for TrustedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `host` is a plain `host[:port]`: a name of letters, digits, `-`,
/// `.` and `_` (or a bracketed IPv6 literal), and at most one port of one to
/// five digits — no userinfo (`@`), no path, no second `:`. What the router
/// accepts as a `Host` header and what a [`TrustedHost`] holds.
pub fn is_valid_host(host: &str) -> bool {
    let digits =
        |port: &str| (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit());
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        let Some((literal, after)) = rest.split_once(']') else {
            return false;
        };
        if literal.is_empty()
            || !literal
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        {
            return false;
        }
        return match after {
            "" => true,
            _ => after.strip_prefix(':').is_some_and(digits),
        };
    } else {
        match host.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        }
    };
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        && port.is_none_or(digits)
}

/// The origin an absolute link starts with: `scheme://` a [`TrustedHost`], no
/// path and no trailing slash. Every function that builds an absolute link
/// takes one (TODO-code-quality Q4.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicBaseUrl(String);

impl PublicBaseUrl {
    /// `https://host` (or `http://`).
    pub fn new(secure: bool, host: &TrustedHost) -> PublicBaseUrl {
        let scheme = if secure { "https" } else { "http" };
        PublicBaseUrl(format!("{scheme}://{host}"))
    }

    /// The origin's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// An absolute link to `path_and_query` (which starts with `/`) on this
    /// origin.
    pub fn join(&self, path_and_query: &str) -> String {
        format!("{}{path_and_query}", self.0)
    }
}

impl std::fmt::Display for PublicBaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<PublicBaseUrl> for String {
    fn from(url: PublicBaseUrl) -> String {
        url.0
    }
}

impl PublicOrigin {
    /// The base URL of the application served on `subdomain`, from this
    /// configuration: [`url_for`](PublicOrigin::url_for) as a
    /// [`PublicBaseUrl`].
    pub fn base_url_for(&self, subdomain: &str) -> sc_error::Result<PublicBaseUrl> {
        let default_port = if self.secure { 443 } else { 80 };
        let name = self.host_for(subdomain);
        let host = if self.port == default_port {
            TrustedHost::configured(&name)?
        } else {
            TrustedHost::configured(&format!("{name}:{}", self.port))?
        };
        Ok(PublicBaseUrl::new(self.secure, &host))
    }
}

impl PublicOrigin {
    /// An origin on `base_domain`, port `port`, over plain HTTP.
    pub fn new(base_domain: impl Into<String>, port: u16) -> PublicOrigin {
        PublicOrigin {
            base_domain: base_domain.into(),
            port,
            secure: false,
        }
    }

    /// The same origin over TLS, returning `self` for chaining.
    pub fn secure(mut self, secure: bool) -> PublicOrigin {
        self.secure = secure;
        self
    }

    /// `http` or `https`.
    pub fn scheme(&self) -> &'static str {
        if self.secure { "https" } else { "http" }
    }

    /// The host an application is served on: `blog.example.com`, or the base
    /// domain itself for the application whose subdomain is `@` (which it may
    /// be once the admin UI has moved to a subdomain of its own).
    pub fn host_for(&self, subdomain: &str) -> String {
        if subdomain == "@" {
            return self.base_domain.clone();
        }
        format!("{subdomain}.{}", self.base_domain)
    }

    /// The URL to open an application at: `http://blog.example.com:3032`.
    ///
    /// The port is **omitted when it is the scheme's default**, because a URL
    /// with `:443` in it is one a reader has to think about, and this one is
    /// written into documentation to be pasted rather than read.
    pub fn url_for(&self, subdomain: &str) -> String {
        let default_port = if self.secure { 443 } else { 80 };
        let host = self.host_for(subdomain);
        if self.port == default_port {
            format!("{}://{host}", self.scheme())
        } else {
            format!("{}://{host}:{}", self.scheme(), self.port)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Security review #1: a `Host` with userinfo, a path or a second port is
    /// not a host, and no link is built to it.
    #[test]
    fn a_trusted_host_is_a_plain_host_and_port() {
        for good in [
            "example.com",
            "blog.example.com:3000",
            "localhost:8080",
            "[::1]:443",
            "127.0.0.1",
        ] {
            assert!(TrustedHost::served(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "blog.example.com:@evil.com",
            "user@evil.com",
            "evil.com/path",
            "a.com:80:81",
            "a.com:",
            "a.com:123456",
            "[::1",
            "exa mple.com",
        ] {
            assert!(TrustedHost::served(bad).is_err(), "{bad:?}");
            assert!(TrustedHost::configured(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            TrustedHost::served("Blog.Example.COM").unwrap().as_str(),
            "blog.example.com"
        );
    }

    #[test]
    fn a_base_url_is_scheme_and_trusted_host() {
        let host = TrustedHost::served("blog.example.com:3000").unwrap();
        assert_eq!(host.port(), Some("3000"));
        let url = PublicBaseUrl::new(true, &host);
        assert_eq!(url.as_str(), "https://blog.example.com:3000");
        assert_eq!(
            url.join("/set-password#token=t"),
            "https://blog.example.com:3000/set-password#token=t"
        );
        let admin = TrustedHost::configured("admin.example.com").unwrap();
        assert_eq!(admin.on_port_of(&host).as_str(), "admin.example.com:3000");
        assert_eq!(
            admin
                .on_port_of(&TrustedHost::served("x.example.com").unwrap())
                .as_str(),
            "admin.example.com"
        );
        assert_eq!(TrustedHost::served("[::1]").unwrap().port(), None);
        assert_eq!(
            PublicOrigin::new("example.com", 443)
                .secure(true)
                .base_url_for("blog")
                .unwrap()
                .as_str(),
            "https://blog.example.com"
        );
    }

    #[test]
    fn an_apps_url_is_its_subdomain_under_the_base_domain() {
        let origin = PublicOrigin::new("example.com", 3032);
        assert_eq!(origin.host_for("blog"), "blog.example.com");
        assert_eq!(origin.url_for("blog"), "http://blog.example.com:3032");
    }

    #[test]
    fn the_root_application_is_on_the_base_domain() {
        let origin = PublicOrigin::new("example.com", 443).secure(true);
        assert_eq!(origin.host_for("@"), "example.com");
        assert_eq!(origin.url_for("@"), "https://example.com");
    }

    #[test]
    fn a_default_port_is_left_out_of_the_url() {
        // Both defaults, because a deployment on 80 is as real as one on 443 and
        // `http://blog.example.com:80` is nobody's idea of a URL to paste.
        assert_eq!(
            PublicOrigin::new("example.com", 80).url_for("blog"),
            "http://blog.example.com"
        );
        assert_eq!(
            PublicOrigin::new("example.com", 443)
                .secure(true)
                .url_for("blog"),
            "https://blog.example.com"
        );
        // A non-default port survives, on either scheme.
        assert_eq!(
            PublicOrigin::new("example.com", 8443)
                .secure(true)
                .url_for("blog"),
            "https://blog.example.com:8443"
        );
    }
}

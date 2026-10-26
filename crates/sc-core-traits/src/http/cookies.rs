//! A cookie jar for one run's requests (TODO-agent-skills AS9.3).
//!
//! An `http` instance with `cookies` on keeps what servers set across the
//! run's requests — a login, then the pages behind it — in the trait's run
//! state, so the jar lives exactly as long as the conversation and is never
//! shared with another.
//!
//! Deliberately narrower than a browser's: a cookie is sent **only to the host
//! that set it** (a `Domain` attribute does not widen it to the subdomains),
//! only under its `Path`, and a `Secure` one only over https. `Max-Age=0`, or an
//! `Expires` in the past, deletes one. Nothing here is sent to a host the
//! configuration's policy would not reach anyway: the jar is consulted per
//! request, after the policy admitted the URL.

use reqwest::Url;
use serde::{Deserialize, Serialize};

/// The most cookies one jar keeps; past this, the oldest go first.
pub const MAX_COOKIES: usize = 200;
/// The longest `name=value` kept, as browsers bound it.
pub const MAX_COOKIE_BYTES: usize = 4096;

/// The cookies one run's requests have been set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cookie {
    host: String,
    name: String,
    value: String,
    path: String,
    #[serde(default)]
    secure: bool,
}

impl CookieJar {
    /// How many cookies are kept.
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// The `Cookie` header a request to `url` carries, if any.
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let host = url.host_str()?.to_ascii_lowercase();
        let path = url.path();
        let https = url.scheme() == "https";
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|c| c.host == host && path_matches(&c.path, path) && (https || !c.secure))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }

    /// Keep what one `Set-Cookie` header from `url` says.
    pub fn store(&mut self, url: &Url, set_cookie: &str) {
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return;
        };
        let mut parts = set_cookie.split(';');
        let Some((name, value)) = parts.next().and_then(|p| p.split_once('=')) else {
            return;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() || name.len() + value.len() > MAX_COOKIE_BYTES {
            return;
        }
        let mut path = "/".to_owned();
        let mut secure = false;
        let mut expired = false;
        for attribute in parts {
            let (key, val) = attribute.split_once('=').unwrap_or((attribute, ""));
            match key.trim().to_ascii_lowercase().as_str() {
                "path" if val.trim().starts_with('/') => val.trim().clone_into(&mut path),
                "secure" => secure = true,
                "max-age" => expired |= val.trim().parse::<i64>().is_ok_and(|n| n <= 0),
                "expires" => expired |= expires_in_the_past(val),
                _ => {}
            }
        }
        self.cookies
            .retain(|c| !(c.host == host && c.name == name && c.path == path));
        if expired {
            return;
        }
        self.cookies.push(Cookie {
            host,
            name: name.to_owned(),
            value: value.to_owned(),
            path,
            secure,
        });
        if self.cookies.len() > MAX_COOKIES {
            let excess = self.cookies.len() - MAX_COOKIES;
            self.cookies.drain(..excess);
        }
    }
}

/// RFC 6265's path match: the same path, or one under it.
fn path_matches(cookie: &str, request: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request[cookie.len()..].starts_with('/')))
}

/// Whether an `Expires` date is before now. Only the year is read: what a
/// server sends to delete a cookie is a date long past (`Thu, 01 Jan 1970`),
/// and a cookie that expires later this year outlives the conversation anyway.
fn expires_in_the_past(date: &str) -> bool {
    let this_year = 1970
        + std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() / 31_556_952);
    date.split(|c: char| !c.is_ascii_digit())
        .filter(|t| t.len() == 4)
        .filter_map(|t| t.parse::<u64>().ok())
        .any(|year| year < this_year)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn a_cookie_goes_back_only_to_the_host_that_set_it() {
        let mut jar = CookieJar::default();
        jar.store(
            &url("https://shop.example/login"),
            "sid=abc; Path=/; HttpOnly; Domain=example",
        );
        assert_eq!(
            jar.header_for(&url("https://shop.example/cart")).as_deref(),
            Some("sid=abc")
        );
        assert_eq!(jar.header_for(&url("https://api.shop.example/")), None);
        assert_eq!(jar.header_for(&url("https://other.example/")), None);
    }

    #[test]
    fn path_secure_and_deletion_are_honoured() {
        let mut jar = CookieJar::default();
        jar.store(&url("https://a.example/"), "admin=1; Path=/admin; Secure");
        jar.store(&url("https://a.example/"), "theme=dark");
        assert_eq!(
            jar.header_for(&url("https://a.example/admin/users"))
                .as_deref(),
            Some("admin=1; theme=dark")
        );
        assert_eq!(
            jar.header_for(&url("https://a.example/administrator"))
                .as_deref(),
            Some("theme=dark")
        );
        assert_eq!(
            jar.header_for(&url("http://a.example/admin")).as_deref(),
            Some("theme=dark")
        );
        // Replaced, then deleted.
        jar.store(&url("https://a.example/"), "theme=light");
        assert_eq!(jar.len(), 2);
        jar.store(&url("https://a.example/"), "theme=; Max-Age=0");
        jar.store(
            &url("https://a.example/"),
            "admin=; Path=/admin; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        );
        assert!(jar.is_empty());
    }

    #[test]
    fn a_jar_survives_the_run_state_round_trip() {
        let mut jar = CookieJar::default();
        jar.store(&url("http://127.0.0.1:8080/"), "k=v");
        let state = serde_json::to_value(&jar).unwrap();
        let back: CookieJar = serde_json::from_value(state).unwrap();
        assert_eq!(back, jar);
        assert_eq!(
            back.header_for(&url("http://127.0.0.1:8080/x")).as_deref(),
            Some("k=v")
        );
    }
}

//! Values that go into an HTTP response header, each built by one rule
//! (security review 2026-11 D6, D7; TODO-type-hardening H6.3).
//!
//! - [`RedirectTarget`]: a `Location` on the host the request came to. Before
//!   this type there were three validators with three rules: the identity
//!   provider's `return_to` (`sc-server`), a module consent flow's
//!   (`sc-module`) and a v1 form's `redirect` (`sc-viewpattern`). Two refused
//!   a backslash only right after the first `/`, one encoded what it kept.
//!   All three were sound, and they would have drifted.
//! - [`DownloadName`]: the name in `Content-Disposition`. One route wrote
//!   `attachment; filename="{name}"` and relied on each producer to have
//!   built a name with no quote in it.
//!
//! Both convert to a [`HeaderValue`] that cannot fail: what they hold is
//! visible ASCII by construction, so neither `Redirect::to`'s panic nor a
//! dropped header is reachable from a value somebody sent.

use std::fmt;

use http::HeaderValue;

/// A path on the host the request came to, safe to send as `Location`.
///
/// Made only by [`parse`](RedirectTarget::parse) (or [`root`](RedirectTarget::root)),
/// which accepts a value that
///
/// - starts with exactly one `/`: never `//evil.example` (protocol-relative),
///   never an absolute URL (`https://…`, `javascript:…`);
/// - has no `\` anywhere: browsers read `/\evil.example` as `//evil.example`,
///   and a backslash later in the path is not one any route here uses;
/// - has no control character (a CR/LF would split the header; a tab is
///   dropped by the URL parser, so `/\t/evil.example` is `//evil.example`).
///
/// What it keeps is percent-encoded the way Express's `res.redirect` encodes
/// a `Location` (`encodeurl`): a space, a non-ASCII byte and `"<>^`{|}`
/// become `%XX`, and an escape already there is left alone.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RedirectTarget(String);

impl RedirectTarget {
    /// `raw`, surrounding whitespace trimmed, if it is a path on this host
    /// (see the type's docs); `None` otherwise.
    pub fn parse(raw: &str) -> Option<RedirectTarget> {
        let path = raw.trim();
        let mut chars = path.chars();
        if chars.next() != Some('/') || chars.next() == Some('/') {
            return None;
        }
        if path.contains('\\') || path.chars().any(char::is_control) {
            return None;
        }
        Some(RedirectTarget(encode_location(path)))
    }

    /// [`parse`](RedirectTarget::parse), or `/` for a value that is absent
    /// or not a path on this host.
    pub fn or_root(raw: Option<&str>) -> RedirectTarget {
        raw.and_then(RedirectTarget::parse)
            .unwrap_or_else(RedirectTarget::root)
    }

    /// `/`: the host's front page.
    pub fn root() -> RedirectTarget {
        RedirectTarget("/".to_owned())
    }

    /// This target with `key=value` added to its query, before any fragment;
    /// both are form-encoded. Still a path on this host.
    #[must_use]
    pub fn with_param(&self, key: &str, value: &str) -> RedirectTarget {
        let (path, fragment) = match self.0.split_once('#') {
            Some((path, fragment)) => (path, Some(fragment)),
            None => (self.0.as_str(), None),
        };
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut out = format!("{path}{sep}{}={}", form_encode(key), form_encode(value));
        if let Some(fragment) = fragment {
            out.push('#');
            out.push_str(fragment);
        }
        RedirectTarget(out)
    }

    /// The encoded path.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The encoded path, owned.
    pub fn into_string(self) -> String {
        self.0
    }

    /// The `Location` header's value.
    pub fn header_value(&self) -> HeaderValue {
        // Visible ASCII by construction (`encode_location`, `form_encode`), so
        // this never fails; `/` is the answer if it somehow did.
        HeaderValue::from_str(&self.0).unwrap_or_else(|_| HeaderValue::from_static("/"))
    }
}

impl fmt::Debug for RedirectTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RedirectTarget").field(&self.0).finish()
    }
}

impl fmt::Display for RedirectTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<RedirectTarget> for HeaderValue {
    fn from(target: RedirectTarget) -> HeaderValue {
        target.header_value()
    }
}

/// `encodeurl`'s rule: every byte that is not visible ASCII, and
/// `"<>^`{|}`, as `%XX`; everything else (including `%`) as it is.
fn encode_location(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte <= b' ' || byte >= 0x7f || b"\"<>^`{|}".contains(&byte) {
            out.push_str(&format!("%{byte:02X}"));
        } else {
            out.push(char::from(byte));
        }
    }
    out
}

/// Form-encode one value: the unreserved characters as they are, every other
/// byte as `%XX`.
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The name a browser saves a download as, carried in `Content-Disposition`.
///
/// Made only by [`sanitised`](DownloadName::sanitised): a control character
/// (CR, LF, …) and a path separator (`/`, `\`) become `_`, and an empty name
/// is `download`. [`disposition`](DownloadName::disposition) renders RFC 6266
/// with an RFC 5987 `filename*` for the exact name: the ASCII `filename`
/// beside it has `"` and `\` (and anything outside printable ASCII) as `_`,
/// so nothing in the name can end the quoted string or add a parameter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DownloadName(String);

impl DownloadName {
    /// The name `raw` is saved under (see the type's docs).
    pub fn sanitised(raw: &str) -> DownloadName {
        let name: String = raw
            .chars()
            .map(|c| {
                if c.is_control() || c == '/' || c == '\\' {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        if name.is_empty() {
            DownloadName("download".to_owned())
        } else {
            DownloadName(name)
        }
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `inline` or `attachment`, with an ASCII `filename` for old clients and
    /// the exact name as `filename*`.
    pub fn disposition(&self, attachment: bool) -> String {
        let kind = if attachment { "attachment" } else { "inline" };
        let ascii: String = self
            .0
            .chars()
            .map(|c| match c {
                ' '..='~' if c != '"' && c != '\\' => c,
                _ => '_',
            })
            .collect();
        let mut encoded = String::with_capacity(self.0.len());
        for byte in self.0.bytes() {
            if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
                encoded.push(char::from(byte));
            } else {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
        format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
    }

    /// [`disposition`](DownloadName::disposition) as a header value, which
    /// cannot fail: it is printable ASCII.
    pub fn header_value(&self, attachment: bool) -> HeaderValue {
        HeaderValue::from_str(&self.disposition(attachment))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
    }
}

impl fmt::Display for DownloadName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_here_is_kept() {
        for good in ["/", "/orders?x=1", "/settings/mail#top", "/view/a%20b"] {
            assert_eq!(RedirectTarget::parse(good).unwrap().as_str(), good);
        }
        assert_eq!(RedirectTarget::parse("  /x  ").unwrap().as_str(), "/x");
    }

    #[test]
    fn an_off_site_target_is_refused() {
        for bad in [
            "//evil.example",
            "//evil.example/path",
            "/\\evil.example",
            "/a\\b",
            "\\\\evil.example",
            "https://evil.example",
            "http:/evil.example",
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>",
            "evil.example",
            "",
            "/\t/evil.example",
            "/\r\nSet-Cookie: x=1",
            "/x\0",
        ] {
            assert!(RedirectTarget::parse(bad).is_none(), "{bad:?}");
            assert_eq!(RedirectTarget::or_root(Some(bad)).as_str(), "/", "{bad:?}");
        }
        assert_eq!(RedirectTarget::or_root(None).as_str(), "/");
    }

    #[test]
    fn what_is_kept_is_encoded_for_the_header() {
        let target = RedirectTarget::parse("/view/List Books?author=\"x\"&n=<1>").unwrap();
        assert_eq!(
            target.as_str(),
            "/view/List%20Books?author=%22x%22&n=%3C1%3E"
        );
        assert_eq!(
            RedirectTarget::parse("/view/Café").unwrap().as_str(),
            "/view/Caf%C3%A9"
        );
        // Always a valid header, whatever went in.
        let value: HeaderValue = RedirectTarget::parse("/ü/ß?q=☃").unwrap().into();
        assert!(value.to_str().is_ok());
    }

    #[test]
    fn a_param_goes_before_the_fragment() {
        let target = RedirectTarget::parse("/orders?x=1#list").unwrap();
        assert_eq!(
            target
                .with_param("oauth_error", "no such provider")
                .as_str(),
            "/orders?x=1&oauth_error=no%20such%20provider#list"
        );
        assert_eq!(
            RedirectTarget::root().with_param("e", "a&b=c").as_str(),
            "/?e=a%26b%3Dc"
        );
    }

    #[test]
    fn a_download_name_cannot_break_out_of_the_header() {
        let name = DownloadName::sanitised("a\r\nSet-Cookie: x=1; \"q\".txt");
        let disposition = name.disposition(true);
        assert!(!disposition.contains('\r') && !disposition.contains('\n'));
        assert_eq!(
            disposition,
            "attachment; filename=\"a__Set-Cookie: x=1; _q_.txt\"; \
             filename*=UTF-8''a__Set-Cookie%3A%20x%3D1%3B%20%22q%22.txt"
        );
        assert!(name.header_value(true).to_str().is_ok());
    }

    #[test]
    fn a_download_name_has_no_path() {
        assert_eq!(
            DownloadName::sanitised("../../etc/passwd").as_str(),
            ".._.._etc_passwd"
        );
        assert_eq!(
            DownloadName::sanitised("C:\\Users\\x.txt").as_str(),
            "C:_Users_x.txt"
        );
        assert_eq!(DownloadName::sanitised("").as_str(), "download");
    }

    #[test]
    fn an_ordinary_name_renders_as_it_did() {
        // The rendering `sc-server`'s `content_disposition` had, unchanged.
        assert_eq!(
            DownloadName::sanitised("naïve 名前.png").disposition(false),
            "inline; filename=\"na_ve __.png\"; \
             filename*=UTF-8''na%C3%AFve%20%E5%90%8D%E5%89%8D.png"
        );
        assert_eq!(
            DownloadName::sanitised("backup-2026-11-05.zip").disposition(true),
            "attachment; filename=\"backup-2026-11-05.zip\"; \
             filename*=UTF-8''backup-2026-11-05.zip"
        );
    }
}

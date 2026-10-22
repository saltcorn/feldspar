//! **Content-Security-Policy sources a module suggests** (TODO-payments P1.4,
//! design C6).
//!
//! An application's policy is strict by default: everything from its own
//! origin. A payment provider's embedded form is a script and a frame from the
//! provider's own host, and a call to its API — three directives the default
//! refuses. The module that talks to the provider knows those hosts; the admin
//! decides whether an application may load them. So a module **suggests**:
//!
//! ```js
//! csp: {
//!   stripe_elements: {
//!     description: "Stripe's embedded payment form",
//!     sources: {
//!       "script-src": ["https://js.stripe.com"],
//!       "frame-src": ["https://js.stripe.com", "https://hooks.stripe.com"],
//!       "connect-src": ["https://api.stripe.com"],
//!     },
//!   },
//! }
//! ```
//!
//! and nothing changes until the admin switches a suggestion on for an
//! application (`csp_sources` on the application, by [`CspSuggestion::key`]),
//! because installing a module is not a decision about every application's
//! policy.
//!
//! A suggestion may name **hosts** only, over `https:` or `wss:`, in the
//! directives that load or reach something. No keyword (`'unsafe-inline'`), no
//! scheme on its own (`https:`), no `*`: the screen prints what is suggested,
//! and a line an admin cannot judge at a glance is one the module may not
//! write. A suggestion that breaks the rule is an issue on the module's card
//! and is not offered.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};

/// The directives a module may suggest sources for: the ones that load or reach
/// something. `default-src`, `frame-ancestors` and the sandbox are the
/// application's own business.
pub const SUGGESTIBLE_DIRECTIVES: &[&str] = &[
    "script-src",
    "style-src",
    "img-src",
    "font-src",
    "connect-src",
    "frame-src",
    "child-src",
    "worker-src",
    "media-src",
    "form-action",
];

/// One named set of sources a module suggests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CspSuggestion {
    /// The module suggesting it.
    pub module: String,
    /// Its name within the module.
    pub name: String,
    /// One line for the application's page.
    pub description: String,
    /// Directive → the sources to add to it.
    pub sources: BTreeMap<String, Vec<String>>,
}

impl CspSuggestion {
    /// What an application stores to switch it on: `<module>:<name>`.
    pub fn key(&self) -> String {
        format!("{}:{}", self.module, self.name)
    }

    /// As the module card lists it.
    pub fn to_json(&self) -> Json {
        json!({
            "key": self.key(),
            "name": self.name,
            "description": self.description,
            "sources": self.sources,
        })
    }
}

/// Read a module's `csp` declarations, each `{ name, description, sources }`
/// as the host script passes them on: the valid ones, and a sentence for each
/// one that is not.
pub fn read_suggestions(module: &str, declared: &[Json]) -> (Vec<CspSuggestion>, Vec<String>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    for raw in declared {
        let name = raw.get("name").and_then(Json::as_str).unwrap_or_default();
        match read_one(module, raw) {
            Ok(suggestion) => out.push(suggestion),
            Err(why) => issues.push(format!(
                "its content-security suggestion `{name}` is not offered: {why}"
            )),
        }
    }
    (out, issues)
}

fn read_one(module: &str, raw: &Json) -> Result<CspSuggestion, String> {
    let name = raw
        .get("name")
        .and_then(Json::as_str)
        .filter(|n| !n.is_empty())
        .ok_or("it has no name")?;
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("its name is letters, digits, `-` and `_`".to_owned());
    }
    let description = raw
        .get("description")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned();
    let declared = raw
        .get("sources")
        .and_then(Json::as_object)
        .ok_or("it has no `sources` object of directive → sources")?;
    let mut sources = BTreeMap::new();
    for (directive, list) in declared {
        if !SUGGESTIBLE_DIRECTIVES.contains(&directive.as_str()) {
            return Err(format!(
                "`{directive}` is not a directive a module may add to; it may add to {}",
                SUGGESTIBLE_DIRECTIVES.join(", ")
            ));
        }
        let list = list
            .as_array()
            .ok_or_else(|| format!("`{directive}` is not a list of sources"))?;
        let mut hosts = Vec::new();
        for source in list {
            let source = source
                .as_str()
                .ok_or_else(|| format!("`{directive}` lists something that is not text"))?;
            if !is_host_source(source) {
                return Err(format!(
                    "`{source}` in `{directive}` is not a host: a module suggests \
                     `https://` or `wss://` hosts only"
                ));
            }
            hosts.push(source.to_owned());
        }
        if !hosts.is_empty() {
            sources.insert(directive.clone(), hosts);
        }
    }
    if sources.is_empty() {
        return Err("it adds no sources".to_owned());
    }
    Ok(CspSuggestion {
        module: module.to_owned(),
        name: name.to_owned(),
        description,
        sources,
    })
}

/// Whether `source` is an `https:` or `wss:` host-source: a host (its first
/// label may be `*`), an optional port and an optional path, and nothing a
/// policy would read as a second source or a keyword.
pub fn is_host_source(source: &str) -> bool {
    let Some(rest) = source
        .strip_prefix("https://")
        .or_else(|| source.strip_prefix("wss://"))
    else {
        return false;
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let host = host.strip_prefix("*.").unwrap_or(host);
    let labels_ok = !host.is_empty()
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    let path_ok = path
        .bytes()
        .all(|b| b.is_ascii_graphic() && !matches!(b, b';' | b',' | b'\'' | b'"'));
    labels_ok && path_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_offered_and_anything_else_is_an_issue() {
        let (ok, issues) = read_suggestions(
            "@feldspar/payments",
            &[
                json!({
                    "name": "stripe_elements",
                    "description": "Stripe's embedded payment form",
                    "sources": {
                        "script-src": ["https://js.stripe.com"],
                        "frame-src": ["https://js.stripe.com", "https://*.stripe.com"],
                        "connect-src": ["https://api.stripe.com:443/v1", "wss://ws.stripe.com"],
                    },
                }),
                json!({ "name": "inline", "sources": { "script-src": ["'unsafe-inline'"] } }),
                json!({ "name": "any", "sources": { "img-src": ["*"] } }),
                json!({ "name": "bare", "sources": { "img-src": ["https:"] } }),
                json!({ "name": "framing", "sources": { "frame-ancestors": ["https://a.com"] } }),
                json!({ "name": "two", "sources": { "img-src": ["https://a.com https://b.com"] } }),
            ],
        );
        assert_eq!(ok.len(), 1, "{issues:?}");
        assert_eq!(ok[0].key(), "@feldspar/payments:stripe_elements");
        assert_eq!(ok[0].sources["frame-src"].len(), 2);
        assert_eq!(issues.len(), 5, "{issues:?}");
        assert!(issues[0].contains("`'unsafe-inline'`"), "{}", issues[0]);
        assert!(issues[3].contains("frame-ancestors"), "{}", issues[3]);
    }
}

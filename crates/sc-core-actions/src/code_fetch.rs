//! The network a **code body** reaches through `fetch` (§10.1).
//!
//! [`sc_expr::FetchHost`] implemented over the same `reqwest` client the
//! [`Fetch`](crate::Fetch) action uses, because it is the same capability
//! reached two ways: an admin who can configure a `fetch` action can write a
//! `run_js_code` body, and a trigger body is server-side configuration either
//! way. What the body gains over the action is the thing a body exists for —
//! the response is a value it can branch on, loop over and write to a table.
//!
//! The rules are the action's, kept deliberately identical so there is one
//! answer to "what can a trigger send":
//!
//! - **absolute `http`/`https` only** — a relative URL has nothing to resolve
//!   against server-side, and `file:`/`data:` would make the server read
//!   something local, which is not what "call an endpoint" means;
//! - **the request is re-validated here**, not trusted from the guest. The
//!   prelude checks a header name so the *message* can name the line that wrote
//!   it; this checks it again because the guest is where the admin's own
//!   JavaScript runs and the seam is a boundary, not a formality;
//! - **bounded**: the wall clock arrives already clamped to what is left of the
//!   run (`sc_expr` does that), and the response is capped at
//!   [`MAX_RESPONSE_BYTES`] — refused rather than truncated, for the reason a
//!   read of 1000 rows is refused rather than truncated: half a body computed
//!   against is a wrong answer that looks like a right one.
//!
//! What is deliberately *not* here is any notion of which hosts may be called.
//! A code body is administrator-authored configuration running on the server,
//! exactly as the `fetch` action is, so the network it can reach is the network
//! the server can reach. A deployment that needs less than that has a firewall,
//! which is where that rule belongs and where it cannot be argued with.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sc_error::{Error, Result};
use sc_expr::{DEFAULT_FETCH_TIMEOUT, FetchHost};
use serde_json::{Value as Json, json};

/// The most of a response a body may be handed.
///
/// A resident run holds what it reads in the V8 heap, and the isolate's heap is
/// shared with every other run on it — so this is the same kind of bound the
/// row cap is, in the same place, for the same reason.
pub(crate) const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The methods a body may send. `CONNECT` and `TRACE` are left out: neither is
/// something a trigger has any business doing, and a proxy or an echo of the
/// request headers is a way to use this server as a tool rather than a way to
/// call an endpoint.
const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// One code run's network.
///
/// Cheap: the client is cloned from the action's, so the connection pool and the
/// TLS configuration are shared and a body's first request pays for no
/// handshake the server has already made.
pub(crate) struct CodeFetchHost {
    client: reqwest::Client,
    /// The trigger this body belongs to, so a failure names it the way every
    /// other failure of a firing does.
    trigger: String,
    /// The checklist run the body runs on behalf of, whose outbox a request
    /// goes to instead of out (TODO-checklists C4.1). Read when the host is
    /// built, in the action's task: the isolate's calls may arrive on another.
    outbox: Option<std::sync::Arc<sc_catalog::outbox::RunOutbox>>,
}

impl CodeFetchHost {
    pub(crate) fn new(client: reqwest::Client, trigger: impl Into<String>) -> CodeFetchHost {
        CodeFetchHost {
            client,
            trigger: trigger.into(),
            outbox: sc_catalog::outbox::current(),
        }
    }
}

#[async_trait::async_trait]
impl FetchHost for CodeFetchHost {
    async fn fetch(&self, request: Json) -> Result<Json> {
        let url = parse_url(request.get("url").and_then(Json::as_str).unwrap_or(""))?;
        let method = parse_method(
            request
                .get("method")
                .and_then(Json::as_str)
                .unwrap_or("GET"),
        )?;
        let headers = parse_headers(request.get("headers"))?;
        let timeout = parse_timeout(request.get("timeout_ms"));

        // On behalf of a checklist run: kept, not made, and answered with an
        // empty `200` — there is nothing to answer with.
        if let Some(outbox) = &self.outbox {
            let body = parse_body(&request)?
                .map_or(Json::Null, |b| Json::String(String::from_utf8_lossy(&b).into_owned()));
            outbox.capture(sc_catalog::outbox::Sent::new(
                sc_catalog::outbox::SentKind::Webhook,
                vec![url.to_string()],
                method.as_str(),
                body,
            ));
            return Ok(json!({
                "status": 200,
                "status_text": "OK",
                "url": url.as_str(),
                "redirected": false,
                "headers": [],
                "text": "",
            }));
        }

        let mut send = self
            .client
            .request(method.clone(), url.clone())
            .timeout(timeout)
            .headers(headers);
        if let Some(body) = parse_body(&request)? {
            send = send.body(body);
        }

        let response = send
            .send()
            .await
            .map_err(|e| self.failed(&method, &url, &e))?;

        // The final URL, which is not the requested one when a redirect was
        // followed — and the pair of them is how `res.redirected` is answered
        // without the guest having to know what was asked for.
        let landed = response.url().clone();
        let status = response.status();
        let headers: Vec<Json> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                json!([name.as_str(), value.to_str().unwrap_or_default().to_owned()])
            })
            .collect();
        let bytes = self.read_capped(response, &method, &url).await?;

        // Text always, and base64 as well when the bytes are not valid UTF-8. A
        // JSON or HTML answer — which is nearly all of them — crosses the seam
        // once, as itself; a PNG crosses twice, because `res.text()` on one is
        // defined (lossily, as in a browser) and doing that decode here is a
        // `from_utf8_lossy` rather than a decoder written in the guest.
        let mut answer = json!({
            "status": status.as_u16(),
            "status_text": status.canonical_reason().unwrap_or(""),
            "url": landed.as_str(),
            "redirected": landed != url,
            "headers": headers,
        });
        let object = answer.as_object_mut().ok_or_else(|| {
            Error::msg("the fetch answer was not built as an object, which cannot happen")
        })?;
        match String::from_utf8(bytes) {
            Ok(text) => {
                object.insert("text".to_owned(), Json::String(text));
            }
            Err(e) => {
                let bytes = e.into_bytes();
                object.insert(
                    "text".to_owned(),
                    Json::String(String::from_utf8_lossy(&bytes).into_owned()),
                );
                object.insert("base64".to_owned(), Json::String(BASE64.encode(&bytes)));
            }
        }
        Ok(answer)
    }
}

impl CodeFetchHost {
    /// Read the body, refusing one that is larger than the bound.
    ///
    /// Chunk by chunk rather than `bytes()`, because the point of a cap that is
    /// checked after the whole response is in memory is hard to explain: an
    /// endpoint that answers with a gigabyte must be dropped while it is being
    /// answered, not afterwards.
    async fn read_capped(
        &self,
        mut response: reqwest::Response,
        method: &reqwest::Method,
        url: &reqwest::Url,
    ) -> Result<Vec<u8>> {
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => return Err(self.failed(method, url, &e)),
            };
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(Error::invalid(format!(
                    "trigger `{}`: the response to {method} {url} is larger than the \
                     {} MB a code body may read; ask the endpoint for less of it",
                    self.trigger,
                    MAX_RESPONSE_BYTES / (1024 * 1024),
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// A transport failure, in words that say which bound was reached when it
    /// was one of ours. It reaches the body as a `TypeError`, which is what a
    /// browser rejects a failed request with.
    fn failed(&self, method: &reqwest::Method, url: &reqwest::Url, e: &reqwest::Error) -> Error {
        let what = if e.is_timeout() {
            "timed out (a request is bounded by what is left of this code's own \
             time limit)"
                .to_owned()
        } else {
            e.to_string()
        };
        Error::invalid(format!("trigger `{}`: {method} {url} {what}", self.trigger))
    }
}

/// An absolute `http`/`https` URL, or a named refusal.
fn parse_url(raw: &str) -> Result<reqwest::Url> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::invalid("fetch() needs a URL"));
    }
    let url = reqwest::Url::parse(raw)
        .map_err(|e| Error::invalid(format!("fetch(): `{raw}` is not a valid URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::invalid(format!(
            "fetch(): the scheme must be http or https, got `{}`",
            url.scheme()
        )));
    }
    Ok(url)
}

/// One of [`METHODS`], upper-cased by the guest and checked again here.
fn parse_method(raw: &str) -> Result<reqwest::Method> {
    let name = raw.trim().to_uppercase();
    if !METHODS.contains(&name.as_str()) {
        return Err(Error::invalid(format!(
            "fetch(): `{name}` is not one of {}",
            METHODS.join(", ")
        )));
    }
    reqwest::Method::from_bytes(name.as_bytes())
        .map_err(|e| Error::invalid(format!("fetch(): `{name}`: {e}")))
}

/// The `[[name, value], …]` pairs the guest built, as a header map.
///
/// Appended rather than inserted, so a header the body repeated is repeated on
/// the wire — which is the difference that matters for `set-cookie` and costs
/// nothing anywhere else.
fn parse_headers(raw: Option<&Json>) -> Result<reqwest::header::HeaderMap> {
    let mut map = reqwest::header::HeaderMap::new();
    let Some(Json::Array(pairs)) = raw else {
        return Ok(map);
    };
    for pair in pairs {
        let (Some(name), Some(value)) = (
            pair.get(0).and_then(Json::as_str),
            pair.get(1).and_then(Json::as_str),
        ) else {
            return Err(Error::invalid(
                "fetch(): a header is a [name, value] pair of strings",
            ));
        };
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::invalid(format!("fetch(): `{name}` is not a valid header name")))?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
            Error::invalid(format!(
                "fetch(): the value of `{name}` is not a valid header value"
            ))
        })?;
        map.append(name, value);
    }
    Ok(map)
}

/// The request body: the text as written, or the bytes the guest base64'd.
fn parse_body(request: &Json) -> Result<Option<Vec<u8>>> {
    let body = match request.get("body") {
        None | Some(Json::Null) => return Ok(None),
        Some(Json::String(body)) => body,
        Some(other) => {
            return Err(Error::invalid(format!(
                "fetch(): the body reached the host as {other}, which is not text"
            )));
        }
    };
    if request.get("body_base64") == Some(&Json::Bool(true)) {
        let bytes = BASE64
            .decode(body)
            .map_err(|e| Error::invalid(format!("fetch(): the body is not valid base64: {e}")))?;
        return Ok(Some(bytes));
    }
    Ok(Some(body.clone().into_bytes()))
}

/// The wall clock for one request.
///
/// Already clamped to what is left of the run by `sc_expr`'s op, so this is a
/// reader and not a policy: an absent or unreadable value means the default,
/// because a request with no clock at all is the one thing that must not happen.
fn parse_timeout(raw: Option<&Json>) -> Duration {
    raw.and_then(Json::as_u64)
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_FETCH_TIMEOUT, Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_must_be_absolute_http() {
        for (raw, expected) in [
            ("", "needs a URL"),
            ("/hook", "not a valid URL"),
            ("file:///etc/passwd", "must be http or https"),
            ("data:text/plain,hi", "must be http or https"),
        ] {
            let msg = parse_url(raw).unwrap_err().to_string();
            assert!(msg.contains(expected), "{raw}: {msg}");
        }
        assert_eq!(
            parse_url(" https://example.com/hook?a=1 ")
                .unwrap()
                .as_str(),
            "https://example.com/hook?a=1"
        );
    }

    #[test]
    fn the_methods_are_the_seven_a_trigger_has_business_sending() {
        for name in METHODS {
            assert_eq!(parse_method(name).unwrap().as_str(), name);
        }
        assert_eq!(parse_method("post").unwrap(), reqwest::Method::POST);
        // A proxy or an echo of this server's own request headers is not what
        // "call an endpoint" means.
        for refused in ["CONNECT", "TRACE", "TRACK", "nonsense"] {
            let msg = parse_method(refused).unwrap_err().to_string();
            assert!(msg.contains("is not one of"), "{refused}: {msg}");
        }
    }

    #[test]
    fn a_repeated_header_stays_repeated_and_an_illegal_one_is_refused() {
        let map = parse_headers(Some(&json!([
            ["x-a", "1"],
            ["x-a", "2"],
            ["content-type", "application/json"]
        ])))
        .unwrap();
        assert_eq!(map.get_all("x-a").iter().count(), 2);
        assert_eq!(map.get("content-type").unwrap(), "application/json");
        // The guest checks these too, and the message there names the line that
        // wrote it — but the seam is a boundary, so they are checked again here.
        for bad in [
            json!([["not a name", "v"]]),
            json!([["x-a", "two\r\nlines"]]),
            json!([["x-a"]]),
        ] {
            assert!(parse_headers(Some(&bad)).is_err(), "{bad}");
        }
        assert!(parse_headers(None).unwrap().is_empty());
    }

    #[test]
    fn a_body_is_text_or_the_bytes_it_was_base64ed_from() {
        assert_eq!(parse_body(&json!({})).unwrap(), None);
        assert_eq!(parse_body(&json!({ "body": Json::Null })).unwrap(), None);
        assert_eq!(
            parse_body(&json!({ "body": "a,b", "body_base64": false })).unwrap(),
            Some(b"a,b".to_vec())
        );
        assert_eq!(
            parse_body(&json!({ "body": "AJ+Slg==", "body_base64": true })).unwrap(),
            Some(vec![0, 159, 146, 150])
        );
        assert!(parse_body(&json!({ "body": 7 })).is_err());
        assert!(parse_body(&json!({ "body": "not base64!", "body_base64": true })).is_err());
    }

    #[test]
    fn the_clock_arrives_clamped_and_is_only_read_here() {
        assert_eq!(
            parse_timeout(Some(&json!(1500))),
            Duration::from_millis(1500)
        );
        // Absent, zero or nonsense is the default rather than "no clock".
        for raw in [None, Some(json!(0)), Some(json!("soon")), Some(json!(-1))] {
            assert_eq!(parse_timeout(raw.as_ref()), DEFAULT_FETCH_TIMEOUT);
        }
    }
}

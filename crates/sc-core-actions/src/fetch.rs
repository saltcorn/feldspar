//! `fetch` — call an HTTP endpoint and make its response the action's result.

use std::time::Duration;

use sc_error::{Error, Result};
use sc_expr::{Formula, RenderMode, Template};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use sc_action::{
    Action, ActionContext, ConfigCheck, EVENT_SCOPE, Event, check_formula, event_formula_value,
    optional_formula, render_event_template, required_template, template_scope,
};

/// The `url` setting — a template, so a request can be addressed to the row it
/// is about.
const CFG_URL: &str = "url";
/// The HTTP method.
const CFG_METHOD: &str = "method";
/// Literal request headers, as a JSON object of name → value.
const CFG_HEADERS: &str = "headers";
/// The formula computing the request body; absent means "the event".
const CFG_BODY: &str = "body";
/// The per-request timeout.
const CFG_TIMEOUT: &str = "timeout_ms";

/// The methods a trigger may use, in the order the admin picker lists them.
const METHODS: [&str; 5] = ["POST", "GET", "PUT", "PATCH", "DELETE"];

/// How long a request may take when the configuration does not say.
const DEFAULT_TIMEOUT_MS: i64 = 10_000;
/// The upper bound on a configured timeout.
///
/// Bounded on purpose: a trigger runs inside the request or the write that fired
/// it, so an unbounded `fetch` is an unbounded hold on that caller. A minute is
/// more than any webhook should need and short enough that a hung endpoint is a
/// visible failure rather than a stuck server.
const MAX_TIMEOUT_MS: i64 = 60_000;

/// How much of a failed response's body is quoted in the error.
const ERROR_BODY_CHARS: usize = 300;

/// Send an HTTP request built from the event, and return the response.
///
/// Named `fetch` rather than `webhook` because the **response is the point**:
/// the parsed body is the action's result, so a directly-run trigger can return
/// it to its caller and a workflow step will put it in the run context. A
/// fire-and-forget notification is the same action with the result ignored.
pub struct Fetch {
    /// One client, built at registration: it carries the connection pool and the
    /// TLS configuration, and rebuilding those per request would pay for a new
    /// handshake on every firing.
    client: reqwest::Client,
}

impl Fetch {
    /// Build the action and its HTTP client.
    ///
    /// Fallible because constructing the client initialises the TLS stack, and a
    /// deployment where that fails must say so at boot rather than at the first
    /// firing.
    pub fn new() -> Result<Fetch> {
        Ok(Fetch {
            client: http_client()?,
        })
    }
}

/// The HTTP client this crate sends with — this action's, and a code body's
/// `fetch` (`crate::code_fetch`). One builder for both, so a deployment's TLS
/// settings and connection pooling are one thing to reason about rather than
/// two that could drift.
pub(crate) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .build()
        .map_err(|e| Error::config(format!("could not build the HTTP client: {e}")))
}

#[async_trait::async_trait]
impl Action for Fetch {
    fn name(&self) -> &str {
        "fetch"
    }

    fn description(&self) -> &str {
        "Send an HTTP request and return the response body as the result"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_URL, BasicType::Text)
                .label("URL")
                .required(),
            FormField::new(CFG_METHOD, BasicType::Text)
                .label("Method")
                .options(METHODS)
                .default_value(METHODS[0]),
            FormField::new(CFG_HEADERS, BasicType::Json).label("Headers"),
            FormField::new(CFG_BODY, BasicType::Text).label("Body"),
            FormField::new(CFG_TIMEOUT, BasicType::Int)
                .label("Timeout (ms)")
                .default_value(DEFAULT_TIMEOUT_MS),
        ]
    }

    async fn validate_config(&self, check: &ConfigCheck<'_>) -> Result<()> {
        // Everything the request is built from, resolved now so a request that
        // cannot be built is a message on the form rather than a firing that
        // fails. Each of these is the *same* function `run` uses, so there is no
        // second parser to disagree with the first.
        let url = url_template(check.config)?;
        if url.is_literal() {
            // No tokens: the URL is what it will be at every firing, and it is
            // parsed and checked here exactly as it always was.
            parse_url(url.source())?;
        } else {
            // With tokens there is nothing to parse until a row is in hand, so
            // what is checked here is what *can* be: that every identifier in
            // every token resolves in the event's scope. The parse happens at
            // send, with the same "not a valid URL" message.
            check
                .template(template_scope(check.channel), &url, &format!("`{CFG_URL}`"))
                .await?;
        }
        headers(check.config)?;
        timeout(check.config)?;
        let method = method(check.config)?;
        if let Some(formula) = body_formula(check.config)? {
            // A GET carries no body, so a configured one would be silently
            // dropped — which is worse than saying so while the admin is looking
            // at the form. (The *default* body is simply not sent for those
            // methods; nothing is lost, because nothing was written.)
            if !takes_body(&method) {
                return Err(Error::invalid(format!(
                    "a `{method}` request sends no body, but a `{CFG_BODY}` formula was given"
                )));
            }
            check_formula(check.shape, EVENT_SCOPE, &formula, &format!("`{CFG_BODY}`"))?;
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut ActionContext<'_>) -> Result<Json> {
        // Rendered as **text**: a URL is not HTML, and escaping one would turn
        // the `&` between two query parameters into `&amp;`. A URL with no
        // tokens costs nothing — a literal template renders to itself without
        // reaching the isolate.
        let rendered = render_event_template(
            ctx,
            &url_template(ctx.config)?,
            &format!("`{CFG_URL}`"),
            RenderMode::Text,
        )
        .await?;
        let url = parse_url(&rendered)?;
        let method = method(ctx.config)?;
        let headers = headers(ctx.config)?;
        let timeout = timeout(ctx.config)?;

        let mut request = self
            .client
            .request(method.clone(), url.clone())
            .timeout(timeout);
        if takes_body(&method) {
            // The default body is the event itself: the common case is "tell that
            // service what happened", and spelling it out as a formula would be
            // ceremony. A formula replaces it wholesale.
            let body = match body_formula(ctx.config)? {
                // A formula needs the engine; the default body does not, so it is
                // only asked for when there is a formula to evaluate.
                Some(formula) => {
                    event_formula_value(ctx, &formula, &format!("`{CFG_BODY}`")).await?
                }
                None => event_json(ctx.event),
            };
            request = request.json(&body);
        }
        // The configured headers go on last, so an explicit `content-type`
        // overrides the one the JSON body set.
        let response = request
            .headers(headers)
            .send()
            .await
            .map_err(|e| failed(ctx.trigger, &url, e))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| failed(ctx.trigger, &url, e))?;
        let text = String::from_utf8_lossy(&bytes);
        if !status.is_success() {
            // An application error: the endpoint answered, and what it said is
            // the most useful thing the admin can be told.
            return Err(Error::invalid(format!(
                "trigger `{}`: {method} {url} returned {}{}",
                ctx.trigger,
                status.as_u16(),
                quoted_body(&text),
            )));
        }
        Ok(parse_body(&text))
    }
}

/// The response body as the action's result: parsed JSON when it is JSON, the
/// text as a JSON string when it is not, and `null` when it is empty.
///
/// A non-JSON body is **not** an error: an endpoint that answers `OK` to a
/// notification has done what was asked, and failing the trigger over its
/// content type would make `fetch` unusable for half of what it is for.
fn parse_body(text: &str) -> Json {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Json::Null;
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| Json::String(text.to_owned()))
}

/// The event as the default request body — the shape a receiving service is
/// handed: what happened, to what, with which rows, and who caused it.
///
/// Written out here rather than derived, because this JSON is a **wire contract**
/// with whatever is on the other end: it must not change silently when a field is
/// added to [`Event`].
fn event_json(event: &Event) -> Json {
    json!({
        "event": event.kind.as_str(),
        "channel": event.channel,
        "row": event.row,
        "old_row": event.old_row,
        "payload": event.payload,
        "role": event.role,
        "user": event.user,
    })
}

/// The URL setting, as a template.
fn url_template(config: &Attrs) -> Result<Template> {
    required_template(config, CFG_URL)
}

/// A rendered URL, which must be an absolute `http`/`https` one.
///
/// Anything else is refused by name: a relative URL has nothing to resolve
/// against server-side, and a `file:`/`data:` scheme is a way to make the server
/// read something local, which is not what "call an endpoint" means. That check
/// matters more now that the URL is a template: what a row interpolates into one
/// is data, and data must not be able to choose the scheme.
fn parse_url(raw: &str) -> Result<reqwest::Url> {
    let raw = raw.trim();
    let url = reqwest::Url::parse(raw)
        .map_err(|e| Error::invalid(format!("`{CFG_URL}`: `{raw}` is not a valid URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::invalid(format!(
            "`{CFG_URL}`: the scheme must be http or https, got `{}`",
            url.scheme()
        )));
    }
    Ok(url)
}

/// The configured method, defaulting to the first of [`METHODS`].
fn method(config: &Attrs) -> Result<reqwest::Method> {
    let name = match config.get(CFG_METHOD) {
        Some(Json::String(s)) if !s.trim().is_empty() => s.trim().to_uppercase(),
        _ => METHODS[0].to_owned(),
    };
    if !METHODS.contains(&name.as_str()) {
        return Err(Error::invalid(format!(
            "`{CFG_METHOD}`: `{name}` is not one of {}",
            METHODS.join(", ")
        )));
    }
    reqwest::Method::from_bytes(name.as_bytes())
        .map_err(|e| Error::invalid(format!("`{CFG_METHOD}`: `{name}`: {e}")))
}

/// Whether a request with this method carries a body.
fn takes_body(method: &reqwest::Method) -> bool {
    !matches!(*method, reqwest::Method::GET | reqwest::Method::HEAD)
}

/// The configured headers, as literal name → value pairs.
///
/// Not formulas: a header is a protocol detail of the endpoint being called, and
/// the values that vary with the event belong in the body. Each name and value is
/// checked here, so an illegal one is a save-time error rather than a request
/// that cannot be built.
fn headers(config: &Attrs) -> Result<reqwest::header::HeaderMap> {
    let mut map = reqwest::header::HeaderMap::new();
    let value = match config.get(CFG_HEADERS) {
        None | Some(Json::Null) => return Ok(map),
        Some(value) => value,
    };
    let Json::Object(entries) = value else {
        return Err(Error::invalid(format!(
            "`{CFG_HEADERS}` must be an object of header name → value"
        )));
    };
    for (name, value) in entries {
        let Json::String(value) = value else {
            return Err(Error::invalid(format!(
                "`{CFG_HEADERS}`.`{name}` must be a string"
            )));
        };
        let name = reqwest::header::HeaderName::try_from(name.as_str()).map_err(|e| {
            Error::invalid(format!("`{CFG_HEADERS}`: `{name}` is not a header: {e}"))
        })?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|e| {
            Error::invalid(format!("`{CFG_HEADERS}`.`{name}`: not a header value: {e}"))
        })?;
        map.insert(name, value);
    }
    Ok(map)
}

/// The configured timeout, defaulted and **bounded** (see [`MAX_TIMEOUT_MS`]).
///
/// An out-of-range value is refused rather than clamped: an admin who typed five
/// minutes should be told it is not allowed, not quietly given one.
fn timeout(config: &Attrs) -> Result<Duration> {
    let ms = match config.get(CFG_TIMEOUT) {
        None | Some(Json::Null) => DEFAULT_TIMEOUT_MS,
        Some(Json::Number(n)) => n.as_i64().ok_or_else(|| {
            Error::invalid(format!(
                "`{CFG_TIMEOUT}` must be a whole number of milliseconds"
            ))
        })?,
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{CFG_TIMEOUT}` must be a whole number of milliseconds, got {other}"
            )));
        }
    };
    if !(1..=MAX_TIMEOUT_MS).contains(&ms) {
        return Err(Error::invalid(format!(
            "`{CFG_TIMEOUT}` must be between 1 and {MAX_TIMEOUT_MS} milliseconds, got {ms}"
        )));
    }
    Ok(Duration::from_millis(ms.unsigned_abs()))
}

/// The body formula, when one is configured.
fn body_formula(config: &Attrs) -> Result<Option<Formula>> {
    optional_formula(config, CFG_BODY)
}

/// A transport failure — DNS, connection, TLS, timeout — as an application error
/// naming the trigger and what it was calling.
///
/// `Invalid` rather than `Internal`: the endpoint the admin configured did not
/// answer, which is a fact about their configuration or about the world, not a
/// bug in the server. The timeout case says so in as many words, because
/// "operation timed out" without the bound is a message that sends people
/// looking in the wrong place.
fn failed(trigger: &str, url: &reqwest::Url, e: reqwest::Error) -> Error {
    let what = if e.is_timeout() {
        "timed out".to_owned()
    } else {
        e.to_string()
    };
    Error::invalid(format!("trigger `{trigger}`: request to {url} {what}"))
}

/// A response body quoted into an error message, truncated so a page of HTML
/// cannot become the error.
fn quoted_body(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut quoted: String = trimmed.chars().take(ERROR_BODY_CHARS).collect();
    if trimmed.chars().count() > ERROR_BODY_CHARS {
        quoted.push('…');
    }
    format!(": {quoted}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(entries: &[(&str, Json)]) -> Attrs {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn the_spec_defaults_the_method_and_the_timeout() {
        let fetch = Fetch::new().expect("client builds");
        let spec = fetch.config_spec();
        let names: Vec<&str> = spec.iter().map(|f| f.name()).collect();
        assert_eq!(
            names,
            vec!["url", "method", "headers", "body", "timeout_ms"]
        );
        // Only the URL is required: everything else has a working default.
        assert_eq!(spec.iter().filter(|f| f.required).count(), 1);
        // An empty configuration is a POST of the event with the default timeout.
        let empty = Attrs::new();
        assert_eq!(method(&empty).unwrap(), reqwest::Method::POST);
        assert_eq!(
            timeout(&empty).unwrap(),
            Duration::from_millis(DEFAULT_TIMEOUT_MS.unsigned_abs())
        );
        assert!(body_formula(&empty).unwrap().is_none());
        assert!(headers(&empty).unwrap().is_empty());
    }

    #[test]
    fn a_url_must_be_absolute_http() {
        for (raw, expected) in [
            ("/webhook", "not a valid URL"),
            ("file:///etc/passwd", "must be http or https"),
        ] {
            let msg = parse_url(raw).unwrap_err().to_string();
            assert!(msg.contains(expected), "{raw}: {msg}");
        }
        assert_eq!(
            parse_url("https://example.com/hook?a=1")
                .unwrap()
                .host_str(),
            Some("example.com")
        );
        // A blank URL is a missing setting, said with the setting's name.
        let msg = url_template(&config(&[("url", json!(""))]))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("required"), "{msg}");
    }

    /// A URL with tokens is a template; one without is the same string it always
    /// was, and never reaches the template machinery for anything but a copy.
    #[test]
    fn a_url_with_tokens_is_a_template_and_one_without_is_a_literal() {
        let literal = url_template(&config(&[("url", json!("https://example.com/hook"))])).unwrap();
        assert!(literal.is_literal());
        assert_eq!(literal.source(), "https://example.com/hook");

        let templated = url_template(&config(&[(
            "url",
            json!("https://example.com/orders/{{ id }}"),
        )]))
        .unwrap();
        assert!(!templated.is_literal());
        // The token's identifier is what save-time validation resolves against
        // the event's table; nothing here can parse a URL yet.
        assert_eq!(templated.tokens().count(), 1);

        // An unclosed token is a parse error naming the setting.
        let msg = url_template(&config(&[("url", json!("https://x.test/{{ id"))]))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("url"), "{msg}");
    }

    #[test]
    fn the_timeout_is_bounded_and_refused_rather_than_clamped() {
        let too_long = config(&[("timeout_ms", json!(MAX_TIMEOUT_MS + 1))]);
        let msg = timeout(&too_long).unwrap_err().to_string();
        assert!(msg.contains(&MAX_TIMEOUT_MS.to_string()), "{msg}");
        assert!(timeout(&config(&[("timeout_ms", json!(0))])).is_err());
        assert!(timeout(&config(&[("timeout_ms", json!("soon"))])).is_err());
        assert_eq!(
            timeout(&config(&[("timeout_ms", json!(250))])).unwrap(),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn headers_are_checked_where_the_admin_can_fix_them() {
        let ok = config(&[("headers", json!({ "x-token": "abc" }))]);
        assert_eq!(headers(&ok).unwrap().len(), 1);
        for bad in [json!({ "bad header": "x" }), json!({ "x": 7 }), json!("x")] {
            let cfg = config(&[("headers", bad.clone())]);
            assert!(headers(&cfg).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_response_body_is_parsed_when_it_is_json_and_kept_when_it_is_not() {
        assert_eq!(parse_body("{\"a\":1}"), json!({ "a": 1 }));
        assert_eq!(parse_body("  [1,2] "), json!([1, 2]));
        assert_eq!(parse_body(""), Json::Null);
        // Not JSON: the text itself, rather than an error that would make `fetch`
        // useless against endpoints that answer `OK`.
        assert_eq!(parse_body("OK"), json!("OK"));
    }

    #[test]
    fn the_default_body_is_the_event_as_a_stable_object() {
        let event = Event::new(sc_action::EventKind::Insert)
            .on("books")
            .row(json!({ "id": 1 }))
            .caller(1, Some(json!({ "email": "a@b.c" })));
        assert_eq!(
            event_json(&event),
            json!({
                "event": "insert",
                "channel": "books",
                "row": { "id": 1 },
                "old_row": Json::Null,
                "payload": Json::Null,
                "role": 1,
                "user": { "email": "a@b.c" },
            })
        );
    }

    #[test]
    fn a_quoted_error_body_is_truncated() {
        let long = "x".repeat(ERROR_BODY_CHARS * 2);
        let quoted = quoted_body(&long);
        assert!(quoted.ends_with('…'), "{quoted}");
        assert_eq!(quoted.chars().count(), ERROR_BODY_CHARS + 3);
        assert_eq!(quoted_body("  "), "");
    }
}

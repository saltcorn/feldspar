//! `call_api` — send one HTTP request to the application and see what it
//! answers.
//!
//! An agent writing a page against the application's API writes it against
//! what it believes an endpoint returns: the shape of a row, the error a
//! refused write gets, whether a custom query sees the rows it should. This
//! tool is how it finds out instead. The request goes through the server's own
//! router to the application's **live mount** (its API is the catalog's, not the
//! build's, so the live mount and a run's preview answer it alike), with a CSRF
//! token, exactly as the application's own page would send it.
//!
//! **As whom** is an argument, because the question is often "what does *this*
//! user get?":
//!
//! - nothing: the run's caller — the person chatting, or for a run nobody is
//!   present for, the account `view_app_user` names;
//! - `"public"`: no session, what an anonymous visitor gets;
//! - an email: that user, with a session made for this one request and ended
//!   after it. Anyone may name themselves; naming **another** user is for an
//!   administrator's run only, because it is acting as them.
//!
//! **It is not a grant.** It is offered in every mode wherever the `coding`
//! trait's `application` setting names one, as `list_assets` is. Outside `act`
//! — a `plan` run, an `explore` helper — only `GET` is offered (and `GET` and
//! `HEAD` accepted), because those runs only look. In `act` every method is,
//! and the description says that the data is the live data.

use std::time::Duration;

use sc_agent::{AppHttpRequest, AppHttpResponse, Elidable, RunMode, TraitContext};
use sc_auth::User;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use super::check::configured_application;
use super::is_admin;
use super::view_app::viewer;
use crate::files::FileScope;

/// How long one request may take, reading the body included.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The most of a body the model is shown, in characters.
const MAX_BODY_CHARS: usize = 12_000;

/// The `user` that sends no session.
const PUBLIC: &str = "public";

/// The methods that only look: all a `plan` run is offered.
const LOOKING: [&str; 2] = ["GET", "HEAD"];

/// Every method the tool sends.
const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// The methods the tool's schema names; `HEAD` and `OPTIONS` are accepted
/// too, and not spent on every request's prefix.
const OFFERED: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// Headers the requester sets itself, which an argument may not.
const RESERVED_HEADERS: [&str; 3] = ["host", "cookie", "content-length"];

/// Response headers every answer carries and that say nothing about the
/// endpoint: left out of the result.
const BOILERPLATE_HEADERS: [&str; 6] = [
    "content-security-policy",
    "x-content-type-options",
    "x-frame-options",
    "referrer-policy",
    "content-length",
    "date",
];

/// The tool one configured scope offers.
pub fn tool_name(scope: &FileScope) -> String {
    format!("call_api_{}", scope.slug())
}

/// Whether the tool is offered: wherever there is an application to send to.
pub fn offered(config: &Attrs) -> bool {
    configured_application(config).is_some()
}

/// The tool, with only the looking methods outside `act`.
pub fn spec(scope: &FileScope, config: &Attrs, mode: RunMode) -> ToolSpec {
    let application = configured_application(config).unwrap_or_default();
    let acts = mode == RunMode::Act;
    let methods: Vec<&str> = if acts { OFFERED.to_vec() } else { vec!["GET"] };
    ToolSpec::new(
        tool_name(scope),
        format!(
            "Send one HTTP request to `{application}`'s live server and return the response \
             status, headers and body. Use it to see what an API endpoint actually returns \
             (the shape of its rows, its errors) before writing code against it. The request \
             is sent as the user you are working for, unless `user` names another user by \
             email (only in an admin's run) or is \"public\" for a visitor who is not signed \
             in. Table permissions apply as they would to that user.{}",
            if acts {
                " The data is live: POST, PUT, PATCH and DELETE change real rows."
            } else {
                " Only GET is available here."
            }
        ),
        json!({
            "type": "object",
            "properties": {
                "method": {"type": "string", "enum": methods},
                "path": {"type": "string", "description": "Path and query, e.g. /api/tasks?done=false"},
                "body": {"description": "Request body: JSON, or a string sent as it is"},
                "headers": {"type": "object", "description": "Extra request headers, e.g. {\"accept\": \"text/csv\"}"},
                "user": {"type": "string", "description": "Email of the user to send as, or \"public\"; default: you"},
            },
            "required": ["path"],
            "additionalProperties": false,
        }),
    )
}

/// The method the arguments ask for, `GET` when they name none.
fn method(args: &Json, mode: RunMode) -> Result<String> {
    let method = args
        .get("method")
        .and_then(Json::as_str)
        .unwrap_or("GET")
        .trim()
        .to_uppercase();
    if !METHODS.contains(&method.as_str()) {
        return Err(Error::invalid(format!(
            "`{method}` is not a method call_api sends; use {}",
            METHODS.join(", ")
        )));
    }
    if mode != RunMode::Act && !LOOKING.contains(&method.as_str()) {
        return Err(Error::invalid(format!(
            "`{method}` can change data, and a `{mode}` run only looks: use GET or HEAD"
        )));
    }
    Ok(method)
}

/// The path and query the arguments name. An absolute URL is taken for its
/// path, since the host is always the application's.
fn path(args: &Json) -> Result<String> {
    let raw = args
        .get("path")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| Error::invalid("call_api needs `path`, e.g. /api/tasks"))?;
    let path = match raw.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => raw,
    };
    let path = path.split('#').next().unwrap_or_default();
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(Error::invalid(format!(
            "`{raw}` is not a path on the application; start it with /"
        )));
    }
    if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::invalid(format!(
            "`{raw}` has whitespace in it; percent-encode it (%20)"
        )));
    }
    Ok(path.to_owned())
}

/// The headers the arguments add, refusing the ones the requester owns.
fn headers(args: &Json) -> Result<Vec<(String, String)>> {
    let Some(given) = args.get("headers").filter(|h| !h.is_null()) else {
        return Ok(Vec::new());
    };
    let Some(given) = given.as_object() else {
        return Err(Error::invalid(
            "`headers` should be an object of names to strings",
        ));
    };
    let mut out = Vec::new();
    for (name, value) in given {
        let lower = name.trim().to_lowercase();
        if RESERVED_HEADERS.contains(&lower.as_str()) {
            return Err(Error::invalid(format!(
                "call_api sets `{name}` itself; choose whom the request is from with `user`"
            )));
        }
        let value = match value {
            Json::String(s) => s.clone(),
            other => other.to_string(),
        };
        out.push((lower, value));
    }
    Ok(out)
}

/// The body's bytes, adding a JSON content type when the arguments set none
/// and the body is JSON.
///
/// A string is sent as it is — it is what a model writes for a form body or a
/// JSON document it has already serialised — and labelled JSON only when it
/// parses as JSON. Anything else is JSON, serialised.
fn body(args: &Json, headers: &mut Vec<(String, String)>) -> Vec<u8> {
    let typed = headers.iter().any(|(name, _)| name == "content-type");
    let (bytes, json) = match args.get("body") {
        None | Some(Json::Null) => return Vec::new(),
        Some(Json::String(text)) => (
            text.clone().into_bytes(),
            serde_json::from_str::<Json>(text).is_ok(),
        ),
        Some(other) => (other.to_string().into_bytes(), true),
    };
    if !typed {
        let content_type = if json {
            "application/json"
        } else {
            "text/plain; charset=utf-8"
        };
        headers.push(("content-type".to_owned(), content_type.to_owned()));
    }
    bytes
}

/// Whom the request is from: `None` for no session.
async fn sender(config: &Attrs, args: &Json, ctx: &TraitContext<'_>) -> Result<Option<User>> {
    let named = args
        .get("user")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let Some(named) = named else {
        // A visitor who is not signed in sends as the public, never as the
        // agent's `view_app_user` (security review #20).
        if ctx.caller.is_anonymous() {
            return Ok(None);
        }
        return viewer(config, ctx).await.map(Some).map_err(|_| {
            Error::invalid(format!(
                "nobody is chatting in this run, so there is no `you` to send as, and agent `{}` \
                 has no `view_app_user`: pass `user` as an email, or \"{PUBLIC}\"",
                ctx.agent
            ))
        });
    };
    if named.eq_ignore_ascii_case(PUBLIC) {
        return Ok(None);
    }
    if let Some(me) = &ctx.caller.user {
        if email_of(me).is_some_and(|email| email.eq_ignore_ascii_case(named)) {
            return Ok(Some(me.clone()));
        }
    }
    if !is_admin(ctx.caller) {
        return Err(Error::invalid(format!(
            "only an administrator's run may send a request as another user; send it as \
             yourself (leave out `user`) or as \"{PUBLIC}\""
        )));
    }
    sc_auth::load_user_by_email(ctx.catalog, named)
        .await?
        .map(Some)
        .ok_or_else(|| Error::invalid(format!("no user has the email `{named}`")))
}

/// A user's email, where the row has one.
fn email_of(user: &User) -> Option<&str> {
    match user.extra.get("email") {
        Some(sc_query::Value::Text(email)) => Some(email),
        _ => None,
    }
}

/// Send the request and report the answer.
pub async fn call(config: &Attrs, args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let application = configured_application(config).ok_or_else(|| {
        Error::invalid(format!(
            "call_api needs the `coding` trait's `{}` setting",
            crate::CFG_APPLICATION
        ))
    })?;
    let method = method(args, ctx.mode)?;
    let path = path(args)?;
    let mut headers = headers(args)?;
    let body = body(args, &mut headers);
    let user = sender(config, args, ctx).await?;
    let who = match &user {
        None => PUBLIC.to_owned(),
        Some(user) => email_of(user)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("user {}", user.id)),
    };
    let response = ctx
        .require_requests()?
        .request(AppHttpRequest {
            subdomain: &application,
            preview: None,
            method: method.clone(),
            path: path.clone(),
            headers,
            body,
            user: user.as_ref(),
            timeout: TIMEOUT,
        })
        .await?;
    Ok(Json::String(report(&method, &path, &who, &response)))
}

/// The answer as text: a status line, the headers that say something, and the
/// body — pretty-printed when it is JSON, cut at [`MAX_BODY_CHARS`].
fn report(method: &str, path: &str, who: &str, response: &AppHttpResponse) -> String {
    let reason = reason_phrase(response.status);
    let mut out = format!(
        "{method} {path} as {who} → {}{}",
        response.status,
        if reason.is_empty() {
            String::new()
        } else {
            format!(" {reason}")
        }
    );
    let mut content_type = "";
    for (name, value) in &response.headers {
        if BOILERPLATE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if name == "content-type" {
            content_type = value;
        }
        if name == "set-cookie" {
            // The name says what happened; the value is a credential.
            let cookie = value.split('=').next().unwrap_or_default();
            out.push_str(&format!("\nset-cookie: {cookie}=…"));
            continue;
        }
        out.push_str(&format!("\n{name}: {value}"));
    }
    if response.body.is_empty() {
        out.push_str("\n(no body)");
    } else {
        out.push_str(&format!("\nbody ({} bytes):\n", response.body.len()));
        out.push_str(&shown_body(content_type, &response.body));
    }
    if let Some(why) = &response.truncated {
        out.push_str(&format!("\nnote: the body is incomplete: {why}"));
    }
    out
}

/// The body as the model reads it.
fn shown_body(content_type: &str, body: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(body) else {
        let kind = if content_type.is_empty() {
            "binary"
        } else {
            content_type
        };
        return format!("[{} bytes of {kind}, not shown]", body.len());
    };
    let pretty = serde_json::from_str::<Json>(text)
        .ok()
        .and_then(|json| serde_json::to_string_pretty(&json).ok());
    let text = pretty.as_deref().unwrap_or(text);
    let count = text.chars().count();
    if count <= MAX_BODY_CHARS {
        return text.to_owned();
    }
    let cut: String = text.chars().take(MAX_BODY_CHARS).collect();
    format!(
        "{cut}\n… [{} more characters not shown; narrow the request, e.g. with a query]",
        count - MAX_BODY_CHARS
    )
}

/// The reason phrase for a status, where it has a standard one.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// An old result, as its status line: `[elided GET /api/tasks as public → 200 OK]`.
pub fn elide(old: &Elidable<'_>) -> String {
    format!(
        "[elided {}]",
        old.content.lines().next().unwrap_or_default()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plan_only_looks() {
        assert_eq!(method(&json!({}), RunMode::Plan).unwrap(), "GET");
        assert_eq!(
            method(&json!({"method": "post"}), RunMode::Act).unwrap(),
            "POST"
        );
        let refused = method(&json!({"method": "DELETE"}), RunMode::Plan).unwrap_err();
        assert!(refused.to_string().contains("only looks"), "{refused}");
        assert!(method(&json!({"method": "TRACE"}), RunMode::Act).is_err());

        let scope = FileScope {
            store: "code".to_owned(),
            root: "web".to_owned(),
        };
        let config: Attrs = [("application".to_owned(), json!("todo"))]
            .into_iter()
            .collect();
        let plan = spec(&scope, &config, RunMode::Plan);
        let act = spec(&scope, &config, RunMode::Act);
        assert_eq!(plan.name, "call_api_code_web");
        assert!(!plan.parameters.to_string().contains("DELETE"));
        assert!(act.parameters.to_string().contains("DELETE"));
        assert!(act.description.contains("data is live"));
        assert!(act.description.contains("\"public\""));
        assert!(!plan.description.contains("data is live"));
        assert!(plan.description.contains("Only GET"));
    }

    #[test]
    fn a_path_is_on_the_application() {
        assert_eq!(
            path(&json!({"path": "/api/tasks?done=false"})).unwrap(),
            "/api/tasks?done=false"
        );
        assert_eq!(
            path(&json!({"path": "https://todo.example.com/api/tasks#top"})).unwrap(),
            "/api/tasks"
        );
        assert_eq!(
            path(&json!({"path": "http://todo.example.com"})).unwrap(),
            "/"
        );
        assert!(path(&json!({"path": "api/tasks"})).is_err());
        assert!(path(&json!({"path": "//evil.example/x"})).is_err());
        assert!(path(&json!({"path": "/a b"})).is_err());
        assert!(path(&json!({})).is_err());
    }

    #[test]
    fn the_requester_owns_the_session_headers() {
        let refused = headers(&json!({"headers": {"Cookie": "sc_session=x"}})).unwrap_err();
        assert!(refused.to_string().contains("`user`"), "{refused}");
        assert!(headers(&json!({"headers": {"Host": "elsewhere"}})).is_err());
        assert_eq!(
            headers(&json!({"headers": {"Accept": "text/csv"}})).unwrap(),
            vec![("accept".to_owned(), "text/csv".to_owned())]
        );
    }

    #[test]
    fn a_body_is_labelled_json_when_it_is() {
        let mut h = Vec::new();
        assert_eq!(
            body(&json!({"body": {"title": "x"}}), &mut h),
            br#"{"title":"x"}"#
        );
        assert_eq!(
            h,
            vec![("content-type".to_owned(), "application/json".to_owned())]
        );

        let mut h = Vec::new();
        assert_eq!(body(&json!({"body": "{\"a\":1}"}), &mut h), br#"{"a":1}"#);
        assert_eq!(h[0].1, "application/json");

        let mut h = Vec::new();
        body(&json!({"body": "plain words"}), &mut h);
        assert_eq!(h[0].1, "text/plain; charset=utf-8");

        let mut h = vec![(
            "content-type".to_owned(),
            "application/x-www-form-urlencoded".to_owned(),
        )];
        assert_eq!(body(&json!({"body": "a=1&b=2"}), &mut h), b"a=1&b=2");
        assert_eq!(h.len(), 1, "the given type is kept");

        let mut h = Vec::new();
        assert!(body(&json!({}), &mut h).is_empty());
        assert!(h.is_empty());
    }

    #[test]
    fn the_report_shows_what_the_endpoint_said() {
        let response = AppHttpResponse {
            status: 201,
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("x-frame-options".to_owned(), "DENY".to_owned()),
                (
                    "set-cookie".to_owned(),
                    "sc_session=secret; Path=/".to_owned(),
                ),
                ("location".to_owned(), "/api/tasks/7".to_owned()),
            ],
            body: br#"{"id":7,"title":"x"}"#.to_vec(),
            truncated: None,
        };
        let text = report("POST", "/api/tasks", "alice@example.com", &response);
        assert!(
            text.starts_with("POST /api/tasks as alice@example.com → 201 Created\n"),
            "{text}"
        );
        assert!(text.contains("\nlocation: /api/tasks/7"), "{text}");
        assert!(text.contains("\nset-cookie: sc_session=…"), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert!(!text.contains("x-frame-options"), "{text}");
        assert!(text.contains("\"title\": \"x\""), "pretty JSON: {text}");

        let big = AppHttpResponse {
            status: 200,
            body: "x".repeat(MAX_BODY_CHARS + 5).into_bytes(),
            truncated: Some("over 1 MB".to_owned()),
            ..AppHttpResponse::default()
        };
        let text = report("GET", "/", "public", &big);
        assert!(text.contains("5 more characters not shown"), "{text}");
        assert!(
            text.ends_with("note: the body is incomplete: over 1 MB"),
            "{text}"
        );

        let binary = AppHttpResponse {
            status: 200,
            headers: vec![("content-type".to_owned(), "image/png".to_owned())],
            body: vec![0x89, 0xff, 0xfe],
            truncated: None,
        };
        assert!(report("GET", "/logo.png", "public", &binary).contains("[3 bytes of image/png"));
    }

    #[test]
    fn an_old_result_is_its_status_line() {
        let call = sc_llm::ToolCall {
            id: "1".to_owned(),
            name: "call_api_code_web".to_owned(),
            arguments: json!({"path": "/api/tasks"}),
        };
        let old = Elidable {
            index: 3,
            call: &call,
            content: "GET /api/tasks as public → 200 OK\ncontent-type: application/json",
            images: 0,
            transcript: &[],
            state: None,
        };
        assert_eq!(elide(&old), "[elided GET /api/tasks as public → 200 OK]");
    }
}

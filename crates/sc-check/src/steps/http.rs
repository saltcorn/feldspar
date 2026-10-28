//! The `http.*` step types (TODO C1.9): a request to the checklist's
//! application through the server's own router (`AppRequester`, as the coding
//! agent's `call_api` sends one), with a session for `run_as` made for the one
//! request and a valid CSRF token — so the answer is the one the application's
//! own page would get. How REST, GraphQL and exposed triggers are checked
//! without a browser.

use std::collections::BTreeMap;

use sc_agent::AppHttpRequest;
use sc_error::{Error, Result};
use sc_types::{BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::registry::{Needs, StepContext, StepOutcome, StepType};
use crate::steps::data::matches;

fn method_field() -> FormField {
    FormField::new("method", BasicType::Text)
        .label("Method")
        .options(["GET", "POST", "PUT", "PATCH", "DELETE"])
        .default_value("GET")
}

fn body_field() -> FormField {
    FormField::new("body", BasicType::Json)
        .label("Body")
        .sublabel("Sent as JSON")
}

/// Send the request a step describes and return the response as the run sees
/// it: `{status, headers, body}`, the body parsed when it is JSON.
async fn send(cx: &mut StepContext<'_>, params: &Json) -> Result<Json> {
    let app = cx.require_application()?;
    let requests = cx.requests()?;
    let path = params
        .get("path")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| Error::invalid("`path` is required, e.g. `/api/orders`"))?;
    if !path.starts_with('/') {
        return Err(Error::invalid(format!(
            "`path` is a path within the application and starts with `/`, not `{path}`"
        )));
    }
    let method = params
        .get("method")
        .and_then(Json::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let mut headers: Vec<(String, String)> = match params.get("headers") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Object(map)) => map
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    Json::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (k.clone(), value)
            })
            .collect(),
        Some(other) => {
            return Err(Error::invalid(format!(
                "`headers` should be an object, got {other}"
            )));
        }
    };
    let body = match params.get("body") {
        None | Some(Json::Null) => Vec::new(),
        Some(value) => {
            headers.push(("content-type".to_owned(), "application/json".to_owned()));
            serde_json::to_vec(value).map_err(|e| Error::msg(e.to_string()))?
        }
    };
    let response = requests
        .request(AppHttpRequest {
            subdomain: &app.subdomain,
            preview: cx.preview(),
            method: method.clone(),
            path: path.to_owned(),
            headers,
            body,
            user: cx.run_as,
            timeout: cx.timeout,
        })
        .await?;
    let text = String::from_utf8_lossy(&response.body).into_owned();
    let body = serde_json::from_str::<Json>(&text).unwrap_or(Json::String(text));
    let headers: BTreeMap<String, String> = response.headers.into_iter().collect();
    let value = json!({
        "method": method,
        "path": path,
        "status": response.status,
        "headers": headers,
        "body": body,
    });
    cx.set_last_response(value.clone());
    Ok(value)
}

/// The response a step asserts on: the one it sends when it has a `path`, or
/// the last one this run received.
async fn response(cx: &mut StepContext<'_>, params: &Json) -> Result<Json> {
    if params.get("path").and_then(Json::as_str).is_some_and(|p| !p.trim().is_empty()) {
        return send(cx, params).await;
    }
    cx.last_response().cloned().ok_or_else(|| {
        Error::invalid(
            "there is no response to check: give this step a `path`, or put an http.request \
             before it",
        )
    })
}

fn describe(response: &Json) -> String {
    format!(
        "{} {}",
        response["method"].as_str().unwrap_or(""),
        response["path"].as_str().unwrap_or("")
    )
}

/// `http.request`: send a request and capture the response.
pub struct Request;

#[async_trait::async_trait]
impl StepType for Request {
    fn name(&self) -> &str {
        "http.request"
    }

    fn description(&self) -> &str {
        "Send a request to the checklist's application as its user (signed in, with a valid \
         CSRF token). Its value is the response: {status, headers, body}, the body parsed when \
         it is JSON. Later http.expect_* steps check it."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            method_field(),
            FormField::new("path", BasicType::Text)
                .label("Path")
                .required()
                .sublabel("Within the application, e.g. /api/orders/{{ order.id }}"),
            body_field(),
            FormField::new("headers", BasicType::Json).label("Headers"),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::HTTP
    }

    fn mutates(&self) -> bool {
        // A request may well write: a POST to the REST API inserts a row.
        true
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        match send(cx, params).await {
            Ok(response) => StepOutcome::pass(format!(
                "{} answered {}",
                describe(&response),
                response["status"]
            ))
            .with_value(response),
            Err(e) => e.into(),
        }
    }
}

/// `http.expect_status`: the status of a request.
pub struct ExpectStatus;

#[async_trait::async_trait]
impl StepType for ExpectStatus {
    fn name(&self) -> &str {
        "http.expect_status"
    }

    fn description(&self) -> &str {
        "Assert a response's status code: of the request this step sends when it has a `path`, \
         or else of the last http.request. E.g. a visitor's POST answers 403."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("status", BasicType::Int)
                .label("Status")
                .required()
                .default_value(200),
            method_field(),
            FormField::new("path", BasicType::Text)
                .label("Path")
                .sublabel("Leave empty to check the last http.request"),
            body_field(),
            FormField::new("headers", BasicType::Json).label("Headers"),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::HTTP
    }

    fn mutates(&self) -> bool {
        true
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let response = match response(cx, params).await {
            Ok(response) => response,
            Err(e) => return e.into(),
        };
        let expected = params.get("status").and_then(Json::as_u64).unwrap_or(200);
        let actual = response["status"].as_u64().unwrap_or(0);
        if actual == expected {
            StepOutcome::pass(format!("{} answered {actual}", describe(&response)))
                .with_value(response)
        } else {
            let body = match &response["body"] {
                Json::String(s) => s.chars().take(300).collect::<String>(),
                other => other.to_string().chars().take(300).collect(),
            };
            StepOutcome::fail(format!(
                "{} answered {actual}, expected {expected}: {body}",
                describe(&response)
            ))
            .with_value(response)
        }
    }
}

/// `http.expect_json`: a value in the last response's JSON body.
pub struct ExpectJson;

#[async_trait::async_trait]
impl StepType for ExpectJson {
    fn name(&self) -> &str {
        "http.expect_json"
    }

    fn description(&self) -> &str {
        "Assert a value in the last http.request's JSON body, at a path like `items[0].name` \
         (`$` or empty is the whole body): it `equals` the value, `contains` it (a substring, \
         a list element or an object's fields), `matches` a regular expression, or merely \
         `exists`."
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new("json_path", BasicType::Text)
                .label("Path in the body")
                .sublabel("e.g. items[0].name, or $ for the whole body"),
            FormField::new("op", BasicType::Text)
                .label("Check")
                .required()
                .options(["equals", "contains", "matches", "exists"])
                .default_value("equals"),
            FormField::new("value", BasicType::Json).label("Value"),
        ]
    }

    fn needs(&self) -> Needs {
        Needs::HTTP
    }

    fn mutates(&self) -> bool {
        false
    }

    async fn run(&self, cx: &mut StepContext<'_>, params: &Json) -> StepOutcome {
        let Some(response) = cx.last_response().cloned() else {
            return StepOutcome::error(
                "there is no response to check: put an http.request before this step",
            );
        };
        let path = params.get("json_path").and_then(Json::as_str).unwrap_or("$");
        let op = params.get("op").and_then(Json::as_str).unwrap_or("equals");
        let expected = params.get("value").cloned().unwrap_or(Json::Null);
        let found = match lookup(&response["body"], path) {
            Ok(found) => found,
            Err(e) => return e.into(),
        };
        let what = format!("`{path}` of {}", describe(&response));
        let Some(actual) = found else {
            return StepOutcome::fail(format!("{what} has no such value"));
        };
        let ok = match op {
            "exists" => true,
            "equals" => matches(&expected, actual) && matches(actual, &expected),
            "contains" => contains(actual, &expected),
            "matches" => {
                let Some(pattern) = expected.as_str() else {
                    return StepOutcome::error("`matches` takes a regular expression as `value`");
                };
                let re = match regex_lite::Regex::new(pattern) {
                    Ok(re) => re,
                    Err(e) => {
                        return StepOutcome::error(format!(
                            "`{pattern}` is not a regular expression: {e}"
                        ));
                    }
                };
                let text = match actual {
                    Json::String(s) => s.clone(),
                    other => other.to_string(),
                };
                re.is_match(&text)
            }
            other => {
                return StepOutcome::error(format!(
                    "`op` should be equals, contains, matches or exists, not `{other}`"
                ));
            }
        };
        if ok {
            StepOutcome::pass(format!("{what} {op} {expected}")).with_value(actual.clone())
        } else {
            StepOutcome::fail(format!("{what} is {actual}, which does not {op} {expected}"))
                .with_value(actual.clone())
        }
    }
}

/// Whether `actual` contains `expected`: a substring, an element, or an
/// object's fields.
fn contains(actual: &Json, expected: &Json) -> bool {
    match (actual, expected) {
        (Json::String(s), Json::String(e)) => s.contains(e.as_str()),
        (Json::Array(items), e) => items.iter().any(|item| matches(e, item)),
        (Json::Object(_), Json::Object(_)) => matches(expected, actual),
        _ => false,
    }
}

/// The value at `path` in `body`: `a.b[0].c`, with `$` or an empty path for
/// the whole body. `Ok(None)` when the body has no such value; an error when
/// the path does not parse.
fn lookup<'a>(body: &'a Json, path: &str) -> Result<Option<&'a Json>> {
    let path = path.trim();
    let path = path.strip_prefix('$').unwrap_or(path);
    let mut segments = Vec::new();
    for part in path.split('.').filter(|p| !p.is_empty()) {
        let (name, mut rest) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        if !name.is_empty() {
            segments.push(name.to_owned());
        }
        while let Some(stripped) = rest.strip_prefix('[') {
            let end = stripped
                .find(']')
                .ok_or_else(|| Error::invalid(format!("`{part}`: an unclosed `[`")))?;
            segments.push(stripped[..end].trim_matches(['"', '\'']).to_owned());
            rest = &stripped[end + 1..];
        }
        if !rest.is_empty() {
            return Err(Error::invalid(format!("`{part}` is not a path segment")));
        }
    }
    let mut value = body;
    for segment in &segments {
        let next = match value {
            Json::Object(map) => map.get(segment),
            Json::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get(i)),
            _ => None,
        };
        match next {
            Some(next) => value = next,
            None => return Ok(None),
        }
    }
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_path_reaches_into_objects_and_lists() {
        let body = json!({ "items": [{ "name": "a" }, { "name": "b" }], "n": 2 });
        assert_eq!(lookup(&body, "$").unwrap(), Some(&body));
        assert_eq!(lookup(&body, "").unwrap(), Some(&body));
        assert_eq!(lookup(&body, "n").unwrap(), Some(&json!(2)));
        assert_eq!(lookup(&body, "$.items[1].name").unwrap(), Some(&json!("b")));
        assert_eq!(lookup(&body, "items.0.name").unwrap(), Some(&json!("a")));
        assert_eq!(lookup(&body, "items[5]").unwrap(), None);
        assert!(lookup(&body, "items[0").is_err());
        assert!(contains(&json!(["x", "y"]), &json!("y")));
        assert!(contains(&json!("hello world"), &json!("lo w")));
    }
}

//! Running JavaScript the **model** wrote (TODO-agent-skills AS5), for
//! `generate_js` (AS9.4).
//!
//! `run_js_code` runs a body an admin wrote, with the admin's reach: every
//! table at the trigger's authority, the network the server can reach, the
//! file stores, the other triggers. Code a model wrote — perhaps after reading
//! a page that told it what to write — gets a much narrower host, built here
//! for one evaluation:
//!
//! - **Only what is granted is in scope** ([`Grants`]). `db` exists only with a
//!   table grant, `fetch` only with a network grant, and `lib()` imports only
//!   with the libraries grant. `fs`, `trigger`, `modfn`, `live` and v1's
//!   `Table` are never bound, so naming one is a `ReferenceError` the model can
//!   read and correct.
//! - **Tables as the caller.** `db` is `viewer_only`: whatever the code asks,
//!   it reads and writes as the person the agent is talking to, so §7.3's
//!   ownership and row-level security decide every row. A read-only grant
//!   refuses `insert`, `update` and `delete` by name, an allow-list refuses any
//!   other table, and `db.sql` is refused outright (raw SQL bypasses the row
//!   layer, which is the one thing that knows the caller).
//! - **The network under the `http` trait's policy**: an optional host
//!   allow-list, and public addresses only unless the grant opens the private
//!   network, checked on every redirect ([`HttpClient`]).
//! - **Stricter limits** ([`SandboxLimits`]): a shorter wall clock, fewer
//!   database calls and rows per read, fewer and smaller responses, and a cap
//!   on what the code may return. The isolate's heap is shared with every other
//!   code body (§3: the same pool), so the heap a call may fill is bounded by
//!   what can reach it — rows, responses — rather than by a per-call heap
//!   limit V8 cannot give one context of a shared isolate.
//! - **Every evaluation is ledgered** with its source (AS5.3), whether it
//!   returned or threw, through [`TraitContext::record_code`].

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Url};
use sc_agent::{CodeEvaluation, TraitContext};
use sc_api::code_host::{HostLimits, Op, Plan, TableHost};
use sc_error::{Error, Result};
use sc_expr::{CodeCall, CodeHost, FetchHost};
use serde_json::{Value as Json, json};

use crate::http::client::{HttpClient, Request};
use crate::http::guard::HostPolicy;

/// How long one evaluation may take when nothing says otherwise.
pub const DEFAULT_SANDBOX_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest one evaluation may be given.
pub const MAX_SANDBOX_TIMEOUT: Duration = Duration::from_secs(30);

/// What the code may reach.
#[derive(Debug, Clone, Default)]
pub struct Grants {
    /// The network, under this policy; `None` leaves `fetch` out of scope.
    pub fetch: Option<HostPolicy>,
    /// The tables.
    pub tables: TableAccess,
    /// The tables `db` may name; empty is every table the caller can reach.
    pub only_tables: Vec<String>,
    /// Whether `lib()` imports the instance's code libraries.
    pub libraries: bool,
}

/// How far `db` reaches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TableAccess {
    /// `db` is not in scope.
    #[default]
    None,
    /// Reads only.
    ReadOnly,
    /// Reads and writes.
    ReadWrite,
}

impl TableAccess {
    /// The access a configuration names: `none`, `read` or `write`.
    pub fn parse(name: &str) -> Result<TableAccess> {
        match name.trim() {
            "" | "none" => Ok(TableAccess::None),
            "read" | "read_only" => Ok(TableAccess::ReadOnly),
            "write" | "read_write" => Ok(TableAccess::ReadWrite),
            other => Err(Error::invalid(format!(
                "table access `{other}` is not one of none, read, write"
            ))),
        }
    }
}

/// The bounds of one evaluation, stricter than a code body's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxLimits {
    /// The wall clock, at most [`MAX_SANDBOX_TIMEOUT`].
    pub timeout: Duration,
    /// Database calls.
    pub max_calls: u32,
    /// Rows per read.
    pub max_rows: u64,
    /// Requests.
    pub max_fetches: u32,
    /// Bytes of one response.
    pub max_response_bytes: usize,
    /// Bytes of JSON the code may return.
    pub max_output_bytes: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        SandboxLimits {
            timeout: DEFAULT_SANDBOX_TIMEOUT,
            max_calls: 50,
            max_rows: 200,
            max_fetches: 10,
            max_response_bytes: 1024 * 1024,
            max_output_bytes: 64 * 1024,
        }
    }
}

/// Evaluate `source` — written by the model, for tool `tool` — with `bindings`
/// in scope and only what `grants` gives, as the run's caller. The result is
/// what the code returned; the evaluation is ledgered either way.
pub async fn run_model_code(
    ctx: &mut TraitContext<'_>,
    tool: &str,
    source: &str,
    bindings: BTreeMap<String, Json>,
    grants: &Grants,
    limits: &SandboxLimits,
) -> Result<Json> {
    let started = Instant::now();
    let outcome = evaluate(ctx, source, bindings, grants, limits).await;
    let outcome = outcome.and_then(|value| {
        let bytes = value.to_string().len();
        if bytes > limits.max_output_bytes {
            return Err(Error::invalid(format!(
                "the code returned {bytes} bytes of JSON; it may return at most {}, so return \
                 a summary or fewer rows",
                limits.max_output_bytes
            )));
        }
        Ok((value, bytes))
    });
    ctx.record_code(CodeEvaluation {
        step: 0,
        tool: tool.to_owned(),
        source: source.to_owned(),
        ok: outcome.is_ok(),
        error: outcome.as_ref().err().map(|e| sc_error::format_causes(e)),
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        output_bytes: outcome.as_ref().map_or(0, |(_, bytes)| *bytes as u64),
    });
    outcome.map(|(value, _)| value)
}

async fn evaluate(
    ctx: &TraitContext<'_>,
    source: &str,
    bindings: BTreeMap<String, Json>,
    grants: &Grants,
    limits: &SandboxLimits,
) -> Result<Json> {
    let evaluator = ctx.require_evaluator()?;
    let caller = sc_api::caller_context_at(ctx.caller.role, ctx.caller.user.as_ref());
    let table = (grants.tables != TableAccess::None).then(|| SandboxTables {
        inner: TableHost::new(ctx.catalog)
            .caused_by(caller.role, caller.user.clone())
            .chained(ctx.caller.tool_chain())
            // The caller's authority is the ceiling: a plan that names none,
            // or names the admin, runs as the caller.
            .viewer_only()
            .with_limits(HostLimits {
                max_rows: limits.max_rows,
                max_calls: limits.max_calls,
            })
            .with_evaluator(ctx.evaluator.cloned()),
        access: grants.tables,
        only: grants.only_tables.clone(),
    });
    let fetch = match &grants.fetch {
        Some(policy) => Some(SandboxFetch {
            client: HttpClient::new()?,
            policy: policy.clone(),
            max_bytes: limits.max_response_bytes,
        }),
        None => None,
    };
    let call = CodeCall {
        code: source.to_owned(),
        bindings,
        host: table.as_ref().map(|t| t as &dyn CodeHost),
        fetch: fetch.as_ref().map(|f| f as &dyn FetchHost),
        timeout: Some(limits.timeout.min(MAX_SANDBOX_TIMEOUT)),
        max_calls: limits.max_calls,
        max_fetches: if grants.fetch.is_some() {
            limits.max_fetches
        } else {
            0
        },
        max_file_ops: 0,
        max_trigger_runs: 0,
        max_module_calls: 0,
        libraries: if grants.libraries {
            ctx.catalog.code_libraries_for(None)
        } else {
            None
        },
        ..CodeCall::default()
    };
    evaluator.run_code(call).await
}

/// `db` for model-written code: the caller's tables, as far as the grant goes.
struct SandboxTables<'a> {
    inner: TableHost<'a>,
    access: TableAccess,
    only: Vec<String>,
}

#[async_trait::async_trait]
impl CodeHost for SandboxTables<'_> {
    async fn call(&self, request: Json) -> Result<Json> {
        // Only plans over tables: raw SQL bypasses the row layer, and model
        // handles and row history are not part of the grant.
        let op = request.get("op").and_then(Json::as_str).unwrap_or_default();
        if matches!(op, "sql" | "models" | "history") {
            return Err(Error::auth(format!(
                "`db.{op}` is not available to this code; read and write tables through \
                 db.<table>"
            )));
        }
        let plan: Plan = serde_json::from_value(request.clone()).map_err(|e| {
            Error::invalid(format!(
                "this database request is not one the server understands: {e}"
            ))
        })?;
        if !self.only.is_empty() && !self.only.iter().any(|t| t == &plan.table) {
            return Err(Error::auth(format!(
                "table `{}` is not one this code may use; it may use {}",
                plan.table,
                self.only.join(", ")
            )));
        }
        if self.access == TableAccess::ReadOnly
            && matches!(plan.op, Op::Insert | Op::Update | Op::Delete)
        {
            return Err(Error::auth(format!(
                "table `{}` is read-only to this code, so it cannot be written",
                plan.table
            )));
        }
        self.inner.call(request).await
    }
}

/// `fetch` for model-written code, under the `http` trait's host policy.
struct SandboxFetch {
    client: HttpClient,
    policy: HostPolicy,
    max_bytes: usize,
}

#[async_trait::async_trait]
impl FetchHost for SandboxFetch {
    async fn fetch(&self, request: Json) -> Result<Json> {
        let raw = request
            .get("url")
            .and_then(Json::as_str)
            .unwrap_or("")
            .trim();
        let url = Url::parse(raw)
            .map_err(|e| Error::invalid(format!("fetch(): `{raw}` is not a valid URL: {e}")))?;
        let method = request
            .get("method")
            .and_then(Json::as_str)
            .unwrap_or("GET")
            .to_uppercase();
        if !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"].contains(&method.as_str()) {
            return Err(Error::invalid(format!(
                "fetch(): `{method}` is not a method this code may send"
            )));
        }
        let mut headers = HeaderMap::new();
        if let Some(Json::Array(pairs)) = request.get("headers") {
            for pair in pairs {
                let (Some(name), Some(value)) = (
                    pair.get(0).and_then(Json::as_str),
                    pair.get(1).and_then(Json::as_str),
                ) else {
                    return Err(Error::invalid(
                        "fetch(): a header is a [name, value] pair of strings",
                    ));
                };
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    Error::invalid(format!("fetch(): `{name}` is not a valid header name"))
                })?;
                let value = HeaderValue::from_str(value).map_err(|_| {
                    Error::invalid(format!(
                        "fetch(): the value of `{name}` is not a valid header value"
                    ))
                })?;
                headers.append(name, value);
            }
        }
        if request.get("body_base64") == Some(&Json::Bool(true)) {
            return Err(Error::invalid(
                "fetch(): this code may send a text or JSON body, not bytes",
            ));
        }
        let body = match request.get("body") {
            Some(Json::String(text)) => Some(Json::String(text.clone())),
            _ => None,
        };
        let timeout = request
            .get("timeout_ms")
            .and_then(Json::as_u64)
            .filter(|ms| *ms > 0)
            .map_or(sc_expr::DEFAULT_FETCH_TIMEOUT, Duration::from_millis);
        let response = self
            .client
            .send(
                &self.policy,
                Request {
                    method: Method::from_bytes(method.as_bytes())
                        .map_err(|e| Error::invalid(format!("fetch(): {e}")))?,
                    url: url.clone(),
                    headers,
                    body,
                    timeout,
                },
            )
            .await?;
        if response.truncated || response.body.len() > self.max_bytes {
            return Err(Error::invalid(format!(
                "the response from {} is larger than the {} KB this code may read",
                response.url,
                self.max_bytes / 1024
            )));
        }
        let mut answer = json!({
            "status": response.status,
            "status_text": reqwest::StatusCode::from_u16(response.status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or(""),
            "url": response.url.as_str(),
            "redirected": !response.redirects.is_empty(),
            "headers": response
                .headers
                .iter()
                .map(|(name, value)| json!([name, value]))
                .collect::<Vec<_>>(),
        });
        match String::from_utf8(response.body) {
            Ok(text) => answer["text"] = Json::String(text),
            Err(e) => {
                let bytes = e.into_bytes();
                answer["text"] = Json::String(String::from_utf8_lossy(&bytes).into_owned());
                answer["base64"] =
                    Json::String(base64::engine::general_purpose::STANDARD.encode(&bytes));
            }
        }
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_access_is_named_in_a_configuration() {
        assert_eq!(TableAccess::parse("").unwrap(), TableAccess::None);
        assert_eq!(TableAccess::parse("read").unwrap(), TableAccess::ReadOnly);
        assert_eq!(TableAccess::parse("write").unwrap(), TableAccess::ReadWrite);
        assert!(TableAccess::parse("admin").is_err());
    }

    #[test]
    fn the_limits_are_stricter_than_a_code_bodys() {
        let limits = SandboxLimits::default();
        assert!(limits.timeout < sc_expr::MAX_CODE_TIMEOUT);
        assert!(limits.max_calls < sc_expr::DEFAULT_MAX_HOST_CALLS);
        assert!(limits.max_fetches < sc_expr::DEFAULT_MAX_FETCHES);
        assert!(limits.max_rows < sc_api::code_host::DEFAULT_MAX_ROWS);
    }
}

//! **Module webhooks**: requests from another service that the server routes to
//! a module (TODO-payments P1.1, design C1).
//!
//! A payment provider tells the module that a payment succeeded by posting to a
//! URL the admin registered with it. The module declares what it receives:
//!
//! ```js
//! webhooks: {
//!   events: {
//!     description: "Payment events from the provider",
//!     handle: async (req, ctx) => ({ status: 200, body: { ok: true } }),
//!   },
//! }
//! ```
//!
//! and the server serves it at `POST /_feldspar/hooks/<module>/<name>[/<rest>]`
//! on the admin host — one stable URL to register, as the OAuth callback is.
//! `<module>` is the package name without its npm scope ([`hook_segment`]):
//! `@feldspar/payments` receives at `/_feldspar/hooks/payments/…`.
//!
//! ## What a request is
//!
//! `req.rawBody` is the **bytes as sent**, because a provider signs those bytes
//! and a body parsed and re-serialised on the way would no longer verify.
//! `req.headers` are lower-cased; `req.rest` is the path after the webhook's
//! name. There is no session and no CSRF check: the caller is a provider's
//! server, and a webhook authenticates by verifying the signature itself.
//!
//! ## The module's authority, over the module's tables
//!
//! Nobody signed in, so a webhook runs as **the module** — the admin role, with
//! `{ "module": <name>, "webhook": <name> }` as its writes' user — over **only
//! the tables its module declared**, and reads of the ones its settings name
//! ([`crate::reads`]): the same [`ScopedTables`] a service's `ctx.db` is. A webhook is a service's moment rather than a service's life,
//! so its call is bounded like an action's: the wall clock and the budget of
//! asks.

use base64::Engine;
use sc_action::TriggerDispatcher;
use sc_api::code_host::{TableHost, TriggerRunHost, schema_snapshot};
use sc_auth::ROLE_ADMIN;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_expr::{CodeHosts, TriggerHost};
use serde_json::{Map, Value as Json, json};

use crate::files::{ModuleFiles, file_areas};
use crate::host::{CallHosts, ModuleHost};
use crate::module::ModuleLanguage;
use crate::modules::{LoadedModule, ModuleSet};
use crate::services::ScopedTables;

/// Where every module webhook is served, before `<module>/<name>`.
pub const HOOKS_PREFIX: &str = "/_feldspar/hooks";

/// The largest body a webhook is handed (1 MiB). A provider's event is a few
/// kilobytes; this is the cap on what an unauthenticated caller can make the
/// server hold before the module has verified anything.
pub const WEBHOOK_MAX_BODY: usize = 1024 * 1024;

/// One request to a module's webhook, as it arrived.
#[derive(Debug, Clone, Default)]
pub struct WebhookRequest {
    /// The HTTP method — `POST`.
    pub method: String,
    /// The headers, lower-cased, without `cookie` (a webhook has no session,
    /// and a module has no business reading a browser's).
    pub headers: Vec<(String, String)>,
    /// The query string's parameters, in order.
    pub query: Vec<(String, String)>,
    /// The path after the webhook's name, without its leading slash: `""` for
    /// `/_feldspar/hooks/payments/events`, `"acct_1"` for `…/events/acct_1`.
    pub rest: String,
    /// The body, byte for byte.
    pub body: Vec<u8>,
}

/// What a webhook answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookResponse {
    /// The status the provider sees: a 2xx stops its retries.
    pub status: u16,
    /// The headers the handler set, lower-cased, with a `content-type` for a
    /// body it did not type itself.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// The URL segment `module` is addressed by: the package name without its npm
/// scope — `@feldspar/payments` is `payments`.
pub fn hook_segment(module: &str) -> &str {
    match module.strip_prefix('@') {
        Some(scoped) => scoped.split_once('/').map_or(module, |(_, name)| name),
        None => module,
    }
}

/// The path a module's webhook is served at, for the module card.
pub fn hook_path(module: &str, webhook: &str) -> String {
    format!("{HOOKS_PREFIX}/{}/{webhook}", hook_segment(module))
}

/// The loaded JavaScript module `segment` addresses, by [`hook_segment`].
///
/// `Ok(None)` when no module is addressed by it. Two modules whose names differ
/// only by scope (`@a/pay` and `@b/pay`) would share a segment, and that is an
/// error naming both rather than a coin toss over which one a provider's events
/// reach.
pub fn webhook_module<'a>(set: &'a ModuleSet, segment: &str) -> Result<Option<&'a LoadedModule>> {
    let matching: Vec<&LoadedModule> = set
        .modules()
        .iter()
        .filter(|m| m.module.language == ModuleLanguage::JavaScript)
        .filter(|m| {
            m.manifest
                .as_ref()
                .is_some_and(|mf| !mf.webhooks.is_empty())
        })
        .filter(|m| hook_segment(&m.module.name) == segment)
        .collect();
    match matching.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one)),
        many => Err(Error::config(format!(
            "`{segment}` names the webhooks of {} modules ({}), so no request to it can be \
             routed: uninstall one of them",
            many.len(),
            many.iter()
                .map(|m| format!("`{}`", m.module.name))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The user a webhook's writes carry: the module and the webhook, by name.
fn webhook_user(module: &str, webhook: &str) -> Json {
    json!({ "module": module, "webhook": webhook })
}

/// Run `webhook` of `loaded` for `request`, over the module's own tables.
///
/// An error is a failure to run it — the module is not loaded, the handler
/// threw, a refused table — and the caller answers `500` for it, so the
/// provider retries. A handler that *answers* a 4xx or 5xx is not an error.
pub async fn run_webhook(
    catalog: &Catalog,
    host: &ModuleHost,
    dispatcher: Option<&TriggerDispatcher>,
    loaded: &LoadedModule,
    webhook: &str,
    request: WebhookRequest,
) -> Result<WebhookResponse> {
    let module = loaded.module.name.as_str();
    let declared = loaded
        .manifest
        .as_ref()
        .is_some_and(|m| m.webhooks.iter().any(|w| w.name == webhook));
    if !declared {
        return Err(Error::not_found(format!(
            "module `{module}` has no webhook `{webhook}`"
        )));
    }
    let tables = crate::services::TableScope::of(loaded);
    let areas = file_areas(&loaded.table_declarations());
    let user = webhook_user(module, webhook);
    let scoped = ScopedTables {
        inner: TableHost::new(catalog).caused_by(ROLE_ADMIN, Some(user.clone())),
        catalog,
        module,
        tables: &tables,
        kind: "webhook",
    };
    let triggers =
        dispatcher.map(|d| TriggerRunHost::new(d, catalog).caused_by(ROLE_ADMIN, Some(user)));
    let files = ModuleFiles::new(catalog, module, &areas);
    let roles = crate::roles::ModuleRoles::new(catalog, module, loaded.module.permissions.roles);
    let schema = schema_snapshot(catalog).ok();
    let call = CallHosts {
        hosts: CodeHosts {
            host: Some(&scoped),
            triggers: triggers.as_ref().map(|t| t as &dyn TriggerHost),
            ..CodeHosts::default()
        },
        schema: schema.as_deref(),
        views: None,
        service: None,
        module_files: Some(&files),
        roles: Some(&roles),
    };
    let answer = host
        .run_webhook(module, webhook, request_json(&request), call)
        .await?;
    response_from(&answer)
}

/// The request as the host script takes it: the body base64, the headers an
/// object (a repeated header joined with `, `, as HTTP allows).
fn request_json(request: &WebhookRequest) -> Json {
    let mut headers = Map::new();
    for (name, value) in &request.headers {
        let name = name.to_ascii_lowercase();
        match headers.get_mut(&name) {
            Some(Json::String(existing)) => {
                existing.push_str(", ");
                existing.push_str(value);
            }
            _ => {
                headers.insert(name, Json::String(value.clone()));
            }
        }
    }
    let mut query = Map::new();
    for (name, value) in &request.query {
        query
            .entry(name.clone())
            .or_insert_with(|| Json::String(value.clone()));
    }
    json!({
        "method": request.method,
        "headers": headers,
        "query": query,
        "rest": request.rest,
        "body": base64::engine::general_purpose::STANDARD.encode(&request.body),
    })
}

/// The host script's `{ status, headers, body }` as a response.
fn response_from(answer: &Json) -> Result<WebhookResponse> {
    let status = answer
        .get("status")
        .and_then(Json::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .filter(|s| (100..=599).contains(s))
        .ok_or_else(|| Error::invalid(format!("a webhook answered no status: {answer}")))?;
    let headers = answer
        .get("headers")
        .and_then(Json::as_object)
        .map(|h| {
            h.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let body = match answer.get("body").and_then(Json::as_str) {
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| Error::invalid(format!("a webhook's answer is not base64: {e}")))?,
        None => Vec::new(),
    };
    Ok(WebhookResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_module_is_addressed_by_its_name_without_its_scope() {
        assert_eq!(hook_segment("@feldspar/payments"), "payments");
        assert_eq!(hook_segment("payments"), "payments");
        assert_eq!(hook_segment("@odd"), "@odd");
        assert_eq!(
            hook_path("@feldspar/payments", "events"),
            "/_feldspar/hooks/payments/events"
        );
    }

    #[test]
    fn repeated_headers_are_joined_and_the_body_crosses_as_base64() {
        let request = WebhookRequest {
            method: "POST".into(),
            headers: vec![("X-Sig".into(), "a".into()), ("x-sig".into(), "b".into())],
            query: vec![("q".into(), "1".into())],
            rest: "acct".into(),
            body: vec![0, 255, 10],
        };
        let json = request_json(&request);
        assert_eq!(json["headers"]["x-sig"], json!("a, b"));
        assert_eq!(json["body"], json!("AP8K"));
        assert_eq!(json["query"]["q"], json!("1"));
    }

    #[test]
    fn an_answer_becomes_a_response() {
        let response = response_from(&json!({
            "status": 202,
            "headers": { "content-type": "text/plain" },
            "body": "cG9uZw==",
        }))
        .unwrap();
        assert_eq!(response.status, 202);
        assert_eq!(response.body, b"pong");
        assert!(response_from(&json!({ "status": 42 })).is_err());
    }
}

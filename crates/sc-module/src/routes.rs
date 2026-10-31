//! v1's **routes**: URLs a plugin serves itself.
//!
//! A v1 plugin exports
//!
//! ```js
//! routes: (configuration) => [
//!   { method: "get", url: "/heartbeat", callback: async (req, res) => res.json({ ok: true }) },
//! ]
//! ```
//!
//! and v1 mounts each at its `url` on the tenant's site. Here a module is
//! installed once for every application, and a URL at the root of one
//! application's host would claim it in all of them, so a module's routes are
//! served on the **admin host** only, under [`ROUTES_PREFIX`] and the module's
//! segment ([`hook_segment`], as its webhooks are):
//! `/_feldspar/routes/heartbeat-monitor/heartbeat`.
//!
//! ## How a route differs from a webhook
//!
//! A [webhook](crate::webhooks) is called by another company's server: it gets
//! the raw bytes, no session, no credentials, and runs as the module over its
//! own tables. A route is called by somebody — a browser or a monitor holding
//! an API token — and runs **as them**: `req.user` is the signed-in user (or
//! the token's), and the module's `Table` reads and writes under their
//! authority as a view's do ([`TableHost::viewer_only`]). A `POST` is checked
//! for CSRF unless the route says `noCsrf`; `apiToken` means a token is
//! required, as v1's middleware requires one.
//!
//! The server matches the URL ([`match_route`]) — literal segments and
//! `:param`s — before it calls the module, because whether a token is needed
//! and whether CSRF applies are properties of the route a request reaches.

use sc_action::TriggerDispatcher;
use sc_api::code_host::{TableHost, TriggerRunHost, schema_snapshot};
use sc_catalog::{Catalog, Principal};
use sc_error::{Error, Result};
use sc_expr::{CodeHosts, TriggerHost};
use serde_json::{Map, Value as Json, json};

use crate::host::{CallHosts, ModuleHost, RouteManifest, SystemAsks};
use crate::module::ModuleLanguage;
use crate::modules::{LoadedModule, ModuleSet};
use crate::webhooks::{WebhookResponse, hook_segment, request_json, response_from};

/// Where every module route is served, before `<module>/<url>`.
pub const ROUTES_PREFIX: &str = "/_feldspar/routes";

/// The path a module's route is served at, for the module card.
pub fn route_path(module: &str, url: &str) -> String {
    format!("{ROUTES_PREFIX}/{}{url}", hook_segment(module))
}

/// The loaded JavaScript module with routes that `segment` addresses.
///
/// `Ok(None)` when none does; two modules that share a segment are an error
/// naming both, as for webhooks.
pub fn route_module<'a>(set: &'a ModuleSet, segment: &str) -> Result<Option<&'a LoadedModule>> {
    let matching: Vec<&LoadedModule> = set
        .modules()
        .iter()
        .filter(|m| m.module.language == ModuleLanguage::JavaScript)
        .filter(|m| m.manifest.as_ref().is_some_and(|mf| !mf.routes.is_empty()))
        .filter(|m| hook_segment(&m.module.name) == segment)
        .collect();
    match matching.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one)),
        many => Err(Error::config(format!(
            "`{segment}` names the routes of {} modules ({}), so no request to it can be \
             routed: uninstall one of them",
            many.len(),
            many.iter()
                .map(|m| format!("`{}`", m.module.name))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// A route a request reached, and the `:param`s it bound.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchedRoute<'a> {
    pub route: &'a RouteManifest,
    pub params: Map<String, Json>,
}

/// The `:param`s `pattern` binds in `path`, or `None` when it does not match.
fn bind(pattern: &str, path: &str) -> Option<Map<String, Json>> {
    let pattern: Vec<&str> = pattern.trim_end_matches('/').split('/').collect();
    let path: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    if pattern.len() != path.len() {
        return None;
    }
    let mut params = Map::new();
    for (want, got) in pattern.iter().zip(&path) {
        match want.strip_prefix(':') {
            Some(name) if !got.is_empty() => {
                params.insert(name.to_owned(), Json::String(percent_decode(got)));
            }
            Some(_) => return None,
            None if want == got => {}
            None => return None,
        }
    }
    Some(params)
}

/// A path segment with its `%XX` escapes decoded, as Express decodes a param.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = segment
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The route of `routes` that `method` and `path` (the path after the module's
/// segment, with its leading `/`) reach — the first declared, as Express takes
/// the first.
pub fn match_route<'a>(
    routes: &'a [RouteManifest],
    method: &str,
    path: &str,
) -> Option<MatchedRoute<'a>> {
    routes
        .iter()
        .filter(|r| r.method.eq_ignore_ascii_case(method))
        .find_map(|route| bind(&route.url, path).map(|params| MatchedRoute { route, params }))
}

/// The methods some route of `routes` answers at `path`, for a `405`'s `Allow`.
pub fn allowed_methods(routes: &[RouteManifest], path: &str) -> Vec<String> {
    let mut methods: Vec<String> = routes
        .iter()
        .filter(|r| bind(&r.url, path).is_some())
        .map(|r| r.method.to_ascii_uppercase())
        .collect();
    methods.dedup();
    methods
}

/// One request to a route, as it arrived — the [`crate::WebhookRequest`] shape
/// (`rest` is the path after the module's segment), plus where it came from.
#[derive(Debug, Clone, Default)]
pub struct RouteRequest {
    pub request: crate::WebhookRequest,
    /// The client's address, as the server determined it.
    pub ip: Option<String>,
    /// `http` or `https`.
    pub protocol: String,
}

/// `req.user` as v1 has it: the user's columns, with the role as `role_id`.
fn v1_user(caller: &Principal) -> Json {
    let Some(Json::Object(fields)) = &caller.user else {
        return Json::Null;
    };
    let mut user = fields.clone();
    user.insert("role_id".to_owned(), json!(caller.role));
    Json::Object(user)
}

/// Run the route `matched` of `loaded` for `request`, as `caller`.
///
/// `system` answers the module's `systeminformation`, `EventLog` and
/// `MetaData`; `installation` is `{ config: { … } }`, the installation-wide
/// settings `getState().getConfig` answers by v1's names. An error is a failure
/// to run the route — the caller answers `500` for it.
#[allow(clippy::too_many_arguments)]
pub async fn run_route(
    catalog: &Catalog,
    host: &ModuleHost,
    dispatcher: Option<&TriggerDispatcher>,
    loaded: &LoadedModule,
    matched: &MatchedRoute<'_>,
    request: RouteRequest,
    caller: &Principal,
    system: &dyn SystemAsks,
    installation: Json,
) -> Result<WebhookResponse> {
    let module = loaded.module.name.as_str();
    let table = TableHost::new(catalog)
        .caused_by(caller.role, caller.user.clone())
        .viewer_only();
    let triggers = dispatcher
        .map(|d| TriggerRunHost::new(d, catalog).caused_by(caller.role, caller.user.clone()));
    let schema = schema_snapshot(catalog).ok();
    let call = CallHosts {
        hosts: CodeHosts {
            host: Some(&table),
            triggers: triggers.as_ref().map(|t| t as &dyn TriggerHost),
            ..CodeHosts::default()
        },
        schema: schema.as_deref(),
        system: Some(system),
        ..CallHosts::default()
    };
    let mut arrived = request_json(&request.request);
    if let Json::Object(map) = &mut arrived {
        let rest = map.remove("rest").unwrap_or(Json::Null);
        let path = format!("/{}", rest.as_str().unwrap_or_default());
        map.insert("path".to_owned(), json!(path));
        map.insert(
            "base".to_owned(),
            json!(format!("{ROUTES_PREFIX}/{}", hook_segment(module))),
        );
        map.insert("ip".to_owned(), json!(request.ip));
        map.insert("protocol".to_owned(), json!(request.protocol));
        map.insert("user".to_owned(), v1_user(caller));
    }
    let answer = host
        .run_route(
            module,
            json!({
                "route": { "method": matched.route.method, "url": matched.route.url },
                "params": matched.params,
                "request": arrived,
                "installation": installation,
            }),
            call,
        )
        .await?;
    response_from(&answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_types::Role;

    fn route(method: &str, url: &str) -> RouteManifest {
        RouteManifest {
            method: method.into(),
            url: url.into(),
            api_token: false,
            no_csrf: false,
        }
    }

    #[test]
    fn a_route_is_served_under_its_module_segment() {
        assert_eq!(
            route_path("@saltcorn/heartbeat-monitor", "/heartbeat"),
            "/_feldspar/routes/heartbeat-monitor/heartbeat"
        );
    }

    #[test]
    fn literal_segments_and_params_match_and_methods_are_kept_apart() {
        let routes = vec![
            route("GET", "/heartbeat"),
            route("POST", "/items/:id/tag/:tag"),
        ];
        assert_eq!(
            match_route(&routes, "get", "/heartbeat").map(|m| m.route.url.as_str()),
            Some("/heartbeat")
        );
        assert!(match_route(&routes, "POST", "/heartbeat").is_none());
        assert!(match_route(&routes, "GET", "/heartbeat/more").is_none());
        let matched = match_route(&routes, "POST", "/items/7/tag/a%20b").unwrap();
        assert_eq!(matched.params["id"], json!("7"));
        assert_eq!(matched.params["tag"], json!("a b"));
        assert!(match_route(&routes, "POST", "/items//tag/x").is_none());
        assert_eq!(allowed_methods(&routes, "/heartbeat"), vec!["GET"]);
    }

    #[test]
    fn req_user_is_the_row_with_v1s_role_id() {
        let caller = Principal::new(
            Role::ADMIN,
            Some(json!({ "id": "u1", "email": "a@b.c", "role": 1 })),
        );
        assert_eq!(v1_user(&caller)["role_id"], json!(1));
        assert_eq!(v1_user(&caller)["email"], json!("a@b.c"));
        let public = Principal::anonymous(Role::PUBLIC);
        assert_eq!(v1_user(&public), Json::Null);
    }
}

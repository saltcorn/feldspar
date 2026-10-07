//! The administration MCP server: `POST /mcp` (design §13.6).
//!
//! One route, streamable HTTP, no server-initiated stream. It projects the
//! administrative tool surface — the fifteen composite tools of `sc_api::mcp` and
//! `sc_app::mcp`, plus the tools generated from the endpoints tagged
//! [`Endpoint::mcp`](sc_api::Endpoint::mcp) — to an external coding agent
//! holding a bearer token an administrator minted.
//!
//! ## Why it is here and not behind the `EndpointSet`
//!
//! It sits beside `/upload`, the backup routes and the two WebSocket upgrades,
//! and **outside** the typed endpoint set, for the reason they do: JSON-RPC over
//! a raw body is not a shape `TypeSchema` describes. Every tool here is
//! request/response and there is nothing to push, so there is no SSE channel to
//! keep alive for no traffic.
//!
//! ## The four things that must be true before a token is even read
//!
//! In this order, and the order is the point:
//!
//! 1. **The feature is on.** `mcp_enabled` is off by default and a disabled
//!    server answers `404` *without consulting the token table* — a disabled
//!    feature should not be distinguishable from an absent one, and should not be
//!    a code path that reads credentials.
//! 2. **No `Origin` header.** Per the MCP specification's DNS-rebinding
//!    guidance, a request that arrives with one is refused outright rather than
//!    validated against a list: nothing that legitimately speaks this protocol is
//!    a browser page.
//! 3. **The peer is local**, when `mcp_loopback_only` is on — which it is by
//!    default. An installation that will never be reached remotely should be able
//!    to say so in a checkbox rather than in a reverse proxy.
//! 4. **A bearer credential**, and *only* a bearer credential.
//!
//! ## Bearer only, and that is the CSRF answer
//!
//! **A session cookie on this route is ignored, not accepted**, and that is the
//! whole of the confused-deputy story rather than belt-and-braces. A page an
//! administrator visits cannot set an `Authorization` header on a cross-origin
//! request without a preflight this server will not answer, so no site they
//! browse can reach the administrative surface through the session they happen
//! to be logged into. Were cookies honoured here, this would be a
//! JSON-RPC-shaped hole beside every CSRF-protected endpoint in the server.
//!
//! The consequence is the CSRF exemption in [`crate::security`], written as
//! *this request carries a bearer credential* rather than as *this path is
//! `/mcp`*: a path-shaped exemption is one refactor away from being wrong.
//!
//! ## One authorization model, not two
//!
//! A token resolves to a [`User`], and from there nothing is different. A
//! tier-2 tool call is rendered back into an `ApiRequest` and dispatched through
//! the **same** [`HandlerRegistry`] the SPA's requests go through, with that user
//! as the caller and the endpoint's own `AuthRequirement` enforced; a tier-1 tool
//! runs the same body the chat copilot runs, under the token's six flags instead
//! of an agent's six checkboxes.
//!
//! ## What a refusal reads like
//!
//! A **tool** failure is a result the model reads — `isError: true` with the
//! sentence in it — because an error is not a failure of the run, it is the
//! result. JSON-RPC errors are reserved for what is wrong with the *call*: an
//! unknown method, an unknown tool, an unsupported revision, a refused
//! credential.
//!
//! One kind of failure is not even that: a **build that did not compile** is
//! news about the application rather than about the call, so an endpoint tagged
//! as a build answers `built: false` with the tools' own output and the
//! diagnostics parsed out of it. The same decision `build_application` made for
//! the chat copilot, and for the same reason — a model told only "the build
//! failed" cannot fix anything.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sc_api::mcp::{
    AdminTool, Area, Projection, ToolContext, ToolSet, areas_from_attrs, check_caller,
    grants_from_attrs,
};
use sc_api::schema_edit::Grants;
use sc_api::{Endpoint, EndpointSet, HandlerRef};
use sc_app::build_diagnostics;
use sc_auth::{ApiCaller, authenticate_api_token};
use sc_catalog::Catalog;
use sc_config::mcp_settings;
use sc_error::{Error, Result};
use sc_log::Verbosity;
use serde_json::{Value as Json, json};

use crate::handler::{HandlerCtx, HandlerRegistry};
use crate::router::AppState;

/// The route the server listens for MCP on.
pub const MCP_ROUTE: &str = "/mcp";

/// The one MCP protocol revision this server speaks.
///
/// Pinned in a single constant and **answered with**, never echoed. The
/// specification's lifecycle is that a server which does not support the
/// revision the client asked for replies with one it does, and the client then
/// decides whether it can proceed — so the handshake states this constant
/// whatever arrived, and every later request is served under it. Echoing the
/// client's revision back is the thing that would be negotiating one by
/// accident: a client and a server that disagree about the shape of a tool
/// result fail on the call that matters rather than on the handshake.
///
/// Refusing the handshake outright was tried and was wrong, which a real client
/// found within a minute: Claude Code asks for a later revision than this, and a
/// server that answers `initialize` with an error is a server that cannot be
/// connected to at all — by any client newer than its constant, forever. The
/// check that matters is the one below, not an error.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// The name this server introduces itself by in `initialize`.
pub const MCP_SERVER_NAME: &str = "saltcorn-feldspar-admin";

/// The header a bearer credential arrives in, and the scheme it must use.
const BEARER_PREFIX: &str = "Bearer ";

// --- JSON-RPC error codes ---------------------------------------------------
// The standard four, plus the one MCP reserves for authorization. A tool that
// *fails* is not any of these: it is a result with `isError` set, which the
// model reads (see the module docs).

/// The request body was not JSON, or not a JSON-RPC object.
const PARSE_ERROR: i64 = -32700;
/// The request was JSON but not a request this protocol has.
const INVALID_REQUEST: i64 = -32600;
/// No such method, or no such tool.
const METHOD_NOT_FOUND: i64 = -32601;
/// The parameters were not what the method takes.
const INVALID_PARAMS: i64 = -32602;
/// The credential was refused.
const UNAUTHORIZED: i64 = -32001;

/// Ceiling on one JSON-RPC body.
///
/// Generous for the largest thing that legitimately arrives — an `edit_schema`
/// batch describing a dozen connected tables — and small enough that a request
/// cannot ask this process to allocate arbitrarily.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Serve one MCP request.
///
/// The four gates of the module docs, then the JSON-RPC method. Every early
/// refusal is deliberately terse: a caller that has not got past the credential
/// is not a caller to explain the tool surface to.
///
/// The whole [`Request`] rather than extractors, for one reason: the peer
/// address arrives as a request extension that is present only when the server
/// was bound with connect info, and reading it here lets an absent one mean
/// *unknown*, which the loopback check then treats as remote. An extractor would
/// have to choose between failing the request and inventing an address.
pub(crate) async fn mcp(State(state): State<AppState>, request: Request) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .copied();
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return json_rpc_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                Json::Null,
                INVALID_REQUEST,
                format!("a request body here is at most {MAX_BODY_BYTES} bytes"),
            );
        }
    };
    // (1) A catalog is what the settings, the token table and every tool need.
    // A server assembled without one has no MCP surface to switch on, which is
    // the same answer as switching it off.
    let Some(catalog) = state.apps.catalog().cloned() else {
        return not_found();
    };
    let settings = match mcp_settings(&catalog).await {
        Ok(settings) => settings,
        Err(e) => {
            sc_log::log_error!("MCP: reading the settings failed: {e}");
            return json_rpc_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json::Null,
                INVALID_REQUEST,
                "the MCP settings could not be read",
            );
        }
    };
    if !settings.enabled {
        return not_found();
    }

    // (2) A browser `Origin` is a refusal, not a value to validate.
    if headers.contains_key(header::ORIGIN) {
        return refused(
            "this endpoint does not serve browser requests: it arrived with an \
             `Origin` header, and nothing that legitimately speaks MCP is a page",
        );
    }

    // (3) The loopback switch. An unknown peer counts as remote — the settings
    // read as the shut answer when they are junk, and so does this.
    if settings.loopback_only && !is_loopback(peer) {
        return refused(
            "this installation serves MCP to local clients only; an administrator \
             can turn off `mcp_loopback_only` under Settings \u{2192} Development, \
             or you can reach it through a tunnel that terminates here",
        );
    }

    // (4) The credential, and *only* the credential: any session cookie on this
    // request is ignored rather than accepted (module docs).
    let presented = match bearer_token(&headers) {
        Some(token) => token,
        None => {
            return json_rpc_error(
                StatusCode::UNAUTHORIZED,
                Json::Null,
                UNAUTHORIZED,
                "this endpoint authenticates by `Authorization: Bearer <token>` and \
                 by nothing else — a session cookie is ignored here. An \
                 administrator mints a token under Settings \u{2192} Development.",
            );
        }
    };
    let caller = match authenticate_api_token(&catalog, &presented).await {
        Ok(caller) => caller,
        Err(e) => {
            // The message is the storage layer's, which already says *which* of
            // revoked, expired, unknown or demoted this was — the four are
            // different things to tell somebody.
            return json_rpc_error(
                StatusCode::UNAUTHORIZED,
                Json::Null,
                UNAUTHORIZED,
                e.to_string(),
            );
        }
    };

    dispatch_rpc(&state, &catalog, &caller, &body).await
}

/// Parse one JSON-RPC message and answer it.
async fn dispatch_rpc(
    state: &AppState,
    catalog: &Arc<Catalog>,
    caller: &ApiCaller,
    body: &Bytes,
) -> Response {
    let Ok(message) = serde_json::from_slice::<Json>(body) else {
        return json_rpc_error(
            StatusCode::BAD_REQUEST,
            Json::Null,
            PARSE_ERROR,
            "the request body is not JSON",
        );
    };
    // JSON-RPC batching was removed in this protocol revision, so an array is a
    // client speaking an older one — worth saying rather than "not an object".
    if message.is_array() {
        return json_rpc_error(
            StatusCode::BAD_REQUEST,
            Json::Null,
            INVALID_REQUEST,
            format!(
                "batched requests are not part of MCP {MCP_PROTOCOL_VERSION}; \
                 send one request per body"
            ),
        );
    }
    let Some(object) = message.as_object() else {
        return json_rpc_error(
            StatusCode::BAD_REQUEST,
            Json::Null,
            INVALID_REQUEST,
            "a JSON-RPC message is an object",
        );
    };

    let id = object.get("id").cloned().unwrap_or(Json::Null);
    let method = object.get("method").and_then(Json::as_str).unwrap_or("");
    let params = object.get("params").cloned().unwrap_or(Json::Null);

    // A notification carries no `id` and takes no answer. `notifications/initialized`
    // is the one this server expects; anything else is accepted and dropped,
    // because a notification the receiver does not know is not an error the
    // sender can do anything about.
    if object.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    match method {
        "initialize" => initialize(&id, &params),
        "tools/list" => {
            let tools = tool_set(state, caller);
            let listed: Vec<Json> = tools
                .specs(catalog)
                .into_iter()
                .map(|spec| {
                    json!({
                        "name": spec.name,
                        "description": spec.description,
                        "inputSchema": spec.parameters,
                    })
                })
                .collect();
            json_rpc_result(&id, json!({ "tools": listed }))
        }
        "tools/call" => call_tool(state, catalog, caller, &id, &params).await,
        // `ping` is the specification's one-line liveness check and costs
        // nothing to answer honestly.
        "ping" => json_rpc_result(&id, json!({})),
        other => json_rpc_error(
            StatusCode::OK,
            id,
            METHOD_NOT_FOUND,
            format!(
                "this server implements `initialize`, `tools/list`, `tools/call` \
                 and `ping`, not `{other}`"
            ),
        ),
    }
}

/// The handshake, and the revision check that is the whole of it.
fn initialize(id: &Json, params: &Json) -> Response {
    // What the client asked for is logged and **not** answered with: the reply
    // is always this server's own revision, which is what tells a client that
    // asked for a later one what it is actually talking to. A client that cannot
    // work with the answer closes the connection, which is its decision to make
    // and not one this server can make for it.
    if let Some(asked) = params.get("protocolVersion").and_then(Json::as_str)
        && asked != MCP_PROTOCOL_VERSION
    {
        sc_log::log_verbose!(
            "MCP: client asked for protocol {asked}; answering with {MCP_PROTOCOL_VERSION}"
        );
    }
    json_rpc_result(
        id,
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            // Tools and nothing else: there are no resources and no prompts
            // here, and `listChanged` is false because there is no stream to
            // send a change notification down.
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": MCP_SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": INSTRUCTIONS,
        }),
    )
}

/// What the client is told this server is for, once, at the handshake.
///
/// The two halves of an application are the thing worth saying: a coding agent
/// arrives here already holding the repository, and the mistake it would
/// otherwise make is looking for the schema in a file.
const INSTRUCTIONS: &str = "This is the administrative surface of a Saltcorn Feldspar \
installation: the half of an application that lives in the database rather than in its source \
directory — the applications themselves, the tables and their fields, the access rules, the \
triggers, the workflows and the agents. Write an application's source code through the \
filesystem as usual; use these tools for everything that is configuration. There are \
deliberately no tools for reading or writing row data, for browsing or writing files, or for \
user management. Code you store here — a `run_js_code` trigger or workflow step, a \
`javascript` API query — runs against this server's own JavaScript API, which is not one you \
know from elsewhere: call `describe_code_api` before writing any.

Building a new application. When the user asks you to build an application, a site or a tool \
(\"build me a to-do list\"), build the whole first working draft without asking them anything \
you can decide: \
(1) `describe_applications` and `describe_schema`, to see what exists. \
(2) `create_application`. Unless the user named another technology, it is a React \
application in a new local file store — do not ask. Only if they asked for the code to be in a \
git repository, first `create_file_store` with `backend: \"git\"` and pass its name as \
`file_store`. The result's `project_dir` is the application's source directory on this \
machine, already scaffolded: a React + TypeScript + Vite project with a typed client for the \
application's API in `src/feldspar/` and sign-in wired up. Work in that directory from now \
on, and read its `AGENTS.md`. \
(3) Create every table the application needs in one `edit_schema` batch — tables before \
code, because the client is generated from them. \
(4) `update_application` with `tables: { add: [...] }` to connect those tables to the \
application; this regenerates \
`src/feldspar/` so the client has a typed method for each. \
(5) Write the pages in the project directory, using only the generated client to reach the \
data, and every user-visible string through `t(\"…\")` (or `<T text=\"…\" />` for a sentence \
with an element inside it) — the project's `AGENTS.md` has the call shapes. \
(6) `buildApplication` (its id is in `describe_applications`) to build it and serve it on its \
subdomain; fix whatever the diagnostics report and build again until it succeeds. \
(7) Tell the user the address and what the draft does.

To change an existing application, `describe_applications` gives its `project_dir`: work \
there, connect any new table with `update_application`, and rebuild.

Translating an application (\"translate this app to German\", \"the German for 'Save' should \
be …\"). You are the translator; the tools hold the strings and check your work. \
(1) `describe_translations` lists every string the source wraps in `t()`, the locales the app \
serves and what each has; its `unwrapped` list is user-visible text nothing wraps, which cannot \
be translated until you wrap it in the source (and rebuild). \
(2) `update_application` with `locales: { add: [\"de\"] }` if the locale is not served yet. \
(3) `describe_translations` with `locale` and `missing_only: true`, then `save_translations` \
with your translations, in batches of about a hundred. The key is the English source text \
exactly; keep every `{placeholder}` name unchanged. A correction is one `save_translations` \
call naming just that key — other translations are kept. No build is needed: the app serves \
the change on its next page load.";

async fn call_tool(
    state: &AppState,
    catalog: &Arc<Catalog>,
    caller: &ApiCaller,
    id: &Json,
    params: &Json,
) -> Response {
    let Some(name) = params.get("name").and_then(Json::as_str) else {
        return json_rpc_error(
            StatusCode::OK,
            id.clone(),
            INVALID_PARAMS,
            "`tools/call` takes the tool's `name`",
        );
    };
    let arguments = params.get("arguments").cloned().unwrap_or(Json::Null);

    let tools = tool_set(state, caller);
    // An unknown tool is wrong with the *call*, so it is a JSON-RPC error rather
    // than a result the model reads — there is nothing for the model to correct
    // inside a tool that does not exist.
    if !tools.offers(name) {
        return json_rpc_error(
            StatusCode::OK,
            id.clone(),
            METHOD_NOT_FOUND,
            format!(
                "this server offers no tool called `{name}`; call `tools/list` for \
                 the ones this token was granted"
            ),
        );
    }

    let ctx = ToolContext {
        catalog,
        user: Some(&caller.user),
        role: caller.user.role,
        triggers: state.apps.triggers(),
        actor: &caller.token.label,
    };

    // The audit line of §13.6: the token's **label**, never the token and never
    // its hash. At Verbose the arguments too, which is the rung an agent's
    // tool-call arguments already sit on.
    if sc_log::enabled(Verbosity::Verbose) {
        sc_log::log_verbose!("MCP [{}] {name} {arguments}", caller.token.label);
    }
    let started = Instant::now();
    let outcome = tools.call(name, &arguments, &ctx).await;
    let elapsed = sc_log::human_duration(started.elapsed());
    match &outcome {
        Ok(_) => sc_log::log_info!("MCP [{}] {name} ok in {elapsed}", caller.token.label),
        Err(e) => sc_log::log_info!(
            "MCP [{}] {name} refused in {elapsed}: {e}",
            caller.token.label
        ),
    }

    json_rpc_result(id, tool_result(outcome))
}

/// One tool's outcome as an MCP tool result.
///
/// A failure is `isError: true` with the sentence in the content, **not** a
/// JSON-RPC error: an error here is not a failure of the run, it is the result
/// the model reads, and a transport-level error is the one shape the model does
/// not see.
fn tool_result(outcome: Result<Json>) -> Json {
    match outcome {
        Ok(value) => {
            let text = match &value {
                Json::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
            };
            let mut result = json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            });
            // The same value again, machine-readable, for a client that prefers
            // it — and **only when it is an object**, because that is what
            // `structuredContent` is defined as. The key is left out rather than
            // set to `null` for a list: a client that validates the field reads
            // `null` as a malformed result and fails the call, which is what a
            // real one did to every `list…` tool here (they all answer arrays).
            // Wrapping a list in a key nobody declared would be inventing a
            // shape; omitting the field says the truthful thing, that this
            // result's structure is the text.
            if let Json::Object(map) = value {
                result["structuredContent"] = Json::Object(map);
            }
            result
        }
        Err(e) => json!({
            "content": [{ "type": "text", "text": e.to_string() }],
            "isError": true,
        }),
    }
}

/// The tools this token was granted: the fifteen composite ones and the tagged
/// endpoints, under the token's six flags.
///
/// Built per request rather than cached, because the flags are the token's and
/// the catalog behind the descriptions is live — a set built at boot would list
/// yesterday's tables in `describe_schema`'s prose.
fn tool_set(state: &AppState, caller: &ApiCaller) -> ToolSet {
    let grants = grants_from_attrs(&caller.token.grants);
    let areas = areas_from_attrs(&caller.token.grants);
    sc_app::mcp::tool_set(grants, areas).with(state.mcp_endpoint_tools.iter().cloned())
}

/// Build the tier-2 tools of an endpoint set, once, at router assembly.
///
/// Here rather than in `sc-api` because dispatching one needs the
/// [`HandlerRegistry`], which only the server holds — which is exactly the seam
/// [`AdminTool`] being a trait object was for.
pub(crate) fn endpoint_tools(
    endpoints: &EndpointSet,
    handlers: &Arc<HandlerRegistry>,
) -> Vec<Arc<dyn AdminTool>> {
    Projection::all(endpoints)
        .map(|projection| {
            // `AdminTool::name` is `&'static str` because the hand-written tools
            // are constants; an endpoint's name is an owned `String`. Leaking it
            // once here — at router assembly, bounded by the number of tagged
            // endpoints — is what lets one trait serve both, and the alternative
            // (a `Cow` on every tool in the codebase) is a worse trade.
            let name: &'static str = Box::leak(projection.name().to_owned().into_boxed_str());
            Arc::new(EndpointTool {
                name,
                projection,
                handlers: Arc::clone(handlers),
            }) as Arc<dyn AdminTool>
        })
        .collect()
}

/// One tagged endpoint, as a tool.
struct EndpointTool {
    /// The endpoint's own name (see [`endpoint_tools`] for why it is leaked).
    name: &'static str,
    projection: Projection,
    handlers: Arc<HandlerRegistry>,
}

#[async_trait::async_trait]
impl AdminTool for EndpointTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn area(&self) -> Option<Area> {
        self.projection.area()
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        self.projection.description().to_owned()
    }

    fn parameters(&self) -> Json {
        self.projection.parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        // The caller, the endpoint's own `AuthRequirement`, and the grant the
        // tag declared — all three, before anything is dispatched.
        check_caller(&self.projection, ctx, grants)?;
        let call = self.projection.split(args)?;
        let handler = resolve(&self.handlers, self.projection.endpoint())?;
        let outcome = handler(HandlerCtx {
            path_params: call.path_params,
            query: call.request.query,
            body: call.request.body,
            user: ctx.user.cloned(),
            // A projected tool never carries bytes: the endpoints that do are
            // tier 3, and a JSON arguments object has no shape for them.
            raw_body: None,
            // An MCP call is a coding agent's, not a browser's: there is no
            // `Accept-Language` and no cookie to negotiate from, so it is served
            // in the installation's default language (§16.1).
            locale: sc_i18n::active().default_locale().clone(),
        })
        .await;
        match (outcome, self.projection.is_a_build()) {
            (Ok(response), false) => Ok(response.body),
            (Ok(response), true) => Ok(with_diagnostics(response.body)),
            (Err(e), false) => Err(e),
            // A build that did not compile is the result, not the refusal
            // (§13.6): the endpoint reports it as an error because a screen
            // wants a red box, and a model wants the lines to fix.
            (Err(e), true) => Ok(build_failure(&e)),
        }
    }
}

/// A successful build, with the diagnostics its log names added.
///
/// Kept on success too, because that is where a bundler puts its **warnings** —
/// a model told only "built" would never see them — and because a shape that
/// changes between success and failure is one the model has to learn twice.
fn with_diagnostics(mut body: Json) -> Json {
    let log = body
        .get("log")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Some(object) = body.as_object_mut() {
        object.insert("diagnostics".to_owned(), json!(build_diagnostics(&log)));
    }
    body
}

/// A failed build as the result the model reads: the same three keys a
/// successful one has, so there is one shape rather than two.
fn build_failure(e: &Error) -> Json {
    // The bundler's own output *is* the error message here — `build_and_mount`
    // carries it through §16 — so it is both the log and what the diagnostics
    // are parsed out of.
    let log = e.to_string();
    json!({
        "built": false,
        "log": log,
        "diagnostics": build_diagnostics(&log),
    })
}

/// The code behind a tagged endpoint, or the configuration error that says this
/// process has none.
fn resolve(handlers: &HandlerRegistry, endpoint: &Endpoint) -> Result<crate::handler::HandlerFn> {
    let HandlerRef::Named(name) = &endpoint.handler else {
        return Err(Error::config(format!(
            "`{}` is not served by a built-in handler in this process",
            endpoint.name
        )));
    };
    handlers.get(name).cloned().ok_or_else(|| {
        Error::config(format!(
            "`{}` is offered as a tool but this process registers no handler for it",
            endpoint.name
        ))
    })
}

// --- the wire ---------------------------------------------------------------

/// The bearer credential this request presents, if it presents one.
///
/// A cookie is not consulted, here or anywhere on this route.
/// The admin handlers, offered to the administrative tools through the
/// catalog (see [`sc_catalog::AdminHost`]): how `create_application` — whether
/// the chat copilot or an MCP client called it — runs the same handler the
/// admin's Create button does.
///
/// Holds the registry **weakly**: the catalog outlives every router, and a
/// strong handle here would be a cycle through the handlers' own clones of the
/// catalog.
pub(crate) struct AdminHandlers {
    handlers: std::sync::Weak<HandlerRegistry>,
    catalog: std::sync::Weak<Catalog>,
}

impl AdminHandlers {
    /// Install `handlers` on `catalog`.
    pub(crate) fn install(catalog: &Arc<Catalog>, handlers: &Arc<HandlerRegistry>) -> Result<()> {
        catalog.set_admin_host(Arc::new(AdminHandlers {
            handlers: Arc::downgrade(handlers),
            catalog: Arc::downgrade(catalog),
        }))
    }
}

#[async_trait::async_trait]
impl sc_catalog::AdminHost for AdminHandlers {
    async fn call_admin(&self, call: sc_catalog::AdminCall) -> Result<Json> {
        let handlers = self.handlers.upgrade().ok_or_else(|| {
            Error::config("the server that installed the admin handlers has stopped")
        })?;
        let handler = handlers.get(&call.endpoint).cloned().ok_or_else(|| {
            Error::config(format!(
                "this process registers no handler for `{}`",
                call.endpoint
            ))
        })?;
        drop(handlers);
        let user = match call.user {
            Some(id) => {
                let catalog = self
                    .catalog
                    .upgrade()
                    .ok_or_else(|| Error::config("the server's catalog has been dropped"))?;
                sc_auth::load_user(&catalog, id).await?
            }
            None => None,
        };
        let response = handler(HandlerCtx {
            path_params: call.path_params.into_iter().collect(),
            query: Vec::new(),
            body: call.body,
            user,
            raw_body: None,
            locale: sc_i18n::active().default_locale().clone(),
        })
        .await?;
        Ok(response.body)
    }
}

pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix(BEARER_PREFIX)?.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// Whether the peer is on this machine. An unknown peer is not.
fn is_loopback(peer: Option<ConnectInfo<SocketAddr>>) -> bool {
    peer.is_some_and(|ConnectInfo(addr)| addr.ip().is_loopback())
}

/// The answer a switched-off server gives, and the one a server without a
/// catalog gives: indistinguishable, on purpose.
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({ "error": "not found" })),
    )
        .into_response()
}

/// A transport-level refusal: the request never became a JSON-RPC call.
fn refused(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({ "error": message })),
    )
        .into_response()
}

/// A JSON-RPC success.
fn json_rpc_result(id: &Json, result: Json) -> Response {
    axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

/// A JSON-RPC error — reserved for what is wrong with the *call* (module docs).
fn json_rpc_error(status: StatusCode, id: Json, code: i64, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message.into() },
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn only_a_bearer_credential_is_read_and_a_cookie_never_is() {
        assert_eq!(
            bearer_token(&headers(&[("authorization", "Bearer fspk_abc")])).as_deref(),
            Some("fspk_abc")
        );
        // A session cookie is not a credential on this route, whatever it says.
        assert_eq!(
            bearer_token(&headers(&[("cookie", "sc_session=live")])),
            None
        );
        // Nor is another scheme, nor an empty token.
        assert_eq!(
            bearer_token(&headers(&[("authorization", "Basic dXNlcjpwdw==")])),
            None
        );
        assert_eq!(
            bearer_token(&headers(&[("authorization", "Bearer  ")])),
            None
        );
    }

    #[test]
    fn an_unknown_peer_is_treated_as_remote() {
        // The safer answer: the loopback switch is on by default, and a peer
        // nobody can name must not pass a check about where it is.
        assert!(!is_loopback(None));
        assert!(is_loopback(Some(ConnectInfo(
            "127.0.0.1:9000".parse().unwrap()
        ))));
        assert!(is_loopback(Some(ConnectInfo(
            "[::1]:9000".parse().unwrap()
        ))));
        assert!(!is_loopback(Some(ConnectInfo(
            "203.0.113.7:9000".parse().unwrap()
        ))));
    }

    #[test]
    fn a_failed_tool_is_a_result_the_model_reads_not_a_transport_error() {
        let refusal = tool_result(Err(Error::invalid(
            "not permitted to drop a table; the whole batch was refused",
        )));
        assert_eq!(refusal["isError"], json!(true));
        let text = refusal["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not permitted to drop a table"), "{text}");

        let ok = tool_result(Ok(json!({ "tables": ["tasks"] })));
        assert_eq!(ok["isError"], json!(false));
        assert_eq!(ok["structuredContent"], json!({ "tables": ["tasks"] }));
        assert!(ok["content"][0]["text"].as_str().unwrap().contains("tasks"));

        // A tool that answers a **list** — which every `list…` tool here does —
        // carries no `structuredContent` at all rather than a null one. A client
        // that validates the field against the object it is defined to be reads
        // a null as a malformed result and fails the call.
        let list = tool_result(Ok(json!([{ "name": "copilot" }])));
        assert_eq!(list["isError"], json!(false));
        assert!(list.get("structuredContent").is_none(), "{list}");
        assert!(
            list["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("copilot"),
            "{list}"
        );
    }

    #[test]
    fn a_client_asking_for_another_revision_is_answered_with_this_ones() {
        let response = initialize(&json!(1), &json!({ "protocolVersion": "2999-01-01" }));
        assert_eq!(response.status(), StatusCode::OK);
        // The body is checked in the integration suite, which can read it; what
        // is asserted here is that the constant is the only revision named.
        assert_eq!(MCP_PROTOCOL_VERSION, "2025-06-18");
    }

    #[test]
    fn a_build_that_did_not_compile_is_a_result_with_the_lines_to_fix() {
        // The decision `build_application` already made, kept here: a model told
        // only "the build failed" cannot fix anything, so the tools' own output
        // travels whole and the diagnostics are indexed out of it.
        let failed = build_failure(&Error::config(
            "build command `npm run build` failed in /srv/web with exit status: 2\n\
             src/App.tsx(12,5): error TS2322: Type 'number' is not assignable to type 'string'.",
        ));
        assert_eq!(failed["built"], json!(false));
        assert!(
            failed["log"]
                .as_str()
                .unwrap_or_default()
                .contains("TS2322"),
            "{failed}"
        );
        assert_eq!(failed["diagnostics"][0]["file"], json!("src/App.tsx"));
        assert_eq!(failed["diagnostics"][0]["line"], json!(12));

        // And a build that succeeded keeps the endpoint's own body, with the
        // same `diagnostics` key — where a bundler's warnings live. One shape,
        // not two for the model to learn.
        let ok = with_diagnostics(json!({
            "built": true,
            "git_repo": false,
            "log": "src/App.tsx(3,1): warning TS6133: 'x' is declared but never read.",
        }));
        assert_eq!(ok["built"], json!(true));
        assert_eq!(ok["git_repo"], json!(false));
        assert_eq!(ok["diagnostics"][0]["line"], json!(3));
    }
}

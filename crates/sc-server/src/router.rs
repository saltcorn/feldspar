//! Router assembly and endpoint dispatch (technical design §13.1, §16).
//!
//! [`build_router`] turns an [`EndpointSet`] into an axum [`Router`]. Rather than
//! registering one axum route per endpoint, all routes — the compile-time admin
//! API and any runtime-registered application routes — are dispatched through a
//! single [`matchit`] router built from the endpoint values, exactly the "routes
//! need not be known at compile time" machinery the design calls for. A request
//! is matched to an endpoint, its [`AuthRequirement`] is enforced against the
//! session, and its handler is resolved from the [`HandlerRegistry`]; anything
//! that isn't an API route falls through to the static `ui/admin` bundle (via
//! `tower-http`'s [`ServeDir`]) or the minimal bootstrap document — except under
//! [`IDE_PREFIX`], which is the file-store IDE's own bundle, served admin-only and
//! under its own Content-Security-Policy (design §12.1).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::Cookie;
use sc_api::{
    ApiRequest, AuthRequirement, Endpoint, EndpointSet, HandlerRef, Method as ApiMethod,
    SessionAction,
};
use sc_app::AppRequest;
use sc_auth::{SessionStore, User};
use sc_error::{Error, ErrorKind, Repr, Result};
use sc_i18n::t;
use serde_json::Value;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::apps::{AppMounts, MountedApp, subdomain_in};
use crate::backup::{BACKUP_CREATE_ROUTE, BACKUP_UPLOAD_ROUTE};
use crate::chat::{AGENT_CHAT_ROUTE, agent_chat_upgrade};
use crate::config::ServerConfig;
use crate::fit_progress::{FIT_PROGRESS_ROUTE, fit_progress_upgrade};
use crate::handler::{HandlerCtx, HandlerRegistry, HandlerResponse};
use crate::lsp::{LSP_ROUTE, ServerSlots, language_server_upgrade, server_slots};
use crate::mcp::MCP_ROUTE;
use crate::observe::{STREAM_OBSERVE_ROUTE, stream_observe_by_name, stream_observe_upgrade};
use crate::security::{
    ANALYTICS_CONTENT_SECURITY_POLICY, AnonymousCaller, CONTENT_SECURITY_POLICY, CSRF_COOKIE,
    CSRF_HEADER, CsrfPolicy, IDE_CONTENT_SECURITY_POLICY, PREVIEW_COOKIE, SESSION_COOKIE,
    admin_content_security_policy, build_cookie, csrf_middleware, is_native_client,
};

/// A WebSocket upgrade, **if this request is one** — the extractor the fallback
/// needs and axum does not provide (TODO "Streams" §10, task 8.2).
///
/// An application's routes are not axum routes: every one of them arrives at
/// the single [`dispatch`] fallback, because an app's paths are its own and
/// change while the server runs. An upgrade therefore has to be taken there,
/// and `WebSocketUpgrade` rejects a request that is not one rather than
/// answering `None` (it has no `OptionalFromRequestParts` impl). So this wraps
/// it: extracted from the parts, *before* the body is read, which is the one
/// ordering rule an upgrade has.
struct MaybeUpgrade(Option<axum::extract::ws::WebSocketUpgrade>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for MaybeUpgrade {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> std::result::Result<MaybeUpgrade, Self::Rejection> {
        Ok(MaybeUpgrade(
            axum::extract::ws::WebSocketUpgrade::from_request_parts(parts, state)
                .await
                .ok(),
        ))
    }
}

/// The document served for a navigation when there is **no admin bundle to
/// serve** — no `--static-dir`, or a directory with no `index.html` in it.
///
/// It links no assets, and that is the whole point of it. The bundle's entry
/// points carry a content hash in their names (see `ui/admin/vite.config.ts`),
/// so this constant *cannot* name them; a document that guessed would be served
/// for the guessed path too, and the browser would report a module with a
/// `text/html` MIME type — the blank page with a puzzling console error that
/// this says out loud instead. No inline script or style, so the strict CSP
/// holds here as it does everywhere else.
pub const BOOTSTRAP_HTML: &str = "<!doctype html>\n\
<html lang=\"en\">\n\
<head>\n\
<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>Saltcorn</title>\n\
</head>\n\
<body>\n\
<div id=\"root\"></div>\n\
<p>The Saltcorn admin UI is not built. Run <code>npm ci &amp;&amp; npm run build</code>\n\
in <code>ui/admin</code>, and start the server with <code>--static-dir</code> pointing\n\
at <code>ui/admin/dist</code> (or rebuild <code>sc-cli</code>, which does both).</p>\n\
</body>\n\
</html>\n";

/// `Cache-Control` for a file whose name carries a content hash: a year, and
/// never revalidated. Changing the file changes its name, so a stale copy of
/// this exact URL cannot exist.
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// `Cache-Control` for everything else a bundle holds — above all the document,
/// which keeps its URL across every rebuild and is what names the hashed assets.
/// `no-cache` is "revalidate", not "do not store": the browser still gets its
/// 304s, but it can never show yesterday's document (and so yesterday's app)
/// without asking.
const REVALIDATE_CACHE_CONTROL: &str = "no-cache";

/// Whether a bundle-relative path is one of Vite's content-hashed outputs.
///
/// Everything the build emits apart from `index.html` lands in `assets/` with a
/// hash in its name, so the prefix is the test.
fn is_hashed_asset(path: &str) -> bool {
    path.starts_with("/assets/")
}

/// The path prefix the file-store IDE is served under (design §12.1).
pub const IDE_PREFIX: &str = "/ide";

/// The route prefix the Analytics UI's bundle is served under (analytics TODO
/// A1.14): admin-only, under its own CSP, like the IDE.
pub const ANALYTICS_PREFIX: &str = "/analytics";

/// Shared server state threaded through dispatch.
#[derive(Clone)]
pub(crate) struct AppState {
    /// Path pattern → the endpoints registered at that path (one per method).
    routes: Arc<matchit::Router<Vec<Endpoint>>>,
    /// Name → handler resolution for `HandlerRef::Named`.
    handlers: Arc<HandlerRegistry>,
    /// The session store backing login/logout and per-request auth.
    sessions: Arc<SessionStore>,
    /// Directory holding the built `ui/ide` bundle, if configured.
    ide_dir: Option<Arc<PathBuf>>,
    /// Directory holding the built `ui/analytics` bundle, if configured.
    analytics_dir: Option<Arc<PathBuf>>,
    /// Directory holding the built `ui/admin` bundle, if configured.
    static_dir: Option<Arc<PathBuf>>,
    /// Directory holding the built `ui/builder` bundle, if configured.
    builder_dir: Option<Arc<PathBuf>>,
    /// Whether to set `Secure` on the session cookie.
    secure_cookies: bool,
    /// The applications served on their own subdomains, if any.
    ///
    /// `pub(crate)` because it is also where the catalog and the trigger
    /// dispatcher live, which the MCP route needs (§13.6).
    pub(crate) apps: Arc<AppMounts>,
    /// The domain apps are served under; `None` disables app routing.
    base_domain: Option<Arc<String>>,
    /// Further domains the same apps answer under (`--extra-base-domain`).
    extra_base_domains: Arc<Vec<String>>,
    /// How many more language servers the IDE may start (design §12.1).
    lsp_slots: ServerSlots,
    /// The tier-2 MCP tools: one per endpoint tagged
    /// [`Endpoint::mcp`](sc_api::Endpoint::mcp), built once here because
    /// dispatching one needs the handler registry (§13.6).
    ///
    /// Built even when the MCP server is switched off, and that costs nothing:
    /// it is a projection of values already in memory, and the switch is read
    /// per request rather than at boot precisely so that turning it on is a save
    /// rather than a restart.
    pub(crate) mcp_endpoint_tools: Arc<Vec<Arc<dyn sc_api::mcp::AdminTool>>>,
}

/// Build the axum router for an endpoint set, serving no applications.
///
/// Fails only if the endpoint paths cannot be assembled into a [`matchit`]
/// router (a duplicate/conflicting route pattern — a registration bug).
///
/// The [`AppMounts`] this builds with carries **no catalog**, so the router it
/// returns serves no administration MCP surface — `/mcp` answers `404`, which is
/// the same answer the switch being off gives (§13.6). A server that wants one
/// calls [`build_router_with_apps`] with mounts that hold the catalog, which is
/// what [`serve`](crate::serve) does.
pub fn build_router(
    endpoints: &EndpointSet,
    handlers: HandlerRegistry,
    sessions: Arc<SessionStore>,
    config: &ServerConfig,
) -> Result<Router> {
    build_router_with_apps(
        endpoints,
        handlers,
        sessions,
        config,
        Arc::new(AppMounts::none()),
    )
}

/// Build the axum router, also serving `apps` on their own subdomains
/// (design §13.2).
///
/// A request is routed to an app by its `Host`: `blog.<base_domain>` reaches the
/// app whose subdomain is `blog`. Anything else — the base domain, an unknown
/// subdomain, or any host when no base domain is configured — is the admin, so
/// mounting an app cannot take the admin away from an operator.
///
/// `apps` is a **shared, live** [`AppMounts`] handle (design §13.2): the caller
/// keeps a clone to mount/unmount apps at runtime, and this router resolves each
/// request against the registry's current contents — an app mounted after the
/// router was built serves immediately, with no restart.
pub fn build_router_with_apps(
    endpoints: &EndpointSet,
    handlers: HandlerRegistry,
    sessions: Arc<SessionStore>,
    config: &ServerConfig,
    apps: Arc<AppMounts>,
) -> Result<Router> {
    if !apps.is_empty() && config.base_domain.is_none() {
        return Err(Error::config(format!(
            "applications are mounted ({}) but no --base-domain is set, so no request \
             could ever reach them",
            apps.subdomains().join(", ")
        )));
    }
    if !apps.is_empty() && apps.catalog().is_none() {
        return Err(Error::config(
            "applications are mounted but no catalog was given for their APIs to run against",
        ));
    }

    let routes = Arc::new(build_matchit(endpoints)?);
    let mut handlers = handlers;
    crate::builder::register_status_handler(&mut handlers, config.builder_dir.clone());
    let handlers = Arc::new(handlers);
    let mcp_endpoint_tools = Arc::new(crate::mcp::endpoint_tools(endpoints, &handlers));
    // The administrative tools create applications and file stores through
    // these same handlers (§13.6), so the router that serves them installs them.
    if let Some(catalog) = apps.catalog() {
        crate::mcp::AdminHandlers::install(catalog, &handlers)?;
    }
    let state = AppState {
        routes,
        handlers,
        sessions,
        static_dir: config.static_dir.clone().map(Arc::new),
        ide_dir: config.ide_dir.clone().map(Arc::new),
        analytics_dir: config.analytics_dir.clone().map(Arc::new),
        builder_dir: config.builder_dir.clone().map(Arc::new),
        secure_cookies: config.secure_cookies,
        apps,
        base_domain: config.base_domain.clone().map(Arc::new),
        extra_base_domains: Arc::new(config.extra_base_domains.clone()),
        lsp_slots: server_slots(),
        mcp_endpoint_tools,
    };

    let app = Router::new()
        // Operational health check: a fixed, unauthenticated route (outside the
        // typed API) so a CLI smoke test, load balancer, or orchestrator can
        // confirm the process is up. It takes precedence over the SPA fallback.
        .route("/health", axum::routing::get(health))
        // Binary file upload, deliberately **outside** the typed `EndpointSet`.
        //
        // The endpoint model is JSON-only — `TypeSchema` is
        // `Value`/`Struct`/`Array`/`Optional` and `HandlerResponse` carries
        // `Json` — so a raw body cannot be described by it. `writeFile`'s base64
        // path stays for small files and remains in the generated TypeScript
        // client; this exists for the ones that should not be held in memory
        // twice and inflated by a third to cross the wire.
        //
        // The cost, accepted knowingly: this is the first admin operation absent
        // from the generated client, so the SPA hand-writes this one call. It
        // therefore has to do for itself everything `dispatch` does for a typed
        // endpoint — session lookup and the admin check — which is why the auth
        // is repeated here rather than inherited. CSRF is *not* repeated: the
        // middleware wraps every route including this one.
        .route("/upload/{store}/{*path}", axum::routing::post(upload))
        // …and its mirror: a store file as itself, for the file manager's
        // Download. `readFile` answers base64 inside JSON, which a browser has
        // to hold twice and decode by hand — fine for a source file, hopeless
        // for a 100 MB APK. A plain `GET`, so a link downloads it; it reads
        // nothing it does not check access to, as `readFile` does.
        .route("/download/{store}/{*path}", axum::routing::get(download))
        // Backup and restore, **outside** the typed `EndpointSet` for the same
        // reason the upload above is: one route's response is a file and the
        // other's request is one, and a `TypeSchema` has no bytes shape. Both
        // still dispatch through the handler registry, so both are behind the
        // same session lookup, the same admin check and the same CSRF middleware
        // as every typed endpoint — the auth is repeated here rather than
        // inherited because `dispatch` is what usually applies it.
        //
        // The *choice* of what to back up and what to restore travels as JSON
        // through typed endpoints (`getBackupOptions`, `restoreBackup`); only the
        // archive itself comes through here.
        .route(BACKUP_CREATE_ROUTE, axum::routing::post(create_backup))
        .route(BACKUP_UPLOAD_ROUTE, axum::routing::post(upload_backup))
        // The file-store IDE's language server (design §12.1). A real route
        // rather than a branch of the fallback, because a WebSocket upgrade is
        // not a request the fallback's `Bytes` body could survive: it has to be
        // extracted before the body is touched.
        .route(LSP_ROUTE, axum::routing::get(language_server))
        // The admin chat socket (§11.4). A real route for the same reason the
        // language server's is: an upgrade cannot survive the fallback's `Bytes`
        // body, and a chat turn is bidirectional in a way the typed endpoint
        // model has no shape for.
        .route(AGENT_CHAT_ROUTE, axum::routing::get(agent_chat))
        // A stream's Observe socket (TODO "Streams" §9). The third route that
        // is an upgrade rather than a typed endpoint, for the reason the other
        // two are: an `EndpointSet` is a typed request/response model and a
        // socket has no shape in it. It sits on `/api/streams/{id}/observe`,
        // beside the stream endpoints rather than under `/admin`, because it is
        // the same resource the CRUD endpoints address — and axum matches this
        // literal route ahead of the `dispatch` fallback the rest of `/api`
        // goes through.
        .route(STREAM_OBSERVE_ROUTE, axum::routing::get(stream_observe))
        // A fit's progress, pushed to the model editor (analytics TODO A3.3).
        // An upgrade for the reason the Observe socket's is.
        .route(FIT_PROGRESS_ROUTE, axum::routing::get(fit_progress))
        // The administration MCP server (§13.6). A real route rather than a
        // typed endpoint for the reason the upload and backup routes are:
        // JSON-RPC over a raw body is not a shape `TypeSchema` describes.
        //
        // It authenticates by `Authorization: Bearer` and by **nothing else** —
        // a session cookie on this route is ignored, not accepted — which is
        // what makes it safe to exempt from the CSRF middleware below, and is
        // the whole of the confused-deputy story rather than belt-and-braces.
        .route(MCP_ROUTE, axum::routing::post(crate::mcp::mcp))
        .fallback(dispatch)
        .with_state(state.clone())
        // CSRF runs outside dispatch so it guards every route and can mint the
        // double-submit cookie on the way out. It asks the live app registry
        // whether a failing request is for an endpoint open to the public role,
        // which it lets through as an anonymous caller.
        .layer(axum::middleware::from_fn_with_state(
            CsrfPolicy {
                secure: config.secure_cookies,
                open_to_public: Arc::new(move |request| open_to_public(&state, request)),
            },
            csrf_middleware,
        ))
        // Strict security headers on every response (design §16). CSP is
        // `if_not_present`, not `overriding`: an application carries its own
        // `CspPolicy` (§13.2) and sets it on its own responses, and this must not
        // replace it. Everything that does not set one — the whole admin surface —
        // still gets the strict default.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            // Computed rather than static because one directive depends on the
            // base domain: the applications this admin may frame beside a
            // builder agent's chat (`admin_content_security_policy`). A header
            // that will not parse is not a reason to serve none, so the strict
            // policy is the fallback.
            HeaderValue::from_str(&admin_content_security_policy(
                config.base_domain.as_deref(),
            ))
            .unwrap_or_else(|_| HeaderValue::from_static(CONTENT_SECURITY_POLICY)),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        // `if_not_present`, like CSP: one document states its own. The builder's
        // is `same-origin`, because its canvas's image requests must say which
        // application they are for (`builder::redirect_file`).
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        // Compression for every client that asks for it with `Accept-Encoding`:
        // brotli or gzip. tower-http's default predicate already leaves alone
        // Server-Sent Events (a compressor would hold events back until its
        // buffer filled), images, gRPC and bodies under 32 bytes, and the layer
        // skips any response that is already encoded or answers a range. Zip
        // archives (backups, exports) are left alone too: they are compressed
        // already, so a second pass costs CPU and saves nothing. The reason this
        // is here at all is an application's first load: a JSON snapshot of
        // its content, sent uncompressed over a slow mobile connection, took
        // over a minute.
        .layer(
            CompressionLayer::new()
                .no_deflate()
                .no_zstd()
                .compress_when(
                    DefaultPredicate::new().and(NotForContentType::const_new("application/zip")),
                ),
        )
        // Request logging goes on **last**, which in axum is outermost: it
        // therefore sees every request — including the ones that never reach
        // `dispatch` (uploads, backups, the WebSocket upgrades, an application's
        // own routes) — and reports the status the client actually got, after
        // CSRF and the header layers have had their say. What it prints is the
        // stored log verbosity (Settings → Development): nothing below Info,
        // one line per request at Info (`sc_log::Verbosity`).
        .layer(axum::middleware::from_fn(crate::logging::log_requests));

    Ok(app)
}

/// Ceiling on a single upload. Generous enough for the assets a code framework's
/// source tree carries (images, fonts, sample data) while still bounding what one
/// request can allocate.
const MAX_UPLOAD_BYTES: usize = 256 * 1024 * 1024;

/// The operational health check. Always `200 {"status":"ok"}`; reaching it at
/// all is the signal that the server booted and is accepting requests.
async fn health() -> Response {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response()
}

/// Stream a request body straight into a file store (see the route's comment for
/// why this lives outside the typed endpoint set).
///
/// The store and destination come from the URL rather than a JSON body, because
/// there is no JSON body — the body *is* the file. `{*path}` is a greedy capture,
/// so a nested destination like `assets/img/logo.png` arrives whole.
///
/// The work itself is done by the `uploadFile` handler in the registry, not here.
/// That is deliberate: the handler already closes over the catalog and applies
/// the same access rule as every other file operation, so routing around the
/// `EndpointSet` does not also mean routing around the access model. All this
/// function adds is the plumbing dispatch would otherwise have done — session
/// lookup, the admin check, and reading the body.
async fn upload(
    State(state): State<AppState>,
    jar: CookieJar,
    AxumPath((store_name, path)): AxumPath<(String, String)>,
    body: axum::body::Body,
) -> Response {
    let session_token = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
    let user = match session_user(&state, &jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        return rejection;
    }

    let Some(handler) = state.handlers.get("uploadFile").cloned() else {
        return json_error(
            StatusCode::NOT_FOUND,
            "this server has no file-upload handler registered",
        );
    };

    // Bounded rather than unbounded: the cap is what keeps a single request from
    // exhausting memory. True streaming to disk would remove the ceiling and is
    // the next step if it ever binds; this is already far above what
    // base64-in-JSON could carry.
    let bytes = match axum::body::to_bytes(body, MAX_UPLOAD_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("upload exceeds the {MAX_UPLOAD_BYTES} byte limit"),
            );
        }
    };

    // Kept for the `error` event, which needs to say where it happened and to
    // whom; the originals move into the handler's context below.
    let (store_for_event, path_for_event) = (store_name.clone(), path.clone());
    let caller = user.clone();
    let ctx = HandlerCtx {
        raw_body: Some(bytes),
        path_params: HashMap::from([("store".to_owned(), store_name), ("path".to_owned(), path)]),
        query: Vec::new(),
        body: serde_json::Value::Null,
        user,
        // A route outside the endpoint set that serves bytes, not prose: the
        // installation default is the honest answer, and nothing here renders a
        // message for a person to read.
        locale: sc_i18n::active().default_locale().clone(),
    };
    match handler(ctx).await {
        // `false` rather than `is_native_client`: this handler never starts a
        // session, and `native` only matters for a login's cookie (`apply_session`).
        Ok(resp) => apply_response(&state, jar, session_token, false, resp).await,
        Err(e) => {
            error_out(
                &state,
                &e,
                Audience::Admin,
                "POST",
                &format!("/upload/{store_for_event}/{path_for_event}"),
                caller.as_ref(),
            )
            .await
        }
    }
}

/// Serve one store file as a download (`GET /download/{store}/{*path}`), behind
/// the same session lookup and admin check as the typed endpoints.
async fn download(
    State(state): State<AppState>,
    jar: CookieJar,
    AxumPath((store_name, path)): AxumPath<(String, String)>,
) -> Response {
    let session_token = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
    let user = match session_user(&state, &jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        return rejection;
    }
    let Some(handler) = state.handlers.get("downloadFile").cloned() else {
        return json_error(
            StatusCode::NOT_FOUND,
            "this server has no file-download handler registered",
        );
    };
    let route = format!("/download/{store_name}/{path}");
    let caller = user.clone();
    let ctx = HandlerCtx {
        raw_body: None,
        path_params: HashMap::from([("store".to_owned(), store_name), ("path".to_owned(), path)]),
        query: Vec::new(),
        body: serde_json::Value::Null,
        user,
        // As `upload`: bytes, not prose.
        locale: sc_i18n::active().default_locale().clone(),
    };
    match handler(ctx).await {
        Ok(resp) => apply_response(&state, jar, session_token, false, resp).await,
        Err(e) => error_out(&state, &e, Audience::Admin, "GET", &route, caller.as_ref()).await,
    }
}

/// Build a backup and serve it as a file (see [`BACKUP_CREATE_ROUTE`]).
///
/// The request body is JSON — the selection — and the response is a zip, which is
/// the half the endpoint model cannot describe. The work is the `createBackup`
/// handler's: it holds the catalog, it persists the selection, and it is where an
/// admin-only check has already been applied by the time the bytes exist.
async fn create_backup(State(state): State<AppState>, jar: CookieJar, body: Bytes) -> Response {
    let (user, session_token) = match admin_of(&state, &jar).await {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    let Some(handler) = state.handlers.get("createBackup").cloned() else {
        return json_error(
            StatusCode::NOT_FOUND,
            "this server has no backup handler registered",
        );
    };
    let selection = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(e) => {
                return json_error(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}"));
            }
        }
    };
    let caller = user.clone();
    let ctx = HandlerCtx {
        raw_body: None,
        path_params: HashMap::new(),
        query: Vec::new(),
        body: selection,
        user,
        // As `upload_file`: this route's response is an archive.
        locale: sc_i18n::active().default_locale().clone(),
    };
    match handler(ctx).await {
        // `false` rather than `is_native_client`: this handler never starts a
        // session, and `native` only matters for a login's cookie (`apply_session`).
        Ok(resp) => apply_response(&state, jar, session_token, false, resp).await,
        Err(e) => {
            error_out(
                &state,
                &e,
                Audience::Admin,
                "POST",
                BACKUP_CREATE_ROUTE,
                caller.as_ref(),
            )
            .await
        }
    }
}

/// Take delivery of a backup file (see [`BACKUP_UPLOAD_ROUTE`]).
///
/// The body is the zip. Nothing is restored here: the response says what the file
/// holds and hands back a token the typed `restoreBackup` names once the admin has
/// chosen from it. That is one upload rather than two — the alternative, sending
/// the file again with the choice, means a browser holding a large archive twice
/// and an admin waiting for it twice.
async fn upload_backup(State(state): State<AppState>, jar: CookieJar, body: Body) -> Response {
    let (user, session_token) = match admin_of(&state, &jar).await {
        Ok(pair) => pair,
        Err(response) => return *response,
    };
    let Some(handler) = state.handlers.get("uploadBackup").cloned() else {
        return json_error(
            StatusCode::NOT_FOUND,
            "this server has no backup handler registered",
        );
    };
    let bytes = match axum::body::to_bytes(body, MAX_UPLOAD_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the backup exceeds the {MAX_UPLOAD_BYTES} byte limit"),
            );
        }
    };
    let caller = user.clone();
    let ctx = HandlerCtx {
        raw_body: Some(bytes),
        path_params: HashMap::new(),
        query: Vec::new(),
        body: Value::Null,
        user,
        // As `upload_file`: this route takes an archive and answers a manifest.
        locale: sc_i18n::active().default_locale().clone(),
    };
    match handler(ctx).await {
        // `false` rather than `is_native_client`: this handler never starts a
        // session, and `native` only matters for a login's cookie (`apply_session`).
        Ok(resp) => apply_response(&state, jar, session_token, false, resp).await,
        Err(e) => {
            error_out(
                &state,
                &e,
                Audience::Admin,
                "POST",
                BACKUP_UPLOAD_ROUTE,
                caller.as_ref(),
            )
            .await
        }
    }
}

/// The session behind an admin-only route outside the endpoint set: the caller,
/// and the token their cookie carried (which [`apply_response`] needs to leave the
/// session where it found it).
///
/// The `Err` is the refusal to serve, ready to return — the same two answers
/// `dispatch` gives, so a route that does its own auth cannot accidentally give a
/// different one.
async fn admin_of(
    state: &AppState,
    jar: &CookieJar,
) -> std::result::Result<(Option<User>, Option<String>), Box<Response>> {
    let session_token = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
    let user = session_user(state, jar).await?;
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        return Err(Box::new(rejection));
    }
    Ok((user, session_token))
}

/// The IDE's language-server socket (design §12.1): admin-only, one process per
/// connection.
///
/// The auth check is the same one every admin surface applies, and it is the
/// *only* refusal answered with an HTTP status: a browser cannot read the body of
/// a failed WebSocket handshake, so every other reason a store cannot be
/// type-checked is carried by the close frame instead (see [`crate::lsp`]).
///
/// CSRF does not apply — this is a `GET`, and the middleware leaves safe methods
/// alone — but the same-origin story still holds: the session cookie is
/// `SameSite=Strict`, so a cross-site page's WebSocket carries no session and
/// lands on the rejection below.
async fn language_server(
    State(state): State<AppState>,
    jar: CookieJar,
    AxumPath(store): AxumPath<String>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    let user = match session_user(&state, &jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        return rejection;
    }
    language_server_upgrade(ws, state.apps.catalog(), &state.lsp_slots, store).await
}

/// The admin chat socket (§11.4): admin-only, one conversation per connection.
///
/// The auth story is the language server's, word for word — this is the *other*
/// route that hands its holder something that runs on the server, and the same
/// two facts apply: a failed handshake carries no readable body, so the auth
/// refusal is the one answered with a status; and the session cookie is
/// `SameSite=Strict`, so a cross-site page's socket carries no session and lands
/// on that refusal.
async fn agent_chat(
    State(state): State<AppState>,
    jar: CookieJar,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    let user = match session_user(&state, &jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        return rejection;
    }
    let Some(user) = user else {
        // Unreachable: `AuthRequirement::admin()` has just refused every request
        // without a user. Answered rather than unwrapped, because a run with no
        // caller is the one thing decision 5 says cannot exist.
        return json_error(StatusCode::UNAUTHORIZED, "this route requires a session");
    };
    agent_chat_upgrade(
        ws,
        state.apps.catalog(),
        state.apps.agents(),
        state.apps.evaluator(),
        state.apps.triggers().cloned(),
        user,
    )
    .await
}

/// A stream's Observe socket (TODO "Streams" §9, task 6.3): admin-only, one
/// subscription per connection.
///
/// The auth story is the chat socket's and the language server's, word for
/// word, and the reason to repeat it rather than share it is that it is the one
/// refusal that has to be an HTTP **status**: a browser cannot read the body of
/// a failed WebSocket handshake, so everything decided after the upgrade — no
/// stream support, no such stream, not running here — is a close frame instead
/// (see [`crate::observe`]). The session cookie is `SameSite=Strict`, so a
/// cross-site page's socket carries no session and lands on the rejection
/// below.
async fn stream_observe(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    jar: CookieJar,
    AxumPath(id): AxumPath<String>,
    // Optional, so a plain `GET` of this path is answered by this handler with
    // a sentence rather than by the extractor's own rejection: the path is one
    // an admin may well try in a browser tab.
    MaybeUpgrade(ws): MaybeUpgrade,
) -> Response {
    // This is a real axum route, so it wins over the fallback every
    // application's request goes through — including an app whose API is
    // mounted at `/api`, whose own socket path is spelled exactly like this
    // one. So the host is asked first, as `dispatch` asks it, and an
    // application's socket is served as the application's (TODO "Streams" §10).
    if let Resolved::App(app) = resolve_app(&state, &headers, &jar) {
        let csp = app.app.csp.header_value();
        return with_csp(app_stream_observe(&state, &app, &id, &jar, ws).await, &csp);
    }
    let user = match session_user(&state, &jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        return rejection;
    }
    // A path segment that is not a uuid is answered with a status rather than
    // an upgrade: there is no stream it could be, and the handshake has not
    // happened yet, so this is still a request whose body can be read.
    let Ok(id) = id.parse::<uuid::Uuid>() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "the stream id in the path is not a uuid",
        );
    };
    let Some(ws) = ws else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "this path is a WebSocket: connect to it with `ws:`/`wss:` rather than fetching it",
        );
    };
    stream_observe_upgrade(
        ws,
        state.apps.streams().map(sc_server_stream_supervisor),
        sc_stream::StreamId(id),
    )
    .await
}

/// `GET /api/model-instances/{id}/progress`: a fit's progress socket
/// (analytics TODO A3.3). Admin only, decided before the upgrade as the
/// Observe socket's is.
async fn fit_progress(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    jar: CookieJar,
    AxumPath(id): AxumPath<String>,
    MaybeUpgrade(ws): MaybeUpgrade,
) -> Response {
    // An application's request on a path spelled like this one is not a fit's:
    // applications have no model editor.
    if let Resolved::App(_) = resolve_app(&state, &headers, &jar) {
        return json_error(StatusCode::NOT_FOUND, "there is nothing at this path");
    }
    let user = match session_user(&state, &jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        return rejection;
    }
    let Ok(id) = id.parse::<uuid::Uuid>() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "the fit id in the path is not a uuid",
        );
    };
    let Some(catalog) = state.apps.catalog().cloned() else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "this server has no catalog, so it has no fits",
        );
    };
    let Some(ws) = ws else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "this path is a WebSocket: connect to it with `ws:`/`wss:` rather than fetching it",
        );
    };
    fit_progress_upgrade(ws, catalog, sc_model::InstanceId(id))
}

/// The supervisor inside a server's stream services — a named function rather
/// than a closure so the `Option::map` above reads as what it is.
fn sc_server_stream_supervisor(
    services: &crate::StreamServices,
) -> &std::sync::Arc<sc_stream::StreamSupervisor> {
    services.supervisor()
}

/// Group endpoints by path pattern and insert them into a `matchit` router.
fn build_matchit(endpoints: &EndpointSet) -> Result<matchit::Router<Vec<Endpoint>>> {
    // Preserve registration order while grouping same-path endpoints together
    // (e.g. GET and POST on `/api/tables`).
    let mut by_path: Vec<(String, Vec<Endpoint>)> = Vec::new();
    for ep in endpoints {
        let pattern = ep.path.pattern();
        match by_path.iter_mut().find(|(p, _)| *p == pattern) {
            Some((_, eps)) => eps.push(ep.clone()),
            None => by_path.push((pattern, vec![ep.clone()])),
        }
    }

    let mut router = matchit::Router::new();
    for (pattern, eps) in by_path {
        router
            .insert(pattern.clone(), eps)
            .map_err(|e| Error::config(format!("cannot mount route `{pattern}`: {e}")))?;
    }
    Ok(router)
}

/// The single fallback that dispatches every request: API routes via `matchit`,
/// everything else to the static bundle / bootstrap document.
// One handler for every request there is, so its arguments are the request's:
// each one is an extractor axum fills in, not a parameter a caller chose.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    State(state): State<AppState>,
    method: axum::http::Method,
    uri: Uri,
    headers: axum::http::HeaderMap,
    jar: CookieJar,
    csrf: Option<axum::Extension<crate::security::CsrfToken>>,
    anonymous: Option<axum::Extension<AnonymousCaller>>,
    // Before `body`, and it has to be: an upgrade lives in the request's
    // extensions and must be taken while the parts are still in hand.
    MaybeUpgrade(ws): MaybeUpgrade,
    body: Bytes,
) -> Response {
    // An application claims the whole of its subdomain, so this comes first: on
    // `blog.example.com` every path is the blog's, not the admin's.
    match resolve_app(&state, &headers, &jar) {
        Resolved::App(app) => {
            let csrf = csrf.map(|axum::Extension(token)| token.0);
            let anonymous = anonymous.is_some();
            return dispatch_app(
                &state, &app, method, &uri, &headers, jar, csrf, anonymous, ws, &body,
            )
            .await;
        }
        // A run's preview, asked for without that run's session: the answer is
        // the one a host that serves nothing gets, so a preview shows nobody
        // else unreviewed code (TODO §7b).
        Resolved::HiddenPreview => return json_error(StatusCode::NOT_FOUND, "not found"),
        Resolved::Admin => {}
    }

    match state.routes.at(uri.path()) {
        Ok(matched) => {
            // Decoded here, once, for every endpoint: a handler that forgot to
            // would look up `Admin%20copilot` and quietly find nothing.
            let params: HashMap<String, String> = matched
                .params
                .iter()
                .map(|(k, v)| (k.to_owned(), path_decode(v)))
                .collect();
            let endpoints = matched.value;

            let Some(api_method) = map_method(method.as_str()) else {
                return json_error(StatusCode::METHOD_NOT_ALLOWED, "unsupported method");
            };
            match endpoints.iter().find(|e| e.method == api_method) {
                Some(ep) => handle_api(&state, ep, params, &uri, &headers, jar, &body).await,
                None => json_error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method not allowed for this route",
                ),
            }
        }
        // Not an API route: the IDE or the builder under its own prefix,
        // otherwise the SPA bundle / bootstrap for navigations.
        Err(_) => {
            if method == axum::http::Method::GET || method == axum::http::Method::HEAD {
                if is_ide_path(uri.path()) {
                    serve_ide(&state, &uri, &headers, &jar).await
                } else if is_analytics_path(uri.path()) {
                    serve_analytics(&state, &uri, &headers, &jar).await
                } else if crate::builder::is_builder_path(uri.path())
                    || crate::builder::is_files_serve_path(uri.path())
                {
                    let csrf = csrf.map(|axum::Extension(token)| token.0);
                    serve_builder(&state, &uri, &headers, &jar, csrf).await
                } else {
                    serve_static(&state, &uri).await
                }
            } else {
                json_error(StatusCode::NOT_FOUND, "not found")
            }
        }
    }
}

/// Whether `request` goes to an application API endpoint open to the public
/// role — the question [`csrf_middleware`] asks of a mutating request that
/// failed its check. Only an API provider's endpoints qualify: an
/// application's own pages and everything on the admin host keep the check.
fn open_to_public(state: &AppState, request: &axum::extract::Request) -> bool {
    let jar = CookieJar::from_headers(request.headers());
    let Resolved::App(app) = resolve_app(state, request.headers(), &jar) else {
        return false;
    };
    let path = request.uri().path();
    map_method(request.method().as_str()).is_some_and(|method| {
        app.provider_for(path)
            .is_some_and(|provider| provider.open_to_public(method, path))
    })
}

/// The application a request's `Host` names, if any.
///
/// Returns an owned [`Arc`] so the live registry's read lock is released before
/// the request is served: a concurrent mount/unmount never blocks on an in-flight
/// request, and a request in flight against a since-replaced app keeps serving the
/// version it resolved.
fn resolve_app(state: &AppState, headers: &axum::http::HeaderMap, jar: &CookieJar) -> Resolved {
    let Some(label) = state.base_domain.as_ref().and_then(|base| {
        let host = headers.get(header::HOST)?.to_str().ok()?;
        subdomain_in(host, Some(base.as_str()), &state.extra_base_domains)
    }) else {
        return Resolved::Admin;
    };
    // `<label>--<subdomain>` is a preview when the label is one. Anything else
    // with a `--` in it is an ordinary subdomain.
    let preview = label.split_once("--");
    if let Some((preview, subdomain)) = preview {
        let tokens: Vec<&str> = [SESSION_COOKIE, PREVIEW_COOKIE]
            .iter()
            .filter_map(|name| jar.get(name).map(|c| c.value()))
            .collect();
        match state.apps.resolve_preview(preview, subdomain, &tokens) {
            Ok(Some(app)) => return Resolved::App(app),
            Err(()) => return Resolved::HiddenPreview,
            Ok(None) => {}
        }
    }
    match state.apps.get(label) {
        Some(app) => Resolved::App(app),
        // The shape of a preview of a served application, and not one (any
        // more): answered as a hidden one is, so an unmounted preview and a
        // label that never existed read the same.
        None if preview.is_some_and(|(_, subdomain)| state.apps.get(subdomain).is_some()) => {
            Resolved::HiddenPreview
        }
        None => Resolved::Admin,
    }
}

/// What a request's `Host` resolves to.
enum Resolved {
    /// An application, or a run's preview of one.
    App(Arc<MountedApp>),
    /// A run's preview, without that run's session.
    HiddenPreview,
    /// Not an application: the admin.
    Admin,
}

/// Serve one request against an application: its API providers first, then its
/// framework (design §13.2/§13.3).
///
/// Providers win over the framework for the paths they claim, so an app's
/// `/api/*` is its data and everything else is its UI. Both answers carry the
/// app's own CSP.
#[allow(clippy::too_many_arguments)]
async fn dispatch_app(
    state: &AppState,
    app: &MountedApp,
    method: axum::http::Method,
    uri: &Uri,
    headers: &axum::http::HeaderMap,
    jar: CookieJar,
    csrf: Option<String>,
    anonymous: bool,
    ws: Option<axum::extract::ws::WebSocketUpgrade>,
    body: &Bytes,
) -> Response {
    let csp = app.app.csp.header_value();
    let Some(api_method) = map_method(method.as_str()) else {
        return with_csp(
            json_error(StatusCode::METHOD_NOT_ALLOWED, "unsupported method"),
            &csp,
        );
    };
    let path = uri.path();

    // An observe socket, before the API providers: the path sits *beside* the
    // endpoint set (`{mount}/streams/{name}/observe`), so on an app whose API is
    // at `/api` it is under a provider's mount and would otherwise be answered
    // by the provider's "no such endpoint" (TODO "Streams" §10).
    if let Some(name) = sc_app::stream_in_path(&app.app, path) {
        return with_csp(app_stream_observe(state, app, name, &jar, ws).await, &csp);
    }

    // The app's catalogue, in the same place and for the same reason
    // (`{mount}/i18n/{locale}.json`, §16.1 D7). An application with no locales
    // never reaches the store: the guard is the first thing
    // [`app_i18n_catalog`] does, which is what D11 costs.
    if let Some(tag) = sc_app::i18n_locale_in_path(&app.app, path) {
        return with_csp(
            app_i18n_catalog(state, app, tag, &method, headers).await,
            &csp,
        );
    }

    // The app's data: an API provider that claims this path.
    if let Some(provider) = app.provider_for(path) {
        // An app is mounted only with a catalog (checked at build time), so this
        // is a server bug rather than a request problem.
        let Some(catalog) = state.apps.catalog() else {
            return with_csp(
                json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "no catalog for application APIs",
                ),
                &csp,
            );
        };

        // A request the CSRF middleware let through without a token, for an
        // endpoint open to the public role, is nobody's: its cookie is not
        // read, so the session it may carry lends it no authority.
        let session_token = jar
            .get(SESSION_COOKIE)
            .filter(|_| !anonymous)
            .map(|c| c.value().to_owned());
        let user = match &session_token {
            Some(token) => match state.sessions.user_for(token).await {
                Ok(u) => u,
                Err(e) => {
                    log_failure("session lookup failed", &e);
                    return with_csp(
                        json_error(StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed"),
                        &csp,
                    );
                }
            },
            None => None,
        };

        // The declared content type decides how the body reaches the provider:
        // JSON (or an unlabelled body, which every JSON caller before file
        // uploads existed sent) is parsed as before; anything else — a file
        // upload's bytes — is handed over raw and unparsed (§4).
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let is_json = content_type.is_empty() || content_type.starts_with("application/json");
        let mut parsed_body = Value::Null;
        let mut raw_body = None;
        if !body.is_empty() {
            if is_json {
                parsed_body = match serde_json::from_slice(body) {
                    Ok(v) => v,
                    Err(e) => {
                        return with_csp(
                            json_error(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")),
                            &csp,
                        );
                    }
                };
            } else {
                raw_body = Some(body.clone());
            }
        }

        let req = ApiRequest {
            method: api_method,
            path: path.to_owned(),
            query: parse_query(uri),
            body: parsed_body,
            raw: raw_body,
            // What an emailed password link is built from: this request's own
            // origin, and the applications served beside it.
            links: Some(sc_api::AppLinks(Arc::new(RequestLinks::new(
                state, headers,
            )))),
        };

        // The provider enforces the endpoint's auth itself (§7), so unlike the
        // admin path there is no separate check here.
        return match provider.handle(req, catalog, user.as_ref()).await {
            // A raw-bytes response — a file download (§4) — goes out as-is under
            // its own content type; there is no JSON body and no session change
            // a download could carry.
            Ok(sc_api::ApiResponse {
                raw: Some(raw),
                status,
                ..
            }) => {
                let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
                let mut out = (status, raw.bytes).into_response();
                if let Ok(ct) = HeaderValue::from_str(&raw.content_type) {
                    out.headers_mut().insert(header::CONTENT_TYPE, ct);
                }
                with_csp(out, &csp)
            }
            // A provider's response is shaped exactly like a handler's — body,
            // status, session change — so it goes out through the same
            // `apply_response`: an app's `login` sets its session cookie the very
            // same way the admin's does.
            Ok(resp) => with_csp(
                apply_response(
                    state,
                    jar,
                    session_token,
                    is_native_client(headers),
                    HandlerResponse {
                        body: resp.body,
                        status: resp.status,
                        // Nor may it change one: an endpoint open to the public
                        // role does not log anybody in or out, and a forged
                        // request is exactly the one that must not.
                        session: if anonymous {
                            sc_api::SessionAction::Keep
                        } else {
                            resp.session
                        },
                        // A provider's raw body is served above, before this
                        // point: an application's download never reaches here.
                        download: None,
                    },
                )
                .await,
                &csp,
            ),
            Err(e) => with_csp(
                error_out(
                    state,
                    &e,
                    Audience::App,
                    api_method.as_str(),
                    path,
                    user.as_ref(),
                )
                .await,
                &csp,
            ),
        };
    }

    // The app's assets: a static directory that claims this path (§13.2). It
    // sits between the providers and the framework because the framework's SPA
    // fallback answers every path, so a directory behind it would never be
    // reached.
    if let Some((dir, rest)) = app.app.static_dir_for(path) {
        return with_csp(
            serve_static_dir(state, app, dir, rest, &method, headers, &jar).await,
            &csp,
        );
    }

    // The app's UI: its framework serves the built bundle. No catalog access
    // happens here for a code framework — the app reaches data only through the
    // API above.
    let Some(catalog) = state.apps.catalog() else {
        return with_csp(
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "no catalog"),
            &csp,
        );
    };
    // What a server-rendered framework reads (TODO "Saltcorn UI" §8): who is
    // looking, the query, the body, a few headers and the app's own origin. A
    // code framework reads the method and the path and ignores the rest.
    let session_token = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
    let user = match &session_token {
        Some(token) => match state.sessions.user_for(token).await {
            Ok(user) => user,
            Err(e) => {
                log_failure("session lookup failed", &e);
                return with_csp(
                    json_error(StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed"),
                    &csp,
                );
            }
        },
        None => None,
    };
    // The locale, negotiated once for this request (§16.1, D8): an application's
    // own pages, its view runtime and anything it fires are served in one
    // language, and it is the one this response's `Content-Language` names.
    // Against the *application's* locales when it declares any: they belong to
    // the thing the admin built, not to the installation serving it.
    let settings = crate::i18n::app_settings(&app.app, &sc_i18n::active());
    let negotiated = crate::i18n::negotiate(&settings, uri, headers, &jar, user.as_ref());
    let mut req = match app_request(state, api_method, uri, headers, body, user.clone()) {
        Ok(req) => req,
        Err(rejection) => return with_csp(*rejection, &csp),
    };
    req.locale = crate::i18n::locale_or_default(&settings, negotiated.as_ref());
    // The token the CSRF middleware checked this request against, or minted for
    // it: what a rendered form carries, and what `req.csrfToken()` answers.
    req.csrf_token = csrf
        .or_else(|| jar.get(CSRF_COOKIE).map(|c| c.value().to_owned()))
        .unwrap_or_default();
    match app.framework.handle(req, catalog).await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK);
            // The same session code an API provider's response goes through, so
            // an application's rendered login sets the same cookie the same way.
            let native = is_native_client(headers);
            let jar = match apply_session(state, jar, session_token, native, resp.session).await {
                Ok(jar) => jar,
                Err(rejection) => return with_csp(*rejection, &csp),
            };
            let mut out = (status, jar, resp.body).into_response();
            if let Ok(ct) = HeaderValue::from_str(&resp.content_type) {
                out.headers_mut().insert(header::CONTENT_TYPE, ct);
            }
            for (name, value) in &resp.headers {
                if let (Ok(name), Ok(value)) = (
                    header::HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(value),
                ) {
                    out.headers_mut().append(name, value);
                }
            }
            with_csp(crate::i18n::with_language(out, negotiated.as_ref()), &csp)
        }
        Err(e) => with_csp(
            crate::i18n::with_language(
                error_out(
                    state,
                    &e,
                    Audience::App,
                    api_method.as_str(),
                    path,
                    user.as_ref(),
                )
                .await,
                negotiated.as_ref(),
            ),
            &csp,
        ),
    }
}

/// One of an application's static directories: the bytes of a file under
/// [`StaticDir::path`] in [`StaticDir::store`] (design §13.2).
///
/// A mount is **not a grant**. It says where in the URL space a store's
/// subdirectory appears; it does not say that everything under it is public.
/// Every read goes through the same [`sc_files::check_access`] the file manager
/// and the REST provider go through, as the *request's* user — so a file (or a
/// folder) closed to a guest is not served to one, and the refusal is the same
/// 404 an unknown path gets, because a 403 would confirm the file exists to
/// somebody not allowed to know.
///
/// The content type is [`sc_app::asset_content_type`], the code framework's own
/// answer, so a `.png` in a bundle and a `.png` in a static directory are served
/// identically. The ETag is over the bytes: these are images, they are requested
/// on every page load, and they do not change.
async fn serve_static_dir(
    state: &AppState,
    app: &MountedApp,
    dir: &sc_app::StaticDir,
    rest: &str,
    method: &axum::http::Method,
    headers: &axum::http::HeaderMap,
    jar: &CookieJar,
) -> Response {
    // Every refusal below is this one: a path that escapes, a store this
    // application does not have, a file that is not there, and a file the
    // viewer may not read all say the same thing to the same stranger.
    let missing = || json_error(StatusCode::NOT_FOUND, "not found");

    if method != axum::http::Method::GET && method != axum::http::Method::HEAD {
        return json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "a static directory is read, not written",
        );
    }
    let Some(mut path) = dir.resolve(rest) else {
        return missing();
    };
    // A directory is served by its `index.html`, so a site of plain files — a
    // `none` application's — has a front page at `/`.
    if rest.is_empty() || rest.ends_with('/') {
        path = match path.is_empty() {
            true => "index.html".to_owned(),
            false => format!("{path}/index.html"),
        };
    }
    // The application's declared subset is the whole truth about which stores it
    // touches (§13.2): a directory naming a store outside it serves nothing,
    // whatever the record says. `save_application` refuses to store one.
    if !app.app.can_access_file_store(&dir.store) {
        return missing();
    }
    let Some(catalog) = state.apps.catalog() else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "no catalog");
    };
    let Ok(store) = catalog.require_file_store(&dir.store.0) else {
        return missing();
    };

    let user = match jar.get(SESSION_COOKIE).map(|c| c.value().to_owned()) {
        Some(token) => match state.sessions.user_for(&token).await {
            Ok(user) => user,
            Err(e) => {
                log_failure("session lookup failed", &e);
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed");
            }
        },
        None => None,
    };
    let role = user.as_ref().map_or(sc_files::ROLE_PUBLIC, |u| u.role);

    let floor = match sc_catalog::load_file_store_by_name(catalog, &dir.store.0).await {
        Ok(def) => def.and_then(|def| def.min_role),
        Err(e) => {
            log_failure("reading a static directory's file store", &e);
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "file store unreadable");
        }
    };
    if sc_files::check_access(store.as_ref(), floor, &path, role)
        .await
        .is_err()
    {
        return missing();
    }

    let bytes = match store.read(&path).await {
        Ok(bytes) => bytes,
        Err(e) if matches!(e.repr(), Repr::NotFound(_)) => return missing(),
        Err(e) => {
            log_failure("reading a static directory's file", &e);
            return missing();
        }
    };

    let etag = crate::apps::etag_of(&bytes);
    let matched = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));

    let mut response = if matched {
        StatusCode::NOT_MODIFIED.into_response()
    } else if method == axum::http::Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        (StatusCode::OK, bytes).into_response()
    };
    let out = response.headers_mut();
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        out.insert(header::ETAG, etag);
    }
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(sc_app::asset_content_type(&path)),
    );
    // Revalidated rather than cached blind: an admin who replaces a logo must
    // not have to explain a stale one, and the ETag makes the second request
    // cheap anyway.
    out.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, must-revalidate"),
    );
    response
}

/// An application's catalogue: `GET {mount}/i18n/{locale}.json` (§16.1, D7).
///
/// Served rather than bundled, which is the decision this route exists to keep:
/// an admin who fixes a mistranslation must not have to wait for a bundler, and
/// "translate this application into Spanish" must not be a deploy. The file
/// still lives in the admin's repository (or, for a Saltcorn UI app, in
/// `_fd_translations`) — it is just read at request time instead of compiled in.
///
/// Three things make that affordable:
///
/// - **The per-mount cache.** The store is read once per locale per mount, and
///   the mount's cache is dropped when a translation is saved and is born empty
///   on a remount, so a `SIGHUP` re-reads without knowing this cache exists.
/// - **The ETag.** The catalogue is a static file to the browser, so the second
///   page load is a 304 with no body.
/// - **The locale guard.** An application with no locales answers 404 without
///   touching a file store or the database, which is what D11 promises. There is
///   no loading state to design around either: the key is the English source
///   text (D1), so the application renders correct English before the fetch
///   lands.
async fn app_i18n_catalog(
    state: &AppState,
    app: &MountedApp,
    tag: &str,
    method: &axum::http::Method,
    headers: &axum::http::HeaderMap,
) -> Response {
    if method != axum::http::Method::GET && method != axum::http::Method::HEAD {
        return json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "a catalogue is read, not written; save a translation through the admin API",
        );
    }
    // Every refusal below is the same 404, deliberately: a locale this
    // application does not serve and a locale that is not a locale are both
    // "there is nothing here", and neither is worth a sentence that tells a
    // stranger which locales an application has.
    let missing = || {
        json_error(
            StatusCode::NOT_FOUND,
            format!("this application has no `{tag}` catalogue"),
        )
    };
    // The zero-cost check, first (D11).
    if !sc_app::app_is_translated(&app.app) {
        return missing();
    }
    let Ok(locale) = sc_i18n::Locale::parse(tag) else {
        return missing();
    };
    let enabled = match sc_app::app_locales(&app.app) {
        Ok(locales) => locales,
        Err(e) => {
            log_failure("reading an application's locales", &e);
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "this application's locales are misconfigured",
            );
        }
    };
    if !enabled.iter().any(|l| l.as_str() == locale.as_str()) {
        return missing();
    }

    let served = match app.cached_catalog(locale.as_str()) {
        Some(served) => served,
        None => {
            let Some(catalog) = state.apps.catalog() else {
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "no catalog");
            };
            let store = match sc_app::app_catalog_store(&app.app) {
                Ok(store) => store,
                Err(e) => {
                    log_failure("resolving an application's catalogue store", &e);
                    return json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "this application's catalogue store is misconfigured",
                    );
                }
            };
            let messages = match store.load(catalog, &locale).await {
                Ok(Some(messages)) => messages,
                // Enabled but not yet translated is an *empty* catalogue rather
                // than a 404: the locale is one the application serves, and the
                // runtime that asked has a well-formed answer to cache. English
                // renders either way.
                Ok(None) => sc_i18n::Catalog::new(locale.clone()),
                Err(e) => {
                    log_failure("reading an application's catalogue", &e);
                    return json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "this application's catalogue could not be read",
                    );
                }
            };
            let body = match serde_json::to_vec(&messages.to_json()) {
                Ok(body) => Bytes::from(body),
                Err(e) => {
                    log_failure(
                        "serialising an application's catalogue",
                        &sc_error::Error::msg(e.to_string()),
                    );
                    return json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "this application's catalogue could not be read",
                    );
                }
            };
            let served = crate::apps::ServedCatalog::new(body);
            app.cache_catalog(locale.as_str(), served.clone());
            served
        }
    };

    let matched = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == served.etag));

    let mut response = if matched {
        StatusCode::NOT_MODIFIED.into_response()
    } else if method == axum::http::Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        (StatusCode::OK, served.body.clone()).into_response()
    };
    let out = response.headers_mut();
    if let Ok(etag) = HeaderValue::from_str(&served.etag) {
        out.insert(header::ETAG, etag);
    }
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(lang) = HeaderValue::from_str(locale.as_str()) {
        out.insert(header::CONTENT_LANGUAGE, lang);
    }
    // A catalogue changes when an admin saves one, and the ETag is what
    // notices. Caching it without revalidation would be the bundling this route
    // exists to avoid, one layer out.
    out.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, must-revalidate"),
    );
    response
}

/// An application's Observe socket: `GET {mount}/streams/{name}/observe` (TODO
/// "Streams" §10, task 8.2).
///
/// The admin socket's sibling, with two refusals the admin's does not have and
/// one it shares:
///
/// - **Not exposed** (or no such stream) is a **404**, not a 403: an app that
///   did not name the stream has nothing there, which is the same answer its
///   triggers give and for the same reason — a 403 would confirm the existence
///   of a flow this application has no business knowing about.
/// - **`min_role`** is the stream's own, and `None` means admin (§5: a flow
///   nobody has thought about the access of is not public). It is read off the
///   **stored row**, not off the running stream, so a stream this process has
///   not started is still authorised by the same number.
/// - Both, and the "is this even an upgrade" check, are answered **before** the
///   handshake, because a browser cannot read the body of a failed one. What is
///   left — no stream support, not running here — is a close frame with a
///   reason, in [`crate::observe`].
async fn app_stream_observe(
    state: &AppState,
    app: &MountedApp,
    name: &str,
    jar: &CookieJar,
    ws: Option<axum::extract::ws::WebSocketUpgrade>,
) -> Response {
    let Some(ws) = ws else {
        // The path is a socket's and the request is not one: an ordinary GET
        // here is a mistake worth naming rather than a 404 that looks like a
        // routing bug.
        return json_error(
            StatusCode::BAD_REQUEST,
            "this path is a WebSocket: connect to it with `ws:`/`wss:` rather than fetching it",
        );
    };
    // Unknown and unexposed are the same answer, deliberately.
    let not_found = || {
        json_error(
            StatusCode::NOT_FOUND,
            format!("this application does not expose a stream named `{name}`"),
        )
    };
    if !app.app.exposes_stream(name) {
        return not_found();
    }
    let Some(catalog) = state.apps.catalog() else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "no catalog");
    };
    let stream = match sc_stream::load_stream_by_name(catalog, name).await {
        Ok(Some(stream)) => stream,
        // Exposed by the app, gone from the server: the app is misconfigured
        // and the honest answer to the caller is still "there is nothing here".
        Ok(None) => return not_found(),
        Err(e) => {
            log_failure("reading a stream for an application's observe socket", &e);
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the stream",
            );
        }
    };
    let user = match jar.get(SESSION_COOKIE).map(|c| c.value().to_owned()) {
        Some(token) => match state.sessions.user_for(&token).await {
            Ok(user) => user,
            Err(e) => {
                log_failure("session lookup failed", &e);
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed");
            }
        },
        None => None,
    };
    let floor = stream.min_role.unwrap_or(sc_auth::ROLE_ADMIN);
    if let Some(rejection) = enforce_auth(&AuthRequirement::MinRole(floor), user.as_ref()) {
        return rejection;
    }
    stream_observe_by_name(
        ws,
        state.apps.streams().map(sc_server_stream_supervisor),
        name,
    )
    .await
}

/// The headers an application framework is shown (§8): what v1's patterns read,
/// and not the cookie, which is the router's.
const APP_REQUEST_HEADERS: [&str; 3] = ["referer", "x-requested-with", "accept"];

/// The [`AppRequest`] for one live request to an application's framework.
///
/// The body is read by its declared content type: a form's pairs, a JSON
/// document, or nothing. A JSON body that does not parse is refused here, as it
/// is for an API provider; any other body (an upload) is not a framework's yet.
fn app_request(
    state: &AppState,
    method: ApiMethod,
    uri: &Uri,
    headers: &axum::http::HeaderMap,
    body: &Bytes,
    user: Option<User>,
) -> std::result::Result<AppRequest, Box<Response>> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let body = if body.is_empty() {
        sc_app::RequestBody::Empty
    } else if content_type.starts_with("application/x-www-form-urlencoded") {
        sc_app::RequestBody::Form(parse_form(&String::from_utf8_lossy(body)))
    } else if content_type.starts_with("application/json") {
        match serde_json::from_slice(body) {
            Ok(value) => sc_app::RequestBody::Json(value),
            Err(e) => {
                return Err(Box::new(json_error(
                    StatusCode::BAD_REQUEST,
                    format!("invalid JSON body: {e}"),
                )));
            }
        }
    } else {
        sc_app::RequestBody::Empty
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let scheme = if state.secure_cookies {
        "https"
    } else {
        "http"
    };
    let mut req = AppRequest::new(method, uri.path());
    req.query = parse_query(uri).into_iter().collect();
    req.body = body;
    req.headers = APP_REQUEST_HEADERS
        .iter()
        .filter_map(|name| {
            let value = headers.get(*name)?.to_str().ok()?;
            Some(((*name).to_owned(), value.to_owned()))
        })
        .collect();
    req.user = user;
    req.base_url = if host.is_empty() {
        String::new()
    } else {
        format!("{scheme}://{host}")
    };
    Ok(req)
}

/// The applications an application's API request can link to
/// ([`sc_api::AppDirectory`]): its own origin, and the origin of any other
/// application this server serves, reached the way this request was — same
/// scheme, same base domain, same port.
///
/// Only a **served** subdomain has an origin, so a link is never built to a host
/// a caller made up.
struct RequestLinks {
    scheme: &'static str,
    host: String,
    base_domain: Option<Arc<String>>,
    apps: Arc<AppMounts>,
}

impl RequestLinks {
    fn new(state: &AppState, headers: &axum::http::HeaderMap) -> RequestLinks {
        RequestLinks {
            scheme: if state.secure_cookies {
                "https"
            } else {
                "http"
            },
            host: headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
            base_domain: state.base_domain.clone(),
            apps: state.apps.clone(),
        }
    }
}

impl sc_api::AppDirectory for RequestLinks {
    fn own_origin(&self) -> String {
        format!("{}://{}", self.scheme, self.host)
    }

    fn app_origin(&self, subdomain: &str) -> Option<String> {
        let base = self.base_domain.as_deref()?;
        self.apps.get(subdomain)?;
        let port = self
            .host
            .rsplit_once(':')
            .map(|(_, port)| port)
            .filter(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
        Some(match port {
            Some(port) => format!("{}://{subdomain}.{base}:{port}", self.scheme),
            None => format!("{}://{subdomain}.{base}", self.scheme),
        })
    }
}

/// Stamp an application's own CSP onto its response (design §13.2).
fn with_csp(mut resp: Response, csp: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(csp) {
        resp.headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    resp
}

/// Enforce auth, parse the request, run the handler, and apply its session
/// action — the full API request lifecycle for one matched endpoint.
async fn handle_api(
    state: &AppState,
    ep: &Endpoint,
    path_params: HashMap<String, String>,
    uri: &Uri,
    headers: &axum::http::HeaderMap,
    jar: CookieJar,
    body: &Bytes,
) -> Response {
    // Recover the authenticated user (if any) from the session cookie.
    let session_token = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
    let user = match &session_token {
        Some(token) => match state.sessions.user_for(token).await {
            Ok(u) => u,
            Err(e) => {
                log_failure("session lookup failed", &e);
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed");
            }
        },
        None => None,
    };

    // Enforce the endpoint's authorization requirement (design §7).
    if let Some(rejection) = enforce_auth(&ep.auth, user.as_ref()) {
        return rejection;
    }

    // Parse the JSON body (empty body → null).
    let parsed_body = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                return json_error(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}"));
            }
        }
    };

    // Resolve the handler; unimplemented / non-Rust handlers are 501.
    let handler = match &ep.handler {
        HandlerRef::Named(name) => match state.handlers.get(name) {
            Some(h) => h.clone(),
            None => return json_error(StatusCode::NOT_IMPLEMENTED, "handler not implemented"),
        },
        HandlerRef::GuestCode { .. } | HandlerRef::Custom(_) => {
            return json_error(
                StatusCode::NOT_IMPLEMENTED,
                "custom handlers not yet supported",
            );
        }
    };

    // The locale, negotiated once for this request now that the user is known
    // (§16.1, D8), and `None` on a monolingual installation — which is what makes
    // this cost nothing there (D11).
    let settings = sc_i18n::active();
    let negotiated = crate::i18n::negotiate(&settings, uri, headers, &jar, user.as_ref());
    let locale = crate::i18n::locale_or_default(&settings, negotiated.as_ref());

    // As above: the event reports the route and the caller, and both move into
    // the handler's context.
    let caller = user.clone();
    let ctx = HandlerCtx {
        raw_body: None,
        path_params,
        query: parse_query(uri),
        body: parsed_body,
        user,
        locale,
    };

    // Both arms carry the language headers: a refusal an admin reads is text in
    // a language too, and a cache in front of this must vary on the same things
    // whether the answer was a 200 or a 403.
    let out = match handler(ctx).await {
        Ok(resp) => {
            apply_response(state, jar, session_token, is_native_client(headers), resp).await
        }
        Err(e) => {
            error_out(
                state,
                &e,
                Audience::Admin,
                ep.method.as_str(),
                uri.path(),
                caller.as_ref(),
            )
            .await
        }
    };
    crate::i18n::with_language(out, negotiated.as_ref())
}

/// Check a user against an [`AuthRequirement`]. Returns `Some(rejection)` when
/// the request is not authorized, `None` when it may proceed.
fn enforce_auth(auth: &AuthRequirement, user: Option<&User>) -> Option<Response> {
    // A caller who passes is answered before the locale is asked for: the
    // overwhelmingly common call costs what it always did.
    if auth.admits(user) {
        return None;
    }
    // Two sentences a person reads, in the language that person reads (§16.1,
    // D5). The locale is the signed-in user's, which is all this function is
    // given and all a refusal needs — see `i18n::locale_for_user`.
    let locale = crate::i18n::locale_for_user(user);
    Some(match user {
        None => json_error(
            StatusCode::UNAUTHORIZED,
            t!(locale, "authentication required"),
        ),
        Some(_) => json_error(StatusCode::FORBIDDEN, t!(locale, "insufficient privilege")),
    })
}

/// Turn a [`HandlerResponse`] into an HTTP response, applying its session action
/// (start/end) to the cookie jar and the store.
///
/// This is also where the **`login` event** is raised (§10.2), because it is the
/// one place a session actually starts: the admin API's login and an
/// application's own both come through here, so a trigger that records logins
/// sees both without either handler knowing triggers exist.
async fn apply_response(
    state: &AppState,
    jar: CookieJar,
    session_token: Option<String>,
    native: bool,
    resp: HandlerResponse,
) -> Response {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK);
    let jar = match apply_session(state, jar, session_token, native, resp.session).await {
        Ok(jar) => jar,
        Err(rejection) => return *rejection,
    };
    // A response that *is* a file: the bytes under their own content type, named
    // so a browser's save dialog offers the right thing. There is no JSON body to
    // send alongside them, which is why this is a separate arm rather than a
    // header on the one below.
    if let Some(file) = resp.download {
        let mut out = (status, jar, file.bytes).into_response();
        if let Ok(value) = HeaderValue::from_str(&file.content_type) {
            out.headers_mut().insert(header::CONTENT_TYPE, value);
        }
        // The filename is server-built (a timestamp and the host's own name), so
        // it needs no escaping beyond the quotes — but it is still checked rather
        // than trusted, because a header value that will not parse must not take
        // the download with it.
        // A download with no name is not for saving (a map's vector tile), so
        // it is not marked as an attachment.
        if !file.filename.is_empty()
            && let Ok(value) =
                HeaderValue::from_str(&format!("attachment; filename=\"{}\"", file.filename))
        {
            out.headers_mut().insert(header::CONTENT_DISPOSITION, value);
        }
        return out;
    }
    (status, jar, Json(resp.body)).into_response()
}

/// Apply a [`SessionAction`] to the cookie jar and the session store: the half of
/// [`apply_response`] an application framework's response goes through too, so
/// there is one session story (TODO "Saltcorn UI" §8).
///
/// `native` is [`is_native_client`] of the request: a session it starts gets a
/// cookie that lasts as long as the session, where a browser's lasts until the
/// browser closes, as it always has.
///
/// `Err` is the response to send instead, when a session could not be started.
async fn apply_session(
    state: &AppState,
    jar: CookieJar,
    session_token: Option<String>,
    native: bool,
    session: SessionAction,
) -> std::result::Result<CookieJar, Box<Response>> {
    Ok(match session {
        SessionAction::Keep => jar,
        SessionAction::Start(user) => match state.sessions.login(user.clone()).await {
            Ok(token) => {
                // The session the request arrived with is replaced, not joined:
                // the cookie is about to name the new one, so leaving the old one
                // live would leave a credential nobody can present and nobody can
                // withdraw. That is what makes `becomeUser` a swap — the admin
                // session that authorised it does not survive behind the session
                // it turned into.
                if let Some(previous) = &session_token {
                    let _ = state.sessions.logout(previous).await;
                }
                // After the session exists, not before: an event that says
                // someone logged in must not fire for a login that then failed.
                fire_login(state, &user).await;
                let mut cookie = build_cookie(SESSION_COOKIE, token, true, state.secure_cookies);
                if native {
                    let ttl = state.sessions.ttl().num_seconds();
                    cookie.set_max_age(time::Duration::seconds(ttl));
                }
                jar.add(cookie)
            }
            Err(e) => {
                log_failure("could not start session", &e);
                return Err(Box::new(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not start session",
                )));
            }
        },
        SessionAction::End => {
            if let Some(token) = &session_token {
                let _ = state.sessions.logout(token).await;
            }
            jar.remove(Cookie::build((SESSION_COOKIE, "")).path("/").build())
        }
        // Somebody else's sessions (or, if the admin named themselves, their own
        // — in which case the cookie they still hold simply stops resolving).
        // The jar is untouched either way: this response is about an account, not
        // about this request's session.
        SessionAction::EndUser(user_id) => {
            if let Err(e) = state.sessions.end_user_sessions(user_id).await {
                log_failure("could not end the user's sessions", &e);
            }
            jar
        }
        SessionAction::EndAll => {
            if let Err(e) = state.sessions.end_all_sessions().await {
                log_failure("could not end every session", &e);
            }
            jar.remove(Cookie::build((SESSION_COOKIE, "")).path("/").build())
        }
    })
}

/// Whether a path belongs to the file-store IDE (design §12.1).
///
/// `/ide` and `/ide/` are both the IDE itself; everything else under the prefix
/// is one of its assets. A path that merely *starts* with the letters —
/// `/ideas` — is not the IDE's, hence the boundary check.
fn is_ide_path(path: &str) -> bool {
    path.strip_prefix(IDE_PREFIX)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Serve the file-store IDE: its bundle, admin-only, under its own CSP.
///
/// Three things distinguish this from [`serve_static`], and each is a §12.1
/// decision rather than an implementation detail:
///
/// 1. **It requires an admin session.** The SPA bundle is public because the login
///    screen is *in* it; the IDE is reached only from an admin UI the caller must
///    already have logged into, so there is no reason to serve twelve megabytes of
///    editor to an anonymous request. A navigation without a session is redirected
///    to the admin UI (where logging in is possible); anything else — an asset
///    fetch whose session expired — gets the ordinary auth rejection.
/// 2. **It has its own CSP.** [`IDE_CONTENT_SECURITY_POLICY`] is set on the
///    response, which the `if_not_present` layer then leaves alone, so relaxing the
///    policy for the workbench does not relax it for the admin UI.
/// 3. **It falls back to its own bootstrap document**, not the SPA's.
async fn serve_ide(
    state: &AppState,
    uri: &Uri,
    headers: &axum::http::HeaderMap,
    jar: &CookieJar,
) -> Response {
    let user = match session_user(state, jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        // A browser navigation gets sent somewhere it can act on the problem; a
        // fetch gets the JSON rejection it can report.
        if accepts_html(headers) {
            return Redirect::to("/").into_response();
        }
        return rejection;
    }

    // `/ide/assets/main-a1b2c3.js` is `assets/main-a1b2c3.js` within the bundle,
    // and `/ide` or `/ide/` is its document.
    let rest = uri
        .path()
        .strip_prefix(IDE_PREFIX)
        .filter(|rest| !rest.is_empty())
        .unwrap_or("/");
    let mut response = None;
    if let Some(dir) = &state.ide_dir {
        response = serve_file(dir.as_ref().as_path(), rest).await;
    }
    // Nothing there: a 404, for the document as much as for an asset. There is no
    // fallback document, and that is the point — the SPA has one so a client-routed
    // deep link still loads the bundle, while the IDE has no client-side routes to
    // deep-link into (a store is a query parameter, §12.1). A document served in
    // answer to a request for one of the bundle's modules is HTML where the browser
    // expected a module: it refuses it on its MIME type and renders a blank page, so
    // the fallback would hide the very thing it was meant to explain.
    let mut response = response.unwrap_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "the file-store IDE bundle is not built (run `npm ci && npm run build` in ui/ide)",
        )
    });
    set_cache_control(&mut response, rest);
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(IDE_CONTENT_SECURITY_POLICY),
    );
    response
}

/// Whether a path belongs to the Analytics UI — `/analytics`, `/analytics/`
/// or anything under it, and not `/analyticsfoo`.
fn is_analytics_path(path: &str) -> bool {
    path.strip_prefix(ANALYTICS_PREFIX)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Serve the Analytics UI (analytics TODO A1.14): its bundle, admin-only, under
/// its own CSP — [`serve_ide`]'s three decisions, for its reasons.
///
/// The bundle routes on the URL's hash (`/analytics/#/w/…`), so the document is
/// only ever `/analytics/`; anything else under the prefix is an asset or a
/// 404. The session is the admin UI's own cookie, so a signed-in admin is
/// signed in here; a navigation without a session goes to the admin UI to sign
/// in, and a signed-in user who is not an admin gets the refusal.
async fn serve_analytics(
    state: &AppState,
    uri: &Uri,
    headers: &axum::http::HeaderMap,
    jar: &CookieJar,
) -> Response {
    let user = match session_user(state, jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        if user.is_none() && accepts_html(headers) {
            return Redirect::to("/").into_response();
        }
        return rejection;
    }
    let rest = uri
        .path()
        .strip_prefix(ANALYTICS_PREFIX)
        .filter(|rest| !rest.is_empty())
        .unwrap_or("/");
    let mut response = None;
    if let Some(dir) = &state.analytics_dir {
        response = serve_file(dir.as_ref().as_path(), rest).await;
    }
    let mut response = response.unwrap_or_else(|| {
        json_error(
            StatusCode::NOT_FOUND,
            "the Analytics UI bundle is not built (run `npm ci && npm run build` in ui/analytics)",
        )
    });
    set_cache_control(&mut response, rest);
    let policy = crate::security::analytics_content_security_policy(&map_hosts(state).await);
    let policy = HeaderValue::from_str(&policy)
        .unwrap_or_else(|_| HeaderValue::from_static(ANALYTICS_CONTENT_SECURITY_POLICY));
    response
        .headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, policy);
    response
}

/// The origins the Analytics UI's maps may load a base map from (analytics
/// TODO A5.6): Settings → Maps, read per response as the MCP switches are, so a
/// changed base map is served on the next page load rather than after a
/// restart. A server without a catalog, or one whose settings do not read,
/// uses the default base map's.
async fn map_hosts(state: &AppState) -> Vec<String> {
    let settings = match state.apps.catalog() {
        Some(catalog) => sc_config::map_settings(catalog)
            .await
            .unwrap_or_else(|e| {
                sc_log::log_warn!(
                    "the map settings do not read, so the default base map is used: {e}"
                );
                sc_config::MapSettings::default()
            }),
        None => sc_config::MapSettings::default(),
    };
    settings.hosts()
}

/// Serve the builder (TODO "The builder" §2): its documents and assets under
/// [`BUILDER_PREFIX`](crate::builder::BUILDER_PREFIX), and `/files/serve/*` for
/// its canvas. Admin-only, for the IDE's reasons: a navigation without a session
/// goes to the admin UI, anything else gets the auth rejection.
async fn serve_builder(
    state: &AppState,
    uri: &Uri,
    headers: &axum::http::HeaderMap,
    jar: &CookieJar,
    csrf: Option<String>,
) -> Response {
    let user = match session_user(state, jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if let Some(rejection) = enforce_auth(&AuthRequirement::admin(), user.as_ref()) {
        if accepts_html(headers) {
            return Redirect::to("/").into_response();
        }
        return rejection;
    }
    let Some(user) = user else {
        return json_error(StatusCode::UNAUTHORIZED, "not signed in");
    };
    let env = crate::builder::BuilderEnv {
        bundle: state.builder_dir.as_deref().map(PathBuf::as_path),
        apps: &state.apps,
        base_domain: state.base_domain.as_deref().map(String::as_str),
        secure: state.secure_cookies,
    };
    if crate::builder::is_files_serve_path(uri.path()) {
        return crate::builder::redirect_file(&env, uri, headers).await;
    }
    crate::builder::serve_builder(&env, uri, headers, &user, csrf.as_deref().unwrap_or("")).await
}

/// Whether a request is a browser navigation rather than a programmatic fetch.
fn accepts_html(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

/// The user a request's session cookie names, or the response to send instead.
///
/// `Ok(None)` is an anonymous request — which is not by itself an error, since
/// what anonymity costs depends on what is being asked for; `Err` is a session
/// store that failed, which no caller can do anything about. The error is boxed
/// because a `Response` is large and this is the rare path.
async fn session_user(
    state: &AppState,
    jar: &CookieJar,
) -> std::result::Result<Option<User>, Box<Response>> {
    let Some(token) = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned()) else {
        return Ok(None);
    };
    state.sessions.user_for(&token).await.map_err(|e| {
        log_failure("session lookup failed", &e);
        Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "session lookup failed",
        ))
    })
}

/// Serve a file from the static bundle, falling back to the bundle's own
/// `index.html` (history fallback) when there is no matching file.
///
/// The document is the bundle's, not a constant here, because it is the only
/// thing that knows the hashed names of the assets it links. A client-routed
/// deep link therefore boots the *same* build a hit on `/` does, and the two
/// cannot drift apart across a rebuild.
async fn serve_static(state: &AppState, uri: &Uri) -> Response {
    if let Some(dir) = &state.static_dir {
        let dir = dir.as_ref().as_path();
        if let Some(mut response) = serve_file(dir, uri.path()).await {
            set_cache_control(&mut response, uri.path());
            return response;
        }
        if let Some(mut response) = serve_file(dir, "/index.html").await {
            set_cache_control(&mut response, "/index.html");
            return response;
        }
    }
    let mut response = (StatusCode::OK, Html(BOOTSTRAP_HTML)).into_response();
    set_cache_control(&mut response, "/index.html");
    response
}

/// One file out of a built bundle, or `None` if the bundle has no such file.
pub(crate) async fn serve_file(dir: &std::path::Path, path: &str) -> Option<Response> {
    let request = Request::builder().uri(path).body(Body::empty()).ok()?;
    // `ServeDir`'s error type is `Infallible`, so a match (not `if let`) keeps
    // the compiler from flagging an irrefutable pattern.
    match ServeDir::new(dir).oneshot(request).await {
        Ok(response) if response.status() != StatusCode::NOT_FOUND => Some(response.map(Body::new)),
        _ => None,
    }
}

/// Tell the browser how long it may keep what it was just given.
///
/// This is the half of content-hashed filenames that does the work: without it
/// a rebuilt bundle is a rebuilt bundle the browser never asks for, and an
/// admin reloads a page that is still running last week's build.
fn set_cache_control(response: &mut Response, path: &str) {
    let value = if is_hashed_asset(path) {
        IMMUTABLE_CACHE_CONTROL
    } else {
        REVALIDATE_CACHE_CONTROL
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
}

/// Map an HTTP method token to the endpoint model's [`ApiMethod`].
fn map_method(method: &str) -> Option<ApiMethod> {
    match method {
        "GET" => Some(ApiMethod::Get),
        "POST" => Some(ApiMethod::Post),
        "PUT" => Some(ApiMethod::Put),
        "PATCH" => Some(ApiMethod::Patch),
        "DELETE" => Some(ApiMethod::Delete),
        _ => None,
    }
}

/// Map a domain error to an HTTP status.
///
/// The specific request-level variants get their conventional codes; everything
/// else is decided by the §16 [`ErrorKind`] split, so an **Application** error
/// (bad configuration or app code the admin must fix — e.g. a failed build,
/// carrying the bundler's diagnostics) is a client-fixable `422`, while a
/// **System** error (a bug or infrastructure failure to report) is a `500`.
fn error_status(err: &Error) -> StatusCode {
    match err.repr() {
        Repr::NotFound(_) => StatusCode::NOT_FOUND,
        Repr::Invalid(_) => StatusCode::BAD_REQUEST,
        Repr::Auth(_) => StatusCode::UNAUTHORIZED,
        _ => match err.kind() {
            ErrorKind::Application => StatusCode::UNPROCESSABLE_ENTITY,
            ErrorKind::System => StatusCode::INTERNAL_SERVER_ERROR,
        },
    }
}

/// Parse a query string into key/value pairs, **in order and with duplicates
/// kept**.
///
/// Both properties are load-bearing rather than incidental: the REST read syntax
/// spells a range as two values under one key
/// (`?published=gte.2020&published=lt.2024`), so a map here would silently drop
/// one of a caller's filters — and a dropped filter is rows they did not ask
/// for. Values are percent-decoded (and `+` read as a space, as
/// `application/x-www-form-urlencoded` and `URLSearchParams` write it), so what
/// a handler reads is what the caller wrote.
fn parse_query(uri: &Uri) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(query) = uri.query() {
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let mut kv = pair.splitn(2, '=');
            let key = form_decode(kv.next().unwrap_or(""));
            let value = form_decode(kv.next().unwrap_or(""));
            out.push((key, value));
        }
    }
    out
}

/// Decode one form-urlencoded component: `+` is a space, `%XX` is a byte.
///
/// A malformed escape (`%zz`, or a `%` at the end) is left as written rather
/// than rejected: it is one character of one query parameter, and the endpoint
/// that reads it is in a better position to say what is wrong with it than a
/// parser that knows only that a `%` was not followed by two hex digits.
/// The pairs of a form-encoded body, decoded as a query string's are: order and
/// repeats kept.
pub(crate) fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let mut kv = pair.splitn(2, '=');
            (
                form_decode(kv.next().unwrap_or("")),
                form_decode(kv.next().unwrap_or("")),
            )
        })
        .collect()
}

fn form_decode(s: &str) -> String {
    percent_decode(s, true)
}

/// One path segment, percent-decoded. `matchit` hands a parameter over exactly
/// as it was in the URL, so a name with a space in it — v1's `List Books` or an
/// agent called `Admin copilot` — arrives as `List%20Books`. A `+` in a path is
/// a plus. Applied to every API path parameter before the handler sees it.
pub(crate) fn path_decode(s: &str) -> String {
    percent_decode(s, false)
}

fn percent_decode(s: &str, plus_is_space: bool) -> String {
    if !s.contains('%') && !(plus_is_space && s.contains('+')) {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi << 4 | lo);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // A percent escape can carry any byte, including an invalid UTF-8 sequence;
    // the lossy conversion keeps the rest of the value rather than losing it.
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of one hex digit, or `None` if it is not one.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// A JSON error body with the given status.
fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

/// Log a domain [`Error`] at the HTTP boundary, then map it to a JSON response.
///
/// Principle 5 — no silent failures: an operator watching the process must see
/// *why* a request failed, not just the terse body the client receives. Every
/// error is printed with its full source chain ([`Error::chain`]) so a wrapped
/// driver error (e.g. the real SQL error behind `tokio_postgres`'s `"db error"`)
/// is visible on the console. System errors (`500`) are the ones that most need
/// eyes, so they are flagged accordingly.
fn error_response(err: &Error, audience: Audience) -> Response {
    let status = error_status(err);
    let label = if status == StatusCode::INTERNAL_SERVER_ERROR {
        "internal error"
    } else {
        "request error"
    };
    // The log always gets everything, including the failing line.
    eprintln!("feldspar: {label}: {}", err.chain());
    let message = match audience {
        // An admin needs the cause. `Display` on a context error renders only the
        // outermost layer — "connecting file store `docs`" with no hint that the
        // directory is missing — which is a message that says something failed
        // and nothing about what to do, on the one screen whose job is to say
        // what to do.
        Audience::Admin => err.causes(),
        // An application's callers are its ordinary users, not operators. The
        // outermost message is the part deliberately written to be shown; the
        // causes below it are internals — SQL, paths, driver text — and belong
        // in the log, which already has them.
        Audience::App => err.to_string(),
    };
    json_error(status, message)
}

/// Raise the **`error` event** (§16) for an error that is about to become a
/// response, then map it to that response.
///
/// Every `Error` that reaches a client goes through here, and only those: a 404
/// for an unrouted path or a 401 from the auth gate is a *rejection*, not a
/// failure, and firing an alerting trigger for every probe of a wrong URL would
/// make the event useless for the thing it is for.
///
/// The event never changes the response and never fails the request — the caller
/// is already being told something went wrong, and a misconfigured trigger must
/// not turn that into something worse. Re-entrancy is guarded inside the
/// dispatcher, so an error raised while handling this one does not fire another.
async fn error_out(
    state: &AppState,
    err: &Error,
    audience: Audience,
    method: &str,
    path: &str,
    user: Option<&User>,
) -> Response {
    if let (Some(triggers), Some(catalog)) = (state.apps.triggers(), state.apps.catalog()) {
        let caller = sc_api::caller_context(user);
        let event = sc_action::Event::error(err.kind(), err.to_string(), method, path)
            .caller(caller.role, caller.user);
        triggers.fire(catalog, &event).await;
    }
    error_response(err, audience)
}

/// Raise the **`login` event** for a session that has just started.
///
/// The user object is built exactly as every other caller's is
/// ([`sc_api::caller_context`]), so `user.email` means the same thing in a login
/// trigger's action as it does in an insert trigger's.
async fn fire_login(state: &AppState, user: &User) {
    let (Some(triggers), Some(catalog)) = (state.apps.triggers(), state.apps.catalog()) else {
        return;
    };
    let caller = sc_api::caller_context(Some(user));
    let event = sc_action::Event::login(caller.role, caller.user.unwrap_or(Value::Null));
    triggers.fire(catalog, &event).await;
}

/// Who will read an error message, which decides how much of it to send.
///
/// This is a trust boundary, not a formatting preference: the same
/// [`error_response`] serves the admin API and every application's API, and the
/// two have different readers. Making it an explicit argument rather than a
/// default means adding a route forces the question to be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Audience {
    /// The admin API — an operator, who needs the whole causal chain.
    Admin,
    /// An application's own API — its end users, who get the top-level message.
    App,
}

/// Log a discarded lower-level failure at a call site that only knows "it broke"
/// (no [`Error`] value to forward). Keeps those paths from failing silently.
fn log_failure(context: &str, err: &(dyn std::error::Error + 'static)) {
    eprintln!("feldspar: {context}: {}", sc_error::format_chain(err));
}

/// The header the SPA must echo the CSRF cookie in (re-exported for callers/tests).
pub const CSRF_REQUEST_HEADER: &str = CSRF_HEADER;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_json(resp: Response) -> Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("body is JSON")
    }

    #[tokio::test]
    async fn system_error_maps_to_500_and_reports_the_message() {
        // A database failure is a System error (§16): the client gets a 500 and
        // the top-level message in the body. (The full cause chain goes to the
        // console via `error_response`'s log line.)
        let err = Error::database(
            "query failed: db error\n  caused by: relation \"apps\" does not exist",
        );
        let resp = error_response(&err, Audience::App);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(
            body["error"],
            Value::String(err.to_string()),
            "client body carries the error's Display"
        );
    }

    /// The audience split. An admin gets the cause; an application's users get
    /// only the top-level message.
    #[tokio::test]
    async fn an_admin_sees_the_cause_and_an_app_user_does_not() {
        use sc_error::Context;

        // The shape that motivated this: a context layer whose own message says
        // nothing useful, wrapping the one that does.
        let inner: std::result::Result<(), std::io::Error> = Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory",
        ));
        let err = inner.context("connecting file store `docs`").unwrap_err();

        let admin = body_json(error_response(&err, Audience::Admin)).await;
        let admin_text = admin["error"].as_str().unwrap();
        assert!(admin_text.contains("connecting file store"), "{admin_text}");
        assert!(
            admin_text.contains("No such file or directory"),
            "an admin must be told what actually went wrong: {admin_text}"
        );

        let app = body_json(error_response(&err, Audience::App)).await;
        let app_text = app["error"].as_str().unwrap();
        assert!(app_text.contains("connecting file store"), "{app_text}");
        assert!(
            !app_text.contains("No such file or directory"),
            "an app's users must not be shown internals: {app_text}"
        );
    }

    #[tokio::test]
    async fn application_error_maps_to_422() {
        // A bad app config is the admin's to fix — a client-fixable 422, not 500.
        let resp = error_response(&Error::config("bad framework config"), Audience::Admin);
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn request_level_errors_keep_their_conventional_codes() {
        assert_eq!(
            error_response(&Error::not_found("app"), Audience::Admin).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            error_response(&Error::auth("nope"), Audience::Admin).status(),
            StatusCode::UNAUTHORIZED
        );
    }

    /// A repeated key is how the REST read syntax spells a range, so both values
    /// must arrive — and in the order the caller wrote them. A map here would
    /// keep one and drop the other, which is a filter the caller asked for and
    /// did not get.
    #[test]
    fn parse_query_keeps_order_and_duplicates() {
        let uri: Uri = "/api/books?published=gte.2020&select=title&published=lt.2024"
            .parse()
            .unwrap();
        assert_eq!(
            parse_query(&uri),
            vec![
                ("published".to_owned(), "gte.2020".to_owned()),
                ("select".to_owned(), "title".to_owned()),
                ("published".to_owned(), "lt.2024".to_owned()),
            ]
        );
    }

    /// What `URLSearchParams` (and so the generated client) writes must come
    /// back as what the caller passed in: `&`, `=`, `+`, spaces and non-ASCII
    /// text all survive the round trip.
    #[test]
    fn parse_query_decodes_what_the_generated_client_encodes() {
        // `new URLSearchParams([["title","eq.rock & roll = 1+1 ☕"]]).toString()`
        let uri: Uri = "/api/books?title=eq.rock+%26+roll+%3D+1%2B1+%E2%98%95"
            .parse()
            .unwrap();
        assert_eq!(
            parse_query(&uri),
            vec![("title".to_owned(), "eq.rock & roll = 1+1 ☕".to_owned())]
        );
    }

    /// A `=` inside a value is part of the value (only the first splits), an
    /// empty value is empty rather than absent, and a malformed escape is left
    /// as written for the endpoint to complain about.
    #[test]
    fn parse_query_handles_the_awkward_edges() {
        let uri: Uri = "/api/books?filter=a%3Db=c&empty=&odd=100%25&trailing=%"
            .parse()
            .unwrap();
        assert_eq!(
            parse_query(&uri),
            vec![
                ("filter".to_owned(), "a=b=c".to_owned()),
                ("empty".to_owned(), String::new()),
                ("odd".to_owned(), "100%".to_owned()),
                ("trailing".to_owned(), "%".to_owned()),
            ]
        );
    }
}

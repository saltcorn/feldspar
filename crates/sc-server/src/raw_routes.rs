//! Every route outside the typed endpoint set, with how it authenticates
//! (security review 2026-11 §8, E4; TODO-type-hardening H6.1).
//!
//! The typed endpoints declare who may call them in their type
//! ([`sc_api::handler_ids`]). The routes here cannot be typed endpoints — a
//! body that is a file, a WebSocket upgrade, JSON-RPC, a redirect and a
//! cookie — so each authenticates in its handler's signature or body: an
//! [`AdminSession`](crate::AdminSession) (`Session<AdminScope>`), an
//! [`OwnSession`](crate::session::OwnSession), a bearer token. [`RAW_ROUTES`]
//! is the one list of them, each with its [`RouteAuth`], and the router
//! mounts **only** what it lists ([`mount`]); `tests/raw_routes.rs` checks the
//! list against a pinned copy (so a new route has to say how it
//! authenticates), checks that no other `.route(` is added to the router, and
//! sends each route an anonymous request to see that the ones that need a
//! caller refuse it.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{MethodFilter, MethodRouter, any, get, on, post};
use sc_backup::{BACKUP_CREATE_ROUTE, BACKUP_UPLOAD_ROUTE};

/// Where a browser hands over a structure zip to be deployed.
pub(crate) const STRUCTURE_ZIP_UPLOAD_ROUTE: &str = "/source-control/upload";

use crate::router::AppState;

/// How a raw route decides who is calling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteAuth {
    /// Nobody needs to be anyone: the health probe, the identity providers'
    /// list and the start of a sign-in.
    Public,
    /// An administrator's session on the admin host
    /// ([`AdminSession`](crate::AdminSession), `Session<AdminScope>`) in the
    /// handler's signature: anybody else is refused before the handler runs.
    AdminSession,
    /// The administration MCP server's bearer token, and nothing else: a
    /// session cookie is ignored.
    McpToken,
    /// An open IDE terminal's session token.
    TerminalToken,
    /// A build host's bearer token.
    BuildHostToken,
    /// A single-use token minted on the admin UI's old host.
    HandoffToken,
    /// An identity provider's answer: the flow's `state`, its binding cookie
    /// and (for completion) its single-use token.
    OAuthFlow,
    /// The session the host itself issued
    /// ([`OwnSession`](crate::session::OwnSession)) in the handler's
    /// signature; anybody else is told they are not signed in.
    OwnSession,
    /// The host's own session (and the password again), or a second-factor
    /// challenge: an account's own second-factor setup.
    SessionOrChallenge,
    /// A second-factor challenge: the cookie a sign-in is given in place of a
    /// session, spent with a right code.
    Challenge,
    /// A module's webhook: whatever the provider sends, which the module
    /// verifies (a signature, a shared secret).
    ModuleVerifies,
    /// A v1 plugin route: the caller resolved with the admin session's scope
    /// check, then held to the route's own role.
    ScopedSession,
    /// A module's consent flow: the session the host reads
    /// ([`HostSession`](crate::session::HostSession)), which must be the user
    /// whose call began it.
    HostSession,
}

impl RouteAuth {
    /// Whether an anonymous request is refused before anything is done: the
    /// kinds that need a caller in hand.
    pub fn refuses_anonymous(self) -> bool {
        matches!(
            self,
            RouteAuth::AdminSession
                | RouteAuth::McpToken
                | RouteAuth::TerminalToken
                | RouteAuth::BuildHostToken
                | RouteAuth::OwnSession
                | RouteAuth::SessionOrChallenge
        )
    }
}

/// One raw route.
pub struct RawRoute {
    /// The path pattern, as axum matches it.
    pub path: &'static str,
    /// The methods it answers (`*` for any).
    pub methods: &'static [&'static str],
    /// How it authenticates.
    pub auth: RouteAuth,
    /// The handlers, built when the router is.
    route: fn() -> MethodRouter<AppState>,
}

const GET: &[&str] = &["GET"];
const POST: &[&str] = &["POST"];
const GET_POST: &[&str] = &["GET", "POST"];
const ANY: &[&str] = &["*"];

/// A module's webhook, or a v1 plugin route: GET or POST, with the webhook
/// body limit (an unauthenticated caller is held to 1 MiB).
fn module_route<H, T>(handler: H) -> MethodRouter<AppState>
where
    H: axum::handler::Handler<T, AppState>,
    T: 'static,
{
    on(MethodFilter::GET.or(MethodFilter::POST), handler)
        .layer(DefaultBodyLimit::max(sc_module::WEBHOOK_MAX_BODY))
}

/// Every route outside the typed endpoint set. [`mount`] adds exactly these.
pub const RAW_ROUTES: &[RawRoute] = &[
    // Operational health check: a fixed, unauthenticated route (outside the
    // typed API) so a CLI smoke test, load balancer, or orchestrator can
    // confirm the process is up. It takes precedence over the SPA fallback.
    RawRoute {
        path: "/health",
        methods: GET,
        auth: RouteAuth::Public,
        route: || get(crate::router::health),
    },
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
    RawRoute {
        path: "/upload/{store}/{*path}",
        methods: POST,
        auth: RouteAuth::AdminSession,
        route: || post(crate::router::upload),
    },
    // …and its mirror: a store file as itself, for the file manager's
    // Download. `readFile` answers base64 inside JSON, which a browser has
    // to hold twice and decode by hand — fine for a source file, hopeless
    // for a 100 MB APK. A plain `GET`, so a link downloads it; it reads
    // nothing it does not check access to, as `readFile` does.
    RawRoute {
        path: "/download/{store}/{*path}",
        methods: GET,
        auth: RouteAuth::AdminSession,
        route: || get(crate::router::download),
    },
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
    RawRoute {
        path: BACKUP_CREATE_ROUTE,
        methods: POST,
        auth: RouteAuth::AdminSession,
        route: || post(crate::router::create_backup),
    },
    RawRoute {
        path: BACKUP_UPLOAD_ROUTE,
        methods: POST,
        auth: RouteAuth::AdminSession,
        route: || post(crate::router::upload_backup),
    },
    // A structure zip for Source control to deploy (APP_SYNC_PROPOSAL §7.4),
    // outside the endpoint set for the backup upload's reason: its request
    // is a file. Planning and deploying it are typed (`planSyncZipDeploy`,
    // `syncZipDeploy`).
    RawRoute {
        path: STRUCTURE_ZIP_UPLOAD_ROUTE,
        methods: POST,
        auth: RouteAuth::AdminSession,
        route: || post(crate::router::upload_structure_zip),
    },
    // The file-store IDE's language server (design §12.1). A real route
    // rather than a branch of the fallback, because a WebSocket upgrade is
    // not a request the fallback's `Bytes` body could survive: it has to be
    // extracted before the body is touched.
    RawRoute {
        path: crate::lsp::LSP_ROUTE,
        methods: GET,
        auth: RouteAuth::AdminSession,
        route: || get(crate::router::language_server),
    },
    // Claude Code in an IDE terminal tab (§12.1): the language server's
    // sibling, an upgrade for the same reason.
    RawRoute {
        path: crate::terminal::TERMINAL_ROUTE,
        methods: GET,
        auth: RouteAuth::AdminSession,
        route: || get(crate::router::ide_terminal),
    },
    // The admin chat socket (§11.4). A real route for the same reason the
    // language server's is: an upgrade cannot survive the fallback's `Bytes`
    // body, and a chat turn is bidirectional in a way the typed endpoint
    // model has no shape for.
    RawRoute {
        path: crate::chat::AGENT_CHAT_ROUTE,
        methods: GET,
        auth: RouteAuth::AdminSession,
        route: || get(crate::router::agent_chat),
    },
    // A stream's Observe socket (TODO "Streams" §9). The third route that
    // is an upgrade rather than a typed endpoint, for the reason the other
    // two are: an `EndpointSet` is a typed request/response model and a
    // socket has no shape in it. It sits on `/api/streams/{id}/observe`,
    // beside the stream endpoints rather than under `/admin`, because it is
    // the same resource the CRUD endpoints address — and axum matches this
    // literal route ahead of the `dispatch` fallback the rest of `/api`
    // goes through.
    RawRoute {
        path: crate::observe::STREAM_OBSERVE_ROUTE,
        methods: GET,
        auth: RouteAuth::AdminSession,
        route: || get(crate::router::stream_observe),
    },
    // A fit's progress, pushed to the model editor (analytics TODO A3.3).
    // An upgrade for the reason the Observe socket's is.
    RawRoute {
        path: crate::fit_progress::FIT_PROGRESS_ROUTE,
        methods: GET,
        auth: RouteAuth::AdminSession,
        route: || get(crate::router::fit_progress),
    },
    // The administration MCP server (§13.6). A real route rather than a
    // typed endpoint for the reason the upload and backup routes are:
    // JSON-RPC over a raw body is not a shape `TypeSchema` describes.
    //
    // It authenticates by `Authorization: Bearer` and by **nothing else** —
    // a session cookie on this route is ignored, not accepted — which is
    // what makes it safe to exempt from the CSRF middleware, and is the
    // whole of the confused-deputy story rather than belt-and-braces.
    RawRoute {
        path: crate::mcp::MCP_ROUTE,
        methods: POST,
        auth: RouteAuth::McpToken,
        route: || post(crate::mcp::mcp),
    },
    // The installation's MCP servers, forwarded for the IDE's Claude Code
    // sessions (§12.1): an open terminal's session token and nothing else,
    // so it is exempt from the CSRF check as `/mcp` is.
    RawRoute {
        path: crate::mcp_proxy::MCP_PROXY_ROUTE,
        methods: ANY,
        auth: RouteAuth::TerminalToken,
        route: || any(crate::mcp_proxy::mcp_proxy),
    },
    // A build host's agent (`TODO-build-hosts.md`): by its bearer token and
    // nothing else, so it is exempt from the CSRF check as `/mcp` is.
    RawRoute {
        path: crate::build_agent::BUILD_AGENT_HEARTBEAT_ROUTE,
        methods: POST,
        auth: RouteAuth::BuildHostToken,
        route: || post(crate::build_agent::heartbeat),
    },
    RawRoute {
        path: crate::build_agent::BUILD_AGENT_CLAIM_ROUTE,
        methods: POST,
        auth: RouteAuth::BuildHostToken,
        route: || post(crate::build_agent::claim),
    },
    RawRoute {
        path: "/build-agent/jobs/{id}/project",
        methods: GET,
        auth: RouteAuth::BuildHostToken,
        route: || get(crate::build_agent::project),
    },
    RawRoute {
        path: "/build-agent/jobs/{id}/log",
        methods: POST,
        auth: RouteAuth::BuildHostToken,
        route: || post(crate::build_agent::log),
    },
    RawRoute {
        path: "/build-agent/jobs/{id}/artifact",
        methods: POST,
        auth: RouteAuth::BuildHostToken,
        route: || post(crate::build_agent::artifact),
    },
    RawRoute {
        path: "/build-agent/jobs/{id}/failed",
        methods: POST,
        auth: RouteAuth::BuildHostToken,
        route: || post(crate::build_agent::failed),
    },
    // The far end of moving the admin UI to its own subdomain (Settings →
    // Development): a single-use token from the old host becomes a session
    // here, so following the move does not mean logging in again. A real
    // route because its answer is a cookie and a redirect, not JSON.
    RawRoute {
        path: crate::admin_host::HANDOFF_ROUTE,
        methods: GET,
        auth: RouteAuth::HandoffToken,
        route: || get(crate::router::admin_handoff),
    },
    // Signing in with an external identity provider (Settings →
    // Authentication): redirects and a cookie, on every host, so real
    // routes rather than typed endpoints (`crate::oauth`).
    RawRoute {
        path: crate::oauth::PROVIDERS_ROUTE,
        methods: GET,
        auth: RouteAuth::Public,
        route: || get(crate::oauth::providers),
    },
    RawRoute {
        path: crate::oauth::START_ROUTE,
        methods: GET,
        auth: RouteAuth::Public,
        route: || get(crate::oauth::start),
    },
    RawRoute {
        path: crate::oauth::CALLBACK_ROUTE,
        methods: GET,
        auth: RouteAuth::OAuthFlow,
        route: || get(crate::oauth::callback),
    },
    RawRoute {
        path: crate::oauth::COMPLETE_ROUTE,
        methods: GET,
        auth: RouteAuth::OAuthFlow,
        route: || get(crate::oauth::complete),
    },
    // Two-factor authentication (design §7.2): the challenge a sign-in is
    // given in place of a session, and an account's own setup, on every
    // host as the OAuth routes are (`crate::second_factor`).
    RawRoute {
        path: crate::second_factor::STATUS_ROUTE,
        methods: GET,
        auth: RouteAuth::OwnSession,
        route: || get(crate::second_factor::status),
    },
    RawRoute {
        path: crate::second_factor::SETUP_ROUTE,
        methods: POST,
        auth: RouteAuth::SessionOrChallenge,
        route: || post(crate::second_factor::setup),
    },
    RawRoute {
        path: crate::second_factor::ENABLE_ROUTE,
        methods: POST,
        auth: RouteAuth::SessionOrChallenge,
        route: || post(crate::second_factor::enable),
    },
    RawRoute {
        path: crate::second_factor::VERIFY_ROUTE,
        methods: POST,
        auth: RouteAuth::Challenge,
        route: || post(crate::second_factor::verify),
    },
    RawRoute {
        path: crate::second_factor::DISABLE_ROUTE,
        methods: POST,
        auth: RouteAuth::SessionOrChallenge,
        route: || post(crate::second_factor::disable),
    },
    RawRoute {
        path: crate::second_factor::RECOVERY_CODES_ROUTE,
        methods: POST,
        auth: RouteAuth::SessionOrChallenge,
        route: || post(crate::second_factor::recovery_codes),
    },
    RawRoute {
        path: crate::second_factor::PAGE_ROUTE,
        methods: GET,
        auth: RouteAuth::Challenge,
        route: || get(crate::second_factor::page),
    },
    // Module webhooks (TODO-payments P1.1): a provider's events, on every
    // host as the OAuth routes are. A route of their own because the
    // answer is whatever the module says, bytes and all, and with a body
    // limit of their own: an unauthenticated caller is held to 1 MiB.
    RawRoute {
        path: crate::webhooks::HOOK_ROUTE,
        methods: GET_POST,
        auth: RouteAuth::ModuleVerifies,
        route: || module_route(crate::webhooks::hook),
    },
    RawRoute {
        path: crate::webhooks::HOOK_REST_ROUTE,
        methods: GET_POST,
        auth: RouteAuth::ModuleVerifies,
        route: || module_route(crate::webhooks::hook_rest),
    },
    // v1 plugin routes: on the admin host, as the person calling, with
    // the webhook's body limit (`crate::module_routes`).
    RawRoute {
        path: crate::module_routes::ROUTE,
        methods: GET_POST,
        auth: RouteAuth::ScopedSession,
        route: || module_route(crate::module_routes::route),
    },
    RawRoute {
        path: crate::oauth::CONSENT_ROUTE,
        methods: GET,
        auth: RouteAuth::HostSession,
        route: || get(crate::oauth::consent),
    },
];

/// `router` with every route of [`RAW_ROUTES`] added: the only place a raw
/// route is mounted.
pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    RAW_ROUTES
        .iter()
        .fold(router, |router, raw| router.route(raw.path, (raw.route)()))
}

/// Every raw route: its path, its methods and how it authenticates.
pub fn raw_routes() -> impl Iterator<Item = (&'static str, &'static [&'static str], RouteAuth)> {
    RAW_ROUTES.iter().map(|r| (r.path, r.methods, r.auth))
}

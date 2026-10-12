//! The [`ApiProvider`] trait (design §13.4).
//!
//! An application enables any number of API providers, each mounted on a
//! sub-path: REST, GraphQL, gRPC, tRPC, MCP. Every provider projects the
//! application's shared [`EndpointSet`](crate::EndpointSet) (§13.1) into its own
//! protocol, so the endpoints are described once and each protocol is a
//! *rendering* of them — which is also what lets every provider participate in
//! the shared TypeScript consumer generation.
//!
//! The MVP ships one provider, [`RestProvider`](crate::RestProvider). The trait
//! exists now so the others slot in without a shape change.
//!
//! Providers are transport-agnostic: [`ApiRequest`]/[`ApiResponse`] speak method,
//! path, query, and JSON — not axum types — so `sc-server` maps HTTP onto them
//! and a provider stays testable without a socket.

use async_trait::async_trait;
use bytes::Bytes;
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::Result;
use serde_json::Value as Json;
use std::sync::Arc;
use uuid::Uuid;

use crate::endpoint::{EndpointSet, Method};

/// A request routed to an API provider.
///
/// `path` is relative to the **application**, not to the provider's mount: a
/// provider mounted at `/api` receiving `/api/posts` sees `path == "/api/posts"`
/// and strips its own mount. That keeps the request as it arrived and leaves
/// mount handling in one place (the provider that owns the mount).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    /// The HTTP method.
    pub method: Method,
    /// The request path within the application, with a leading slash.
    pub path: String,
    /// Query-string parameters, **in the order they arrived and with duplicates
    /// kept**.
    ///
    /// A map would be smaller and wrong: a filter vocabulary spells a range as
    /// two values under one key (`?published=gte.2020&published=lt.2024`), and a
    /// map keeps whichever arrived last — a dropped filter is rows the caller
    /// did not ask for, which is the worst failure a data API can have. Read it
    /// with [`query_get`](ApiRequest::query_get) and
    /// [`query_all`](ApiRequest::query_all).
    pub query: Vec<(String, String)>,
    /// The parsed JSON request body ([`Json::Null`] when there was none).
    pub body: Json,
    /// The raw, unparsed request body, set by the transport when the caller sent
    /// something other than JSON — a file upload's bytes (§4). `None` for every
    /// JSON request, so a provider that never asks for bytes never sees them.
    pub raw: Option<Bytes>,
    /// Where this request arrived and which applications are served beside it —
    /// what an emailed link is built from. Set by the transport for a request
    /// that reached an application; `None` in a test or a tool call, where a
    /// provider asked to build a link says so rather than guessing a host.
    pub links: Option<AppLinks>,
}

/// The applications a request can link to, as the transport that received it
/// knows them.
///
/// A password link names *an* application — an invitation made in the
/// therapists' app is accepted in the patients' — and only the server knows
/// which applications it serves and under what host. A trait, so `sc-api`
/// needs neither the router's types nor `sc-app`'s.
pub trait AppDirectory: Send + Sync {
    /// The origin (`scheme://host[:port]`) the request arrived at.
    fn own_origin(&self) -> String;

    /// The origin the application served on `subdomain` has, reached the way
    /// this request reached its own — or `None` when no application is served
    /// there.
    fn app_origin(&self, subdomain: &str) -> Option<String>;
}

/// An [`AppDirectory`] as a request carries it.
///
/// A newtype so [`ApiRequest`] keeps its derives: two requests are equal when
/// they name the *same* directory, which is all a test comparing them means.
#[derive(Clone)]
pub struct AppLinks(pub Arc<dyn AppDirectory>);

impl std::fmt::Debug for AppLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AppLinks")
            .field(&self.0.own_origin())
            .finish()
    }
}

impl PartialEq for AppLinks {
    fn eq(&self, other: &AppLinks) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for AppLinks {}

impl ApiRequest {
    /// A request with no query parameters and no body.
    pub fn new(method: Method, path: impl Into<String>) -> ApiRequest {
        ApiRequest {
            method,
            path: path.into(),
            query: Vec::new(),
            body: Json::Null,
            raw: None,
            links: None,
        }
    }

    /// Attach the applications a link may point at, returning `self` for
    /// chaining.
    pub fn links(mut self, links: Arc<dyn AppDirectory>) -> ApiRequest {
        self.links = Some(AppLinks(links));
        self
    }

    /// The first value given for `key`, or `None`.
    ///
    /// "First" rather than "last" because a repeated key is a list the caller
    /// wrote in order, and a single-valued reader should see its head.
    pub fn query_get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every value given for `key`, in arrival order.
    pub fn query_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.query
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// A `GET` for `path`.
    pub fn get(path: impl Into<String>) -> ApiRequest {
        ApiRequest::new(Method::Get, path)
    }

    /// Attach a JSON body, returning `self` for chaining.
    pub fn body(mut self, body: Json) -> ApiRequest {
        self.body = body;
        self
    }

    /// Append a query parameter, returning `self` for chaining. Calling it twice
    /// with the same key keeps both values, as the wire does.
    pub fn query(mut self, key: impl Into<String>, value: impl Into<String>) -> ApiRequest {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Attach a raw (non-JSON) body — an upload's bytes — returning `self` for
    /// chaining.
    pub fn raw(mut self, bytes: impl Into<Bytes>) -> ApiRequest {
        self.raw = Some(bytes.into());
        self
    }
}

/// What the transport should do with the caller's session once a handler or
/// provider has run.
///
/// Authentication is a **transport** concern — a session token rides in a cookie
/// for a browser and could ride in a header for another caller — so the code
/// that authenticates says only *what happened to the session*, and whoever owns
/// the wire decides how to carry it. This keeps login/logout pure and testable:
/// `sc-server` turns `Start` into a minted token in a `Set-Cookie`, and `End`
/// into dropping the token and clearing the cookie.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum SessionAction {
    /// Leave the session untouched.
    #[default]
    Keep,
    /// Start a session for this user (login): mint a token and carry it.
    ///
    /// Whatever session the caller arrived with is ended, so a login replaces a
    /// session rather than adding one. That is what makes "become user" a
    /// *swap*: the admin session that authorised it does not stay live behind
    /// the one it turned into.
    Start(User),
    /// End the current session (logout): drop the token.
    End,
    /// End **every** session belonging to this user, whoever is asking.
    ///
    /// Not "the current session" and not necessarily the caller's: an admin
    /// forcing somebody out, or the sessions that must not outlive an account
    /// being disabled or deleted. The caller's own cookie is left alone — if
    /// they named themselves, it simply stops resolving.
    EndUser(Uuid),
    /// End **every** session there is, the caller's cookie included — what
    /// Clear all does once it has deleted every account.
    EndAll,
}

/// A raw-bytes response body — a file download — with the content type the
/// transport should serve it under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBody {
    /// The bytes to serve.
    pub bytes: Bytes,
    /// The `Content-Type` to serve them with, e.g. `image/png`.
    pub content_type: String,
}

/// A provider's response: an HTTP status, a JSON body, and any session change.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The JSON response body.
    pub body: Json,
    /// A raw-bytes body (a file download, §4). When set, the transport serves
    /// these bytes with their content type and `body` is ignored. Boxed so the
    /// download case does not widen every response that is not one.
    pub raw: Option<Box<RawBody>>,
    /// A store file to serve (a `File` field's download, TODO F2 §2.2). When
    /// set, the transport serves it through its one file response builder —
    /// streamed, with Range and its name — and `body` is ignored. The access
    /// check has been made by whoever set it.
    pub file: Option<Box<sc_files::ServeFile>>,
    /// The session change for the transport to apply, if any.
    pub session: SessionAction,
    /// Extra response headers a code query chose with `response({ headers })`
    /// (TODO F1.4): lower-case names, none of them one the server owns
    /// ([`sc_expr::SERVER_OWNED_HEADERS`]). Empty for everything else.
    pub headers: Vec<(String, String)>,
}

impl ApiResponse {
    /// A `200 OK` carrying `body`, with no session change.
    pub fn ok(body: Json) -> ApiResponse {
        ApiResponse {
            status: 200,
            body,
            raw: None,
            file: None,
            session: SessionAction::Keep,
            headers: Vec::new(),
        }
    }

    /// A response with an explicit status and no session change.
    pub fn with_status(status: u16, body: Json) -> ApiResponse {
        ApiResponse {
            status,
            body,
            raw: None,
            file: None,
            session: SessionAction::Keep,
            headers: Vec::new(),
        }
    }

    /// A `200 OK` serving raw bytes under `content_type` — a file download.
    pub fn file(bytes: impl Into<Bytes>, content_type: impl Into<String>) -> ApiResponse {
        ApiResponse {
            status: 200,
            body: Json::Null,
            raw: Some(Box::new(RawBody {
                bytes: bytes.into(),
                content_type: content_type.into(),
            })),
            file: None,
            session: SessionAction::Keep,
            headers: Vec::new(),
        }
    }

    /// A `200 OK` serving a store file through the transport's file builder.
    pub fn serve_file(file: sc_files::ServeFile) -> ApiResponse {
        ApiResponse {
            file: Some(Box::new(file)),
            ..ApiResponse::ok(Json::Null)
        }
    }

    /// An error response: `{"error": message}` with `status`.
    pub fn error(status: u16, message: impl Into<String>) -> ApiResponse {
        ApiResponse {
            status,
            body: serde_json::json!({ "error": message.into() }),
            raw: None,
            file: None,
            session: SessionAction::Keep,
            headers: Vec::new(),
        }
    }

    /// A `200 OK` that also starts a session for `user` (login).
    pub fn start_session(user: User, body: Json) -> ApiResponse {
        ApiResponse {
            status: 200,
            body,
            raw: None,
            file: None,
            session: SessionAction::Start(user),
            headers: Vec::new(),
        }
    }

    /// A `200 OK` that also ends the current session (logout).
    pub fn end_session(body: Json) -> ApiResponse {
        ApiResponse {
            status: 200,
            body,
            raw: None,
            file: None,
            session: SessionAction::End,
            headers: Vec::new(),
        }
    }
}

/// One protocol projection of an application's API (design §13.4).
///
/// All API access flows through the same authorization layer (§7): a provider
/// enforces each endpoint's [`AuthRequirement`](crate::AuthRequirement) against
/// the caller before running it, so an API caller sees exactly what a user of
/// that role would.
#[async_trait]
pub trait ApiProvider: Send + Sync {
    /// The provider's registered name (`rest`, `graphql`, `grpc`, `trpc`, `mcp`).
    fn name(&self) -> &str;

    /// The sub-path within the application this provider is mounted at, e.g.
    /// `/api`.
    fn mount(&self) -> String;

    /// The endpoints this provider projects — the application's `Endpoint` set
    /// rendered into this protocol. The server mounts these and the TypeScript
    /// generator types them.
    fn endpoints(&self) -> &EndpointSet;

    /// Whether the endpoint a `method` request to `path` reaches is open to the
    /// public role ([`AuthRequirement::is_public_role`]) — one anybody may call
    /// with no session, and so with no CSRF token.
    ///
    /// The server asks this of a mutating request that failed the CSRF check,
    /// and serves it as an anonymous caller when the answer is yes. The endpoint
    /// is found the way a provider routes: the first one registered whose method
    /// and path match.
    ///
    /// [`AuthRequirement::is_public_role`]: crate::AuthRequirement::is_public_role
    fn open_to_public(&self, method: Method, path: &str) -> bool {
        self.endpoints()
            .iter()
            .find(|ep| ep.method == method && ep.path.match_path(path).is_some())
            .is_some_and(|ep| ep.auth.is_public_role())
    }

    /// Handle one request. `user` is the authenticated caller, or `None` for an
    /// anonymous one; the provider is responsible for enforcing each endpoint's
    /// auth requirement.
    ///
    /// The catalog arrives as the `Arc` the server already holds rather than as
    /// a bare reference, because a provider may need to *hand it on* to code it
    /// does not own for the length of the request: the GraphQL executor's field
    /// resolvers are `'static` closures, so the catalog reaches them as request
    /// data or not at all. A provider that only reads it borrows through the
    /// `Arc` and pays nothing.
    async fn handle(
        &self,
        req: ApiRequest,
        cat: &Arc<Catalog>,
        user: Option<&User>,
    ) -> Result<ApiResponse>;
}

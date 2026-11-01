//! The handler registry that endpoint dispatch resolves against.
//!
//! [`sc_api::Endpoint`] values are pure data: an endpoint names its handler via
//! [`HandlerRef::Named`](sc_api::HandlerRef), and the server resolves that name
//! here to the async code that runs it. Keeping handlers in a registry (rather
//! than baked into the endpoint values) is what lets the admin API and a
//! runtime-registered application API flow through the *same* dispatch machinery
//! (design §13.1).
//!
//! Handlers stay free of HTTP plumbing: they receive a [`HandlerCtx`] (path/query
//! params, parsed JSON body, who is calling — already checked against the
//! endpoint's auth requirement — and the request's negotiated locale) and
//! return a [`HandlerResponse`]. A
//! handler never touches cookies directly — instead it asks the dispatcher to
//! start or end the session via [`SessionAction`], so login/logout stay pure.
//! That type is [`sc_api::SessionAction`], shared with the API providers: an
//! application's `login` and the admin's `login` describe the same session
//! change and the dispatcher applies both the same way.
//!
//! # Gates
//!
//! Who is calling arrives typed by the **gate** a handler was registered
//! behind (code quality Q2.4). [`register`](HandlerRegistry::register) is
//! open: the context's [`who`](HandlerCtx::who) is the [`Caller`], anybody.
//! [`register_signed_in`](HandlerRegistry::register_signed_in) hands an
//! [`AuthenticatedUser`], [`register_admin`](HandlerRegistry::register_admin)
//! an [`Admin`], and [`register_analytics`](HandlerRegistry::register_analytics)
//! an [`AnalyticsCaller`]. The registry narrows the caller before the handler
//! runs and refuses one the gate does not admit, so an admin handler cannot be
//! called with anything but admin authority however it is reached; and
//! [`check_gates`](HandlerRegistry::check_gates), which the router runs at
//! startup, refuses an endpoint set in which an `admin()` endpoint names a
//! handler registered behind a weaker gate.
//!
//! # Typed handlers
//!
//! [`typed`] adapts an `async fn(HandlerState, HandlerCtx<W>, In) -> Result<Out>` to
//! the shape a gate registers: the body is deserialized into `In` before the
//! function runs (a body that does not fit is an `invalid` error naming what
//! did not), and `Out` is serialized as the response. The function is handed
//! the server's [`HandlerState`](crate::handlers::HandlerState) rather than capturing
//! clones of the catalog and the mount registry, and, being a named function,
//! it can be called directly as well — which is how the catalog's
//! [`AdminHost`](sc_catalog::AdminHost) runs the handlers the admin tools
//! reach. Handlers move to it as they are touched.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use sc_api::SessionAction;
use sc_api::{AuthRequirement, EndpointSet, HandlerRef};
use sc_auth::{AuthenticatedUser, User};
use sc_error::{Error, Result};
use serde_json::Value;

pub use crate::admin_session::{Admin, AdminSession, AnalyticsCaller, Caller, Who};

/// A boxed, `Send` future — the return shape every handler produces.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Adapt a typed handler to the shape a gate registers (module docs, "Typed
/// handlers"): `reg.register_admin("deleteStream", typed(&state, delete_stream))`.
///
/// `In` is read from the request's JSON body — `()` for an endpoint with none,
/// which reads `null` — and the body is moved out of the context the function
/// is handed. `Out` is the response: a [`HandlerResponse`] as it is, any
/// other [`Reply`] as a `200` with its JSON.
pub(crate) fn typed<W, In, Out, F, Fut>(
    state: &crate::handlers::HandlerState,
    handler: F,
) -> impl Fn(HandlerCtx<W>) -> BoxFuture<Result<HandlerResponse>> + Send + Sync + 'static
where
    W: Send + 'static,
    In: serde::de::DeserializeOwned + Send + 'static,
    Out: Reply + 'static,
    F: Fn(crate::handlers::HandlerState, HandlerCtx<W>, In) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Out>> + Send + 'static,
{
    let state = state.clone();
    let handler = Arc::new(handler);
    move |mut ctx: HandlerCtx<W>| {
        let state = state.clone();
        let handler = Arc::clone(&handler);
        Box::pin(async move {
            let input: In = serde_json::from_value(std::mem::take(&mut ctx.body))
                .map_err(|e| Error::invalid(format!("the request body does not fit: {e}")))?;
            handler(state, ctx, input).await?.into_response()
        })
    }
}

/// What a typed handler answers: a whole [`HandlerResponse`] (a status, a
/// session change, a download), or a value serialized as a `200`'s body.
pub(crate) trait Reply {
    /// The response this answer is.
    fn into_response(self) -> Result<HandlerResponse>;
}

impl Reply for HandlerResponse {
    fn into_response(self) -> Result<HandlerResponse> {
        Ok(self)
    }
}

impl<T: serde::Serialize> Reply for T {
    fn into_response(self) -> Result<HandlerResponse> {
        let body = serde_json::to_value(self)
            .map_err(|e| Error::msg(format!("the response could not be written: {e}")))?;
        Ok(HandlerResponse::ok(body))
    }
}

/// A registered handler: given a [`HandlerCtx`], produce a [`HandlerResponse`].
///
/// Every handler is called this way, with the [`Caller`] the dispatcher
/// resolved; the gate it was registered behind narrows the caller inside.
pub type HandlerFn = Arc<dyn Fn(HandlerCtx) -> BoxFuture<Result<HandlerResponse>> + Send + Sync>;

/// Everything a handler needs about the request, with authorization already
/// enforced by the dispatcher.
///
/// `W` is who is calling, typed by the handler's gate (module docs, "Gates"):
/// a [`Caller`] for an open handler, an [`Admin`] for an admin endpoint's.
#[derive(Debug, Clone)]
pub struct HandlerCtx<W = Caller> {
    /// Path parameters captured from the route pattern (e.g. `table`, `id`).
    pub path_params: HashMap<String, String>,
    /// Query-string parameters, in arrival order and with duplicates kept — the
    /// same shape (and for the same reason) as
    /// [`ApiRequest::query`](sc_api::ApiRequest). Read it with
    /// [`query_get`](HandlerCtx::query_get) and [`query_all`](HandlerCtx::query_all).
    pub query: Vec<(String, String)>,
    /// The parsed JSON request body ([`Value::Null`] when there was no body).
    pub body: Value,
    /// Who is calling. Presence and role already satisfy the endpoint's
    /// [`AuthRequirement`]; the type is the handler's gate's.
    pub who: W,
    /// The locale this request is served in (§16.1, D8).
    ///
    /// Negotiated **once**, in the router, from `?lang=`, the user's `language`
    /// column, the `lang` cookie and `Accept-Language`, against the enabled set.
    /// A handler that produces human-readable text — a refusal an admin reads, a
    /// declared spec's labels — translates against *this*, and never against an
    /// ambient locale, because the one it is handed is the one the response's
    /// `Content-Language` promises.
    ///
    /// On a monolingual installation it is the installation default and nothing
    /// was parsed to arrive at it (D11).
    pub locale: sc_i18n::Locale,
    /// The unparsed request body, set only for the routes that carry one.
    ///
    /// A typed endpoint never has this: its body is JSON, described by a
    /// `TypeSchema`, and arrives in [`body`](HandlerCtx::body). Binary upload
    /// cannot be described that way — the endpoint model has no bytes shape — so
    /// it is served by a route outside the `EndpointSet` that still dispatches
    /// *through this registry*, which is how it reaches the same catalog and the
    /// same access checks as everything else. Threading the bytes here rather
    /// than giving that route its own catalog handle is what keeps one code path
    /// for "write a file", however the bytes arrived.
    pub raw_body: Option<bytes::Bytes>,
    /// An unparsed request body that was **streamed to disk** rather than read
    /// into memory — an uploaded backup — for the routes that take one. Shared,
    /// because the context is cloneable; the file is deleted when the last
    /// holder lets go of it, which is how a handler keeps one (an upload waiting
    /// to be restored) or lets it be cleaned up (anything else).
    pub raw_file: Option<std::sync::Arc<sc_backup::SpoolFile>>,
    /// The address the request came from (TODO F4.2): what login throttling
    /// counts per IP. `None` where there was no socket.
    pub client_ip: Option<std::net::IpAddr>,
    /// The Analytics application this request came through (analytics TODO
    /// A9.3), with what it allows; `None` on the admin host, which is the
    /// unrestricted Analytics UI.
    ///
    /// Set by the router alone, from the application the request's host
    /// names, so a handler asks it rather than anything the client sent.
    pub analytics_app: Option<Arc<sc_analytics::app::AppScope>>,
}

impl<W: Who> HandlerCtx<W> {
    /// The user calling, when there is one.
    pub fn user(&self) -> Option<&User> {
        self.who.user()
    }
}

impl<W> HandlerCtx<W> {
    /// The same request with the caller narrowed by `narrow`, or its refusal.
    fn narrow<V>(self, narrow: impl FnOnce(W) -> Result<V>) -> Result<HandlerCtx<V>> {
        let HandlerCtx {
            path_params,
            query,
            body,
            who,
            locale,
            raw_body,
            raw_file,
            client_ip,
            analytics_app,
        } = self;
        Ok(HandlerCtx {
            who: narrow(who)?,
            path_params,
            query,
            body,
            locale,
            raw_body,
            raw_file,
            client_ip,
            analytics_app,
        })
    }

    /// The raw request body, or an error when the route did not supply one
    /// (which would be a registration bug, not user input).
    pub fn raw_body(&self) -> Result<&bytes::Bytes> {
        self.raw_body
            .as_ref()
            .ok_or_else(|| sc_error::Error::invalid("this handler requires a raw request body"))
    }

    /// A required path parameter, or an [`Error::Invalid`](sc_error::Error) if
    /// absent (a routing/registration bug rather than user input).
    pub fn path_param(&self, name: &str) -> Result<&str> {
        self.path_params
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| sc_error::Error::invalid(format!("missing path parameter `{name}`")))
    }

    /// The first value given for a query parameter, or `None`.
    pub fn query_get(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every value given for a query parameter, in arrival order.
    pub fn query_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.query
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A file the response *is*, rather than describes: a backup archive.
///
/// The mirror image of [`HandlerCtx::raw_body`] and there for the same reason —
/// the endpoint model is JSON-only, so a `TypeSchema` cannot describe bytes. A
/// handler that produces a download is therefore reached by a route outside the
/// `EndpointSet` (see [`crate::BACKUP_CREATE_ROUTE`]) while still living in this
/// registry, which is what keeps it behind the same session and admin checks as
/// every typed endpoint.
#[derive(Debug, Clone)]
pub struct Download {
    /// What to serve.
    pub body: DownloadBody,
    /// The `Content-Type` to serve them under.
    pub content_type: String,
    /// The name the browser should save it as, carried in `Content-Disposition`;
    /// empty for bytes a page fetches rather than a person saves.
    pub filename: String,
}

/// What a [`Download`] serves: bytes already in memory, or a file on disk that
/// is streamed and never read whole.
#[derive(Debug, Clone)]
pub enum DownloadBody {
    Bytes(bytes::Bytes),
    File(SpooledFile),
}

/// A file a response streams from — a backup, written to the spool directory
/// and served from there.
///
/// The file is open and, on Unix, already unlinked, so nothing is left on disk
/// once the response that takes it is dropped, however it ends. Cloneable only
/// because a [`HandlerResponse`] is; the first response built from it takes the
/// file, and a clone finds it gone.
#[derive(Debug, Clone)]
pub struct SpooledFile {
    file: std::sync::Arc<std::sync::Mutex<Option<std::fs::File>>>,
    len: u64,
    /// Deletes the file once the response is done with it, where the platform
    /// would not let it be unlinked while open.
    guard: Option<std::sync::Arc<sc_backup::SpoolFile>>,
}

impl SpooledFile {
    /// Serve the file `spool` names, `len` bytes long: open it, and let go of
    /// its name.
    pub fn open(spool: sc_backup::SpoolFile, len: u64) -> sc_error::Result<SpooledFile> {
        let file = std::fs::File::open(spool.path()).map_err(|e| {
            sc_error::Error::file(format!("opening `{}`: {e}", spool.path().display()))
        })?;
        // Unlinked while open: the data lives as long as the handle.
        let guard = if cfg!(unix) {
            drop(spool);
            None
        } else {
            Some(std::sync::Arc::new(spool))
        };
        Ok(SpooledFile {
            file: std::sync::Arc::new(std::sync::Mutex::new(Some(file))),
            len,
            guard,
        })
    }

    /// The file's length in bytes.
    pub fn size(&self) -> u64 {
        self.len
    }

    /// The open file and what keeps it alive, once.
    pub(crate) fn take(
        &self,
    ) -> Option<(std::fs::File, Option<std::sync::Arc<sc_backup::SpoolFile>>)> {
        let file = self.file.lock().ok()?.take()?;
        Some((file, self.guard.clone()))
    }
}

/// A handler's result: a JSON body, an HTTP status, and an optional session
/// action for the dispatcher to apply.
#[derive(Debug, Clone)]
pub struct HandlerResponse {
    /// The JSON response body.
    pub body: Value,
    /// The HTTP status code (defaults to `200`).
    pub status: u16,
    /// The session change to apply, if any.
    pub session: SessionAction,
    /// Bytes to serve instead of the JSON body, for the routes that produce a
    /// file. Only the routes outside the typed endpoint set look at this; a typed
    /// endpoint leaves it `None`, because its response is its `TypeSchema`.
    pub download: Option<Download>,
    /// A store file to serve through the shared response builder (TODO F2,
    /// §2.2) — streamed, with Range and its own name. Like `download`, only the
    /// routes outside the typed endpoint set look at it.
    pub serve: Option<sc_files::ServeFile>,
}

impl HandlerResponse {
    /// A `200 OK` response with a JSON body and no session change.
    pub fn ok(body: Value) -> HandlerResponse {
        HandlerResponse {
            body,
            status: 200,
            session: SessionAction::Keep,
            download: None,
            serve: None,
        }
    }

    /// A `200 OK` response that *is* a file (see [`Download`]).
    pub fn download(download: Download) -> HandlerResponse {
        HandlerResponse {
            body: Value::Null,
            status: 200,
            session: SessionAction::Keep,
            download: Some(download),
            serve: None,
        }
    }

    /// A response that is a store file, served by the shared builder.
    pub fn serve_file(file: sc_files::ServeFile) -> HandlerResponse {
        HandlerResponse {
            body: Value::Null,
            status: 200,
            session: SessionAction::Keep,
            download: None,
            serve: Some(file),
        }
    }

    /// A `200 OK` response that also starts a session for `user` (login).
    pub fn start_session(user: User, body: Value) -> HandlerResponse {
        HandlerResponse {
            body,
            status: 200,
            session: SessionAction::Start(user),
            download: None,
            serve: None,
        }
    }

    /// A `200 OK` response that also ends the current session (logout).
    pub fn end_session(body: Value) -> HandlerResponse {
        HandlerResponse {
            body,
            status: 200,
            session: SessionAction::End,
            download: None,
            serve: None,
        }
    }

    /// A `200 OK` response that also ends every session belonging to `user_id` —
    /// an admin forcing somebody out, or the sessions that must not outlive an
    /// account being disabled or deleted. The caller's own cookie is untouched.
    pub fn end_user_sessions(user_id: uuid::Uuid, body: Value) -> HandlerResponse {
        HandlerResponse {
            body,
            status: 200,
            session: SessionAction::EndUser(user_id),
            download: None,
            serve: None,
        }
    }

    /// A `200 OK` response that ends **every** session, the caller's cookie
    /// included — after Clear all has deleted every account.
    pub fn end_all_sessions(body: Value) -> HandlerResponse {
        HandlerResponse {
            body,
            status: 200,
            session: SessionAction::EndAll,
            download: None,
            serve: None,
        }
    }

    /// Override the HTTP status (e.g. `201` for a created resource).
    pub fn with_status(mut self, status: u16) -> HandlerResponse {
        self.status = status;
        self
    }
}

/// Which callers a handler was registered to serve (module docs, "Gates").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Anybody: the handler is handed the [`Caller`].
    Open,
    /// A signed-in user: an [`AuthenticatedUser`].
    SignedIn,
    /// Admin authority: an [`Admin`].
    Admin,
    /// The Analytics UI's caller: an [`AnalyticsCaller`] — admin authority on
    /// the admin host, an Analytics application's visitor on its own.
    Analytics,
}

impl Gate {
    /// Whether a handler behind this gate may serve an endpoint requiring
    /// `auth`: an `admin()` endpoint needs an admin (or Analytics) handler, a
    /// `LoggedIn` one a handler that is handed a user.
    pub fn serves(self, auth: &AuthRequirement) -> bool {
        match auth {
            AuthRequirement::MinRole(role) if role.is_admin() => {
                matches!(self, Gate::Admin | Gate::Analytics)
            }
            AuthRequirement::LoggedIn => self != Gate::Open,
            AuthRequirement::MinRole(_) | AuthRequirement::Public => true,
        }
    }
}

/// One registered handler and its gate.
#[derive(Clone)]
struct Registered {
    gate: Gate,
    handler: HandlerFn,
}

/// A name → handler map. Endpoints are dispatched by resolving their
/// [`HandlerRef::Named`](sc_api::HandlerRef) here; an endpoint whose handler is
/// absent (or is guest code / SQL) yields `501 Not Implemented`.
#[derive(Clone, Default)]
pub struct HandlerRegistry {
    handlers: HashMap<String, Registered>,
}

impl HandlerRegistry {
    /// An empty registry.
    pub fn new() -> HandlerRegistry {
        HandlerRegistry::default()
    }

    /// Register an **open** async handler under `name`, replacing any previous
    /// one: it is handed whoever the dispatcher resolved, and may serve only
    /// endpoints that do not require a user ([`Gate::serves`]).
    ///
    /// Accepts any async closure `Fn(HandlerCtx) -> Future<Output = Result<HandlerResponse>>`.
    pub fn register<F, Fut>(&mut self, name: impl Into<String>, handler: F) -> &mut HandlerRegistry
    where
        F: Fn(HandlerCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HandlerResponse>> + Send + 'static,
    {
        let boxed: HandlerFn = Arc::new(move |ctx| Box::pin(handler(ctx)));
        self.insert(name, Gate::Open, boxed)
    }

    /// Register a handler for a `LoggedIn` endpoint: it is handed the
    /// signed-in [`AuthenticatedUser`], and never called without one.
    pub fn register_signed_in<F, Fut>(
        &mut self,
        name: impl Into<String>,
        handler: F,
    ) -> &mut HandlerRegistry
    where
        F: Fn(HandlerCtx<AuthenticatedUser>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HandlerResponse>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let boxed: HandlerFn =
            Arc::new(
                move |ctx: HandlerCtx| match ctx.narrow(Caller::into_signed_in) {
                    Ok(ctx) => Box::pin(handler(ctx)) as BoxFuture<_>,
                    Err(refused) => Box::pin(async move { Err(refused) }),
                },
            );
        self.insert(name, Gate::SignedIn, boxed)
    }

    /// Register a handler for an `admin()` endpoint: it is handed the
    /// [`Admin`] authority, and never called without it.
    pub fn register_admin<F, Fut>(
        &mut self,
        name: impl Into<String>,
        handler: F,
    ) -> &mut HandlerRegistry
    where
        F: Fn(HandlerCtx<Admin>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HandlerResponse>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let boxed: HandlerFn =
            Arc::new(
                move |ctx: HandlerCtx| match ctx.narrow(Caller::into_admin) {
                    Ok(ctx) => Box::pin(handler(ctx)) as BoxFuture<_>,
                    Err(refused) => Box::pin(async move { Err(refused) }),
                },
            );
        self.insert(name, Gate::Admin, boxed)
    }

    /// Register a handler for an Analytics endpoint: on the admin host it is
    /// handed [`Admin`] authority, and through an Analytics application its
    /// visitor ([`AnalyticsCaller`]).
    pub fn register_analytics<F, Fut>(
        &mut self,
        name: impl Into<String>,
        handler: F,
    ) -> &mut HandlerRegistry
    where
        F: Fn(HandlerCtx<AnalyticsCaller>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HandlerResponse>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let boxed: HandlerFn = Arc::new(move |ctx: HandlerCtx| {
            let in_app = ctx.analytics_app.is_some();
            match ctx.narrow(|caller| caller.into_analytics(in_app)) {
                Ok(ctx) => Box::pin(handler(ctx)) as BoxFuture<_>,
                Err(refused) => Box::pin(async move { Err(refused) }),
            }
        });
        self.insert(name, Gate::Analytics, boxed)
    }

    fn insert(&mut self, name: impl Into<String>, gate: Gate, handler: HandlerFn) -> &mut Self {
        self.handlers
            .insert(name.into(), Registered { gate, handler });
        self
    }

    /// Look up a handler by name.
    pub fn get(&self, name: &str) -> Option<&HandlerFn> {
        self.handlers.get(name).map(|r| &r.handler)
    }

    /// The gate the handler `name` was registered behind.
    pub fn gate(&self, name: &str) -> Option<Gate> {
        self.handlers.get(name).map(|r| r.gate)
    }

    /// Every handler `endpoints` name is behind a gate that serves the
    /// endpoint's requirement ([`Gate::serves`]): an `admin()` endpoint's
    /// handler takes [`Admin`] authority, a `LoggedIn` one's a user. An
    /// endpoint whose handler is not registered here is left to dispatch,
    /// which answers it `501`.
    ///
    /// The router runs this before it serves anything, so an admin handler
    /// registered behind the open gate is a server that does not start rather
    /// than a route that reaches a handler with whoever asked.
    pub fn check_gates(&self, endpoints: &EndpointSet) -> Result<()> {
        let wrong: Vec<String> = endpoints
            .iter()
            .filter_map(|ep| {
                let HandlerRef::Named(name) = &ep.handler else {
                    return None;
                };
                let gate = self.gate(name)?;
                (!gate.serves(&ep.auth)).then(|| {
                    format!(
                        "`{}` requires {:?} but its handler `{name}` is registered behind the \
                         {gate:?} gate",
                        ep.name, ep.auth
                    )
                })
            })
            .collect();
        if wrong.is_empty() {
            Ok(())
        } else {
            Err(Error::config(format!(
                "handlers registered behind the wrong gate: {}",
                wrong.join("; ")
            )))
        }
    }

    /// The number of registered handlers.
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

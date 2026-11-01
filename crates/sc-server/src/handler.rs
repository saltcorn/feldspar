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
//! params, parsed JSON body, and the authenticated [`User`], already checked
//! against the endpoint's auth requirement, and the request's negotiated
//! locale) and return a [`HandlerResponse`]. A
//! handler never touches cookies directly — instead it asks the dispatcher to
//! start or end the session via [`SessionAction`], so login/logout stay pure.
//! That type is [`sc_api::SessionAction`], shared with the API providers: an
//! application's `login` and the admin's `login` describe the same session
//! change and the dispatcher applies both the same way.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use sc_api::SessionAction;
use sc_auth::User;
use sc_error::Result;
use serde_json::Value;

/// A boxed, `Send` future — the return shape every handler produces.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A registered handler: given a [`HandlerCtx`], produce a [`HandlerResponse`].
pub type HandlerFn = Arc<dyn Fn(HandlerCtx) -> BoxFuture<Result<HandlerResponse>> + Send + Sync>;

/// Everything a handler needs about the request, with authorization already
/// enforced by the dispatcher.
#[derive(Debug, Clone)]
pub struct HandlerCtx {
    /// Path parameters captured from the route pattern (e.g. `table`, `id`).
    pub path_params: HashMap<String, String>,
    /// Query-string parameters, in arrival order and with duplicates kept — the
    /// same shape (and for the same reason) as
    /// [`ApiRequest::query`](sc_api::ApiRequest). Read it with
    /// [`query_get`](HandlerCtx::query_get) and [`query_all`](HandlerCtx::query_all).
    pub query: Vec<(String, String)>,
    /// The parsed JSON request body ([`Value::Null`] when there was no body).
    pub body: Value,
    /// The authenticated user, if any. Presence/role already satisfy the
    /// endpoint's [`AuthRequirement`](sc_api::AuthRequirement).
    pub user: Option<User>,
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
    pub raw_file: Option<std::sync::Arc<crate::backup::SpoolFile>>,
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

impl HandlerCtx {
    /// A required path parameter, or an [`Error::Invalid`](sc_error::Error) if
    /// absent (a routing/registration bug rather than user input).
    /// The raw request body, or an error when the route did not supply one
    /// (which would be a registration bug, not user input).
    pub fn raw_body(&self) -> Result<&bytes::Bytes> {
        self.raw_body
            .as_ref()
            .ok_or_else(|| sc_error::Error::invalid("this handler requires a raw request body"))
    }

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
    guard: Option<std::sync::Arc<crate::backup::SpoolFile>>,
}

impl SpooledFile {
    /// Serve the file `spool` names, `len` bytes long: open it, and let go of
    /// its name.
    pub fn open(spool: crate::backup::SpoolFile, len: u64) -> sc_error::Result<SpooledFile> {
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
    ) -> Option<(
        std::fs::File,
        Option<std::sync::Arc<crate::backup::SpoolFile>>,
    )> {
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

    /// [`start_session`](HandlerResponse::start_session), honouring the
    /// sign-in's "Remember me" (`sc_api::auth::remember` of its body).
    pub fn start_session_remembering(user: User, body: Value, remember: bool) -> HandlerResponse {
        HandlerResponse {
            session: SessionAction::start(user, remember),
            ..HandlerResponse::ok(body)
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

/// A name → handler map. Endpoints are dispatched by resolving their
/// [`HandlerRef::Named`](sc_api::HandlerRef) here; an endpoint whose handler is
/// absent (or is guest code / SQL) yields `501 Not Implemented`.
#[derive(Clone, Default)]
pub struct HandlerRegistry {
    handlers: HashMap<String, HandlerFn>,
}

impl HandlerRegistry {
    /// An empty registry.
    pub fn new() -> HandlerRegistry {
        HandlerRegistry::default()
    }

    /// Register an async handler under `name`, replacing any previous one.
    ///
    /// Accepts any async closure `Fn(HandlerCtx) -> Future<Output = Result<HandlerResponse>>`.
    pub fn register<F, Fut>(&mut self, name: impl Into<String>, handler: F) -> &mut HandlerRegistry
    where
        F: Fn(HandlerCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<HandlerResponse>> + Send + 'static,
    {
        let boxed: HandlerFn = Arc::new(move |ctx| Box::pin(handler(ctx)));
        self.handlers.insert(name.into(), boxed);
        self
    }

    /// Look up a handler by name.
    pub fn get(&self, name: &str) -> Option<&HandlerFn> {
        self.handlers.get(name)
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

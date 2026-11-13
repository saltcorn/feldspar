//! Sessions as types: who a request's session cookie signs in, **and on which
//! host it was issued** (security review 2026-11, §2; TODO-type-hardening
//! H2.1).
//!
//! Five findings of that review (B1, B7, E2/A6, E7, and #15 the month before)
//! were one bug: a session issued for one host used on another. The scope a
//! session was issued for is in its token ([`SessionScope::of_token`]); this
//! module is where it is compared with the host a request arrived on, and the
//! only place in this crate that reads a session from a request.
//!
//! - [`Session<AdminScope>`]: on the admin host, a session issued there.
//! - [`Session<AppScope>`]: on an application's host, a session issued by
//!   **that** application. It names the application.
//! - [`Session<Visiting>`]: on an application's host, a session issued
//!   somewhere else — the admin host, another application, or an application
//!   before tokens named theirs — which a shared sign-in cookie
//!   (Settings → Authentication) carries there. An application serves it as
//!   the user ([`AppSession`]), as it always has; nothing that changes the
//!   account takes one.
//! - [`OwnSession`]: `Session<AdminScope>` or `Session<AppScope>`, whichever
//!   the host is: a session issued for the host it arrived on. What a route
//!   that answers on every host and changes the account (the second-factor
//!   routes) takes, so the type says it cannot be a visiting session.
//!
//! A route says which of these it takes in its signature (an axum extractor:
//! `Option<Session<AdminScope>>`, `Option<OwnSession>`, …) or by calling the
//! matching `of_request`. Nothing else reads the session cookie for
//! authentication: `tests/harness/tests/workspace_invariants.rs` checks that
//! `SessionStore::user_for`/`session` and `SessionScope::of_token` are called
//! here and nowhere else in sc-server.

use axum::extract::OptionalFromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::CookieJar;
use sc_auth::{AuthenticatedUser, SessionScope, SessionStore};
use uuid::Uuid;

use crate::router::{AppState, Resolved, json_error, log_failure, resolve_app};
use crate::security::SESSION_COOKIE;

/// Issued on the admin host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminScope;

/// Issued on one application's host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppScope {
    app: Uuid,
}

impl AppScope {
    /// The application that issued the session.
    pub fn app(self) -> Uuid {
        self.app
    }
}

/// Presented on an application's host, issued elsewhere (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visiting;

/// A request's live session, of scope `S` (module docs). Constructed only
/// here, from a session the store resolved and a host it was checked against.
#[derive(Clone)]
pub struct Session<S> {
    token: String,
    user: AuthenticatedUser,
    scope: S,
}

impl<S: std::fmt::Debug> std::fmt::Debug for Session<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is a bearer credential: never in a log line.
        f.debug_struct("Session")
            .field("user", &self.user.id())
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl<S: Copy> Session<S> {
    /// The signed-in user.
    pub fn user(&self) -> &AuthenticatedUser {
        &self.user
    }

    /// The signed-in user, given up.
    pub fn into_user(self) -> AuthenticatedUser {
        self.user
    }

    /// Where the session was issued.
    pub fn scope(&self) -> S {
        self.scope
    }

    /// The token the request carried: what a response that keeps or replaces
    /// the session hands to the cookie logic.
    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

/// The session store could not answer: the request is answered `500`.
#[derive(Debug)]
pub(crate) struct LookupFailed;

impl IntoResponse for LookupFailed {
    fn into_response(self) -> Response {
        json_error(StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed")
    }
}

/// The token of the request's session cookie.
fn cookie_token(jar: &CookieJar) -> Option<String> {
    jar.get(SESSION_COOKIE)
        .map(|c| c.value().to_owned())
        .filter(|t| !t.is_empty())
}

/// The live session `token` names, with the scope it was issued for.
async fn resolve(
    sessions: &SessionStore,
    token: &str,
) -> Result<Option<(AuthenticatedUser, SessionScope)>, LookupFailed> {
    match sessions.session(token).await {
        Ok(found) => Ok(found.map(|live| (live.user, live.scope))),
        Err(e) => {
            log_failure("session lookup failed", &e);
            Err(LookupFailed)
        }
    }
}

impl Session<AdminScope> {
    /// The admin-host session `jar` carries, for a request the router has
    /// already found to be on the admin host: an application's session is no
    /// session there (security review #15), whatever its user's role.
    pub(crate) async fn on_admin_host(
        sessions: &SessionStore,
        jar: &CookieJar,
    ) -> Result<Option<Session<AdminScope>>, LookupFailed> {
        let Some(token) = cookie_token(jar) else {
            return Ok(None);
        };
        Session::<AdminScope>::of_token(sessions, token).await
    }

    /// The admin-host session `token` names, if it is one.
    pub(crate) async fn of_token(
        sessions: &SessionStore,
        token: String,
    ) -> Result<Option<Session<AdminScope>>, LookupFailed> {
        if SessionScope::of_token(&token) != SessionScope::Admin {
            return Ok(None);
        }
        Ok(resolve(sessions, &token).await?.map(|(user, _)| Session {
            token,
            user,
            scope: AdminScope,
        }))
    }

    /// The request's admin-host session, deciding the host first: on an
    /// application's host there is none.
    pub(crate) async fn of_request(
        state: &AppState,
        headers: &HeaderMap,
        jar: &CookieJar,
    ) -> Result<Option<Session<AdminScope>>, LookupFailed> {
        match resolve_app(state, headers, jar) {
            Resolved::Admin => Session::<AdminScope>::on_admin_host(&state.sessions, jar).await,
            Resolved::App(_) | Resolved::HiddenPreview => Ok(None),
        }
    }
}

/// On an application's host, the session the request carries, whoever issued
/// it (module docs): what the application serves the request as.
#[derive(Debug, Clone)]
pub(crate) enum AppSession {
    /// Issued by this application.
    Own(Session<AppScope>),
    /// Issued elsewhere, carried here by a shared sign-in cookie.
    Visiting(Session<Visiting>),
}

impl AppSession {
    /// The session `jar` carries to the application `app`.
    pub(crate) async fn of_request(
        sessions: &SessionStore,
        app: Uuid,
        jar: &CookieJar,
    ) -> Result<Option<AppSession>, LookupFailed> {
        let Some(token) = cookie_token(jar) else {
            return Ok(None);
        };
        let Some((user, issued)) = resolve(sessions, &token).await? else {
            return Ok(None);
        };
        Ok(Some(match issued {
            SessionScope::App(issuer) if issuer == app => AppSession::Own(Session {
                token,
                user,
                scope: AppScope { app },
            }),
            SessionScope::Admin | SessionScope::App(_) | SessionScope::AnyApp => {
                AppSession::Visiting(Session {
                    token,
                    user,
                    scope: Visiting,
                })
            }
        }))
    }

    /// The signed-in user.
    pub(crate) fn user(&self) -> &AuthenticatedUser {
        match self {
            AppSession::Own(s) => s.user(),
            AppSession::Visiting(s) => s.user(),
        }
    }

    /// The signed-in user, given up.
    pub(crate) fn into_user(self) -> AuthenticatedUser {
        match self {
            AppSession::Own(s) => s.into_user(),
            AppSession::Visiting(s) => s.into_user(),
        }
    }

    /// Only a session this application issued.
    pub(crate) fn own(self) -> Option<Session<AppScope>> {
        match self {
            AppSession::Own(s) => Some(s),
            AppSession::Visiting(_) => None,
        }
    }
}

/// A session issued for the host the request arrived on: the admin host's on
/// the admin host, the application's own on an application's host (module
/// docs).
#[derive(Debug, Clone)]
pub(crate) enum OwnSession {
    /// On the admin host.
    Admin(Session<AdminScope>),
    /// On an application's host, issued by it.
    App(Session<AppScope>),
}

impl OwnSession {
    /// The request's session, if it was issued for the host it arrived on. A
    /// run's hidden preview has none.
    pub(crate) async fn of_request(
        state: &AppState,
        headers: &HeaderMap,
        jar: &CookieJar,
    ) -> Result<Option<OwnSession>, LookupFailed> {
        match resolve_app(state, headers, jar) {
            Resolved::Admin => Ok(Session::<AdminScope>::on_admin_host(&state.sessions, jar)
                .await?
                .map(OwnSession::Admin)),
            Resolved::App(app) => Ok(AppSession::of_request(&state.sessions, app.app.id.0, jar)
                .await?
                .and_then(AppSession::own)
                .map(OwnSession::App)),
            Resolved::HiddenPreview => Ok(None),
        }
    }

    /// The signed-in user.
    pub(crate) fn user(&self) -> &AuthenticatedUser {
        match self {
            OwnSession::Admin(s) => s.user(),
            OwnSession::App(s) => s.user(),
        }
    }

    /// The token the request carried.
    pub(crate) fn token(&self) -> &str {
        match self {
            OwnSession::Admin(s) => s.token(),
            OwnSession::App(s) => s.token(),
        }
    }
}

/// Every session a request can carry, as the host it arrived on reads it: the
/// admin host's own, or whatever an application serves (its own, or a
/// visiting one). For a route on every host that only needs to know who is
/// asking and changes nothing about the account (a module's consent flow).
#[derive(Debug, Clone)]
pub(crate) enum HostSession {
    /// On the admin host.
    Admin(Session<AdminScope>),
    /// On an application's host.
    App(AppSession),
}

impl HostSession {
    /// The request's session, as the host it arrived on reads it.
    pub(crate) async fn of_request(
        state: &AppState,
        headers: &HeaderMap,
        jar: &CookieJar,
    ) -> Result<Option<HostSession>, LookupFailed> {
        match resolve_app(state, headers, jar) {
            Resolved::Admin => Ok(Session::<AdminScope>::on_admin_host(&state.sessions, jar)
                .await?
                .map(HostSession::Admin)),
            Resolved::App(app) => Ok(AppSession::of_request(&state.sessions, app.app.id.0, jar)
                .await?
                .map(HostSession::App)),
            Resolved::HiddenPreview => Ok(None),
        }
    }

    /// The signed-in user.
    pub(crate) fn user(&self) -> &AuthenticatedUser {
        match self {
            HostSession::Admin(s) => s.user(),
            HostSession::App(s) => s.user(),
        }
    }
}

/// Re-read the session a long-lived connection was opened with (an
/// application's live socket, an admin socket): the user it names now, or
/// `None` once it has ended. The scope was checked when the connection was
/// opened, and a token's scope does not change.
pub(crate) async fn revalidate(
    sessions: &SessionStore,
    token: &str,
) -> sc_error::Result<Option<AuthenticatedUser>> {
    sessions.user_for(token).await
}

impl OptionalFromRequestParts<AppState> for Session<AdminScope> {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Option<Session<AdminScope>>, Response> {
        let jar = CookieJar::from_headers(&parts.headers);
        Session::<AdminScope>::of_request(state, &parts.headers, &jar)
            .await
            .map_err(IntoResponse::into_response)
    }
}

impl OptionalFromRequestParts<AppState> for Session<AppScope> {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Option<Session<AppScope>>, Response> {
        let jar = CookieJar::from_headers(&parts.headers);
        match resolve_app(state, &parts.headers, &jar) {
            Resolved::App(app) => AppSession::of_request(&state.sessions, app.app.id.0, &jar)
                .await
                .map(|found| found.and_then(AppSession::own))
                .map_err(IntoResponse::into_response),
            Resolved::Admin | Resolved::HiddenPreview => Ok(None),
        }
    }
}

impl OptionalFromRequestParts<AppState> for OwnSession {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Option<OwnSession>, Response> {
        let jar = CookieJar::from_headers(&parts.headers);
        OwnSession::of_request(state, &parts.headers, &jar)
            .await
            .map_err(IntoResponse::into_response)
    }
}

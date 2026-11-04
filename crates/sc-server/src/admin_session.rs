//! Admin authority as a value (code quality Q2, review §4.3).
//!
//! Before this, every admin-only route outside the endpoint set was written
//! as three calls in a row — `admin_host_only`, `session_user`,
//! `enforce_auth(admin)` — and security review #15 was one of them forgotten
//! in four places. Afterwards the route held an `Option<User>`, so nothing in
//! its type said it had been checked.
//!
//! - [`AdminSession`] is the only way a request becomes admin authority. Its
//!   one constructor ([`AdminSession::of_request`], also its axum
//!   [`FromRequestParts`] impl) checks all of it: the request is on the admin
//!   host (not an application's, not a preview's), it carries a live session,
//!   the session was **issued on the admin host** ([`SessionScope`], security
//!   review #15's open item), and the user is an administrator. A route that
//!   takes one cannot be reached otherwise.
//! - [`Admin`] is what an admin endpoint's handler is handed: an
//!   [`AdminSession`], or the in-process admin tool layers (the MCP server's
//!   endpoint tools and the catalog's admin host), which have run their own
//!   admin check ([`Admin::delegated`]; listed in the security review's
//!   system-authority inventory). The handler registry only calls a handler
//!   registered with [`register_admin`](crate::HandlerRegistry::register_admin)
//!   with one, and the router refuses to start when an admin endpoint names a
//!   handler that was registered any other way.
//! - [`Caller`] is what the dispatcher resolved a request to before the
//!   handler's gate narrows it: nobody, a signed-in user
//!   ([`AuthenticatedUser`]), or admin authority.
//! - A long-lived socket holds its [`AdminSession`] and [`AdminWatch`]es it:
//!   a sign-out, a disabled or deleted account, a demotion — anything that
//!   ends the session or its admin role — closes the socket (security review
//!   #42's class).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::CookieJar;
use sc_api::AuthRequirement;
use sc_auth::{AuthenticatedUser, SessionEnded, SessionListener, SessionScope, SessionStore, User};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::router::{AppState, Resolved, enforce_auth, json_error, log_failure, resolve_app};
use crate::security::SESSION_COOKIE;

/// A request's admin session: on the admin host, live, issued on the admin
/// host, and an administrator's (module docs). Constructed only by
/// [`AdminSession::of_request`].
#[derive(Clone)]
pub struct AdminSession {
    user: AuthenticatedUser,
    token: String,
}

impl std::fmt::Debug for AdminSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is a bearer credential: never in a log line.
        f.debug_struct("AdminSession")
            .field("user", &self.user.id())
            .finish_non_exhaustive()
    }
}

/// Why a request is not an admin session, ready to answer with.
pub(crate) enum AdminRefusal {
    /// Not the admin host: an application's or a preview's, where an admin
    /// route is nothing (`404`).
    NotHere,
    /// Nobody is signed in on the admin host — no cookie, a dead session, or
    /// a session an application issued (`401`).
    Anonymous,
    /// Signed in, not an administrator (`403`).
    NotAdmin(Box<User>),
    /// The session store failed (`500`).
    Failed,
}

impl AdminRefusal {
    /// Whether nobody is signed in: a navigation is sent to the admin UI's
    /// sign-in rather than shown the refusal.
    pub(crate) fn is_anonymous(&self) -> bool {
        matches!(self, AdminRefusal::Anonymous)
    }
}

impl IntoResponse for AdminRefusal {
    fn into_response(self) -> Response {
        match self {
            AdminRefusal::NotHere => {
                json_error(StatusCode::NOT_FOUND, "there is nothing at this path")
            }
            // The same two answers `dispatch` gives an admin endpoint, in the
            // caller's language, so no route that takes an `AdminSession`
            // answers differently from a typed endpoint.
            AdminRefusal::Anonymous => enforce_auth(&AuthRequirement::admin(), None)
                .unwrap_or_else(|| json_error(StatusCode::UNAUTHORIZED, "unauthorized")),
            AdminRefusal::NotAdmin(user) => enforce_auth(&AuthRequirement::admin(), Some(&user))
                .unwrap_or_else(|| json_error(StatusCode::FORBIDDEN, "forbidden")),
            AdminRefusal::Failed => {
                json_error(StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed")
            }
        }
    }
}

impl AdminSession {
    /// The administrator.
    pub fn user(&self) -> &AuthenticatedUser {
        &self.user
    }

    /// The session token the request carried — what a response that leaves
    /// the session alone hands back to the cookie logic.
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    /// The admin session `headers` and `jar` carry, or why there is none.
    ///
    /// The host is decided first: an admin route on an application's host is
    /// nothing at all, whoever asks (security review #15).
    pub(crate) async fn of_request(
        state: &AppState,
        headers: &axum::http::HeaderMap,
        jar: &CookieJar,
    ) -> Result<AdminSession, AdminRefusal> {
        match resolve_app(state, headers, jar) {
            Resolved::Admin => {}
            Resolved::App(_) | Resolved::HiddenPreview => return Err(AdminRefusal::NotHere),
        }
        let Some(token) = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned()) else {
            return Err(AdminRefusal::Anonymous);
        };
        match admin_host_session(&state.sessions, &token).await? {
            Some(user) if user.is_admin() => Ok(AdminSession { user, token }),
            Some(user) => Err(AdminRefusal::NotAdmin(Box::new(user.into_user()))),
            None => Err(AdminRefusal::Anonymous),
        }
    }

    /// The session as it is now: still this administrator's, or lost.
    pub(crate) async fn revalidate(&self, sessions: &SessionStore) -> Revalidated {
        match sessions.user_for(&self.token).await {
            Ok(Some(user)) if user.id() == self.user.id() && user.is_admin() => {
                Revalidated::Admin(AdminSession {
                    user,
                    token: self.token.clone(),
                })
            }
            Ok(Some(user)) if user.id() == self.user.id() => Revalidated::Lost(
                "this account is no longer an administrator, so the connection is closed"
                    .to_owned(),
            ),
            Ok(_) => Revalidated::Lost(SIGNED_OUT.to_owned()),
            Err(e) => {
                sc_log::log_warn!(
                    "an admin socket could not re-read its session: {}",
                    sc_error::format_chain(&e)
                );
                Revalidated::Unknown
            }
        }
    }

    /// Watch this session on `sessions` for as long as the watch lives: told
    /// at once when the store ends it, and re-read every
    /// [`SESSION_RECHECK`] for what the store does not announce (a demotion,
    /// a sign-out on another node).
    pub(crate) fn watch(&self, sessions: Arc<SessionStore>) -> AdminWatch {
        let ear = Arc::new(Ear {
            token: self.token.clone(),
            user: self.user.id(),
            ended: AtomicBool::new(false),
            notify: Notify::new(),
        });
        let listener: Arc<dyn SessionListener> = ear.clone();
        sessions.add_listener(Arc::downgrade(&listener));
        AdminWatch {
            session: self.clone(),
            sessions,
            ear,
        }
    }
}

#[cfg(test)]
impl AdminSession {
    /// A new administrator's session, signed in on `sessions` as a request
    /// would be, for a unit test that needs one without a request.
    pub(crate) async fn signed_in(sessions: &SessionStore) -> AdminSession {
        let admin = User::new(Uuid::new_v4(), sc_types::Role::ADMIN);
        let token = sessions.login(admin).await.unwrap();
        let user = sessions.user_for(&token).await.unwrap().unwrap();
        AdminSession { user, token }
    }
}

impl FromRequestParts<AppState> for AdminSession {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<AdminSession, Response> {
        let jar = CookieJar::from_headers(&parts.headers);
        AdminSession::of_request(state, &parts.headers, &jar)
            .await
            .map_err(IntoResponse::into_response)
    }
}

/// The user an admin-host session token names, or `None` when it names
/// nobody **or was issued on an application's host**: on the admin host an
/// application's session is not a session (security review #15).
pub(crate) async fn admin_host_session(
    sessions: &SessionStore,
    token: &str,
) -> Result<Option<AuthenticatedUser>, AdminRefusal> {
    if SessionScope::of_token(token) != SessionScope::Admin {
        return Ok(None);
    }
    sessions.user_for(token).await.map_err(|e| {
        log_failure("session lookup failed", &e);
        AdminRefusal::Failed
    })
}

/// What a re-read of an admin session found.
pub(crate) enum Revalidated {
    /// Still an administrator's session: the user as they are now.
    Admin(AdminSession),
    /// Ended, or no longer an administrator's, said as a close frame says it.
    Lost(String),
    /// The store could not answer.
    Unknown,
}

/// How often a socket holding an [`AdminSession`] re-reads it between the
/// store's own notices: what reaches a demotion, and a sign-out on another
/// node.
pub(crate) const SESSION_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// The close reason for a session that has ended.
pub(crate) const SIGNED_OUT: &str =
    "your session ended, so the connection is closed; sign in again";

/// An [`AdminSession`] a socket holds open, watched (see
/// [`AdminSession::watch`]).
pub(crate) struct AdminWatch {
    session: AdminSession,
    sessions: Arc<SessionStore>,
    ear: Arc<Ear>,
}

/// The [`SessionListener`] an [`AdminWatch`] registers: told synchronously by
/// the store, it only raises a flag and wakes the socket's task.
struct Ear {
    token: String,
    user: Uuid,
    ended: AtomicBool,
    notify: Notify,
}

impl SessionListener for Ear {
    fn session_ended(&self, ended: &SessionEnded<'_>) {
        let mine = ended.covers(Some(&self.token), Some(self.user));
        if mine {
            self.ended.store(true, Ordering::SeqCst);
            self.notify.notify_one();
        }
    }
}

impl AdminWatch {
    /// The session as it was last found valid.
    pub(crate) fn session(&self) -> &AdminSession {
        &self.session
    }

    /// Re-read the session now. A session the store has announced ended is
    /// lost without asking it.
    pub(crate) async fn now(&self) -> Revalidated {
        if self.ear.ended.load(Ordering::SeqCst) {
            return Revalidated::Lost(SIGNED_OUT.to_owned());
        }
        self.session.revalidate(&self.sessions).await
    }

    /// Wait until the session is lost, and say why. A store that cannot answer
    /// leaves the socket open until the next re-read, as the live socket
    /// does: a database that blinked is not a sign-out.
    pub(crate) async fn lost(&self) -> String {
        loop {
            tokio::select! {
                () = self.ear.notify.notified() => {}
                () = tokio::time::sleep(SESSION_RECHECK) => {}
            }
            if let Revalidated::Lost(reason) = self.now().await {
                return reason;
            }
        }
    }

    /// Serve `socket` until it finishes on its own or the session is lost,
    /// whichever is first. A lost session drops the socket's future, which
    /// closes the connection (and, for the language server, kills its
    /// process).
    pub(crate) async fn guard<F: std::future::Future<Output = ()>>(self, socket: F) {
        tokio::select! {
            () = socket => {}
            reason = self.lost() => {
                sc_log::log_info!("closing an admin socket: {reason}");
            }
        }
    }
}

/// Admin authority, as an admin endpoint's handler receives it (module docs).
#[derive(Clone)]
pub struct Admin(Via);

#[derive(Clone)]
enum Via {
    /// A browser's admin session.
    Session(AdminSession),
    /// An in-process admin tool layer that has made its own admin check.
    Delegated {
        user: Option<User>,
        by: &'static str,
    },
}

impl std::fmt::Debug for Admin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Via::Session(session) => f.debug_tuple("Admin").field(session).finish(),
            Via::Delegated { user, by } => f
                .debug_struct("Admin")
                .field("user", &user.as_ref().map(User::id))
                .field("by", by)
                .finish(),
        }
    }
}

impl Admin {
    /// Admin authority an in-process tool layer vouches for: the
    /// administration MCP server's endpoint tools (`require_admin` and the
    /// endpoint's own requirement, `sc_api::mcp::check_caller`) and the
    /// catalog's admin host (`sc_catalog::AdminHost`, whose callers are admin
    /// tools behind the same check). `by` names which, for the inventory in
    /// docs/SECURITY_REVIEW_2026-10.md.
    pub(crate) fn delegated(user: Option<User>, by: &'static str) -> Admin {
        Admin(Via::Delegated { user, by })
    }

    /// The administrator, where there is one: a delegated call made with no
    /// user (a system-run tool) has none.
    pub fn user(&self) -> Option<&User> {
        match &self.0 {
            Via::Session(session) => Some(session.user()),
            Via::Delegated { user, .. } => user.as_ref(),
        }
    }

    /// The browser session behind this authority, when it is one.
    pub fn session(&self) -> Option<&AdminSession> {
        match &self.0 {
            Via::Session(session) => Some(session),
            Via::Delegated { .. } => None,
        }
    }

    /// The signed-in administrator, when this is a browser session.
    fn authenticated(self) -> Option<AuthenticatedUser> {
        match self.0 {
            Via::Session(session) => Some(session.user),
            Via::Delegated { .. } => None,
        }
    }
}

impl From<AdminSession> for Admin {
    fn from(session: AdminSession) -> Admin {
        Admin(Via::Session(session))
    }
}

/// Who a request to a handler comes from, as the dispatcher resolved it,
/// before the handler's gate narrows it (module docs).
#[derive(Debug, Clone)]
pub enum Caller {
    /// Nobody is signed in.
    Anonymous,
    /// A signed-in user without admin authority here.
    User(AuthenticatedUser),
    /// Admin authority.
    Admin(Admin),
}

impl Caller {
    /// The caller an admin-host request's session cookie makes: admin
    /// authority for an administrator's admin-host session, a user for anybody
    /// else's, and nobody for no session — or for a session an application
    /// issued, which is not a session on the admin host.
    pub(crate) async fn on_admin_host(
        state: &AppState,
        jar: &CookieJar,
    ) -> Result<Caller, Box<Response>> {
        let Some(token) = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned()) else {
            return Ok(Caller::Anonymous);
        };
        let user = admin_host_session(&state.sessions, &token)
            .await
            .map_err(|refusal| Box::new(refusal.into_response()))?;
        Ok(match user {
            Some(user) if user.is_admin() => {
                Caller::Admin(Admin::from(AdminSession { user, token }))
            }
            Some(user) => Caller::User(user),
            None => Caller::Anonymous,
        })
    }

    /// The caller a signed-in (or anonymous) user of an application makes:
    /// never admin authority, whatever the role — that is the admin host's
    /// alone.
    pub(crate) fn in_app(user: Option<AuthenticatedUser>) -> Caller {
        match user {
            Some(user) => Caller::User(user),
            None => Caller::Anonymous,
        }
    }

    /// Narrow to admin authority, or say why not.
    pub(crate) fn into_admin(self) -> sc_error::Result<Admin> {
        match self {
            Caller::Admin(admin) => Ok(admin),
            Caller::User(_) | Caller::Anonymous => Err(not_admin()),
        }
    }

    /// Narrow to an Analytics caller: through an Analytics application
    /// (`in_app`) whoever its router admitted, and on the admin host admin
    /// authority alone.
    pub(crate) fn into_analytics(self, in_app: bool) -> sc_error::Result<AnalyticsCaller> {
        if !in_app {
            return self.into_admin().map(AnalyticsCaller::Admin);
        }
        Ok(AnalyticsCaller::App(match self {
            Caller::User(user) => Some(user),
            Caller::Admin(admin) => admin.authenticated(),
            Caller::Anonymous => None,
        }))
    }

    /// Narrow to a signed-in user, or say why not.
    pub(crate) fn into_signed_in(self) -> sc_error::Result<AuthenticatedUser> {
        match self {
            Caller::User(user) => Ok(user),
            Caller::Admin(admin) => admin
                .authenticated()
                .ok_or_else(|| sc_error::Error::auth("this endpoint requires a signed-in user")),
            Caller::Anonymous => Err(sc_error::Error::auth("authentication required")),
        }
    }
}

/// The refusal a handler gate gives a caller without admin authority. The
/// dispatcher refuses such a caller first, so reaching this is a dispatch
/// path that skipped the check — answered as the check would have.
fn not_admin() -> sc_error::Error {
    sc_error::Error::auth("this endpoint requires an administrator's session on the admin host")
}

/// What an Analytics endpoint's handler receives: admin authority on the admin
/// host, or the caller of an Analytics application on its own host, whom the
/// router has already held to the application's role floor (analytics TODO
/// A9.3).
#[derive(Debug, Clone)]
pub enum AnalyticsCaller {
    /// The admin host's Analytics UI.
    Admin(Admin),
    /// An Analytics application's visitor, signed in or not.
    App(Option<AuthenticatedUser>),
}

/// Who a handler's context says is calling: the one question every gate's
/// caller answers the same way.
pub trait Who: Clone + std::fmt::Debug + Send + Sync + 'static {
    /// The user, when there is one.
    fn user(&self) -> Option<&User>;
}

impl Who for Caller {
    fn user(&self) -> Option<&User> {
        match self {
            Caller::Anonymous => None,
            Caller::User(user) => Some(user),
            Caller::Admin(admin) => admin.user(),
        }
    }
}

impl Who for Admin {
    fn user(&self) -> Option<&User> {
        Admin::user(self)
    }
}

impl Who for AuthenticatedUser {
    fn user(&self) -> Option<&User> {
        Some(self)
    }
}

impl Who for AnalyticsCaller {
    fn user(&self) -> Option<&User> {
        match self {
            AnalyticsCaller::Admin(admin) => admin.user(),
            AnalyticsCaller::App(user) => user.as_deref(),
        }
    }
}

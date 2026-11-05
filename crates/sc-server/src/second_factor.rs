//! Two-factor authentication over HTTP (design §7.2): the challenge a sign-in
//! is given in place of a session, the routes that answer it, and the ones an
//! account sets its second factor up and turns it off with.
//!
//! Real routes under [`PREFIX`], answering on **every host** as the OAuth ones
//! do, rather than endpoints of each application's API: a second factor belongs
//! to the account, and the account is the instance's, whichever application it
//! signed in to. So the admin UI, a React application's generated client and a
//! Saltcorn UI application all use the same few routes.
//!
//! # The challenge
//!
//! [`apply_session`](crate::router::apply_session) is where every session
//! starts. A sign-in by an account that has a second factor on, or whose role
//! requires one, stops there ([`challenge_for`]): instead of a session it is
//! given a **challenge token** — single-use, ten minutes, in
//! `_fd_password_tokens` so whichever node the answer reaches can read it — in
//! a cookie of its own ([`CHALLENGE_COOKIE`]) and, for a client's code, in the
//! JSON it reads:
//!
//! ```json
//! { "id": "…", "email": "…", "role": 1, "email_verified": true,
//!   "second_factor": { "token": "…", "setup": false } }
//! ```
//!
//! No session cookie is set. `setup: true` means the account's role requires a
//! second factor it has not set up, and the token is what lets it set one up
//! ([`SETUP_ROUTE`], then [`ENABLE_ROUTE`] with the first code). Otherwise the
//! code goes to [`VERIFY_ROUTE`]. Either one starts the session the password
//! would have, with its "Remember me".
//!
//! A sign-in made by a page rather than by a client's code — a Saltcorn UI
//! application's form, an identity provider's redirect — is instead sent to
//! [`PAGE_ROUTE`], a page of its own that asks for the code (or sets one up and
//! shows the recovery codes) and then goes on to where the sign-in was going.
//!
//! # Guessing
//!
//! A code is counted against the account before it is checked
//! ([`sc_auth::reserve_attempt`], in the database, so every node shares the
//! budget): five wrong ones in fifteen minutes and it is refused with a `429`
//! until the window passes, which makes guessing a six-digit code that lasts
//! 30 seconds hopeless. Every check takes the [`sc_auth::AttemptPermit`] by
//! value (security review 2026-11, H2.4).
//!
//! # Whose session
//!
//! The routes that change an account's second factor take an
//! [`OwnSession`]: a session issued for the host the request arrived on
//! (security review 2026-11, #3 and §2). A session a shared sign-in cookie
//! carries to an application's host from the admin host or from another
//! application is not one, so an application's script cannot change the
//! second factor of an account that only visited it.

// `clippy::result_large_err` does not apply: every `Err` here is the response to
// send, returned unchanged by the route handler (as in `builder.rs`).
#![allow(clippy::result_large_err)]

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use sc_api::SessionAction;
use sc_api::auth::user_summary_json;
use sc_auth::{
    ChallengeToken, CredentialsChanged, FullyAuthenticated, PasswordVerified, SecondFactor,
    SecondFactorNeeded, SecondFactorProof, User,
};
use sc_catalog::Catalog;
use sc_expr::html::SafeHtml;
use sc_expr::safe_html;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::router::{
    AppState, Resolved, SessionClient, apply_session, json_error, log_failure, parse_form,
    parse_query, resolve_app,
};
use crate::security::{CsrfToken, SESSION_COOKIE};
use crate::session::OwnSession;

/// Where the routes live.
pub const PREFIX: &str = "/_feldspar/second-factor";
/// `GET`: the signed-in account's second factor — `{enabled, required,
/// recovery_codes_left}`.
pub const STATUS_ROUTE: &str = "/_feldspar/second-factor";
/// `POST {token?, password?}`: a new secret to scan — `{secret, otpauth_url,
/// qr_svg}`. Signed in rather than with a challenge, the current `password`.
pub const SETUP_ROUTE: &str = "/_feldspar/second-factor/setup";
/// `POST {code, token?}`: turn it on with the first code — `{recovery_codes}`,
/// and with a challenge, the session too (`user`).
pub const ENABLE_ROUTE: &str = "/_feldspar/second-factor/enable";
/// `POST {token, code}`: answer a challenge — the user summary, and a session.
pub const VERIFY_ROUTE: &str = "/_feldspar/second-factor/verify";
/// `POST {code}`: turn it off.
pub const DISABLE_ROUTE: &str = "/_feldspar/second-factor/disable";
/// `POST {code}`: new recovery codes in place of the old.
pub const RECOVERY_CODES_ROUTE: &str = "/_feldspar/second-factor/recovery-codes";
/// `GET ?return_to=`: the page a browser answers a challenge on.
pub const PAGE_ROUTE: &str = "/_feldspar/second-factor/sign-in";

/// The cookie a challenge rides in: `Lax`, because a sign-in by an identity
/// provider reaches the page at the end of a redirect chain that began at the
/// provider, and scoped to [`PREFIX`], the only routes that read it.
pub const CHALLENGE_COOKIE: &str = "sc_2fa";

// --- the challenge -----------------------------------------------------------

/// The second-factor decision for a sign-in whose first factor checked out:
/// the sign-in, fully authenticated, when the account needs no second factor
/// — or (`Err`) the challenge to answer with instead of a session.
///
/// Fails closed: a database that cannot say whether the account has a second
/// factor starts no session.
pub(crate) async fn challenge_for(
    state: &AppState,
    client: &SessionClient,
    signing_in: PasswordVerified,
    remember: bool,
) -> Result<FullyAuthenticated, Response> {
    let could_not = |context: &str, e: &sc_error::Error| {
        log_failure(context, e);
        json_error(StatusCode::INTERNAL_SERVER_ERROR, "could not start session")
    };
    let catalog = state
        .apps
        .catalog()
        .ok_or_else(|| json_error(StatusCode::INTERNAL_SERVER_ERROR, "could not start session"))?;
    let needed = sc_auth::second_factor_needed(catalog, signing_in.user())
        .await
        .map_err(|e| could_not("could not read an account's second factor", &e))?;
    let setup = match needed {
        SecondFactorNeeded::No(none) => {
            return signing_in
                .complete(SecondFactor::NotNeeded(none))
                .map_err(|e| could_not("could not finish a sign-in", &e));
        }
        SecondFactorNeeded::Code => false,
        SecondFactorNeeded::Setup => true,
    };
    let token = sc_auth::issue_second_factor_challenge(catalog, &signing_in, remember)
        .await
        .map_err(|e| could_not("could not issue a second-factor challenge", &e))?;
    let user = signing_in.user();
    let jar = CookieJar::new().add(challenge_cookie(state, token.expose().to_owned()));
    let mut out = match &client.page_return {
        Some(return_to) => (
            jar,
            Redirect::to(&format!(
                "{PAGE_ROUTE}?return_to={}",
                encode(crate::oauth::safe_return_to(Some(return_to)).as_str())
            )),
        )
            .into_response(),
        None => {
            let mut body = user_summary_json(user);
            body["second_factor"] = json!({
                "token": token.expose(),
                "setup": setup,
            });
            (StatusCode::OK, jar, Json(body)).into_response()
        }
    };
    no_store(&mut out);
    Err(out)
}

fn challenge_cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build((CHALLENGE_COOKIE, token))
        .path(PREFIX)
        .same_site(SameSite::Lax)
        .http_only(true)
        .secure(state.secure_cookies)
        .max_age(time::Duration::minutes(sc_auth::SECOND_FACTOR_TTL_MINUTES))
        .build()
}

fn challenge_removal() -> Cookie<'static> {
    let mut cookie = Cookie::build((CHALLENGE_COOKIE, "")).path(PREFIX).build();
    cookie.make_removal();
    cookie
}

fn no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

// --- reading a request ------------------------------------------------------

/// A request's fields: a JSON body's, or a form's (the page posts a form).
struct Fields {
    values: Map<String, Value>,
    form: bool,
}

impl Fields {
    fn read(headers: &HeaderMap, body: &Bytes) -> Fields {
        let form = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
        let values = if form {
            parse_form(&String::from_utf8_lossy(body))
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect()
        } else {
            match serde_json::from_slice::<Value>(body) {
                Ok(Value::Object(map)) => map,
                _ => Map::new(),
            }
        };
        Fields { values, form }
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    }

    /// The challenge: the body's `token`, else the cookie's.
    fn challenge(&self, jar: &CookieJar) -> Option<ChallengeToken> {
        self.text("token")
            .map(str::to_owned)
            .or_else(|| {
                jar.get(CHALLENGE_COOKIE)
                    .map(|c| c.value().to_owned())
                    .filter(|v| !v.is_empty())
            })
            .map(ChallengeToken::presented)
    }

    fn return_to(&self) -> String {
        crate::oauth::safe_return_to(self.text("return_to")).into_string()
    }
}

/// The session the request carries, if it was issued for the host it arrived
/// on ([`OwnSession`]): an application's session on its own application's
/// host, an admin session on the admin host. Off an application's host, an
/// application's session is no session (security review #15): it must not
/// change the second factor an admin signs in with. And on an application's
/// host a session issued elsewhere — the admin host's or another
/// application's, sent there when sign-in is shared across the base domain —
/// is no session either (security review 2026-11, #3 and §2): the
/// application's script, which that account's owner did not necessarily
/// write, must not enroll a second factor on whoever visits it.
async fn signed_in(
    state: &AppState,
    headers: &HeaderMap,
    jar: &CookieJar,
) -> Result<Option<OwnSession>, Response> {
    OwnSession::of_request(state, headers, jar)
        .await
        .map_err(IntoResponse::into_response)
}

fn catalog_of(state: &AppState) -> Result<&Catalog, Response> {
    state.apps.catalog().map(|c| &**c).ok_or_else(|| {
        json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "this server has no database",
        )
    })
}

fn failed(context: &str, e: &sc_error::Error) -> Response {
    match e.repr() {
        // A refusal the account can act on ("already on") is said as it is.
        sc_error::Repr::Invalid(message) => json_error(StatusCode::BAD_REQUEST, message.clone()),
        _ => {
            log_failure(context, e);
            json_error(StatusCode::INTERNAL_SERVER_ERROR, context.to_owned())
        }
    }
}

/// The socket's peer, when the listener attached one.
type Peer = Option<Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>;

fn ip_of(state: &AppState, peer: Peer, headers: &HeaderMap) -> Option<std::net::IpAddr> {
    crate::router::request_client_ip(state, peer.map(|p| p.0.0), headers)
}

/// The `429` for a code that may not be checked yet.
fn too_many(secs: u64) -> Response {
    let mut out = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "error": "too many wrong codes; try again later", "retry_after": secs })),
    )
        .into_response();
    out.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    out
}

/// Count a code for `user_id` before it is checked — against the IP, and as
/// one of the account's guesses (kept in the database, so shared by every
/// node) — or the `429` to answer instead.
async fn reserve(
    catalog: &Catalog,
    ip: Option<std::net::IpAddr>,
    user_id: Uuid,
) -> Result<sc_auth::AttemptPermit, Response> {
    sc_api::throttle::second_factor_attempt(ip)
        .map_err(|limited| too_many(limited.retry_after_secs()))?;
    match sc_auth::reserve_attempt(catalog, user_id).await {
        Ok(sc_auth::Attempt::Permitted(permit)) => Ok(permit),
        Ok(sc_auth::Attempt::Limited { retry_after_secs }) => Err(too_many(retry_after_secs)),
        Err(e) => Err(failed("could not check the code", &e)),
    }
}

/// `code` checked for `user_id`: the proof when it was right. The guess is
/// counted before the check and given back when it was right.
async fn check_code(
    catalog: &Catalog,
    ip: Option<std::net::IpAddr>,
    user_id: Uuid,
    code: &str,
) -> Result<Option<SecondFactorProof>, Response> {
    let permit = reserve(catalog, ip, user_id).await?;
    sc_auth::verify_second_factor(catalog, permit, user_id, code)
        .await
        .map_err(|e| failed("could not check the code", &e))
}

/// Apply a credential change the account made itself from `session`: every
/// other session of the account ends, and this one goes on at the new epoch
/// (security review 2026-11, #11 and H2.3).
async fn changed_here(
    state: &AppState,
    changed: CredentialsChanged,
    session: &OwnSession,
) -> Result<(), Response> {
    state
        .sessions
        .bump_epoch(changed, Some(session.token()))
        .await
        .map(|_| ())
        .map_err(|e| failed("could not end the account's other sessions", &e))
}

fn wrong_code() -> Response {
    json_error(StatusCode::UNAUTHORIZED, "that code is not right")
}

fn not_signed_in() -> Response {
    json_error(StatusCode::UNAUTHORIZED, "sign in first")
}

fn expired() -> Response {
    json_error(
        StatusCode::UNAUTHORIZED,
        "this sign-in has expired; sign in again",
    )
}

/// Who a request acts for: a challenge's account (and its "Remember me"), or
/// the signed-in account.
enum Actor {
    Challenge {
        pending: sc_auth::PendingChallenge,
        token: ChallengeToken,
    },
    SignedIn(OwnSession),
}

impl Actor {
    fn user_id(&self) -> Uuid {
        match self {
            Actor::Challenge { pending, .. } => pending.user,
            Actor::SignedIn(session) => session.user().id(),
        }
    }
}

async fn actor(
    state: &AppState,
    catalog: &Catalog,
    headers: &HeaderMap,
    fields: &Fields,
    jar: &CookieJar,
) -> Result<Actor, Response> {
    if let Some(token) = fields.challenge(jar) {
        match sc_auth::second_factor_challenge(catalog, &token).await {
            Ok(Some(pending)) => {
                return Ok(Actor::Challenge { pending, token });
            }
            Ok(None) if fields.text("token").is_some() => return Err(expired()),
            Ok(None) => {}
            Err(e) => return Err(failed("could not read the sign-in", &e)),
        }
    }
    match signed_in(state, headers, jar).await? {
        Some(session) => Ok(Actor::SignedIn(session)),
        None if fields.challenge(jar).is_some() => Err(expired()),
        None => Err(not_signed_in()),
    }
}

/// Spend a challenge whose code was right (`proof`), and start the session it
/// was waiting for. `made_here` is the second factor this sign-in has just
/// set up, which the challenge may outlive by that one change. `Err` is the
/// response to send instead.
async fn finish_sign_in(
    state: &AppState,
    catalog: &Catalog,
    headers: &HeaderMap,
    jar: CookieJar,
    token: &ChallengeToken,
    proof: SecondFactorProof,
    made_here: Option<&CredentialsChanged>,
) -> Result<(crate::router::SessionCookies, User), Response> {
    let spent = sc_auth::spend_second_factor_challenge(catalog, token, made_here)
        .await
        .map_err(|e| failed("could not finish the sign-in", &e))?;
    let Some(spent) = spent else {
        return Err(expired());
    };
    let remember = spent.remember;
    let who = spent
        .signing_in
        .complete(SecondFactor::Proof(proof))
        .map_err(|_| expired())?;
    let user = who.user().clone();
    let client = match resolve_app(state, headers, &jar) {
        Resolved::App(app) => SessionClient::of(headers).for_app(&app),
        _ => SessionClient::of(headers),
    };
    let session_token = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
    let jar = jar.remove(challenge_removal());
    let cookies = apply_session(
        state,
        jar,
        session_token,
        client,
        SessionAction::StartVerified { who, remember },
    )
    .await
    .map_err(|rejection| *rejection)?;
    Ok((cookies, user))
}

// --- the routes --------------------------------------------------------------

/// `GET` [`STATUS_ROUTE`].
///
/// The session is the route's extractor: a session issued for this host, or
/// nobody ([`OwnSession`]).
pub(crate) async fn status(State(state): State<AppState>, session: Option<OwnSession>) -> Response {
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Some(session) = session else {
        return not_signed_in();
    };
    let user = session.user();
    let status = match sc_auth::second_factor_status(catalog, user.id()).await {
        Ok(s) => s,
        Err(e) => return failed("could not read the second factor", &e),
    };
    let required = match sc_auth::role_requires_second_factor(catalog, user.role()).await {
        Ok(r) => r,
        Err(e) => return failed("could not read the role", &e),
    };
    let mut out = Json(json!({
        "enabled": status.enabled,
        "required": required,
        "recovery_codes_left": status.recovery_codes_left,
    }))
    .into_response();
    no_store(&mut out);
    out
}

/// The name an app lists this account under.
async fn issuer(state: &AppState, catalog: &Catalog, headers: &HeaderMap) -> String {
    if let Ok(Some(issuer)) = sc_config::second_factor_issuer(catalog).await {
        return issuer;
    }
    if let Some(base) = state.base_domain.as_deref() {
        return base.to_string();
    }
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(crate::admin_host::hostname)
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "Feldspar".to_owned())
}

/// A new secret for `user_id`, as an app is shown it.
async fn setup_for(
    state: &AppState,
    catalog: &Catalog,
    headers: &HeaderMap,
    user_id: Uuid,
) -> Result<Value, Response> {
    let secret = sc_auth::begin_second_factor_setup(catalog, user_id)
        .await
        .map_err(|e| failed("could not set up two-factor authentication", &e))?
        .expose_base32();
    let account = match sc_auth::load_user(catalog, user_id).await {
        Ok(Some(user)) => sc_auth::user_email(&user)
            .map(str::to_owned)
            .unwrap_or_else(|| user.id().to_string()),
        Ok(None) => return Err(expired()),
        Err(e) => return Err(failed("could not set up two-factor authentication", &e)),
    };
    let url = sc_auth::otpauth_url(&issuer(state, catalog, headers).await, &account, &secret);
    let svg = sc_auth::qr_svg(&url).map_err(|e| failed("could not make the QR code", &e))?;
    Ok(json!({ "secret": secret, "otpauth_url": url, "qr_svg": svg, "account": account }))
}

/// That the signed-in `user` gave their current password as `password`: what
/// setting a second factor up from a session takes. Held to the login form's
/// budget (per IP, and wrong passwords per address), so it is no better a
/// place to guess the password than signing in is.
async fn reauthenticated(
    state: &AppState,
    catalog: &Catalog,
    peer: Peer,
    headers: &HeaderMap,
    user: &User,
    fields: &Fields,
) -> Result<(), Response> {
    let Some(password) = fields.text("password") else {
        return Err(json_error(
            StatusCode::UNAUTHORIZED,
            "enter your current password to set up two-factor authentication",
        ));
    };
    let Some(email) = sc_auth::user_email(user) else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "this account has no email address to check a password for",
        ));
    };
    sc_api::throttle::login_attempt(ip_of(state, peer, headers), email)
        .map_err(|limited| too_many(limited.retry_after_secs()))?;
    // Only that the password is right: this is no sign-in, so what
    // `authenticate` hands back is dropped rather than completed.
    let right = sc_auth::authenticate(catalog, email, password)
        .await
        .map_err(|e| failed("could not check the password", &e))?
        .is_some_and(|found| found.user().id() == user.id());
    if !right {
        sc_api::throttle::login_failed(email);
    }
    if right {
        Ok(())
    } else {
        Err(json_error(
            StatusCode::UNAUTHORIZED,
            "that password is not right",
        ))
    }
}

/// `POST` [`SETUP_ROUTE`]: `{password}` when signed in, `{token}` with a
/// challenge.
pub(crate) async fn setup(
    State(state): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    jar: CookieJar,
    body: Bytes,
) -> Response {
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let fields = Fields::read(&headers, &body);
    let actor = match actor(&state, catalog, &headers, &fields, &jar).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    // A challenge has just proved the password. A session has not, and a
    // second factor set up from one decides who can sign in to the account,
    // so it proves the password again (security review 2026-11, #3).
    if let Actor::SignedIn(session) = &actor
        && let Err(r) =
            reauthenticated(&state, catalog, peer, &headers, session.user(), &fields).await
    {
        return r;
    }
    match setup_for(&state, catalog, &headers, actor.user_id()).await {
        Ok(body) => {
            let mut out = Json(body).into_response();
            no_store(&mut out);
            out
        }
        Err(r) => r,
    }
}

/// `POST` [`ENABLE_ROUTE`].
pub(crate) async fn enable(
    State(state): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    jar: CookieJar,
    body: Bytes,
) -> Response {
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let fields = Fields::read(&headers, &body);
    let actor = match actor(&state, catalog, &headers, &fields, &jar).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let user_id = actor.user_id();
    let permit = match reserve(catalog, ip_of(&state, peer, &headers), user_id).await {
        Ok(permit) => permit,
        Err(r) => return r,
    };
    let code = fields.text("code").unwrap_or_default();
    let enabled = match sc_auth::finish_second_factor_setup(catalog, permit, user_id, code).await {
        Ok(Some(enabled)) => enabled,
        Ok(None) => {
            if fields.form {
                return page_redirect(&fields.return_to(), true);
            }
            return wrong_code();
        }
        Err(e) => return failed("could not turn on two-factor authentication", &e),
    };
    let codes: Vec<String> = enabled
        .recovery_codes
        .iter()
        .map(|c| c.expose().to_owned())
        .collect();
    match actor {
        Actor::SignedIn(session) => {
            // A session somebody else holds must not outlive the account
            // turning a second factor on; this one goes on.
            if let Err(r) = changed_here(&state, enabled.changed, &session).await {
                return r;
            }
            let mut out = Json(json!({ "recovery_codes": codes })).into_response();
            no_store(&mut out);
            out
        }
        Actor::Challenge { token, .. } => {
            // Whatever sessions the account had end; the one this sign-in
            // starts is at the new epoch.
            let changed = enabled.changed;
            if let Err(e) = state.sessions.bump_epoch(changed.clone(), None).await {
                return failed("could not end the account's other sessions", &e);
            }
            let (cookies, user) = match finish_sign_in(
                &state,
                catalog,
                &headers,
                jar,
                &token,
                enabled.proof,
                Some(&changed),
            )
            .await
            {
                Ok(done) => done,
                Err(r) => return r,
            };
            let mut out = if fields.form {
                let page = recovery_codes_page(&codes, &fields.return_to());
                (cookies.jar, cookies.stale, crate::html_page::html(page)).into_response()
            } else {
                (
                    cookies.jar,
                    cookies.stale,
                    Json(json!({ "recovery_codes": codes, "user": user_summary_json(&user) })),
                )
                    .into_response()
            };
            no_store(&mut out);
            out
        }
    }
}

/// `POST` [`VERIFY_ROUTE`].
pub(crate) async fn verify(
    State(state): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    jar: CookieJar,
    body: Bytes,
) -> Response {
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let fields = Fields::read(&headers, &body);
    let Some(token) = fields.challenge(&jar) else {
        return expired();
    };
    let pending = match sc_auth::second_factor_challenge(catalog, &token).await {
        Ok(Some(found)) => found,
        Ok(None) => return expired(),
        Err(e) => return failed("could not read the sign-in", &e),
    };
    let code = fields.text("code").unwrap_or_default();
    let proof = match check_code(catalog, ip_of(&state, peer, &headers), pending.user, code).await {
        Ok(Some(proof)) => proof,
        Ok(None) if fields.form => return page_redirect(&fields.return_to(), true),
        Ok(None) => return wrong_code(),
        Err(r) => return r,
    };
    let (cookies, user) =
        match finish_sign_in(&state, catalog, &headers, jar, &token, proof, None).await {
            Ok(done) => done,
            Err(r) => return r,
        };
    let mut out = if fields.form {
        (
            cookies.jar,
            cookies.stale,
            Redirect::to(&fields.return_to()),
        )
            .into_response()
    } else {
        (cookies.jar, cookies.stale, Json(user_summary_json(&user))).into_response()
    };
    no_store(&mut out);
    out
}

/// The session of the account, with a code that is right for it: what
/// turning the second factor off and replacing the recovery codes both need.
async fn signed_in_with_code(
    state: &AppState,
    catalog: &Catalog,
    ip: Option<std::net::IpAddr>,
    headers: &HeaderMap,
    jar: &CookieJar,
    fields: &Fields,
) -> Result<(OwnSession, SecondFactorProof), Response> {
    let session = signed_in(state, headers, jar)
        .await?
        .ok_or_else(not_signed_in)?;
    let code = fields.text("code").unwrap_or_default();
    match check_code(catalog, ip, session.user().id(), code).await? {
        Some(proof) => Ok((session, proof)),
        None => Err(wrong_code()),
    }
}

/// `POST` [`DISABLE_ROUTE`].
pub(crate) async fn disable(
    State(state): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    jar: CookieJar,
    body: Bytes,
) -> Response {
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let fields = Fields::read(&headers, &body);
    // Asked before the code is, so a code is not spent on a refusal.
    match signed_in(&state, &headers, &jar).await {
        Ok(Some(session)) => {
            match sc_auth::role_requires_second_factor(catalog, session.user().role()).await {
                Ok(true) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "your role requires two-factor authentication, so it cannot be turned off",
                    );
                }
                Ok(false) => {}
                Err(e) => return failed("could not read the role", &e),
            }
        }
        Ok(None) => return not_signed_in(),
        Err(r) => return r,
    }
    let (session, _proof) = match signed_in_with_code(
        &state,
        catalog,
        ip_of(&state, peer, &headers),
        &headers,
        &jar,
        &fields,
    )
    .await
    {
        Ok(found) => found,
        Err(r) => return r,
    };
    match sc_auth::disable_second_factor(catalog, session.user().id()).await {
        Ok(removed) => {
            let ended = match removed.changed {
                Some(changed) => changed_here(&state, changed, &session).await,
                None => Ok(()),
            };
            match ended {
                Ok(()) => Json(json!({ "ok": true })).into_response(),
                Err(r) => r,
            }
        }
        Err(e) => failed("could not turn off two-factor authentication", &e),
    }
}

/// `POST` [`RECOVERY_CODES_ROUTE`].
pub(crate) async fn recovery_codes(
    State(state): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    jar: CookieJar,
    body: Bytes,
) -> Response {
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let fields = Fields::read(&headers, &body);
    let (_session, proof) = match signed_in_with_code(
        &state,
        catalog,
        ip_of(&state, peer, &headers),
        &headers,
        &jar,
        &fields,
    )
    .await
    {
        Ok(found) => found,
        Err(r) => return r,
    };
    match sc_auth::regenerate_recovery_codes(catalog, proof).await {
        Ok(codes) => {
            let codes: Vec<&str> = codes.iter().map(sc_auth::RecoveryCode::expose).collect();
            let mut out = Json(json!({ "recovery_codes": codes })).into_response();
            no_store(&mut out);
            out
        }
        Err(e) => failed("could not make new recovery codes", &e),
    }
}

// --- the page ----------------------------------------------------------------

fn page_redirect(return_to: &str, wrong: bool) -> Response {
    let mut target = format!("{PAGE_ROUTE}?return_to={}", encode(return_to));
    if wrong {
        target.push_str("&error=code");
    }
    Redirect::to(&target).into_response()
}

/// `GET` [`PAGE_ROUTE`]: the code form for the challenge in the cookie, or —
/// for an account whose role requires a second factor it has not set up — the
/// QR code to scan and the form for its first code.
pub(crate) async fn page(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
    csrf: Option<Extension<CsrfToken>>,
) -> Response {
    let query = parse_query(&uri);
    let get = |name: &str| {
        query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let return_to = crate::oauth::safe_return_to(get("return_to")).into_string();
    let wrong = get("error").is_some();
    let csrf = csrf
        .map(|Extension(CsrfToken(token))| token)
        .unwrap_or_default();
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let challenge = match jar.get(CHALLENGE_COOKIE).map(|c| c.value().to_owned()) {
        Some(token) => sc_auth::second_factor_challenge(catalog, &ChallengeToken::presented(token))
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let Some(sc_auth::PendingChallenge { user: user_id, .. }) = challenge else {
        return html_page(
            "Sign-in expired",
            safe_html!(
                "<h1>This sign-in has expired</h1>\
                 <p>Sign in again to continue.</p>\
                 <p><a class=\"button\" href=\"{}\">Back</a></p>",
                SafeHtml::escape(&return_to)
            ),
        );
    };
    let error = SafeHtml::trusted(if wrong {
        "<p class=\"error\" role=\"alert\">That code is not right. Try again.</p>"
    } else {
        ""
    });
    let hidden = safe_html!(
        "<input type=\"hidden\" name=\"_csrf\" value=\"{}\">\
         <input type=\"hidden\" name=\"return_to\" value=\"{}\">",
        SafeHtml::escape(&csrf),
        SafeHtml::escape(&return_to)
    );
    let enabled = match sc_auth::second_factor_enabled(catalog, user_id).await {
        Ok(enabled) => enabled,
        Err(e) => return failed("could not read the second factor", &e),
    };
    if enabled {
        return html_page(
            "Two-factor authentication",
            safe_html!(
                "<h1>Two-factor authentication</h1>\
                 <p>Enter the code from your authenticator app, or one of your recovery codes.</p>\
                 {error}\
                 <form method=\"post\" action=\"{verify}\">{hidden}\
                 <label for=\"code\">Code</label>\
                 <input id=\"code\" name=\"code\" autocomplete=\"one-time-code\" \
                  inputmode=\"numeric\" autofocus required>\
                 <button type=\"submit\">Continue</button></form>",
                error = error,
                verify = SafeHtml::trusted(VERIFY_ROUTE),
                hidden = hidden,
            ),
        );
    }
    let setup = match setup_for(&state, catalog, &headers, user_id).await {
        Ok(setup) => setup,
        Err(r) => return r,
    };
    // The QR code is an SVG sc-auth's encoder drew from the otpauth URI.
    let svg = SafeHtml::generated(
        setup["qr_svg"].as_str().unwrap_or_default().to_owned(),
        "the two-factor setup QR code (sc-auth totp)",
    );
    let secret = SafeHtml::escape(setup["secret"].as_str().unwrap_or_default());
    html_page(
        "Set up two-factor authentication",
        safe_html!(
            "<h1>Set up two-factor authentication</h1>\
             <p>Your account needs a code from an authenticator app to sign in. Scan this \
             with the app (Google Authenticator, Microsoft Authenticator, 1Password, …), \
             then enter the code it shows.</p>\
             <div class=\"qr\">{svg}</div>\
             <p class=\"secret\">Or enter this key by hand: <code>{secret}</code></p>\
             {error}\
             <form method=\"post\" action=\"{enable}\">{hidden}\
             <label for=\"code\">Code</label>\
             <input id=\"code\" name=\"code\" autocomplete=\"one-time-code\" \
              inputmode=\"numeric\" autofocus required>\
             <button type=\"submit\">Turn on and continue</button></form>",
            svg = svg,
            secret = secret,
            error = error,
            enable = SafeHtml::trusted(ENABLE_ROUTE),
            hidden = hidden,
        ),
    )
}

fn recovery_codes_page(codes: &[String], return_to: &str) -> SafeHtml {
    let items: SafeHtml = codes
        .iter()
        .map(|c| safe_html!("<li><code>{}</code></li>", SafeHtml::escape(c)))
        .collect();
    page_html(
        "Recovery codes",
        safe_html!(
            "<h1>Two-factor authentication is on</h1>\
             <p>Keep these recovery codes somewhere safe. Each one signs you in once if you \
             lose your phone. They are not shown again.</p>\
             <ul class=\"codes\">{items}</ul>\
             <p><a class=\"button\" href=\"{return_to}\">Continue</a></p>",
            items = items,
            return_to = SafeHtml::escape(return_to),
        ),
    )
}

fn html_page(title: &str, body: SafeHtml) -> Response {
    let mut out = (
        StatusCode::OK,
        crate::html_page::html(page_html(title, body)),
    )
        .into_response();
    no_store(&mut out);
    out
}

/// A page of this module: the title escaped, the body already `SafeHtml`
/// (TODO-type-hardening H3.7).
fn page_html(title: &str, body: SafeHtml) -> SafeHtml {
    safe_html!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title><style>{style}</style></head>\
         <body><main>{body}</main></body></html>",
        title = SafeHtml::escape(title),
        style = SafeHtml::trusted(PAGE_STYLE),
        body = body,
    )
}

const PAGE_STYLE: &str = "\
:root{color-scheme:light dark;--bg:#f5f7fb;--card:#fff;--text:#1d273b;--muted:#667382;\
--accent:#206bc4;--error:#d63939;--border:#dce1e7}\
@media (prefers-color-scheme:dark){:root{--bg:#151f2c;--card:#1d2a3a;--text:#e6ebf1;\
--muted:#9aa7b5;--border:#2c3a4d}}\
body{margin:0;background:var(--bg);color:var(--text);font:16px/1.5 system-ui,sans-serif}\
main{max-width:26rem;margin:3rem auto;padding:2rem;background:var(--card);\
border:1px solid var(--border);border-radius:8px}\
@media (max-width:30rem){main{margin:1rem;padding:1.25rem}}\
h1{font-size:1.4rem;margin:0 0 1rem}p{color:var(--muted)}\
label{display:block;font-weight:600;margin:1rem 0 .25rem}\
input{box-sizing:border-box;width:100%;padding:.6rem;font-size:1.2rem;letter-spacing:.15em;\
border:1px solid var(--border);border-radius:6px;background:var(--card);color:var(--text)}\
button,.button{display:inline-block;margin-top:1rem;padding:.6rem 1.2rem;border:0;\
border-radius:6px;background:var(--accent);color:#fff;font-size:1rem;text-decoration:none;cursor:pointer}\
.error{color:var(--error)}.qr{background:#fff;padding:.5rem;display:inline-block;border-radius:6px}\
.qr svg{display:block;width:200px;height:200px}code{word-break:break-all}\
.codes{columns:2;font-size:1.05rem}";

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

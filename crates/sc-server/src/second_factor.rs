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
//! A wrong code counts against the account (`sc_api::throttle`): five in
//! fifteen minutes and it is refused with a `429` until the window passes,
//! which makes guessing a six-digit code that lasts 30 seconds hopeless.

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use sc_api::SessionAction;
use sc_api::auth::user_summary_json;
use sc_auth::{SecondFactorNeeded, User};
use sc_catalog::Catalog;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::router::{
    AppState, Resolved, SessionClient, apply_session, json_error, log_failure, parse_form,
    parse_query, resolve_app,
};
use crate::security::{CsrfToken, SESSION_COOKIE};

/// Where the routes live.
pub const PREFIX: &str = "/_feldspar/second-factor";
/// `GET`: the signed-in account's second factor — `{enabled, required,
/// recovery_codes_left}`.
pub const STATUS_ROUTE: &str = "/_feldspar/second-factor";
/// `POST {token?}`: a new secret to scan — `{secret, otpauth_url, qr_svg}`.
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

/// The answer to a sign-in by `user` that must give a second factor before it
/// gets a session — or `None` when it need not, and the session starts.
///
/// Fails closed: a database that cannot say whether the account has a second
/// factor starts no session.
pub(crate) async fn challenge_for(
    state: &AppState,
    client: &SessionClient,
    user: &User,
    remember: bool,
) -> Option<Response> {
    let catalog = state.apps.catalog()?;
    let needed = match sc_auth::second_factor_needed(catalog, user).await {
        Ok(needed) => needed,
        Err(e) => {
            log_failure("could not read an account's second factor", &e);
            return Some(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not start session",
            ));
        }
    };
    if needed == SecondFactorNeeded::No {
        return None;
    }
    let token = match sc_auth::issue_second_factor_challenge(catalog, user.id, remember).await {
        Ok(token) => token,
        Err(e) => {
            log_failure("could not issue a second-factor challenge", &e);
            return Some(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not start session",
            ));
        }
    };
    let jar = CookieJar::new().add(challenge_cookie(state, token.clone()));
    let mut out = match &client.page_return {
        Some(return_to) => (
            jar,
            Redirect::to(&format!(
                "{PAGE_ROUTE}?return_to={}",
                encode(&crate::oauth::safe_return_to(Some(return_to)))
            )),
        )
            .into_response(),
        None => {
            let mut body = user_summary_json(user);
            body["second_factor"] = json!({
                "token": token,
                "setup": needed == SecondFactorNeeded::Setup,
            });
            (StatusCode::OK, jar, Json(body)).into_response()
        }
    };
    no_store(&mut out);
    Some(out)
}

fn challenge_cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build((CHALLENGE_COOKIE, token))
        .path(PREFIX)
        .same_site(SameSite::Lax)
        .http_only(true)
        .secure(state.secure_cookies)
        .max_age(time::Duration::minutes(
            sc_auth::SECOND_FACTOR_TTL_MINUTES,
        ))
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
    fn challenge<'a>(&'a self, jar: &'a CookieJar) -> Option<String> {
        self.text("token").map(str::to_owned).or_else(|| {
            jar.get(CHALLENGE_COOKIE)
                .map(|c| c.value().to_owned())
                .filter(|v| !v.is_empty())
        })
    }

    fn return_to(&self) -> String {
        crate::oauth::safe_return_to(self.text("return_to"))
    }
}

/// The account a request's session cookie signs in, if any.
async fn signed_in(state: &AppState, jar: &CookieJar) -> Result<Option<User>, Response> {
    let Some(token) = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned()) else {
        return Ok(None);
    };
    state.sessions.user_for(&token).await.map_err(|e| {
        log_failure("could not read the session", &e);
        json_error(StatusCode::INTERNAL_SERVER_ERROR, "could not read the session")
    })
}

fn catalog_of(state: &AppState) -> Result<&Catalog, Response> {
    state
        .apps
        .catalog()
        .map(|c| &**c)
        .ok_or_else(|| json_error(StatusCode::SERVICE_UNAVAILABLE, "this server has no database"))
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

/// Throttle a code for `user_id`, or the `429` to answer instead.
fn throttle(ip: Option<std::net::IpAddr>, user_id: Uuid) -> Result<(), Response> {
    sc_api::throttle::second_factor_attempt(ip, &user_id.to_string())
        .map_err(|limited| {
            let secs = limited.retry_after_secs();
            let mut out = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": "too many wrong codes; try again later", "retry_after": secs })),
            )
                .into_response();
            out.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
            out
        })
}

/// Whether `code` is right for `user_id`, counting it against the account when
/// it is not.
async fn check_code(catalog: &Catalog, user_id: Uuid, code: &str) -> Result<bool, Response> {
    let ok = sc_auth::verify_second_factor(catalog, user_id, code)
        .await
        .map_err(|e| failed("could not check the code", &e))?;
    if !ok {
        sc_api::throttle::second_factor_failed(&user_id.to_string());
    }
    Ok(ok)
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
    Challenge { user: Uuid, remember: bool, token: String },
    SignedIn(User),
}

impl Actor {
    fn user_id(&self) -> Uuid {
        match self {
            Actor::Challenge { user, .. } => *user,
            Actor::SignedIn(user) => user.id,
        }
    }
}

async fn actor(
    state: &AppState,
    catalog: &Catalog,
    fields: &Fields,
    jar: &CookieJar,
) -> Result<Actor, Response> {
    if let Some(token) = fields.challenge(jar) {
        match sc_auth::second_factor_challenge(catalog, &token).await {
            Ok(Some((user, remember))) => {
                return Ok(Actor::Challenge {
                    user,
                    remember,
                    token,
                });
            }
            Ok(None) if fields.text("token").is_some() => return Err(expired()),
            Ok(None) => {}
            Err(e) => return Err(failed("could not read the sign-in", &e)),
        }
    }
    match signed_in(state, jar).await? {
        Some(user) => Ok(Actor::SignedIn(user)),
        None if fields.challenge(jar).is_some() => Err(expired()),
        None => Err(not_signed_in()),
    }
}

/// Spend a challenge whose code was right, and start the session it was
/// waiting for. `Err` is the response to send instead.
async fn finish_sign_in(
    state: &AppState,
    catalog: &Catalog,
    headers: &HeaderMap,
    jar: CookieJar,
    token: &str,
    remember: bool,
) -> Result<(crate::router::SessionCookies, User), Response> {
    let spent = sc_auth::spend_second_factor_challenge(catalog, token)
        .await
        .map_err(|e| failed("could not finish the sign-in", &e))?;
    let Some(user_id) = spent else {
        return Err(expired());
    };
    let user = match sc_auth::load_user(catalog, user_id).await {
        Ok(Some(user)) if !user.is_disabled() => user,
        Ok(_) => return Err(expired()),
        Err(e) => return Err(failed("could not finish the sign-in", &e)),
    };
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
        SessionAction::StartVerified {
            user: user.clone(),
            remember,
        },
    )
    .await
    .map_err(|rejection| *rejection)?;
    Ok((cookies, user))
}

// --- the routes --------------------------------------------------------------

/// `GET` [`STATUS_ROUTE`].
pub(crate) async fn status(State(state): State<AppState>, jar: CookieJar) -> Response {
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let user = match signed_in(&state, &jar).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_signed_in(),
        Err(r) => return r,
    };
    let status = match sc_auth::second_factor_status(catalog, user.id).await {
        Ok(s) => s,
        Err(e) => return failed("could not read the second factor", &e),
    };
    let required = match sc_auth::role_requires_second_factor(catalog, user.role).await {
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
        .map_err(|e| failed("could not set up two-factor authentication", &e))?;
    let account = match sc_auth::load_user(catalog, user_id).await {
        Ok(Some(user)) => sc_auth::user_email(&user)
            .map(str::to_owned)
            .unwrap_or_else(|| user.id.to_string()),
        Ok(None) => return Err(expired()),
        Err(e) => return Err(failed("could not set up two-factor authentication", &e)),
    };
    let url = sc_auth::otpauth_url(&issuer(state, catalog, headers).await, &account, &secret);
    let svg = sc_auth::qr_svg(&url).map_err(|e| failed("could not make the QR code", &e))?;
    Ok(json!({ "secret": secret, "otpauth_url": url, "qr_svg": svg, "account": account }))
}

/// `POST` [`SETUP_ROUTE`].
pub(crate) async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    body: Bytes,
) -> Response {
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let fields = Fields::read(&headers, &body);
    let actor = match actor(&state, catalog, &fields, &jar).await {
        Ok(a) => a,
        Err(r) => return r,
    };
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
    let actor = match actor(&state, catalog, &fields, &jar).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let user_id = actor.user_id();
    if let Err(r) = throttle(ip_of(&state, peer, &headers), user_id) {
        return r;
    }
    let code = fields.text("code").unwrap_or_default();
    let codes = match sc_auth::finish_second_factor_setup(catalog, user_id, code).await {
        Ok(Some(codes)) => codes,
        Ok(None) => {
            sc_api::throttle::second_factor_failed(&user_id.to_string());
            if fields.form {
                return page_redirect(&fields.return_to(), true);
            }
            return wrong_code();
        }
        Err(e) => return failed("could not turn on two-factor authentication", &e),
    };
    match actor {
        Actor::SignedIn(_) => {
            let mut out = Json(json!({ "recovery_codes": codes })).into_response();
            no_store(&mut out);
            out
        }
        Actor::Challenge {
            remember, token, ..
        } => {
            let (cookies, user) =
                match finish_sign_in(&state, catalog, &headers, jar, &token, remember).await {
                    Ok(done) => done,
                    Err(r) => return r,
                };
            let mut out = if fields.form {
                let page = recovery_codes_page(&codes, &fields.return_to());
                (cookies.jar, cookies.stale, Html(page)).into_response()
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
    let (user_id, remember) = match sc_auth::second_factor_challenge(catalog, &token).await {
        Ok(Some(found)) => found,
        Ok(None) => return expired(),
        Err(e) => return failed("could not read the sign-in", &e),
    };
    if let Err(r) = throttle(ip_of(&state, peer, &headers), user_id) {
        return r;
    }
    let code = fields.text("code").unwrap_or_default();
    match check_code(catalog, user_id, code).await {
        Ok(true) => {}
        Ok(false) if fields.form => return page_redirect(&fields.return_to(), true),
        Ok(false) => return wrong_code(),
        Err(r) => return r,
    }
    let (cookies, user) =
        match finish_sign_in(&state, catalog, &headers, jar, &token, remember).await {
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

/// The signed-in account, with a code that is right for it: what turning the
/// second factor off and replacing the recovery codes both need.
async fn signed_in_with_code(
    state: &AppState,
    catalog: &Catalog,
    ip: Option<std::net::IpAddr>,
    jar: &CookieJar,
    fields: &Fields,
) -> Result<User, Response> {
    let user = signed_in(state, jar).await?.ok_or_else(not_signed_in)?;
    throttle(ip, user.id)?;
    if !check_code(catalog, user.id, fields.text("code").unwrap_or_default()).await? {
        return Err(wrong_code());
    }
    Ok(user)
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
    match signed_in(&state, &jar).await {
        Ok(Some(user)) => match sc_auth::role_requires_second_factor(catalog, user.role).await {
            Ok(true) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "your role requires two-factor authentication, so it cannot be turned off",
                );
            }
            Ok(false) => {}
            Err(e) => return failed("could not read the role", &e),
        },
        Ok(None) => return not_signed_in(),
        Err(r) => return r,
    }
    let user = match signed_in_with_code(&state, catalog, ip_of(&state, peer, &headers), &jar, &fields).await {
        Ok(user) => user,
        Err(r) => return r,
    };
    match sc_auth::disable_second_factor(catalog, user.id).await {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
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
    let user = match signed_in_with_code(&state, catalog, ip_of(&state, peer, &headers), &jar, &fields).await {
        Ok(user) => user,
        Err(r) => return r,
    };
    match sc_auth::regenerate_recovery_codes(catalog, user.id).await {
        Ok(codes) => {
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
    let return_to = crate::oauth::safe_return_to(get("return_to"));
    let wrong = get("error").is_some();
    let csrf = csrf.map(|Extension(CsrfToken(token))| token).unwrap_or_default();
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let challenge = match jar.get(CHALLENGE_COOKIE).map(|c| c.value().to_owned()) {
        Some(token) => sc_auth::second_factor_challenge(catalog, &token)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let Some((user_id, _)) = challenge else {
        return html_page(
            "Sign-in expired",
            &format!(
                "<h1>This sign-in has expired</h1>\
                 <p>Sign in again to continue.</p>\
                 <p><a class=\"button\" href=\"{}\">Back</a></p>",
                html_escape(&return_to)
            ),
        );
    };
    let error = if wrong {
        "<p class=\"error\" role=\"alert\">That code is not right. Try again.</p>"
    } else {
        ""
    };
    let hidden = format!(
        "<input type=\"hidden\" name=\"_csrf\" value=\"{}\">\
         <input type=\"hidden\" name=\"return_to\" value=\"{}\">",
        html_escape(&csrf),
        html_escape(&return_to)
    );
    let enabled = match sc_auth::second_factor_enabled(catalog, user_id).await {
        Ok(enabled) => enabled,
        Err(e) => return failed("could not read the second factor", &e),
    };
    if enabled {
        return html_page(
            "Two-factor authentication",
            &format!(
                "<h1>Two-factor authentication</h1>\
                 <p>Enter the code from your authenticator app, or one of your recovery codes.</p>\
                 {error}\
                 <form method=\"post\" action=\"{VERIFY_ROUTE}\">{hidden}\
                 <label for=\"code\">Code</label>\
                 <input id=\"code\" name=\"code\" autocomplete=\"one-time-code\" \
                  inputmode=\"numeric\" autofocus required>\
                 <button type=\"submit\">Continue</button></form>"
            ),
        );
    }
    let setup = match setup_for(&state, catalog, &headers, user_id).await {
        Ok(setup) => setup,
        Err(r) => return r,
    };
    let svg = setup["qr_svg"].as_str().unwrap_or_default();
    let secret = html_escape(setup["secret"].as_str().unwrap_or_default());
    html_page(
        "Set up two-factor authentication",
        &format!(
            "<h1>Set up two-factor authentication</h1>\
             <p>Your account needs a code from an authenticator app to sign in. Scan this \
             with the app (Google Authenticator, Microsoft Authenticator, 1Password, …), \
             then enter the code it shows.</p>\
             <div class=\"qr\">{svg}</div>\
             <p class=\"secret\">Or enter this key by hand: <code>{secret}</code></p>\
             {error}\
             <form method=\"post\" action=\"{ENABLE_ROUTE}\">{hidden}\
             <label for=\"code\">Code</label>\
             <input id=\"code\" name=\"code\" autocomplete=\"one-time-code\" \
              inputmode=\"numeric\" autofocus required>\
             <button type=\"submit\">Turn on and continue</button></form>"
        ),
    )
}

fn recovery_codes_page(codes: &[String], return_to: &str) -> String {
    let items: String = codes
        .iter()
        .map(|c| format!("<li><code>{}</code></li>", html_escape(c)))
        .collect();
    page_html(
        "Recovery codes",
        &format!(
            "<h1>Two-factor authentication is on</h1>\
             <p>Keep these recovery codes somewhere safe. Each one signs you in once if you \
             lose your phone. They are not shown again.</p>\
             <ul class=\"codes\">{items}</ul>\
             <p><a class=\"button\" href=\"{}\">Continue</a></p>",
            html_escape(return_to)
        ),
    )
}

fn html_page(title: &str, body: &str) -> Response {
    let mut out = (StatusCode::OK, Html(page_html(title, body))).into_response();
    no_store(&mut out);
    out
}

fn page_html(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>{PAGE_STYLE}</style></head>\
         <body><main>{body}</main></body></html>",
        html_escape(title)
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

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

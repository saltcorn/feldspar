//! Feldspar as an OAuth 2.0 / OpenID Connect **identity provider** (TODO
//! IP1.3–IP1.5): another service signs people in with an application's users.
//!
//! An application turns the provider on (`Application::identity_provider`).
//! Its issuer is the application's own host — `https://blog.example.com` —
//! and these routes answer there and nowhere else: on an application that has
//! not turned it on, on a run's preview and on the admin host they are 404.
//!
//! - **`GET /.well-known/openid-configuration`** and
//!   **`/.well-known/oauth-authorization-server`** (RFC 8414): the metadata.
//! - **`GET /_feldspar/oidc/jwks`**: the public signing keys
//!   (`sc_auth::published_jwks`).
//! - **`GET /_feldspar/oidc/authorize`**: the authorization code flow, with
//!   PKCE `S256` required of every client. The client and its **exact**
//!   redirect URI are checked before anything else, and a bad one is an error
//!   page, never a redirect. Anybody not signed in gets a sign-in page, which
//!   starts the session through the router's one session path
//!   ([`apply_session`]: the second factor, the application's access settings);
//!   then a consent page unless the user's grant already covers the scopes or
//!   the client skips consent. Both pages `POST` back here as forms the CSRF
//!   check covers.
//! - **`POST /_feldspar/oidc/token`**: the code (and later a refresh token)
//!   exchanged for tokens by the relying party's server. Its route is
//!   [`RouteAuth::OAuthClient`](crate::raw_routes::RouteAuth::OAuthClient),
//!   which **ignores cookies**: the CSRF middleware strips them and lets the
//!   request through, so the client's own credentials are all it is judged by.
//!
//! # The session cookie and a cross-site arrival
//!
//! A relying party sends the browser here from another site, and the session
//! cookie is `SameSite=Strict`, so it is not sent with that first request. An
//! authorize request that arrives with no session cookie is therefore answered
//! with a page that goes on to the same address once (marked with
//! [`BOUNCED_PARAM`]); that second request starts from this host, so the
//! cookie comes with it. Only then is "not signed in" believed.
//!
//! For the same reason the end of a form post — a sign-in or a consent — is a
//! page that goes on to the relying party rather than a redirect: CSP's
//! `form-action 'self'` stops a browser following a form's redirect to
//! another site.

use std::net::SocketAddr;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use base64::Engine;
use chrono::Utc;
use sc_api::SessionAction;
use sc_auth::{
    AuthenticatedUser, NewAuthorizationCode, OAuthClient, SCOPE_EMAIL, SCOPE_OPENID, SCOPE_PROFILE,
    SUPPORTED_SCOPES, SigningAlg, TokenSubject, User,
};
use sc_catalog::{Catalog, PublicBaseUrl};
use sc_expr::html::SafeHtml;
use sc_expr::safe_html;
use sc_ratelimit::Rate;
use serde_json::{Value, json};

use crate::apps::MountedApp;
use crate::router::{
    AppState, Resolved, SessionClient, apply_session, form_decode, log_failure, parse_form,
    parse_query, refuse_sign_in, request_client_ip, resolve_app, served_request_host,
};
use crate::security::{CSRF_FORM_FIELD, CsrfToken, SESSION_COOKIE};
use crate::session::AppSession;

/// OpenID Connect Discovery's metadata.
pub const DISCOVERY_ROUTE: &str = "/.well-known/openid-configuration";
/// RFC 8414's metadata: the same document.
pub const AS_METADATA_ROUTE: &str = "/.well-known/oauth-authorization-server";
/// The public signing keys.
pub const JWKS_ROUTE: &str = "/_feldspar/oidc/jwks";
/// The authorization endpoint, and where its pages post back to.
pub const AUTHORIZE_ROUTE: &str = "/_feldspar/oidc/authorize";
/// The token endpoint.
pub const TOKEN_ROUTE: &str = "/_feldspar/oidc/token";
/// The UserInfo endpoint.
pub const USERINFO_ROUTE: &str = "/_feldspar/oidc/userinfo";
/// The revocation endpoint (RFC 7009).
pub const REVOKE_ROUTE: &str = "/_feldspar/oidc/revoke";
/// The end-session endpoint (OpenID Connect RP-Initiated Logout).
pub const LOGOUT_ROUTE: &str = "/_feldspar/oidc/logout";

/// Marks the second arrival of an authorize request (module docs).
pub const BOUNCED_PARAM: &str = "_fd_bounced";

/// How long an `id_token` is good for.
pub const ID_TOKEN_TTL_SECONDS: i64 = 600;

/// Token requests per client address.
pub const TOKEN_PER_IP: Rate = Rate {
    max: 60,
    window: std::time::Duration::from_secs(60),
};
/// Token requests per client.
pub const TOKEN_PER_CLIENT: Rate = Rate {
    max: 120,
    window: std::time::Duration::from_secs(60),
};

/// The authorization request's parameters this endpoint reads, in the order
/// they are carried through a page's form.
const REQUEST_PARAMS: &[&str] = &[
    "client_id",
    "redirect_uri",
    "response_type",
    "scope",
    "state",
    "nonce",
    "code_challenge",
    "code_challenge_method",
    "prompt",
];

// --- the issuer ---------------------------------------------------------------

/// The application whose host this request reached, when it is an identity
/// provider, and its issuer: the host as the request named it, which must be
/// one this server serves. A preview, an application that has not turned the
/// provider on, and the admin host are `None`.
fn issuer_of(
    state: &AppState,
    headers: &HeaderMap,
    jar: &CookieJar,
) -> Option<(std::sync::Arc<MountedApp>, String)> {
    let Resolved::App(app) = resolve_app(state, headers, jar) else {
        return None;
    };
    if !app.app.identity_provider || app.is_preview() {
        return None;
    }
    let host = served_request_host(state, headers, jar)?;
    Some((
        app,
        PublicBaseUrl::new(state.secure_cookies, &host).to_string(),
    ))
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "this application is not an identity provider" })),
    )
        .into_response()
}

fn catalog(state: &AppState) -> Option<&std::sync::Arc<Catalog>> {
    state.apps.catalog()
}

// --- discovery and keys -------------------------------------------------------

/// The provider metadata for `issuer`: OpenID Connect Discovery's, which is
/// also a valid RFC 8414 document.
pub fn metadata(issuer: &str) -> Value {
    json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}{AUTHORIZE_ROUTE}"),
        "token_endpoint": format!("{issuer}{TOKEN_ROUTE}"),
        "jwks_uri": format!("{issuer}{JWKS_ROUTE}"),
        "userinfo_endpoint": format!("{issuer}{USERINFO_ROUTE}"),
        "revocation_endpoint": format!("{issuer}{REVOKE_ROUTE}"),
        "revocation_endpoint_auth_methods_supported":
            ["client_secret_basic", "client_secret_post", "none"],
        "end_session_endpoint": format!("{issuer}{LOGOUT_ROUTE}"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported":
            SigningAlg::ALL.iter().map(|alg| alg.name()).collect::<Vec<_>>(),
        "scopes_supported": SUPPORTED_SCOPES,
        "token_endpoint_auth_methods_supported":
            ["client_secret_basic", "client_secret_post", "none"],
        "code_challenge_methods_supported": ["S256"],
        "prompt_values_supported": ["none", "login", "consent"],
        "claims_supported": claims_supported(),
        "authorization_response_iss_parameter_supported": true,
    })
}

/// The claims this provider can give: the protocol's, `email`'s and the
/// standard profile claims. A client's mapped claims are its own.
fn claims_supported() -> Vec<&'static str> {
    [
        "iss",
        "sub",
        "aud",
        "exp",
        "iat",
        "auth_time",
        "nonce",
        "at_hash",
        "email",
        "email_verified",
    ]
    .into_iter()
    .chain(sc_auth::PROFILE_CLAIMS.iter().copied())
    .collect()
}

/// `GET /.well-known/openid-configuration` and
/// `/.well-known/oauth-authorization-server`.
pub(crate) async fn discovery(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some((_, issuer)) = issuer_of(&state, &headers, &CookieJar::new()) else {
        return not_found();
    };
    cacheable(Json(metadata(&issuer)).into_response())
}

/// `GET /_feldspar/oidc/jwks`.
pub(crate) async fn jwks(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if issuer_of(&state, &headers, &CookieJar::new()).is_none() {
        return not_found();
    }
    let Some(catalog) = catalog(&state) else {
        return not_found();
    };
    let keys = match sc_auth::ensure_signing_keys(catalog).await {
        Ok(_) => sc_auth::published_jwks(catalog).await,
        Err(e) => Err(e),
    };
    match keys {
        Ok(keys) => cacheable(Json(keys).into_response()),
        Err(e) => {
            log_failure("could not read the identity provider's signing keys", &e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "the signing keys could not be read" })),
            )
                .into_response()
        }
    }
}

/// A public answer a relying party may cache for a few minutes: short, so a
/// rotated key is fetched soon.
fn cacheable(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    response
}

// --- the authorization request ------------------------------------------------

/// A checked authorization request.
#[derive(Debug, Clone)]
struct AuthorizeRequest {
    client: OAuthClient,
    redirect_uri: String,
    scopes: Vec<String>,
    state: Option<String>,
    nonce: Option<String>,
    code_challenge: String,
    prompt: Vec<String>,
    /// The parameters as sent, for carrying through a page's form.
    raw: Vec<(String, String)>,
}

impl AuthorizeRequest {
    fn prompt(&self, value: &str) -> bool {
        self.prompt.iter().any(|p| p == value)
    }

    /// The authorize address this request is at, as a path on this host,
    /// with `prompt=login` taken out: what a sign-in goes on to, which has
    /// just met it.
    fn after_sign_in(&self) -> String {
        let pairs: Vec<(String, String)> = self
            .raw
            .iter()
            .filter_map(|(k, v)| {
                if k == "prompt" {
                    let rest: Vec<&str> = v
                        .split(' ')
                        .filter(|p| !p.is_empty() && *p != "login")
                        .collect();
                    (!rest.is_empty()).then(|| (k.clone(), rest.join(" ")))
                } else {
                    Some((k.clone(), v.clone()))
                }
            })
            .chain(std::iter::once((BOUNCED_PARAM.to_owned(), "1".to_owned())))
            .collect();
        let refs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        format!("{AUTHORIZE_ROUTE}?{}", crate::oauth::form(&refs))
    }
}

/// Where an authorization request is answered: an error page, before the
/// client and its redirect URI are known to be good; after, a redirect to
/// the client.
enum Refusal {
    /// Not a request to send anything back to.
    Page(String),
    /// An RFC 6749 §4.1.2.1 error for the client.
    Client(&'static str, String),
}

/// One value of each parameter, or `None` when one came twice (RFC 6749 §3.1:
/// a parameter must not be repeated).
fn single(params: &[(String, String)]) -> Option<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in params {
        if !REQUEST_PARAMS.contains(&k.as_str()) {
            continue;
        }
        if out.iter().any(|(seen, _)| seen == k) {
            return None;
        }
        if !v.is_empty() {
            out.push((k.clone(), v.clone()));
        }
    }
    Some(out)
}

/// Check an authorization request in the order RFC 6749 §4.1.2.1 asks: the
/// client and the redirect URI first, then everything else.
async fn check_request(
    catalog: &Catalog,
    app: &MountedApp,
    params: &[(String, String)],
) -> Result<AuthorizeRequest, (Option<Box<AuthorizeRequest>>, Refusal)> {
    let page = |why: &str| Err((None, Refusal::Page(why.to_owned())));
    let Some(raw) = single(params) else {
        return page("a parameter of the sign-in request was sent twice");
    };
    let get = |name: &str| raw.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let Some(client_id) = get("client_id") else {
        return page("the sign-in request names no client (client_id)");
    };
    let client = match sc_auth::load_oauth_client(catalog, app.app.id.0, &client_id).await {
        Ok(Some(client)) => client,
        Ok(None) => return page("the service asking you to sign in is not registered here"),
        Err(e) => {
            log_failure("could not read an OAuth client", &e);
            return page("the sign-in could not be started");
        }
    };
    let Some(redirect_uri) = get("redirect_uri") else {
        return page("the sign-in request names no redirect_uri");
    };
    if !client.allows_redirect(&redirect_uri) {
        return page(
            "the sign-in request's redirect_uri is not one registered for this service, so \
             it is not followed",
        );
    }
    let mut request = AuthorizeRequest {
        client,
        redirect_uri,
        scopes: Vec::new(),
        state: get("state"),
        nonce: get("nonce"),
        code_challenge: String::new(),
        prompt: Vec::new(),
        raw: raw.clone(),
    };
    let client_error = |request: &AuthorizeRequest, code: &'static str, why: &str| {
        Err((
            Some(Box::new(request.clone())),
            Refusal::Client(code, why.to_owned()),
        ))
    };
    if get("response_type").as_deref() != Some("code") {
        return client_error(
            &request,
            "unsupported_response_type",
            "only response_type=code is supported",
        );
    }
    let scopes: Vec<String> = get("scope")
        .unwrap_or_default()
        .split(' ')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if scopes.is_empty() {
        return client_error(&request, "invalid_scope", "the request names no scope");
    }
    if let Some(unknown) = scopes
        .iter()
        .find(|s| !SUPPORTED_SCOPES.contains(&s.as_str()))
    {
        return client_error(
            &request,
            "invalid_scope",
            &format!("unknown scope `{unknown}`"),
        );
    }
    if let Some(refused) = scopes.iter().find(|s| !request.client.scopes.contains(s)) {
        return client_error(
            &request,
            "invalid_scope",
            &format!("this client may not ask for the scope `{refused}`"),
        );
    }
    let mut deduped: Vec<String> = Vec::new();
    for scope in scopes {
        if !deduped.contains(&scope) {
            deduped.push(scope);
        }
    }
    request.scopes = deduped;
    let Some(challenge) = get("code_challenge") else {
        return client_error(
            &request,
            "invalid_request",
            "a PKCE code_challenge is required (RFC 7636), with code_challenge_method=S256",
        );
    };
    if get("code_challenge_method").as_deref() != Some("S256") {
        return client_error(
            &request,
            "invalid_request",
            "code_challenge_method must be S256",
        );
    }
    let well_formed = challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !well_formed {
        return client_error(
            &request,
            "invalid_request",
            "code_challenge is not the base64url of a SHA-256",
        );
    }
    request.code_challenge = challenge;
    request.prompt = get("prompt")
        .unwrap_or_default()
        .split(' ')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    if request.prompt("none") && request.prompt.len() > 1 {
        return client_error(
            &request,
            "invalid_request",
            "prompt=none cannot be combined with another prompt",
        );
    }
    Ok(request)
}

/// The client's redirect URI with `pairs` added to its query, and the
/// issuer (RFC 9207) and the request's `state`.
fn back_to_client(request: &AuthorizeRequest, issuer: &str, pairs: &[(&str, &str)]) -> String {
    let mut all: Vec<(&str, &str)> = pairs.to_vec();
    if let Some(state) = &request.state {
        all.push(("state", state));
    }
    all.push(("iss", issuer));
    let sep = if request.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{}{sep}{}", request.redirect_uri, crate::oauth::form(&all))
}

/// Send the browser on to `target`, a client's redirect URI: a redirect after
/// a `GET`, a page after a form post (module docs).
fn go(target: &str, after_form: bool) -> Response {
    if after_form {
        continue_page(target)
    } else {
        let mut out = Redirect::to(target).into_response();
        no_store(&mut out);
        out
    }
}

fn client_error(
    request: &AuthorizeRequest,
    issuer: &str,
    code: &str,
    why: &str,
    after_form: bool,
) -> Response {
    go(
        &back_to_client(
            request,
            issuer,
            &[("error", code), ("error_description", why)],
        ),
        after_form,
    )
}

fn refused(
    refusal: (Option<Box<AuthorizeRequest>>, Refusal),
    issuer: &str,
    after_form: bool,
) -> Response {
    match refusal {
        (_, Refusal::Page(why)) => error_page(&why),
        (Some(request), Refusal::Client(code, why)) => {
            client_error(&request, issuer, code, &why, after_form)
        }
        (None, Refusal::Client(_, why)) => error_page(&why),
    }
}

/// `GET /_feldspar/oidc/authorize`.
pub(crate) async fn authorize(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
    csrf: Option<Extension<CsrfToken>>,
) -> Response {
    let Some((app, issuer)) = issuer_of(&state, &headers, &jar) else {
        return not_found();
    };
    let Some(catalog) = catalog(&state).cloned() else {
        return not_found();
    };
    let params = parse_query(&uri);
    let request = match check_request(&catalog, &app, &params).await {
        Ok(request) => request,
        Err(refusal) => return refused(refusal, &issuer, false),
    };
    // A cross-site arrival carries no `Strict` cookie: go round once so it does.
    let bounced = params.iter().any(|(k, _)| k == BOUNCED_PARAM);
    if jar.get(SESSION_COOKIE).is_none() && !bounced {
        let mut pairs: Vec<(&str, &str)> = request
            .raw
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        pairs.push((BOUNCED_PARAM, "1"));
        return continue_page(&format!("{AUTHORIZE_ROUTE}?{}", crate::oauth::form(&pairs)));
    }
    let session = if request.prompt("login") {
        None
    } else {
        match signed_in(&state, &app, &jar).await {
            Ok(session) => session,
            Err(response) => return response,
        }
    };
    let csrf = csrf_of(csrf);
    proceed(
        &state, &catalog, &app, &issuer, &request, session, &csrf, None, false,
    )
    .await
}

/// The user an application session names, and when it started; `None` when
/// there is no session, or its account is disabled.
async fn signed_in(
    state: &AppState,
    app: &MountedApp,
    jar: &CookieJar,
) -> Result<Option<(AuthenticatedUser, Option<chrono::DateTime<Utc>>)>, Response> {
    let session = AppSession::of_request(&state.sessions, app.app.id.0, jar)
        .await
        .map_err(IntoResponse::into_response)?;
    let Some(session) = session else {
        return Ok(None);
    };
    let started = state
        .sessions
        .started_at(session.token())
        .await
        .unwrap_or_else(|e| {
            log_failure("could not read when a session started", &e);
            None
        });
    let user = session.into_user();
    Ok((!user.is_disabled()).then_some((user, started)))
}

/// Everything after the session is known: sign in, admission, consent, the
/// code.
#[allow(clippy::too_many_arguments)]
async fn proceed(
    state: &AppState,
    catalog: &Catalog,
    app: &MountedApp,
    issuer: &str,
    request: &AuthorizeRequest,
    session: Option<(AuthenticatedUser, Option<chrono::DateTime<Utc>>)>,
    csrf: &str,
    problem: Option<&str>,
    after_form: bool,
) -> Response {
    let Some((user, auth_time)) = session else {
        if request.prompt("none") {
            return client_error(
                request,
                issuer,
                "login_required",
                "nobody is signed in",
                after_form,
            );
        }
        return sign_in_page(catalog, app, request, csrf, problem).await;
    };
    // The application's access settings (TODO F9) decide who may sign in to a
    // client through it, as they decide who may sign in to it.
    if refuse_sign_in(state, &app.app, &user).await.is_some() {
        return client_error(
            request,
            issuer,
            "access_denied",
            "this account may not use this application",
            after_form,
        );
    }
    let grant = match sc_auth::load_oauth_grant(catalog, user.id(), &request.client).await {
        Ok(grant) => grant,
        Err(e) => {
            log_failure("could not read an OAuth grant", &e);
            return error_page("the sign-in could not be completed");
        }
    };
    let covered = grant.is_some_and(|g| g.covers(&request.scopes));
    let ask = request.prompt("consent") || (!request.client.skip_consent && !covered);
    if ask {
        if request.prompt("none") {
            return client_error(
                request,
                issuer,
                "consent_required",
                "the user has not consented to this client",
                after_form,
            );
        }
        return consent_page(app, request, &user, csrf);
    }
    issue_code(catalog, issuer, request, &user, auth_time, after_form).await
}

/// Record the grant (a first-party client's too) and send the client its code.
async fn issue_code(
    catalog: &Catalog,
    issuer: &str,
    request: &AuthorizeRequest,
    user: &User,
    auth_time: Option<chrono::DateTime<Utc>>,
    after_form: bool,
) -> Response {
    let issued = async {
        sc_auth::record_oauth_grant(catalog, user.id(), &request.client, &request.scopes).await?;
        sc_auth::issue_authorization_code(
            catalog,
            NewAuthorizationCode {
                client_id: request.client.client_id.clone(),
                app_id: request.client.app_id,
                user_id: user.id(),
                redirect_uri: request.redirect_uri.clone(),
                code_challenge: request.code_challenge.clone(),
                nonce: request.nonce.clone(),
                scopes: request.scopes.clone(),
                auth_time,
                issuer: issuer.to_owned(),
            },
        )
        .await
    }
    .await;
    match issued {
        Ok(code) => go(
            &back_to_client(request, issuer, &[("code", &code)]),
            after_form,
        ),
        Err(e) => {
            log_failure("could not issue an authorization code", &e);
            client_error(
                request,
                issuer,
                "server_error",
                "the sign-in could not be completed",
                after_form,
            )
        }
    }
}

/// `POST /_feldspar/oidc/authorize`: the sign-in page's form, or the consent
/// page's. The CSRF middleware has already checked the form's token.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn authorize_form(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    jar: CookieJar,
    csrf: Option<Extension<CsrfToken>>,
    body: Bytes,
) -> Response {
    let Some((app, issuer)) = issuer_of(&state, &headers, &jar) else {
        return not_found();
    };
    let Some(catalog) = catalog(&state).cloned() else {
        return not_found();
    };
    let form = parse_form(&String::from_utf8_lossy(&body));
    let field = |name: &str| {
        form.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let request = match check_request(&catalog, &app, &form).await {
        Ok(request) => request,
        Err(refusal) => return refused(refusal, &issuer, true),
    };
    let csrf = csrf_of(csrf);
    match field("action") {
        Some("sign_in") => {
            let ip = request_client_ip(&state, peer.map(|p| p.0.0), &headers);
            sign_in(
                &state,
                &catalog,
                &app,
                &issuer,
                &request,
                SignIn {
                    email: field("email").unwrap_or_default().trim(),
                    password: field("password").unwrap_or_default(),
                    ip,
                },
                jar,
                &headers,
                &csrf,
            )
            .await
        }
        Some("consent") => {
            let Some((user, auth_time)) = (match signed_in(&state, &app, &jar).await {
                Ok(session) => session,
                Err(response) => return response,
            }) else {
                // The session went while the page was open: sign in again.
                return proceed(
                    &state, &catalog, &app, &issuer, &request, None, &csrf, None, true,
                )
                .await;
            };
            if field("decision") != Some("allow") {
                return client_error(
                    &request,
                    &issuer,
                    "access_denied",
                    "the user did not allow it",
                    true,
                );
            }
            if refuse_sign_in(&state, &app.app, &user).await.is_some() {
                return client_error(
                    &request,
                    &issuer,
                    "access_denied",
                    "this account may not use this application",
                    true,
                );
            }
            issue_code(&catalog, &issuer, &request, &user, auth_time, true).await
        }
        _ => error_page("the form was not one this page sent"),
    }
}

/// What the sign-in page's form sent.
struct SignIn<'a> {
    email: &'a str,
    password: &'a str,
    ip: Option<std::net::IpAddr>,
}

/// A password sign-in from the authorize page, through the one session path:
/// the login throttle, the password, the application's access settings, the
/// second factor and the session cookie are each what an application's own
/// login meets. On success the browser goes on to the authorize request again
/// — now signed in — which asks for consent or sends the code.
#[allow(clippy::too_many_arguments)]
async fn sign_in(
    state: &AppState,
    catalog: &Catalog,
    app: &std::sync::Arc<MountedApp>,
    issuer: &str,
    request: &AuthorizeRequest,
    form: SignIn<'_>,
    jar: CookieJar,
    headers: &HeaderMap,
    csrf: &str,
) -> Response {
    let again = |problem: String| async move {
        sign_in_page(catalog, app, request, csrf, Some(&problem)).await
    };
    if let Err(limited) = sc_api::throttle::login_attempt(form.ip, form.email) {
        return again(format!(
            "too many sign-in attempts; try again in {} seconds",
            limited.retry_after_secs()
        ))
        .await;
    }
    let verified = match sc_auth::authenticate(catalog, form.email, form.password).await {
        Ok(Some(verified)) => verified,
        Ok(None) => {
            sc_api::throttle::login_failed(form.email);
            return again("the email address or the password is wrong".to_owned()).await;
        }
        Err(e) => {
            log_failure("could not check a password", &e);
            return again("the sign-in could not be completed".to_owned()).await;
        }
    };
    if refuse_sign_in(state, &app.app, verified.user())
        .await
        .is_some()
    {
        return client_error(
            request,
            issuer,
            "access_denied",
            "this account may not use this application",
            true,
        );
    }
    let next = request.after_sign_in();
    let session_token = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
    let client = SessionClient::of(headers)
        .for_app(app)
        .returning_to(next.clone());
    match apply_session(
        state,
        jar,
        session_token,
        client,
        SessionAction::Start(verified),
    )
    .await
    {
        Ok(cookies) => (cookies.jar, cookies.stale, continue_page(&next)).into_response(),
        // A second factor to ask for (a redirect to its page, which comes back
        // to `next`), or a session that could not be started.
        Err(response) => *response,
    }
}

fn csrf_of(csrf: Option<Extension<CsrfToken>>) -> String {
    csrf.map(|Extension(CsrfToken(token))| token)
        .unwrap_or_default()
}

// --- the pages ----------------------------------------------------------------

const EXTRA_STYLE: &str = "<style>input{letter-spacing:normal;font-size:1rem}\
.actions{display:flex;gap:.75rem;flex-wrap:wrap}.secondary{background:var(--muted)}\
ul.scopes{color:var(--text)}.providers a{display:block;margin-top:.5rem}</style>";

fn page(title: &str, body: SafeHtml, status: StatusCode) -> Response {
    let mut full = SafeHtml::trusted(EXTRA_STYLE);
    full.push(&body);
    let mut out = (
        status,
        crate::html_page::html(crate::second_factor::page_html(title, full)),
    )
        .into_response();
    no_store(&mut out);
    // A sign-in or consent page must not be framed by the site asking for it.
    out.headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    out
}

fn error_page(why: &str) -> Response {
    page(
        "Sign-in refused",
        safe_html!(
            "<h1>This sign-in cannot continue</h1><p class=\"error\">{}</p>",
            SafeHtml::escape(why)
        ),
        StatusCode::BAD_REQUEST,
    )
}

/// The authorization request as hidden fields, the CSRF token and `action`.
fn hidden_fields(request: &AuthorizeRequest, csrf: &str, action: &str) -> SafeHtml {
    let mut out = SafeHtml::default();
    let pairs = request
        .raw
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .chain([(CSRF_FORM_FIELD, csrf), ("action", action)]);
    for (name, value) in pairs {
        out.push(&safe_html!(
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            SafeHtml::escape(name),
            SafeHtml::escape(value)
        ));
    }
    out
}

async fn sign_in_page(
    catalog: &Catalog,
    app: &MountedApp,
    request: &AuthorizeRequest,
    csrf: &str,
    problem: Option<&str>,
) -> Response {
    // The identity providers an application's sign-in offers, each coming
    // back to this request once it is done.
    let mut providers = SafeHtml::default();
    match sc_config::oauth_providers(catalog).await {
        Ok(list) => {
            let return_to = request.after_sign_in();
            for provider in list.iter().filter(|p| p.enabled && p.app_login) {
                let href = format!(
                    "{}/{}/start?{}",
                    crate::oauth::OAUTH_PREFIX,
                    provider.name,
                    crate::oauth::form(&[("return_to", &return_to)])
                );
                providers.push(&safe_html!(
                    "<a class=\"button secondary\" href=\"{}\">Sign in with {}</a>",
                    SafeHtml::escape(&href),
                    SafeHtml::escape(&provider.label)
                ));
            }
        }
        Err(e) => log_failure("could not read the identity providers", &e),
    }
    let problem = match problem {
        Some(why) => safe_html!("<p class=\"error\">{}</p>", SafeHtml::escape(why)),
        None => SafeHtml::default(),
    };
    page(
        &format!("Sign in to {}", app.app.name),
        safe_html!(
            "<h1>Sign in to {app}</h1><p>to continue to {client}</p>{problem}\
             <form method=\"post\" action=\"{action}\">{hidden}\
             <label for=\"email\">Email</label>\
             <input id=\"email\" name=\"email\" type=\"email\" autocomplete=\"username\" required autofocus>\
             <label for=\"password\">Password</label>\
             <input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required>\
             <button type=\"submit\">Sign in</button></form>\
             <div class=\"providers\">{providers}</div>",
            app = SafeHtml::escape(&app.app.name),
            client = SafeHtml::escape(&request.client.name),
            problem = problem,
            action = SafeHtml::trusted(AUTHORIZE_ROUTE),
            hidden = hidden_fields(request, csrf, "sign_in"),
            providers = providers,
        ),
        StatusCode::OK,
    )
}

/// What a scope lets the client see, as the consent page says it.
fn scope_sentence(scope: &str) -> Option<&'static str> {
    match scope {
        SCOPE_OPENID => Some("know who you are when you sign in"),
        SCOPE_EMAIL => Some("see your email address"),
        SCOPE_PROFILE => Some("see your name"),
        _ => None,
    }
}

fn consent_page(app: &MountedApp, request: &AuthorizeRequest, user: &User, csrf: &str) -> Response {
    let mut items = SafeHtml::default();
    for sentence in request.scopes.iter().filter_map(|s| scope_sentence(s)) {
        items.push(&safe_html!("<li>{}</li>", SafeHtml::escape(sentence)));
    }
    if request.scopes.iter().any(|s| s == SCOPE_OPENID) && !request.client.claim_fields.is_empty() {
        let names: Vec<&str> = request
            .client
            .claim_fields
            .iter()
            .map(|c| c.claim.as_str())
            .collect();
        items.push(&safe_html!(
            "<li>see these details of your account: {}</li>",
            SafeHtml::escape(&names.join(", "))
        ));
    }
    let email = user
        .get(sc_auth::COL_EMAIL)
        .and_then(|v| match v {
            sc_query::Value::Text(t) => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_default();
    page(
        &format!("Allow {}?", request.client.name),
        safe_html!(
            "<h1>Allow {client} to use your {app} account?</h1>\
             <p>Signed in as {email}. {client} will be able to:</p>\
             <ul class=\"scopes\">{items}</ul>\
             <form method=\"post\" action=\"{action}\">{hidden}\
             <div class=\"actions\">\
             <button type=\"submit\" name=\"decision\" value=\"allow\">Allow</button>\
             <button type=\"submit\" name=\"decision\" value=\"deny\" class=\"secondary\">Deny</button>\
             </div></form>",
            client = SafeHtml::escape(&request.client.name),
            app = SafeHtml::escape(&app.app.name),
            email = SafeHtml::escape(&email),
            items = items,
            action = SafeHtml::trusted(AUTHORIZE_ROUTE),
            hidden = hidden_fields(request, csrf, "consent"),
        ),
        StatusCode::OK,
    )
}

/// A page that goes on to `target` (module docs).
fn continue_page(target: &str) -> Response {
    let mut out = (
        StatusCode::OK,
        crate::html_page::html(safe_html!(
            "<!doctype html><html><head><meta charset=\"utf-8\">\
             <meta http-equiv=\"refresh\" content=\"0;url={target}\">\
             <title>Continuing…</title></head>\
             <body><p><a href=\"{target}\">Continue</a></p></body></html>",
            target = SafeHtml::escape(target),
        )),
    )
        .into_response();
    no_store(&mut out);
    out
}

fn no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
}

// --- the token endpoint -------------------------------------------------------

/// An RFC 6749 §5.2 error answer.
fn token_error(status: StatusCode, code: &str, why: &str) -> Response {
    let mut out = (
        status,
        Json(json!({ "error": code, "error_description": why })),
    )
        .into_response();
    no_store(&mut out);
    out
}

fn invalid_request(why: &str) -> Response {
    token_error(StatusCode::BAD_REQUEST, "invalid_request", why)
}

fn invalid_grant(why: &str) -> Response {
    token_error(StatusCode::BAD_REQUEST, "invalid_grant", why)
}

/// `client_secret_basic`: the id and the secret in `Authorization: Basic`,
/// each form-encoded first (RFC 6749 §2.3.1).
fn basic_credentials(headers: &HeaderMap) -> Option<Result<(String, String), ()>> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    Some(match decoded.as_deref().and_then(|d| d.split_once(':')) {
        Some((id, secret)) => Ok((form_decode(id), form_decode(secret))),
        None => Err(()),
    })
}

/// A form-encoded body, every parameter at most once (RFC 6749 §3.2).
fn token_form(headers: &HeaderMap, body: &Bytes) -> Result<Vec<(String, String)>, Box<Response>> {
    let form_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
    if !form_type {
        return Err(Box::new(invalid_request(
            "the body must be application/x-www-form-urlencoded",
        )));
    }
    let form = parse_form(&String::from_utf8_lossy(body));
    let mut seen: Vec<&str> = Vec::new();
    for (k, _) in &form {
        if seen.contains(&k.as_str()) {
            return Err(Box::new(invalid_request(&format!(
                "the parameter `{k}` was sent twice"
            ))));
        }
        seen.push(k);
    }
    Ok(form)
}

/// The non-empty value of a form's parameter.
fn form_field<'a>(form: &'a [(String, String)], name: &str) -> Option<&'a str> {
    form.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// The client a token or revocation request is from, authenticated
/// (`client_secret_basic`, `client_secret_post`, or none for a public client)
/// and rate-limited per address and per client.
async fn authenticate_client(
    state: &AppState,
    catalog: &Catalog,
    app: &MountedApp,
    peer: Option<std::net::SocketAddr>,
    headers: &HeaderMap,
    form: &[(String, String)],
) -> Result<OAuthClient, Box<Response>> {
    let field = |name: &str| form_field(form, name);
    if let Some(ip) = request_client_ip(state, peer, headers)
        && let Err(limited) =
            sc_ratelimit::global().hit(&format!("oidc-token:ip:{ip}"), TOKEN_PER_IP)
    {
        return Err(Box::new(slow_down(limited.retry_after_secs())));
    }

    // Who is asking: one way of saying so, not two (RFC 6749 §2.3).
    let (client_id, secret, used_basic) = match basic_credentials(headers) {
        Some(Err(())) => {
            return Err(Box::new(token_error(
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                "the Authorization header is not client_id:client_secret in base64",
            )));
        }
        Some(Ok((id, secret))) => {
            if field("client_secret").is_some() {
                return Err(Box::new(invalid_request(
                    "send the client secret one way, not two",
                )));
            }
            if field("client_id").is_some_and(|body_id| body_id != id) {
                return Err(Box::new(invalid_request(
                    "client_id does not match the Authorization header",
                )));
            }
            (id, Some(secret), true)
        }
        None => {
            let Some(id) = field("client_id") else {
                return Err(Box::new(token_error(
                    StatusCode::UNAUTHORIZED,
                    "invalid_client",
                    "the request does not say which client it is from",
                )));
            };
            (
                id.to_owned(),
                field("client_secret").map(str::to_owned),
                false,
            )
        }
    };
    if let Err(limited) =
        sc_ratelimit::global().hit(&format!("oidc-token:client:{client_id}"), TOKEN_PER_CLIENT)
    {
        return Err(Box::new(slow_down(limited.retry_after_secs())));
    }
    match sc_auth::authenticate_oauth_client(catalog, app.app.id.0, &client_id, secret.as_deref())
        .await
    {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(refused)) => {
            let mut out = token_error(StatusCode::UNAUTHORIZED, "invalid_client", &refused.0);
            if used_basic {
                out.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Basic realm=\"feldspar\""),
                );
            }
            Err(Box::new(out))
        }
        Err(e) => Err(Box::new(server_error(&e))),
    }
}

#[allow(clippy::too_many_lines)]
/// `POST /_feldspar/oidc/token`. Takes no cookie jar: the route ignores
/// cookies (module docs).
pub(crate) async fn token(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((app, _)) = issuer_of(&state, &headers, &CookieJar::new()) else {
        return not_found();
    };
    let Some(catalog) = catalog(&state).cloned() else {
        return not_found();
    };
    let form = match token_form(&headers, &body) {
        Ok(form) => form,
        Err(response) => return *response,
    };
    let field = |name: &str| form_field(&form, name);
    let client =
        match authenticate_client(&state, &catalog, &app, peer.map(|p| p.0.0), &headers, &form)
            .await
        {
            Ok(client) => client,
            Err(response) => return *response,
        };

    let subject = match field("grant_type") {
        None => return invalid_request("grant_type is required"),
        Some("authorization_code") => {
            let (Some(code), Some(redirect_uri)) = (field("code"), field("redirect_uri")) else {
                return invalid_request("code and redirect_uri are required");
            };
            let Some(verifier) = field("code_verifier") else {
                return invalid_request("code_verifier is required: every client uses PKCE");
            };
            match sc_auth::redeem_authorization_code(
                &catalog,
                &client,
                code,
                redirect_uri,
                verifier,
            )
            .await
            {
                Ok(Ok(redeemed)) => Subject::Code(redeemed),
                Ok(Err(refused)) => return invalid_grant(&refused.reason),
                Err(e) => return server_error(&e),
            }
        }
        Some("refresh_token") => {
            let Some(refresh) = field("refresh_token") else {
                return invalid_request("refresh_token is required");
            };
            let narrower: Option<Vec<String>> = field("scope").map(|s| {
                s.split(' ')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            });
            match sc_auth::redeem_refresh_token(&catalog, &client, refresh, narrower.as_deref())
                .await
            {
                Ok(Ok(subject)) => Subject::Refresh(subject),
                Ok(Err(refused)) if refused.invalid_scope => {
                    return token_error(StatusCode::BAD_REQUEST, "invalid_scope", &refused.reason);
                }
                Ok(Err(refused)) => return invalid_grant(&refused.reason),
                Err(e) => return server_error(&e),
            }
        }
        Some(other) => {
            return token_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                &format!("the grant type `{other}` is not supported"),
            );
        }
    };

    // The account, as it is now: gone, disabled or no longer admitted, it
    // gets nothing more.
    let user_id = subject.user_id();
    let user = match sc_auth::load_user(&catalog, user_id).await {
        Ok(Some(user)) if !user.is_disabled() => user,
        Ok(_) => {
            subject.revoke(&catalog).await;
            return invalid_grant("the account is disabled or no longer exists");
        }
        Err(e) => return server_error(&e),
    };
    if refuse_sign_in(&state, &app.app, &user).await.is_some() {
        subject.revoke(&catalog).await;
        return invalid_grant("this account may no longer use this application");
    }
    let grant = match sc_auth::load_oauth_grant(&catalog, user.id(), &client).await {
        Ok(Some(grant)) => grant,
        Ok(None) => {
            subject.revoke(&catalog).await;
            return invalid_grant("the user's consent to this client has been withdrawn");
        }
        Err(e) => return server_error(&e),
    };
    let token_subject = match subject {
        Subject::Code(ref redeemed) => TokenSubject {
            client_id: client.client_id.clone(),
            app_id: client.app_id,
            user_id: user.id(),
            scopes: redeemed.scopes.clone(),
            family: redeemed.family,
            grant_id: grant.id,
            auth_time: redeemed.auth_time,
            issuer: redeemed.issuer.clone(),
            auth_epoch: redeemed.auth_epoch,
        },
        Subject::Refresh(ref subject) => subject.clone(),
    };
    let tokens = match sc_auth::issue_oauth_tokens(&catalog, &token_subject).await {
        Ok(tokens) => tokens,
        Err(e) => return server_error(&e),
    };
    let mut answer = json!({
        "access_token": tokens.access_token,
        "token_type": "Bearer",
        "expires_in": tokens.expires_in,
        "refresh_token": tokens.refresh_token,
        "scope": tokens.scopes.join(" "),
    });
    if tokens.scopes.iter().any(|s| s == SCOPE_OPENID) {
        let nonce = match &subject {
            Subject::Code(redeemed) => redeemed.nonce.clone(),
            Subject::Refresh(_) => None,
        };
        let about = match sc_auth::oauth_user_claims(&catalog, &user, &client, &tokens.scopes).await
        {
            Ok(about) => about,
            Err(e) => return server_error(&e),
        };
        let claims = id_token_claims(&IdToken {
            issuer: &token_subject.issuer,
            client_id: &client.client_id,
            about,
            auth_time: token_subject.auth_time,
            nonce: nonce.as_deref(),
            access_token: &tokens.access_token,
            now: Utc::now().timestamp(),
        });
        let signed = match sc_auth::ensure_signing_keys(&catalog).await {
            Ok(_) => sc_auth::sign_jwt(&catalog, SigningAlg::Rs256, &claims).await,
            Err(e) => Err(e),
        };
        match signed {
            Ok(id_token) => answer["id_token"] = Value::String(id_token),
            Err(e) => return server_error(&e),
        }
    }
    let mut out = Json(answer).into_response();
    no_store(&mut out);
    out
}

/// What a token request was granted on.
enum Subject {
    Code(sc_auth::RedeemedCode),
    Refresh(TokenSubject),
}

impl Subject {
    fn user_id(&self) -> uuid::Uuid {
        match self {
            Subject::Code(redeemed) => redeemed.user_id,
            Subject::Refresh(subject) => subject.user_id,
        }
    }

    /// End the family: the account the grant was for may no longer have it.
    async fn revoke(&self, catalog: &Catalog) {
        let family = match self {
            Subject::Code(redeemed) => redeemed.family,
            Subject::Refresh(subject) => subject.family,
        };
        if let Err(e) = sc_auth::revoke_family(catalog, family).await {
            log_failure("could not revoke an OAuth token family", &e);
        }
    }
}

/// What an `id_token` says.
struct IdToken<'a> {
    issuer: &'a str,
    client_id: &'a str,
    /// The claims about the user ([`sc_auth::oauth_user_claims`]): `sub`, and
    /// what the scopes and the client's mappings add.
    about: serde_json::Map<String, Value>,
    auth_time: Option<chrono::DateTime<Utc>>,
    nonce: Option<&'a str>,
    access_token: &'a str,
    now: i64,
}

/// The `id_token`'s claims (OIDC Core §2): the protocol's, then the claims
/// about the user, which are what `/userinfo` answers with too. A mapped
/// claim never overwrites one of the protocol's (`sc_auth::RESERVED_CLAIMS`).
fn id_token_claims(token: &IdToken<'_>) -> Value {
    let mut claims = serde_json::Map::new();
    for (k, v) in &token.about {
        claims.insert(k.clone(), v.clone());
    }
    claims.insert("iss".to_owned(), json!(token.issuer));
    claims.insert("aud".to_owned(), json!(token.client_id));
    claims.insert("exp".to_owned(), json!(token.now + ID_TOKEN_TTL_SECONDS));
    claims.insert("iat".to_owned(), json!(token.now));
    claims.insert(
        "at_hash".to_owned(),
        json!(sc_auth::left_half_sha256(token.access_token)),
    );
    if let Some(at) = token.auth_time {
        claims.insert("auth_time".to_owned(), json!(at.timestamp()));
    }
    if let Some(nonce) = token.nonce {
        claims.insert("nonce".to_owned(), json!(nonce));
    }
    Value::Object(claims)
}

// --- userinfo -----------------------------------------------------------------

/// An RFC 6750 §3 answer to a bearer request that did not authenticate.
fn bearer_error(status: StatusCode, code: Option<&str>, why: &str) -> Response {
    let challenge = match code {
        Some(code) => format!(
            "Bearer realm=\"feldspar\", error=\"{code}\", error_description=\"{}\"",
            why.replace(['"', '\\'], "'")
        ),
        None => "Bearer realm=\"feldspar\"".to_owned(),
    };
    let mut out = match code {
        Some(code) => (
            status,
            Json(json!({ "error": code, "error_description": why })),
        )
            .into_response(),
        None => status.into_response(),
    };
    no_store(&mut out);
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        out.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    out
}

fn invalid_token(why: &str) -> Response {
    bearer_error(StatusCode::UNAUTHORIZED, Some("invalid_token"), why)
}

/// `GET` / `POST /_feldspar/oidc/userinfo` (OIDC Core §5.3): the claims about
/// the user an access token is for, with `Authorization: Bearer`. The route
/// ignores cookies, so the token is all a request is judged by, and only one
/// of this provider's access tokens, issued on this application, is one:
/// an API token, a session token or a refresh token is not.
pub(crate) async fn userinfo(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some((app, _)) = issuer_of(&state, &headers, &CookieJar::new()) else {
        return not_found();
    };
    let Some(catalog) = catalog(&state).cloned() else {
        return not_found();
    };
    let Some(presented) = crate::security::bearer_token(&headers) else {
        return bearer_error(StatusCode::UNAUTHORIZED, None, "");
    };
    let access =
        match sc_auth::authenticate_oauth_access_token(&catalog, app.app.id.0, &presented).await {
            Ok(Some(access)) => access,
            Ok(None) => {
                return invalid_token(
                    "the access token is not one this provider issued here, or it has expired \
                     or been revoked",
                );
            }
            Err(e) => return server_error(&e),
        };
    if !access.scopes.iter().any(|s| s == SCOPE_OPENID) {
        return bearer_error(
            StatusCode::FORBIDDEN,
            Some("insufficient_scope"),
            "the access token was not granted the openid scope",
        );
    }
    let user = match sc_auth::load_user(&catalog, access.user_id).await {
        Ok(Some(user)) if !user.is_disabled() => user,
        Ok(_) => return invalid_token("the account is disabled or no longer exists"),
        Err(e) => return server_error(&e),
    };
    if refuse_sign_in(&state, &app.app, &user).await.is_some() {
        return invalid_token("this account may no longer use this application");
    }
    let client = match sc_auth::load_oauth_client(&catalog, app.app.id.0, &access.client_id).await {
        Ok(Some(client)) => client,
        Ok(None) => return invalid_token("the client the token was issued to is gone"),
        Err(e) => return server_error(&e),
    };
    match sc_auth::oauth_user_claims(&catalog, &user, &client, &access.scopes).await {
        Ok(claims) => {
            let mut out = Json(Value::Object(claims)).into_response();
            no_store(&mut out);
            out
        }
        Err(e) => server_error(&e),
    }
}

// --- revocation ---------------------------------------------------------------

/// `POST /_feldspar/oidc/revoke` (RFC 7009): a client gives up a token. The
/// client authenticates as at the token endpoint. A refresh token ends every
/// token of its sign-in; an access token ends alone. `token_type_hint` is a
/// hint only: both kinds are looked for. The answer is `200` whether or not
/// the token was one (§2.2), so it tells nobody which tokens exist.
pub(crate) async fn revoke(
    State(state): State<AppState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((app, _)) = issuer_of(&state, &headers, &CookieJar::new()) else {
        return not_found();
    };
    let Some(catalog) = catalog(&state).cloned() else {
        return not_found();
    };
    let form = match token_form(&headers, &body) {
        Ok(form) => form,
        Err(response) => return *response,
    };
    let client =
        match authenticate_client(&state, &catalog, &app, peer.map(|p| p.0.0), &headers, &form)
            .await
        {
            Ok(client) => client,
            Err(response) => return *response,
        };
    let Some(token) = form_field(&form, "token") else {
        return invalid_request("token is required");
    };
    match sc_auth::revoke_oauth_token(&catalog, &client, token).await {
        Ok(_) => {
            let mut out = StatusCode::OK.into_response();
            no_store(&mut out);
            out
        }
        Err(e) => server_error(&e),
    }
}

// --- RP-initiated logout ------------------------------------------------------

/// The end-session parameters this endpoint reads.
const LOGOUT_PARAMS: &[&str] = &[
    "id_token_hint",
    "client_id",
    "post_logout_redirect_uri",
    "state",
];

/// A checked logout request.
struct LogoutRequest {
    /// The client it came from, when the hint or `client_id` names one.
    client: Option<OAuthClient>,
    /// The `sub` of a valid `id_token_hint`: who the client thinks is signed in.
    hint_sub: Option<String>,
    /// Where to go afterwards: a registered post-logout redirect URI.
    redirect: Option<String>,
    state: Option<String>,
    /// The parameters as sent, for the confirmation page's form.
    raw: Vec<(String, String)>,
}

/// Check a logout request: an `id_token_hint` must be one this provider
/// signed for this issuer, a `client_id` must agree with it and be registered
/// here, and a `post_logout_redirect_uri` must be registered **exactly** for
/// that client — anything else is an error page, never a redirect.
async fn check_logout(
    catalog: &Catalog,
    app: &MountedApp,
    issuer: &str,
    params: &[(String, String)],
) -> Result<LogoutRequest, Box<Response>> {
    let mut raw: Vec<(String, String)> = Vec::new();
    for (k, v) in params {
        if !LOGOUT_PARAMS.contains(&k.as_str()) || v.is_empty() {
            continue;
        }
        if raw.iter().any(|(seen, _)| seen == k) {
            return Err(Box::new(error_page(
                "a parameter of the sign-out request was sent twice",
            )));
        }
        raw.push((k.clone(), v.clone()));
    }
    let get = |name: &str| raw.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let mut client_id = get("client_id");
    let mut hint_sub = None;
    if let Some(hint) = get("id_token_hint") {
        let claims = match sc_auth::verify_own_jwt(catalog, &hint).await {
            Ok(Some(claims)) => claims,
            Ok(None) => {
                return Err(Box::new(error_page(
                    "the sign-out request's id_token_hint was not issued by this provider",
                )));
            }
            Err(e) => {
                log_failure("could not check an id_token_hint", &e);
                return Err(Box::new(error_page("the sign-out could not be checked")));
            }
        };
        if claims.get("iss").and_then(Value::as_str) != Some(issuer) {
            return Err(Box::new(error_page(
                "the sign-out request's id_token_hint was issued for another application",
            )));
        }
        let aud = match claims.get("aud") {
            Some(Value::String(one)) => Some(one.clone()),
            Some(Value::Array(many)) => many.first().and_then(Value::as_str).map(str::to_owned),
            _ => None,
        };
        if let (Some(asked), Some(aud)) = (&client_id, &aud)
            && asked != aud
        {
            return Err(Box::new(error_page(
                "the sign-out request's client_id is not the one its id_token_hint was issued to",
            )));
        }
        client_id = client_id.or(aud);
        hint_sub = claims.get("sub").and_then(Value::as_str).map(str::to_owned);
    }
    let client = match &client_id {
        Some(id) => match sc_auth::load_oauth_client(catalog, app.app.id.0, id).await {
            Ok(Some(client)) => Some(client),
            Ok(None) => {
                return Err(Box::new(error_page(
                    "the service asking you to sign out is not registered here",
                )));
            }
            Err(e) => {
                log_failure("could not read an OAuth client", &e);
                return Err(Box::new(error_page("the sign-out could not be checked")));
            }
        },
        None => None,
    };
    let redirect = match get("post_logout_redirect_uri") {
        None => None,
        Some(uri) => {
            if !client
                .as_ref()
                .is_some_and(|c| c.allows_logout_redirect(&uri))
            {
                return Err(Box::new(error_page(
                    "the sign-out request's post_logout_redirect_uri is not one registered for \
                     this service, so it is not followed",
                )));
            }
            Some(uri)
        }
    };
    Ok(LogoutRequest {
        client,
        hint_sub,
        redirect,
        state: get("state"),
        raw,
    })
}

/// `GET /_feldspar/oidc/logout`: a relying party sends the browser here to
/// sign out. With an `id_token_hint` for the user signed in, the session ends
/// at once; without one, the user is asked first (a CSRF-checked form posts
/// back here), so another site cannot sign anybody out by linking here. The
/// tokens the client holds are its own business and stay; it revokes them
/// at `/revoke`.
pub(crate) async fn logout(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
    csrf: Option<Extension<CsrfToken>>,
) -> Response {
    let Some((app, issuer)) = issuer_of(&state, &headers, &jar) else {
        return not_found();
    };
    let Some(catalog) = catalog(&state).cloned() else {
        return not_found();
    };
    let params = parse_query(&uri);
    let request = match check_logout(&catalog, &app, &issuer, &params).await {
        Ok(request) => request,
        Err(page) => return *page,
    };
    // As at `/authorize`: a cross-site arrival carries no `Strict` cookie.
    let bounced = params.iter().any(|(k, _)| k == BOUNCED_PARAM);
    if jar.get(SESSION_COOKIE).is_some() || bounced {
        let user = match signed_in(&state, &app, &jar).await {
            Ok(session) => session.map(|(user, _)| user),
            Err(response) => return response,
        };
        let Some(user) = user else {
            return signed_out(&request, false);
        };
        if request.hint_sub.as_deref() != Some(user.id().to_string().as_str()) {
            return logout_page(&app, &request, &user, &csrf_of(csrf));
        }
        return end_session(&state, &app, &request, jar, &headers, false).await;
    }
    let mut pairs: Vec<(&str, &str)> = request
        .raw
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    pairs.push((BOUNCED_PARAM, "1"));
    continue_page(&format!("{LOGOUT_ROUTE}?{}", crate::oauth::form(&pairs)))
}

/// `POST /_feldspar/oidc/logout`: the confirmation page's form. The CSRF
/// middleware has checked its token, so a relying party's own logout is a
/// `GET`.
pub(crate) async fn logout_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    body: Bytes,
) -> Response {
    let Some((app, issuer)) = issuer_of(&state, &headers, &jar) else {
        return not_found();
    };
    let Some(catalog) = catalog(&state).cloned() else {
        return not_found();
    };
    let form = parse_form(&String::from_utf8_lossy(&body));
    let request = match check_logout(&catalog, &app, &issuer, &form).await {
        Ok(request) => request,
        Err(page) => return *page,
    };
    if form_field(&form, "decision") != Some("sign_out") {
        return page(
            "Still signed in",
            safe_html!(
                "<h1>You are still signed in to {}</h1>",
                SafeHtml::escape(&app.app.name)
            ),
            StatusCode::OK,
        );
    }
    end_session(&state, &app, &request, jar, &headers, true).await
}

/// End the application's session, then go on as [`signed_out`] does.
async fn end_session(
    state: &AppState,
    app: &std::sync::Arc<MountedApp>,
    request: &LogoutRequest,
    jar: CookieJar,
    headers: &HeaderMap,
    after_form: bool,
) -> Response {
    let session_token = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
    let client = SessionClient::of(headers).for_app(app);
    match apply_session(state, jar, session_token, client, SessionAction::End).await {
        Ok(cookies) => {
            (cookies.jar, cookies.stale, signed_out(request, after_form)).into_response()
        }
        Err(response) => *response,
    }
}

/// Back to the client's post-logout redirect URI, with its `state`, or a page
/// saying so when it named none.
fn signed_out(request: &LogoutRequest, after_form: bool) -> Response {
    let Some(redirect) = &request.redirect else {
        return page(
            "Signed out",
            safe_html!("<h1>You are signed out</h1>"),
            StatusCode::OK,
        );
    };
    let target = match &request.state {
        Some(state) => {
            let sep = if redirect.contains('?') { '&' } else { '?' };
            format!(
                "{redirect}{sep}{}",
                crate::oauth::form(&[("state", state.as_str())])
            )
        }
        None => redirect.clone(),
    };
    go(&target, after_form)
}

fn logout_page(app: &MountedApp, request: &LogoutRequest, user: &User, csrf: &str) -> Response {
    let email = match user.get(sc_auth::COL_EMAIL) {
        Some(sc_query::Value::Text(t)) => t.clone(),
        _ => String::new(),
    };
    let from = match &request.client {
        Some(client) => safe_html!(
            "<p>{} asks you to sign out.</p>",
            SafeHtml::escape(&client.name)
        ),
        None => SafeHtml::default(),
    };
    let mut hidden = SafeHtml::default();
    let pairs = request
        .raw
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .chain([(CSRF_FORM_FIELD, csrf)]);
    for (name, value) in pairs {
        hidden.push(&safe_html!(
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            SafeHtml::escape(name),
            SafeHtml::escape(value)
        ));
    }
    page(
        &format!("Sign out of {}?", app.app.name),
        safe_html!(
            "<h1>Sign out of {app}?</h1>{from}<p>Signed in as {email}.</p>\
             <form method=\"post\" action=\"{action}\">{hidden}\
             <div class=\"actions\">\
             <button type=\"submit\" name=\"decision\" value=\"sign_out\">Sign out</button>\
             <button type=\"submit\" name=\"decision\" value=\"stay\" class=\"secondary\">Stay signed in</button>\
             </div></form>",
            app = SafeHtml::escape(&app.app.name),
            from = from,
            email = SafeHtml::escape(&email),
            action = SafeHtml::trusted(LOGOUT_ROUTE),
            hidden = hidden,
        ),
        StatusCode::OK,
    )
}

fn slow_down(retry_after: u64) -> Response {
    let mut out = token_error(
        StatusCode::TOO_MANY_REQUESTS,
        "slow_down",
        &format!("too many token requests; retry in {retry_after} s"),
    );
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        out.headers_mut().insert(header::RETRY_AFTER, value);
    }
    out
}

fn server_error(e: &sc_error::Error) -> Response {
    log_failure("the identity provider's token endpoint failed", e);
    token_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "the request could not be completed",
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_metadata_names_this_issuer_s_endpoints_and_s256_only() {
        let doc = metadata("https://blog.example.com");
        assert_eq!(doc["issuer"], "https://blog.example.com");
        assert_eq!(
            doc["authorization_endpoint"],
            "https://blog.example.com/_feldspar/oidc/authorize"
        );
        assert_eq!(
            doc["token_endpoint"],
            "https://blog.example.com/_feldspar/oidc/token"
        );
        assert_eq!(
            doc["jwks_uri"],
            "https://blog.example.com/_feldspar/oidc/jwks"
        );
        assert_eq!(doc["code_challenge_methods_supported"], json!(["S256"]));
        assert_eq!(doc["response_types_supported"], json!(["code"]));
        assert_eq!(
            doc["id_token_signing_alg_values_supported"],
            json!(["RS256", "ES256"])
        );
    }

    #[test]
    fn basic_credentials_are_form_decoded_and_a_bad_header_is_refused() {
        let mut headers = HeaderMap::new();
        let encoded = base64::engine::general_purpose::STANDARD.encode("fdc_a%3Ab:s%2Bcret");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
        );
        assert_eq!(
            basic_credentials(&headers),
            Some(Ok(("fdc_a:b".to_owned(), "s+cret".to_owned())))
        );
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic !!!"));
        assert_eq!(basic_credentials(&headers), Some(Err(())));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"));
        assert_eq!(basic_credentials(&headers), None);
    }
}

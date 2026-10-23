//! Signing in with an external identity provider: OAuth 2.0's authorization
//! code flow, with PKCE (Settings → Authentication).
//!
//! The admin UI and every application sign in this way through four routes,
//! all under [`OAUTH_PREFIX`] and matched before any application is, so they
//! answer on every host:
//!
//! 1. **`GET /_feldspar/oauth/providers`** lists the providers this host
//!    offers: the admin UI's (`admin_login`) or an application's
//!    (`app_login`), each with the address that starts it.
//! 2. **`GET /_feldspar/oauth/{provider}/start?return_to=/path`** remembers the
//!    flow, sets a short-lived *binding* cookie on this host, and sends the
//!    browser to the provider.
//! 3. **`GET /_feldspar/oauth/callback`** is where every provider sends the
//!    browser back, and it is always on the **admin UI's host**, so a provider
//!    is told one address however many applications use it. It exchanges the
//!    code for a token, reads the person's email, finds (or, for an
//!    application, may create) the account, and sends the browser back to the
//!    host it started on with a single-use token.
//! 4. **`GET /_feldspar/oauth/complete?token=…`** on that host checks the
//!    binding cookie — the flow must finish in the browser that started it,
//!    which is what stops somebody else's sign-in being planted in yours — and
//!    starts the session through the router's one session path, so Settings →
//!    Authentication's shared cookie and session lifetime apply exactly as they
//!    do to a password.
//!
//! The same callback and completion serve a module's **consent flow**
//! (TODO-imap M10.3, `sc_module::consent`): an action's `oauth.begin(…)` keeps
//! a flow and answers **`GET /_feldspar/oauth/consent/{id}`**, where the
//! application sends the browser. That route checks the session is the user
//! whose call began it, sets the binding cookie and goes to the provider; the
//! callback exchanges the code for the module's tokens; and the completion, in
//! the browser that started, writes them into the row the module named, then
//! sends the browser back to the application. Nobody is signed in by it.
//!
//! Flows and tokens live in this process's memory, as the admin move's handoff
//! does: a callback must reach the node that started its flow, and a restart in
//! between costs the person one more click.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use base64::Engine;
use sc_api::SessionAction;
use sc_auth::User;
use sc_config::OAuthProvider;
use sc_module::consent::ConsentFlow;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::router::{
    AppState, Resolved, SessionClient, apply_session, log_failure, parse_query, refuse_sign_in,
    resolve_app, session_user,
};

/// Where every route of the flow lives.
pub const OAUTH_PREFIX: &str = "/_feldspar/oauth";
/// The providers this host offers.
pub const PROVIDERS_ROUTE: &str = "/_feldspar/oauth/providers";
/// Start signing in with one provider.
pub const START_ROUTE: &str = "/_feldspar/oauth/{provider}/start";
/// Where a provider sends the browser back: the address to register with it.
pub const CALLBACK_ROUTE: &str = "/_feldspar/oauth/callback";
/// Where the browser finishes, on the host it started on.
pub const COMPLETE_ROUTE: &str = "/_feldspar/oauth/complete";
/// Where a module's consent flow starts in the browser
/// ([`sc_module::consent::CONSENT_PREFIX`]`/{id}`).
pub const CONSENT_ROUTE: &str = "/_feldspar/oauth/consent/{id}";

/// The cookie binding a flow to the browser that started it.
pub const BINDING_COOKIE: &str = "sc_oauth";

/// How long a person may take at the provider: long enough to type a password
/// and pass a second factor.
pub const FLOW_TTL: Duration = Duration::from_secs(600);
/// How long the token from the callback may wait to be spent: a redirect.
pub const COMPLETION_TTL: Duration = Duration::from_secs(120);

/// How long a request to a provider may take.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(15);

/// The query parameter an error comes back to the sign-in page under.
pub const ERROR_PARAM: &str = "oauth_error";

/// The flows this process has started and not finished.
#[derive(Default)]
pub struct OAuthFlows {
    /// `state` → the flow it names, until the provider sends it back.
    pending: Mutex<HashMap<String, Pending>>,
    /// Completion token → the signed-in user, until the starting host spends it.
    completions: Mutex<HashMap<String, Completion>>,
}

/// What a flow is for.
enum Purpose {
    /// Signing in with the provider of this name; `admin` when the flow
    /// started on the admin UI's host, where only administrators finish.
    SignIn { provider: String, admin: bool },
    /// A module's tokens, for the row its flow names.
    Consent(Box<ConsentFlow>),
}

/// A flow waiting for the provider to send the browser back.
struct Pending {
    purpose: Purpose,
    verifier: String,
    /// The host it started on (with its port), which it finishes on.
    host: String,
    return_to: String,
    binding: String,
    redirect_uri: String,
    expires: Instant,
}

/// What the provider granted.
enum Outcome {
    /// The account it vouched for.
    SignIn(User),
    /// The token response, for the module's row.
    Consent {
        flow: Box<ConsentFlow>,
        tokens: Value,
    },
}

/// A flow the provider answered, waiting for its host.
struct Completion {
    outcome: Outcome,
    host: String,
    return_to: String,
    binding: String,
    expires: Instant,
}

impl OAuthFlows {
    fn start(&self, state: String, pending: Pending) {
        let now = Instant::now();
        let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, p| p.expires > now);
        map.insert(state, pending);
    }

    /// The flow `state` names, once, if it has not lapsed.
    fn take(&self, state: &str) -> Option<Pending> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(state)?;
        (pending.expires > Instant::now()).then_some(pending)
    }

    fn complete(&self, completion: Completion) -> String {
        let token = crate::security::new_csrf_token();
        let now = Instant::now();
        let mut map = self.completions.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, c| c.expires > now);
        map.insert(token.clone(), completion);
        token
    }

    /// The completion `token` names, once, if it has not lapsed.
    fn redeem(&self, token: &str) -> Option<Completion> {
        let completion = self
            .completions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token)?;
        (completion.expires > Instant::now()).then_some(completion)
    }
}

/// A `return_to` the flow may send the browser to: a path on the host it
/// started on, never another site (`//evil.example` is one).
pub fn safe_return_to(raw: Option<&str>) -> String {
    match raw.map(str::trim) {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.starts_with("/\\")
                && !path.chars().any(char::is_control) =>
        {
            path.to_owned()
        }
        _ => "/".to_owned(),
    }
}

/// `path` with `oauth_error=message` added to its query.
pub fn with_error(path: &str, message: &str) -> String {
    let (path, fragment) = match path.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (path, None),
    };
    let sep = if path.contains('?') { '&' } else { '?' };
    let mut out = format!("{path}{sep}{ERROR_PARAM}={}", encode(message));
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Form-encode one value.
fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `pairs` as a form-encoded string.
fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The PKCE challenge for `verifier` (S256).
pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// What the authorization request names: where, which client, which scopes,
/// and anything else the provider is to be told (`login_hint`, `prompt`).
struct Authorization<'a> {
    url: &'a str,
    client_id: &'a str,
    scope: &'a str,
    extra: &'a [(String, String)],
}

/// The address a provider sends the browser to sign in at.
fn authorization_address(
    request: &Authorization<'_>,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> String {
    let sep = if request.url.contains('?') { '&' } else { '?' };
    let mut pairs = vec![
        ("response_type", "code"),
        ("client_id", request.client_id),
        ("redirect_uri", redirect_uri),
        ("scope", request.scope),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ];
    pairs.extend(request.extra.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    format!("{}{sep}{}", request.url, form(&pairs))
}

/// The request's `Host`, as sent (with its port).
fn request_host(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn scheme(state: &AppState) -> &'static str {
    if state.secure_cookies {
        "https"
    } else {
        "http"
    }
}

/// The host the callback is on: the admin UI's, on the port this request
/// came in on; without a base domain, the only host there is.
fn callback_host(state: &AppState, host: &str) -> String {
    let Some(admin) = state.apps.admin_host() else {
        return host.to_owned();
    };
    match host.rsplit_once(':') {
        Some((_, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{admin}:{port}")
        }
        _ => admin,
    }
}

/// Whether `provider` is offered on a host of this kind.
fn offered(provider: &OAuthProvider, admin: bool) -> bool {
    provider.enabled
        && if admin {
            provider.admin_login
        } else {
            provider.app_login
        }
}

/// Whether the request is for the admin UI rather than an application.
fn is_admin(state: &AppState, headers: &axum::http::HeaderMap, jar: &CookieJar) -> bool {
    !matches!(resolve_app(state, headers, jar), Resolved::App(_))
}

/// `GET /_feldspar/oauth/providers`: what this host's sign-in page offers.
pub(crate) async fn providers(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    jar: CookieJar,
) -> Response {
    let admin = is_admin(&state, &headers, &jar);
    let Some(catalog) = state.apps.catalog() else {
        return Json(json!([])).into_response();
    };
    let list = match sc_config::oauth_providers(catalog).await {
        Ok(list) => list,
        Err(e) => {
            log_failure("could not read the identity providers", &e);
            Vec::new()
        }
    };
    let out: Vec<Value> = list
        .iter()
        .filter(|p| offered(p, admin))
        .map(|p| {
            json!({
                "name": p.name,
                "label": p.label,
                "url": format!("{OAUTH_PREFIX}/{}/start", p.name),
            })
        })
        .collect();
    Json(Value::Array(out)).into_response()
}

/// `GET /_feldspar/oauth/{provider}/start`: off to the provider.
pub(crate) async fn start(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    uri: Uri,
    headers: axum::http::HeaderMap,
    jar: CookieJar,
) -> Response {
    let query = parse_query(&uri);
    let return_to = safe_return_to(
        query
            .iter()
            .find_map(|(k, v)| (k == "return_to").then_some(v.as_str())),
    );
    let admin = is_admin(&state, &headers, &jar);
    let provider = match state.apps.catalog() {
        Some(catalog) => sc_config::oauth_provider(catalog, &name)
            .await
            .unwrap_or_else(|e| {
                log_failure("could not read the identity providers", &e);
                None
            }),
        None => None,
    };
    let Some(provider) = provider.filter(|p| offered(p, admin)) else {
        return Redirect::to(&with_error(&return_to, "unknown identity provider")).into_response();
    };
    let host = request_host(&headers);
    let redirect_uri = format!(
        "{}://{}{CALLBACK_ROUTE}",
        scheme(&state),
        callback_host(&state, &host)
    );
    let flow_state = crate::security::new_csrf_token();
    let verifier = crate::security::new_csrf_token();
    let binding = crate::security::new_csrf_token();
    let address = authorization_address(
        &Authorization {
            url: &provider.authorization_url,
            client_id: &provider.client_id,
            scope: &provider.scopes,
            extra: &[],
        },
        &redirect_uri,
        &flow_state,
        &pkce_challenge(&verifier),
    );
    state.apps.oauth().start(
        flow_state,
        Pending {
            purpose: Purpose::SignIn {
                provider: provider.name.clone(),
                admin,
            },
            verifier,
            host,
            return_to,
            binding: binding.clone(),
            redirect_uri,
            expires: Instant::now() + FLOW_TTL,
        },
    );
    (
        jar.add(binding_cookie(&state, binding)),
        Redirect::to(&address),
    )
        .into_response()
}

/// `GET /_feldspar/oauth/consent/{id}`: a module's consent flow, taken up by
/// the browser of the user whose call began it, and off to the provider.
pub(crate) async fn consent(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: axum::http::HeaderMap,
    jar: CookieJar,
) -> Response {
    let Some(flow) = sc_module::consent::flows().take(&id) else {
        return Redirect::to(&with_error(
            "/",
            "connecting the account took too long or was already started; please try again",
        ))
        .into_response();
    };
    let return_to = flow.return_to.clone();
    // The one who asked, and nobody else: a link to this flow sent to somebody
    // else must not connect *their* account to the asker's row.
    let user = match session_user(&state, &jar).await {
        Ok(user) => user,
        Err(response) => return *response,
    };
    if user.is_none_or(|u| u.id.to_string() != flow.user) {
        return Redirect::to(&with_error(
            &return_to,
            "sign in as the user who asked to connect this account",
        ))
        .into_response();
    }
    let host = request_host(&headers);
    let redirect_uri = format!(
        "{}://{}{CALLBACK_ROUTE}",
        scheme(&state),
        callback_host(&state, &host)
    );
    let flow_state = crate::security::new_csrf_token();
    let verifier = crate::security::new_csrf_token();
    let binding = crate::security::new_csrf_token();
    let address = authorization_address(
        &Authorization {
            url: &flow.authorization_url,
            client_id: &flow.client_id,
            scope: &flow.scope,
            extra: &flow.params,
        },
        &redirect_uri,
        &flow_state,
        &pkce_challenge(&verifier),
    );
    state.apps.oauth().start(
        flow_state,
        Pending {
            purpose: Purpose::Consent(Box::new(flow)),
            verifier,
            host,
            return_to,
            binding: binding.clone(),
            redirect_uri,
            expires: Instant::now() + FLOW_TTL,
        },
    );
    (
        jar.add(binding_cookie(&state, binding)),
        Redirect::to(&address),
    )
        .into_response()
}

/// The binding cookie: `Lax`, unlike the session's, because the request that
/// must carry it arrives by a redirect chain that began at the provider.
fn binding_cookie(state: &AppState, value: String) -> Cookie<'static> {
    Cookie::build((BINDING_COOKIE, value))
        .path(OAUTH_PREFIX)
        .same_site(SameSite::Lax)
        .http_only(true)
        .secure(state.secure_cookies)
        .max_age(time::Duration::seconds(FLOW_TTL.as_secs() as i64))
        .build()
}

fn binding_removal() -> Cookie<'static> {
    let mut cookie = Cookie::build((BINDING_COOKIE, ""))
        .path(OAUTH_PREFIX)
        .build();
    cookie.make_removal();
    cookie
}

/// `GET /_feldspar/oauth/callback`: the provider's answer.
pub(crate) async fn callback(State(state): State<AppState>, uri: Uri) -> Response {
    let query: HashMap<String, String> = parse_query(&uri).into_iter().collect();
    let Some(pending) = query
        .get("state")
        .and_then(|flow_state| state.apps.oauth().take(flow_state))
    else {
        // No flow to send the browser back to: the sign-in page of this host.
        return Redirect::to(&with_error(
            "/",
            "the sign-in took too long or was already used; please try again",
        ))
        .into_response();
    };
    let back = |message: &str| {
        Redirect::to(&format!(
            "{}://{}{}",
            scheme(&state),
            pending.host,
            with_error(&pending.return_to, message)
        ))
        .into_response()
    };
    if let Some(error) = query.get("error") {
        let message = query.get("error_description").unwrap_or(error);
        return back(&format!("the identity provider refused: {message}"));
    }
    let Some(code) = query.get("code") else {
        return back("the identity provider sent no code");
    };
    let outcome = match &pending.purpose {
        Purpose::SignIn { provider, admin } => {
            let Some(catalog) = state.apps.catalog().cloned() else {
                return back("this server has no database");
            };
            let provider = match sc_config::oauth_provider(&catalog, provider).await {
                Ok(Some(provider)) if offered(&provider, *admin) => provider,
                _ => return back("unknown identity provider"),
            };
            let email = match signed_in_email(&provider, code, &pending).await {
                Ok(email) => email,
                Err(message) => return back(&message),
            };
            match account_for(&catalog, &provider, &email, *admin).await {
                Ok(user) => Outcome::SignIn(user),
                Err(message) => return back(&message),
            }
        }
        Purpose::Consent(flow) => {
            let client = Client {
                token_url: &flow.token_url,
                client_id: &flow.client_id,
                client_secret: &flow.client_secret,
            };
            match exchange(&client, code, &pending).await {
                Ok((_, tokens)) => Outcome::Consent {
                    flow: flow.clone(),
                    tokens,
                },
                Err(message) => return back(&message),
            }
        }
    };
    let token = state.apps.oauth().complete(Completion {
        outcome,
        host: pending.host.clone(),
        return_to: pending.return_to.clone(),
        binding: pending.binding.clone(),
        expires: Instant::now() + COMPLETION_TTL,
    });
    Redirect::to(&format!(
        "{}://{}{COMPLETE_ROUTE}?token={token}",
        scheme(&state),
        pending.host
    ))
    .into_response()
}

/// The client a token request is made as.
struct Client<'a> {
    token_url: &'a str,
    client_id: &'a str,
    client_secret: &'a str,
}

/// Exchange `code` at the token endpoint: an HTTP client for further requests
/// and the token response, which has an `access_token`. `Err` is a sentence for
/// the page the browser goes back to.
async fn exchange(
    as_client: &Client<'_>,
    code: &str,
    pending: &Pending,
) -> std::result::Result<(reqwest::Client, Value), String> {
    let client = reqwest::Client::builder()
        .timeout(PROVIDER_TIMEOUT)
        .user_agent("Saltcorn Feldspar")
        .build()
        .map_err(|e| format!("could not reach the identity provider: {e}"))?;
    let mut body = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", pending.redirect_uri.as_str()),
        ("client_id", as_client.client_id),
        ("code_verifier", pending.verifier.as_str()),
    ];
    if !as_client.client_secret.is_empty() {
        body.push(("client_secret", as_client.client_secret));
    }
    let response = client
        .post(as_client.token_url)
        .header(header::ACCEPT, "application/json")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(&body))
        .send()
        .await
        .map_err(|e| format!("could not reach the identity provider: {e}"))?;
    let status = response.status();
    let token: Value = response.json().await.unwrap_or(Value::Null);
    if token.get("access_token").and_then(Value::as_str).is_none() {
        let why = token
            .get("error_description")
            .or_else(|| token.get("error"))
            .and_then(Value::as_str)
            .map_or_else(|| format!("status {status}"), str::to_owned);
        return Err(match pending.purpose {
            Purpose::SignIn { .. } => format!("the identity provider did not sign you in: {why}"),
            Purpose::Consent(_) => format!("the provider granted no access: {why}"),
        });
    }
    Ok((client, token))
}

/// Exchange `code` for an access token and read the verified email it is for.
/// `Err` is a sentence for the sign-in page.
async fn signed_in_email(
    provider: &OAuthProvider,
    code: &str,
    pending: &Pending,
) -> std::result::Result<String, String> {
    let as_client = Client {
        token_url: &provider.token_url,
        client_id: &provider.client_id,
        client_secret: &provider.client_secret,
    };
    let (client, token) = exchange(&as_client, code, pending).await?;
    let access_token = token
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let unreachable = |e: reqwest::Error| format!("could not reach the identity provider: {e}");
    let get = |url: &str| {
        client
            .get(url)
            .bearer_auth(access_token)
            .header(header::ACCEPT, "application/json")
            .send()
    };
    let info: Value = get(&provider.userinfo_url)
        .await
        .map_err(unreachable)?
        .json()
        .await
        .unwrap_or(Value::Null);
    let verified = match info.get("email_verified") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => true,
    };
    if let Some(email) = info
        .get("email")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
    {
        if !verified {
            return Err("the identity provider has not verified that email address".to_owned());
        }
        return Ok(email.trim().to_owned());
    }
    // GitHub's user shows an address only if its owner made one public; the
    // verified primary one is on its own endpoint.
    if provider.preset == sc_config::authentication::PRESET_GITHUB {
        let emails: Value = get("https://api.github.com/user/emails")
            .await
            .map_err(unreachable)?
            .json()
            .await
            .unwrap_or(Value::Null);
        if let Some(email) = emails.as_array().and_then(|list| {
            list.iter().find_map(|e| {
                let primary = e.get("primary").and_then(Value::as_bool) == Some(true);
                let verified = e.get("verified").and_then(Value::as_bool) == Some(true);
                (primary && verified)
                    .then(|| e.get("email").and_then(Value::as_str))
                    .flatten()
            })
        }) {
            return Ok(email.to_owned());
        }
    }
    Err("the identity provider did not say which email address you signed in with".to_owned())
}

/// The account `email` signs in to. An administrator, on the admin UI's host;
/// on an application's, anybody enabled — and, when the provider creates
/// accounts, somebody new.
async fn account_for(
    catalog: &sc_catalog::Catalog,
    provider: &OAuthProvider,
    email: &str,
    admin: bool,
) -> std::result::Result<User, String> {
    let failed = |e: sc_error::Error| {
        log_failure(
            "could not look up the account for an identity provider sign-in",
            &e,
        );
        "the sign-in could not be completed".to_owned()
    };
    let no_account = || format!("there is no account for {email}");
    let user = match sc_auth::load_user_by_email(catalog, email)
        .await
        .map_err(failed)?
    {
        Some(user) => user,
        None if !admin && provider.create_users => {
            let new = sc_auth::NewUser {
                email: email.to_owned(),
                // A password nobody is told: this account signs in through the
                // provider, or sets one with a forgotten-password link.
                password: String::new(),
                role: provider.new_user_role,
                ..Default::default()
            };
            sc_auth::create_user_with(catalog, new)
                .await
                .map_err(failed)?
                .user
        }
        None => return Err(no_account()),
    };
    if user.is_disabled() {
        // As a password sign-in does: a disabled account reads as no account.
        return Err(no_account());
    }
    if admin && !user.is_admin() {
        return Err(format!("{email} is not an administrator"));
    }
    Ok(user)
}

/// `GET /_feldspar/oauth/complete`: the session, on the host the flow started
/// on, in the browser that started it.
pub(crate) async fn complete(
    State(state): State<AppState>,
    uri: Uri,
    headers: axum::http::HeaderMap,
    jar: CookieJar,
) -> Response {
    let token = parse_query(&uri)
        .into_iter()
        .find_map(|(k, v)| (k == "token").then_some(v));
    let host = request_host(&headers);
    let completion = token.and_then(|token| state.apps.oauth().redeem(&token));
    let Some(completion) = completion.filter(|c| c.host == host) else {
        return Redirect::to(&with_error("/", "the sign-in could not be completed"))
            .into_response();
    };
    let bound = jar
        .get(BINDING_COOKIE)
        .is_some_and(|c| !c.value().is_empty() && c.value() == completion.binding);
    if !bound {
        return Redirect::to(&with_error(
            &completion.return_to,
            match completion.outcome {
                Outcome::SignIn(_) => "the sign-in was started in another browser",
                Outcome::Consent { .. } => "connecting the account was started in another browser",
            },
        ))
        .into_response();
    }
    let user = match completion.outcome {
        Outcome::SignIn(user) => user,
        Outcome::Consent { flow, tokens } => {
            let jar = jar.remove(binding_removal());
            let target = match state.apps.catalog() {
                Some(catalog) => match store_tokens(catalog, &flow, &tokens).await {
                    Ok(()) => completion.return_to,
                    Err(message) => with_error(&completion.return_to, &message),
                },
                None => with_error(&completion.return_to, "this server has no database"),
            };
            return (jar, leave_for(&target)).into_response();
        }
    };
    let session_token = jar
        .get(crate::security::SESSION_COOKIE)
        .map(|c| c.value().to_owned());
    // On an application's host, the sign-in must pass the application's access
    // settings (TODO F9): refused here with the way back and a reason, rather
    // than with `apply_session`'s bare 403, which it would otherwise meet.
    let client = match resolve_app(&state, &headers, &jar) {
        Resolved::App(app) => {
            if refuse_sign_in(&state, &app.app, &user).await.is_some() {
                return Redirect::to(&with_error(
                    &completion.return_to,
                    "your account may not use this application",
                ))
                .into_response();
            }
            SessionClient::of(&headers).for_app(&app)
        }
        _ => SessionClient::of(&headers),
    };
    let jar = jar.remove(binding_removal());
    let cookies = match apply_session(
        &state,
        jar,
        session_token,
        client,
        SessionAction::Start(user),
    )
    .await
    {
        Ok(cookies) => cookies,
        Err(rejection) => return *rejection,
    };
    (cookies.jar, cookies.stale, leave_for(&completion.return_to)).into_response()
}

/// A page that goes on to `target`, rather than a redirect: the session cookie
/// is `SameSite=Strict`, and a browser does not send it on a redirect at the
/// end of a chain that began at the provider. Leaving from a page on this host
/// is same-site.
fn leave_for(target: &str) -> Response {
    let target = html_escape(target);
    let mut out = (
        StatusCode::OK,
        Html(format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">\
             <meta http-equiv=\"refresh\" content=\"0;url={target}\">\
             <title>Continuing…</title></head>\
             <body><p><a href=\"{target}\">Continue</a></p></body></html>"
        )),
    )
        .into_response();
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}

/// Write a consent flow's tokens into the row it names, through the row
/// layer, so the Secret fields are sealed and the module's service hears of
/// the change. `Err` is a sentence for the application.
async fn store_tokens(
    catalog: &sc_catalog::Catalog,
    flow: &ConsentFlow,
    tokens: &Value,
) -> std::result::Result<(), String> {
    let store = &flow.store;
    let mut values = store.set.clone();
    values.insert(store.access_token.clone(), tokens["access_token"].clone());
    if let Some(field) = &store.refresh_token
        && let Some(refresh) = tokens.get("refresh_token").filter(|t| t.is_string())
    {
        values.insert(field.clone(), refresh.clone());
    }
    if let Some(field) = &store.expires_at {
        let seconds = match &tokens["expires_in"] {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        };
        let at = seconds.map(|s| (chrono::Utc::now() + chrono::Duration::seconds(s)).to_rfc3339());
        values.insert(field.clone(), at.map_or(Value::Null, Value::String));
    }
    let failed = |e: sc_error::Error| {
        log_failure("could not store the tokens of a consent flow", &e);
        "the account could not be connected: its row could not be written".to_owned()
    };
    let table = catalog.require(&store.table).map_err(failed)?;
    sc_api::rows::update_row(catalog, &table, &store.id, &Value::Object(values))
        .await
        .map_err(failed)?;
    Ok(())
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn only_a_path_on_this_host_is_returned_to() {
        assert_eq!(safe_return_to(Some("/orders?x=1")), "/orders?x=1");
        for bad in [
            "//evil.example",
            "/\\evil.example",
            "https://evil.example",
            "",
            "x",
        ] {
            assert_eq!(safe_return_to(Some(bad)), "/", "{bad}");
        }
        assert_eq!(safe_return_to(None), "/");
    }

    #[test]
    fn an_error_is_added_to_the_query_before_the_fragment() {
        assert_eq!(with_error("/", "no"), "/?oauth_error=no");
        assert_eq!(
            with_error("/a?b=1#/login", "a b&c"),
            "/a?b=1&oauth_error=a%20b%26c#/login"
        );
    }

    #[test]
    fn the_consent_route_is_where_a_module_is_told_to_send_the_browser() {
        assert_eq!(
            CONSENT_ROUTE,
            format!("{}/{{id}}", sc_module::consent::CONSENT_PREFIX)
        );
    }

    /// RFC 7636's own example.
    #[test]
    fn the_pkce_challenge_is_s256() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_flow_and_a_completion_are_spent_once() {
        let flows = OAuthFlows::default();
        let pending = || Pending {
            purpose: Purpose::SignIn {
                provider: "p".into(),
                admin: false,
            },
            verifier: "v".into(),
            host: "h".into(),
            return_to: "/".into(),
            binding: "b".into(),
            redirect_uri: "r".into(),
            expires: Instant::now() + FLOW_TTL,
        };
        flows.start("s".into(), pending());
        assert!(flows.take("s").is_some());
        assert!(flows.take("s").is_none());
        let token = flows.complete(Completion {
            outcome: Outcome::SignIn(User::new(uuid::Uuid::new_v4(), 1).unwrap()),
            host: "h".into(),
            return_to: "/".into(),
            binding: "b".into(),
            expires: Instant::now() + COMPLETION_TTL,
        });
        assert!(flows.redeem(&token).is_some());
        assert!(flows.redeem(&token).is_none());
    }
}

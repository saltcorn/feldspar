//! An application's users minting **API tokens** for themselves (TODO AT1): the
//! routes under [`ROUTE`], and the page a browser does it on ([`PAGE_ROUTE`]).
//!
//! A token runs as its user on the application it was minted on
//! ([`sc_auth::authenticate_app_token`], read by
//! [`app_caller`](crate::router::app_caller)). These routes are how the user gets
//! one, sees what they have handed out and takes one back:
//!
//! - `GET` [`ROUTE`]: `{enabled, may_create, tokens}` — the caller's own tokens on
//!   this application, newest first, never the secret.
//! - `POST` [`ROUTE`] `{label, expires_in_days?}`: `{token, secret}`, the one
//!   time the secret is shown.
//! - `POST` [`REVOKE_ROUTE`]: `{revoked}`.
//! - `GET` [`PAGE_ROUTE`]: the same three as a page, its forms posting to the
//!   routes above.
//!
//! # Who may
//!
//! **A session this application issued** ([`Session<AppScope>`]) and nothing
//! else. Not a session a shared sign-in cookie carries here from the admin host
//! or another application — the application's script, which that account's
//! owner did not necessarily write, must not mint a credential for whoever
//! visits it, the second-factor routes' reason. And not a request carrying a
//! bearer token: a token cannot mint tokens, and a bearer request skips the
//! CSRF check, so one is refused here before its cookie is read.
//!
//! Creating one also needs the application to take tokens (`api_tokens`), to
//! admit the caller, and their role to meet `api_token_min_role`. Listing and
//! revoking do not: somebody whose application stopped taking tokens can
//! still see and take back what they handed out.

// As in `second_factor`: every `Err` here is the response to send.
#![allow(clippy::result_large_err)]

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use chrono::Utc;
use sc_auth::{ApiToken, MintedApiToken, NewAppApiToken};
use sc_catalog::Catalog;
use sc_expr::html::SafeHtml;
use sc_expr::safe_html;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use uuid::Uuid;

use crate::apps::MountedApp;
use crate::router::{AppState, Resolved, json_error, log_failure, parse_form, resolve_app};
use crate::security::CsrfToken;
use crate::session::{AppScope, Session};

/// `GET` the caller's tokens, `POST` a new one.
pub const ROUTE: &str = "/_feldspar/api-tokens";
/// `POST`: revoke one of the caller's tokens.
pub const REVOKE_ROUTE: &str = "/_feldspar/api-tokens/{id}/revoke";
/// `GET`: the page a browser manages its tokens on.
pub const PAGE_ROUTE: &str = "/_feldspar/api-tokens/page";

/// The longest a token may be minted for: ten years, the guest session's
/// bound, so a number typed with a slip is not a forever by accident.
const MAX_DAYS: i64 = 3650;

/// Who is asking, on which application, or the response that says they may not.
struct Caller {
    app: Arc<MountedApp>,
    session: Session<AppScope>,
}

fn caller(
    state: &AppState,
    headers: &HeaderMap,
    jar: &CookieJar,
    session: Option<Session<AppScope>>,
) -> Result<Caller, Response> {
    // Before the cookie is looked at (module docs).
    if headers.contains_key(header::AUTHORIZATION) {
        return Err(json_error(
            StatusCode::FORBIDDEN,
            "an API token cannot manage API tokens; sign in to the application instead",
        ));
    }
    let Some(session) = session else {
        return Err(json_error(
            StatusCode::UNAUTHORIZED,
            "sign in to this application first",
        ));
    };
    match resolve_app(state, headers, jar) {
        Resolved::App(app) if app.app.id.0 == session.scope().app() => Ok(Caller { app, session }),
        _ => Err(json_error(StatusCode::NOT_FOUND, "not found")),
    }
}

fn catalog_of(state: &AppState) -> Result<&Arc<Catalog>, Response> {
    state.apps.catalog().ok_or_else(|| {
        json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "this server has no database",
        )
    })
}

fn failed(context: &str, e: &sc_error::Error) -> Response {
    match e.repr() {
        sc_error::Repr::Invalid(message) => json_error(StatusCode::BAD_REQUEST, message.clone()),
        _ => {
            log_failure(context, e);
            json_error(StatusCode::INTERNAL_SERVER_ERROR, context.to_owned())
        }
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Why the caller may not create a token here, or `None` when they may.
async fn may_not_create(state: &AppState, caller: &Caller) -> Option<&'static str> {
    let app = &caller.app.app;
    if !app.api_tokens_enabled() {
        return Some("this application does not let its users have API tokens");
    }
    let user = caller.session.user().clone().into_user();
    let (admitted, _) = crate::router::admit_caller(state, app, Some(user)).await;
    let Some(admitted) = admitted else {
        return Some("your account may not use this application");
    };
    if let Some(floor) = app.api_token_min_role()
        && !admitted.role().meets(floor)
    {
        return Some("your role may not create API tokens on this application");
    }
    None
}

/// One token as these routes answer it: never the secret, never the hash.
pub(crate) fn token_json(token: &ApiToken) -> Value {
    json!({
        "id": token.id,
        "label": token.label,
        "state": state_of(token),
        "created_at": token.created_at,
        "expires_at": token.expires_at,
        "last_used_at": token.last_used_at,
        "revoked_at": token.revoked_at,
    })
}

/// `live`, `expired` or `revoked`.
fn state_of(token: &ApiToken) -> &'static str {
    if token.revoked_at.is_some() {
        "revoked"
    } else if token.is_live(Utc::now()) {
        "live"
    } else {
        "expired"
    }
}

async fn own_tokens(catalog: &Catalog, caller: &Caller) -> sc_error::Result<Vec<ApiToken>> {
    sc_auth::list_app_api_tokens(
        catalog,
        caller.app.app.id.0,
        Some(caller.session.user().id()),
    )
    .await
}

/// `GET` [`ROUTE`].
pub(crate) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    session: Option<Session<AppScope>>,
) -> Response {
    let caller = match caller(&state, &headers, &jar, session) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let tokens = match own_tokens(catalog, &caller).await {
        Ok(t) => t,
        Err(e) => return failed("could not list API tokens", &e),
    };
    let refusal = may_not_create(&state, &caller).await;
    no_store(
        Json(json!({
            "enabled": caller.app.app.api_tokens_enabled(),
            "may_create": refusal.is_none(),
            "tokens": tokens.iter().map(token_json).collect::<Vec<_>>(),
        }))
        .into_response(),
    )
}

/// A request's fields: a JSON body's, or the page's form.
fn fields(headers: &HeaderMap, body: &Bytes) -> (Map<String, Value>, bool) {
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
    (values, form)
}

/// The expiry a create asked for: `expires_in_days`, a number or (from the
/// form) a string, absent or empty for none.
fn expiry(values: &Map<String, Value>) -> Result<Option<chrono::DateTime<Utc>>, Response> {
    let days = match values.get("expires_in_days") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(Value::Number(n)) => n.as_i64(),
        Some(_) => None,
    };
    match days {
        Some(days) if (1..=MAX_DAYS).contains(&days) => {
            Ok(Some(Utc::now() + chrono::Duration::days(days)))
        }
        _ => Err(json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "`expires_in_days` must be a whole number of days from 1 to {MAX_DAYS}; \
                 leave it out for a token that does not expire"
            ),
        )),
    }
}

/// `POST` [`ROUTE`].
pub(crate) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    session: Option<Session<AppScope>>,
    csrf: Option<Extension<CsrfToken>>,
    body: Bytes,
) -> Response {
    let caller = match caller(&state, &headers, &jar, session) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Some(why) = may_not_create(&state, &caller).await {
        return json_error(StatusCode::FORBIDDEN, why);
    }
    let (values, form) = fields(&headers, &body);
    let label = values
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let expires_at = match expiry(&values) {
        Ok(at) => at,
        Err(r) => return r,
    };
    let minted = match sc_auth::mint_app_api_token(
        catalog,
        NewAppApiToken {
            application_id: caller.app.app.id.0,
            user_id: caller.session.user().id(),
            label,
            expires_at,
        },
    )
    .await
    {
        Ok(m) => m,
        Err(e) => return failed("could not create the API token", &e),
    };
    sc_log::log_info!(
        "API token [{}] ({}) created by user {} on {}",
        minted.token.label,
        minted.token.id,
        minted.token.user_id,
        caller.app.app.subdomain
    );
    if form {
        let csrf = csrf
            .map(|Extension(CsrfToken(token))| token)
            .unwrap_or_default();
        return match page(&state, catalog, &caller, &csrf, Some(&minted)).await {
            Ok(r) => r,
            Err(r) => r,
        };
    }
    no_store(
        (
            StatusCode::CREATED,
            Json(json!({
                "token": token_json(&minted.token),
                "secret": minted.secret,
            })),
        )
            .into_response(),
    )
}

/// `POST` [`REVOKE_ROUTE`].
pub(crate) async fn revoke(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    jar: CookieJar,
    session: Option<Session<AppScope>>,
) -> Response {
    let caller = match caller(&state, &headers, &jar, session) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Ok(id) = Uuid::parse_str(&id) else {
        return json_error(StatusCode::BAD_REQUEST, "not a token id");
    };
    let revoked = match sc_auth::revoke_app_api_token(
        catalog,
        caller.app.app.id.0,
        id,
        Some(caller.session.user().id()),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return failed("could not revoke the API token", &e),
    };
    if revoked {
        sc_log::log_info!(
            "API token ({id}) revoked by user {} on {}",
            caller.session.user().id(),
            caller.app.app.subdomain
        );
    }
    let form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
    if form {
        return Redirect::to(PAGE_ROUTE).into_response();
    }
    Json(json!({ "revoked": revoked })).into_response()
}

/// `GET` [`PAGE_ROUTE`].
pub(crate) async fn page_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    session: Option<Session<AppScope>>,
    csrf: Option<Extension<CsrfToken>>,
) -> Response {
    let caller = match caller(&state, &headers, &jar, session) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let catalog = match catalog_of(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let csrf = csrf
        .map(|Extension(CsrfToken(token))| token)
        .unwrap_or_default();
    match page(&state, catalog, &caller, &csrf, None).await {
        Ok(r) => r,
        Err(r) => r,
    }
}

/// The page: the token just minted (shown once), the caller's tokens with a
/// revoke button each, and the form for a new one.
async fn page(
    state: &AppState,
    catalog: &Catalog,
    caller: &Caller,
    csrf: &str,
    minted: Option<&MintedApiToken>,
) -> Result<Response, Response> {
    let tokens = own_tokens(catalog, caller)
        .await
        .map_err(|e| failed("could not list API tokens", &e))?;
    let refusal = may_not_create(state, caller).await;
    let csrf_field = safe_html!(
        "<input type=\"hidden\" name=\"_csrf\" value=\"{}\">",
        SafeHtml::escape(csrf)
    );
    let fresh = match minted {
        Some(minted) => safe_html!(
            "<div class=\"fresh\"><p><strong>{label}</strong> is ready. Copy it now: it is \
             not shown again.</p><p><code>{secret}</code></p>\
             <p>Send it as <code>Authorization: Bearer …</code>.</p></div>",
            label = SafeHtml::escape(&minted.token.label),
            secret = SafeHtml::escape(&minted.secret),
        ),
        None => SafeHtml::trusted(""),
    };
    let rows: SafeHtml = tokens
        .iter()
        .map(|t| {
            let action = REVOKE_ROUTE.replace("{id}", &t.id.to_string());
            let button = if state_of(t) == "live" {
                safe_html!(
                    "<form method=\"post\" action=\"{action}\">{csrf}\
                     <button type=\"submit\" class=\"small\">Revoke</button></form>",
                    action = SafeHtml::escape(&action),
                    csrf = csrf_field.clone(),
                )
            } else {
                SafeHtml::trusted("")
            };
            let last_used = t
                .last_used_at
                .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "never".to_owned());
            safe_html!(
                "<li><strong>{label}</strong> <span class=\"state\">{state}</span><br>\
                 <small>created {created}, last used {last_used}</small>{button}</li>",
                label = SafeHtml::escape(&t.label),
                state = SafeHtml::escape(state_of(t)),
                created = SafeHtml::escape(&t.created_at.format("%Y-%m-%d").to_string()),
                last_used = SafeHtml::escape(&last_used),
                button = button,
            )
        })
        .collect();
    let list = if tokens.is_empty() {
        SafeHtml::trusted("<p>You have no API tokens on this application.</p>")
    } else {
        safe_html!("<ul class=\"tokens\">{}</ul>", rows)
    };
    let new_form = match refusal {
        Some(why) => safe_html!("<p>{}</p>", SafeHtml::escape(why)),
        None => safe_html!(
            "<h2>New token</h2><form method=\"post\" action=\"{action}\">{csrf}\
             <label for=\"label\">Name</label>\
             <input id=\"label\" name=\"label\" required placeholder=\"nightly export\">\
             <label for=\"days\">Expires after (days, empty for never)</label>\
             <input id=\"days\" name=\"expires_in_days\" inputmode=\"numeric\">\
             <button type=\"submit\">Create token</button></form>",
            action = SafeHtml::trusted(ROUTE),
            csrf = csrf_field.clone(),
        ),
    };
    let body = safe_html!(
        "<h1>API tokens</h1>\
         <p>A token lets a script call this application's API as you. Anyone who has it \
         can do what you can here, so keep it secret and revoke it when it is no longer \
         needed.</p>{fresh}{list}{new_form}",
        fresh = fresh,
        list = list,
        new_form = new_form,
    );
    Ok(no_store(
        (
            StatusCode::OK,
            crate::html_page::html(page_html("API tokens", body)),
        )
            .into_response(),
    ))
}

fn page_html(title: &str, body: SafeHtml) -> SafeHtml {
    safe_html!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title><style>{style}{extra}</style></head>\
         <body><main>{body}</main></body></html>",
        title = SafeHtml::escape(title),
        style = SafeHtml::trusted(crate::second_factor::PAGE_STYLE),
        extra = SafeHtml::trusted(PAGE_EXTRA_STYLE),
        body = body,
    )
}

/// What this page adds to the second-factor page's style: a list, and a
/// label field that is not a code.
const PAGE_EXTRA_STYLE: &str = "\
main{max-width:34rem}h2{font-size:1.1rem;margin:1.5rem 0 0}\
input{letter-spacing:normal;font-size:1rem}\
.tokens{list-style:none;padding:0}.tokens li{padding:.75rem 0;border-bottom:1px solid var(--border)}\
.tokens form{display:inline}.small{margin:.25rem 0 0;padding:.3rem .8rem;font-size:.9rem}\
.state{color:var(--muted);font-size:.9rem}\
.fresh{border:1px solid var(--accent);border-radius:6px;padding:0 1rem;margin:1rem 0}";

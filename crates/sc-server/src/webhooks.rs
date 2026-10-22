//! **Module webhooks** over HTTP (TODO-payments P1.1, design C1):
//! `POST /_feldspar/hooks/<module>/<name>[/<rest>]`.
//!
//! What a provider is told to call, so it lives where the OAuth callback does —
//! on the **admin UI's host**, one stable URL however many applications there
//! are — and answers there only: an application's host is the application's
//! own, and a request for a webhook on it is a `404`.
//!
//! The request carries no session and is checked against no CSRF token: it
//! comes from a provider's server, not a browser, and a webhook authenticates by
//! verifying the provider's signature over the raw body itself. What the server
//! does for it is bound what an unauthenticated caller can cost:
//!
//! - **a body cap** of [`WEBHOOK_MAX_BODY`] (1 MiB) — a `413` past it;
//! - **a per-IP rate limit** ([`WEBHOOK_RATE`]) — a `429` past it.
//!
//! The module runs it ([`sc_module::run_webhook`]): as the module, over its own
//! tables. Whatever status it answers is what the caller gets. A handler that
//! throws is a `500` — so the provider retries — and is reported as the `error`
//! event every other failure that reaches a client is.

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::CookieJar;
use sc_module::{WEBHOOK_MAX_BODY, WebhookRequest};
use sc_ratelimit::Rate;

use crate::router::{AppState, Resolved, json_error, parse_query, request_client_ip, resolve_app};

/// The route with no `<rest>`.
pub const HOOK_ROUTE: &str = "/_feldspar/hooks/{module}/{name}";
/// The route with a `<rest>`: `/_feldspar/hooks/payments/events/acct_1`.
pub const HOOK_REST_ROUTE: &str = "/_feldspar/hooks/{module}/{name}/{*rest}";

/// How often one address may call webhooks: generous, because a provider
/// delivers in bursts (a backlog after an outage), and a limit, because the
/// route answers anybody.
pub const WEBHOOK_RATE: Rate = Rate {
    max: 600,
    window: std::time::Duration::from_secs(60),
};

/// The headers a module is not handed: a webhook has no session, and a module
/// has no business reading a browser's credentials.
const WITHHELD: &[&str] = &["cookie", "authorization", "x-csrf-token", "csrf-token"];

/// `POST /_feldspar/hooks/{module}/{name}`.
pub(crate) async fn hook(
    State(state): State<AppState>,
    AxumPath((module, name)): AxumPath<(String, String)>,
    peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(
        &state,
        &module,
        &name,
        String::new(),
        peer,
        method,
        &uri,
        headers,
        body,
    )
    .await
}

/// `POST /_feldspar/hooks/{module}/{name}/{*rest}`.
pub(crate) async fn hook_rest(
    State(state): State<AppState>,
    AxumPath((module, name, rest)): AxumPath<(String, String, String)>,
    peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(
        &state, &module, &name, rest, peer, method, &uri, headers, body,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    state: &AppState,
    module: &str,
    name: &str,
    rest: String,
    peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
    method: Method,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // The admin host only: an application's host answers for the application.
    let jar = CookieJar::from_headers(&headers);
    if !matches!(resolve_app(state, &headers, &jar), Resolved::Admin) {
        return json_error(StatusCode::NOT_FOUND, "no such route on this host");
    }
    if let Some(ip) = request_client_ip(state, peer.map(|p| p.0.0), &headers)
        && let Err(limited) = sc_ratelimit::global().hit(&format!("webhook:{ip}"), WEBHOOK_RATE)
    {
        let secs = limited.retry_after_secs();
        let mut response = json_error(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many webhook requests from this address; retry in {secs} s"),
        );
        if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    // The route's own body limit has already refused a larger body with a
    // `413`; this is the belt for a transport that streamed past it.
    if body.len() > WEBHOOK_MAX_BODY {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("a webhook's body is at most {WEBHOOK_MAX_BODY} bytes"),
        );
    }
    let Some(modules) = state.apps.modules() else {
        return json_error(StatusCode::NOT_FOUND, "this server runs no modules");
    };
    let request = WebhookRequest {
        method: method.as_str().to_owned(),
        headers: headers
            .iter()
            .filter(|(k, _)| !WITHHELD.contains(&k.as_str()))
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect(),
        query: parse_query(uri),
        rest,
        body: body.to_vec(),
    };
    match modules.webhook(module, name, request).await {
        Ok(Some(answer)) => {
            let mut response = Response::new(Body::from(answer.body));
            *response.status_mut() =
                StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            for (k, v) in answer.headers {
                if let (Ok(k), Ok(v)) = (HeaderName::try_from(k), HeaderValue::from_str(&v)) {
                    response.headers_mut().insert(k, v);
                }
            }
            response
        }
        Ok(None) => json_error(
            StatusCode::NOT_FOUND,
            format!("no module here receives webhooks at `{module}/{name}`"),
        ),
        Err(err) => {
            // A failure, not an answer: logged, raised as the `error` event,
            // and a `500` so the provider tries again later.
            sc_log::log_error!(
                "feldspar: the webhook `{module}/{name}` failed: {}",
                err.chain()
            );
            if let (Some(triggers), Some(catalog)) = (state.apps.triggers(), state.apps.catalog()) {
                let event =
                    sc_action::Event::error(err.kind(), err.to_string(), "POST", uri.path());
                triggers.fire(catalog, &event).await;
            }
            json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        }
    }
    .into_response()
}

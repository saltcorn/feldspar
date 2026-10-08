//! Security posture for the SPA: strict CSP, CSRF, and cookie conventions
//! (technical design §16 "Security posture").
//!
//! - **CSP.** A strict Content-Security-Policy with **no `unsafe-inline` script**
//!   — the React bundle carries no inline scripts or handlers, so `'self'` is
//!   enough for the directive that matters. Inline *styles* are allowed, for the
//!   one thing that needs them (the embedded Monaco editor's theme); see
//!   [`CONTENT_SECURITY_POLICY`]. It is applied as a response header via
//!   `tower-http`'s `set-header` layer (see [`crate::router`]).
//! - **CSRF.** The session cookie authenticates the SPA, so state-changing
//!   requests are protected with the **double-submit-cookie** pattern: the server
//!   hands the SPA a non-`HttpOnly` `sc_csrf` cookie, and every mutating request
//!   must echo it in the `x-csrf-token` header. A cross-site page can send the
//!   cookie but cannot read it to set the header, so the forgery fails.
//!
//!   The token is also sent as an `x-csrf-token` **response header** on API
//!   answers (JSON) and on the refusal. A browser page reads it from the cookie;
//!   a native app — React Native on a phone — has no `document.cookie` to read,
//!   and its cookie store is not visible to its JavaScript, so the header is how
//!   it learns the value to echo. A page on another origin cannot read this
//!   origin's response headers any more than its cookies; what could leak the
//!   token is a **shared cache**, so the header never goes on a response one may
//!   store (see `expose_csrf_token`).
//! - **Cookies.** `SameSite=Strict` on both cookies; the session cookie is
//!   `HttpOnly`; `Secure` is set behind TLS (see `ServerConfig::secure_cookies`).

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use std::sync::Arc;
use uuid::Uuid;

/// Name of the session cookie (opaque token → [`SessionStore`](sc_auth::SessionStore)).
pub const SESSION_COOKIE: &str = "sc_session";
/// Name of the cookie that lets a `view_app` browser context reach its run's
/// previews whether or not it is signed in (see
/// [`AppMounts::allow_preview_token`](crate::AppMounts::allow_preview_token)).
pub const PREVIEW_COOKIE: &str = "sc_preview";
/// Name of the CSRF double-submit cookie (readable by the SPA), and the header a
/// mutating request must echo it in.
///
/// Both come from `sc-api`, which is where the wire contract is stated: the
/// server enforces the check here and the generated TypeScript client satisfies
/// it, and a second spelling of either name is exactly how those two stop
/// agreeing.
pub use sc_api::auth::{CLIENT_KIND_HEADER, CSRF_COOKIE, CSRF_HEADER, NATIVE_CLIENT};

/// The strict Content-Security-Policy served with every response. No
/// `unsafe-inline` **script**: executable code loads only from the app's own
/// origin, so the React bundle is the sole executable source.
///
/// One relaxation, and it is about styles only: **`style-src 'unsafe-inline'`**,
/// because the admin UI embeds the Monaco editor for code settings (a
/// `run_js_code` body) and Monaco writes its theme — the token colours that *are*
/// the syntax highlighting — into a `<style>` element it creates at runtime. It
/// offers no nonce hook to sign that with, so under `style-src 'self'` the
/// element is blocked and the editor renders in one colour. The IDE's policy
/// below already makes the same allowance for the same reason.
///
/// What it costs is bounded by the directives that did not move: script sources
/// are still `'self'` with no `eval` and no `blob:`, and `default-src 'self'`
/// with `img-src 'self' data:` leaves injected CSS nowhere to send anything —
/// the classic CSS exfiltration channel is a remote `url()`, which is still
/// refused. It buys back an editor that highlights, completes and type-checks
/// what an admin is writing.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
script-src 'self'; \
style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; \
font-src 'self'; \
connect-src 'self'; \
base-uri 'none'; \
form-action 'self'; \
frame-ancestors 'none'; \
object-src 'none'";

/// The Content-Security-Policy served with the **Analytics UI** under
/// `/analytics/` (analytics TODO A1.14), before the map hosts are added.
///
/// The admin SPA's strict policy, with two words more, both for MapLibre
/// (analytics TODO A5.6): `blob:` in `img-src`, because MapLibre decodes a
/// tile's or a sprite's image through an object URL where a browser cannot
/// make an `ImageBitmap`, and an explicit `worker-src 'self'`, because its
/// worker is a same-origin module (the bundle sets its URL) and never a
/// `blob:` — which is the relaxation MapLibre's documentation otherwise asks
/// for, and this policy does not make. It is a constant of its own, served per
/// response on its own route like the IDE's, so that what the renderers need
/// widens this policy and never the admin UI's.
///
/// What is served is [`analytics_content_security_policy`]: this, with the
/// hosts the base maps load from.
pub const ANALYTICS_CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
script-src 'self'; \
style-src 'self' 'unsafe-inline'; \
img-src 'self' data: blob:; \
font-src 'self'; \
connect-src 'self'; \
worker-src 'self'; \
base-uri 'none'; \
form-action 'self'; \
frame-ancestors 'none'; \
object-src 'none'";

/// [`ANALYTICS_CONTENT_SECURITY_POLICY`], with the origins a map may load its
/// base map from (Settings → Maps, `sc_config::MapSettings::hosts`) in
/// `connect-src` — MapLibre fetches a style, its tiles, glyphs and sprites —
/// and in `img-src`.
///
/// Each host is an origin `sc_config::origin_of` parsed — a scheme, a host and
/// a port — so nothing in it can end a source list; one that is not is left
/// out rather than trusted.
pub fn analytics_content_security_policy(map_hosts: &[String]) -> String {
    let hosts: Vec<&str> = map_hosts
        .iter()
        .map(String::as_str)
        .filter(|h| sc_config::origin_of(h).is_ok_and(|origin| origin == *h))
        .collect();
    if hosts.is_empty() {
        return ANALYTICS_CONTENT_SECURITY_POLICY.to_owned();
    }
    let list = hosts.join(" ");
    ANALYTICS_CONTENT_SECURITY_POLICY
        .replacen(
            "img-src 'self' data: blob:; ",
            &format!("img-src 'self' data: blob: {list}; "),
            1,
        )
        .replacen(
            "connect-src 'self'; ",
            &format!("connect-src 'self' {list}; "),
            1,
        )
}

/// [`CONTENT_SECURITY_POLICY`], with the **applications** the admin may frame.
///
/// The admin frames an application in one place: the preview pane beside a
/// builder agent's chat (TODO "The preview pane"), where the person watches the
/// application the agent is changing. An application is served on a subdomain of
/// the base domain, so it is a cross-origin child, and the strict policy above
/// has no `frame-src` — under `default-src 'self'` that means the pane is a
/// blocked frame and a console message the admin never sees.
///
/// So one directive is added, naming the origins an application can be served
/// on and nothing else: `*.{base}`, at the default port and at any port, which
/// covers both `todo.example.com` and a development `todo.localhost:3000` (and
/// with it a preview mount, `feature--todo.example.com`, which is a subdomain
/// like any other). `'self'` is there so the admin may frame its own pages —
/// the IDE already does.
///
/// A deployment with no base domain serves no applications at all
/// ([`build_router_with_apps`](crate::build_router_with_apps) refuses to start
/// with mounts and no base domain), so there is nothing to allow and the strict
/// policy is returned unchanged.
pub fn admin_content_security_policy(base_domain: Option<&str>) -> String {
    let Some(base) = base_domain.map(str::trim).filter(|b| !b.is_empty()) else {
        return CONTENT_SECURITY_POLICY.to_owned();
    };
    let base = base.trim_start_matches('.');
    CONTENT_SECURITY_POLICY.replacen(
        "connect-src 'self'; ",
        &format!("connect-src 'self'; frame-src 'self' *.{base} *.{base}:*; "),
        1,
    )
}

/// The Content-Security-Policy served with the **file-store IDE** under `/ide/`
/// (design §12.1), and with nothing else.
///
/// The admin SPA satisfies the strict policy above structurally, because React
/// escapes values and the bundle carries no inline anything. The IDE cannot: it is
/// VS Code, which computes styles at runtime and injects them, runs its editor,
/// textmate, search and extension-host code as workers created from blobs, and
/// hosts the worker extension host in a sandboxed iframe. Each relaxation below is
/// one of those facts, and no more than that:
///
/// - `style-src 'unsafe-inline'` — the workbench's own injected styles.
/// - `script-src 'unsafe-eval' blob:` — worker bootstrap code, and the
///   `WebAssembly` compilation textmate's oniguruma engine needs.
/// - `worker-src blob:` and `frame-src blob:` — the workers and the extension
///   host's iframe.
///
/// What it does **not** relax is where code may come from: `default-src 'self'`
/// stands, there is no `https:` or wildcard source, and `connect-src 'self'` keeps
/// the IDE talking to this server only (which is also what admits the same-origin
/// WebSocket a language server will need). An extension marketplace would need a
/// remote origin here, which is one more reason installing extensions is out of
/// scope.
///
/// It is served **per response** on the IDE's own route rather than as a layer, so
/// the strict policy remains the default for everything else: relaxing CSP for a
/// route must not be a way of relaxing it for the admin UI.
pub const IDE_CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
script-src 'self' 'unsafe-eval' blob:; \
style-src 'self' 'unsafe-inline'; \
img-src 'self' data: blob:; \
font-src 'self' data:; \
connect-src 'self' data: blob:; \
worker-src 'self' blob:; \
child-src 'self' blob:; \
frame-src 'self' blob:; \
base-uri 'none'; \
form-action 'self'; \
frame-ancestors 'none'; \
object-src 'none'";

/// The Content-Security-Policy served with **the builder** under `/builder/`
/// (TODO "The builder" §2, 8.3): its documents, its assets, and its refusals.
///
/// It is the admin UI's policy with **one** relaxation, and each of the ones
/// §2 expected was checked against the builder as vendored and left out. The
/// facts that make each one unnecessary are asserted by
/// `tests/builder_route.rs`'s `the_builder_policy_is_what_the_bundle_needs`, so a
/// refresh of the vendored builder that changes one fails there, naming the
/// directive.
///
/// - **`img-src` + the application's origin**, added per document by
///   [`builder_content_security_policy`]. The canvas renders an application's
///   images as `/files/serve/…`, which the admin server redirects to the
///   application (8.5). An image follows a redirect under the policy of the
///   document that asked for it, so the application's origin must be in it.
///   Only that application's origin, and only for images.
/// - `style-src 'unsafe-inline'` is **not new**: the admin policy has it.
///   Craft's inline `style` props and CKEditor's and Monaco's injected `<style>`
///   elements need it.
/// - `worker-src 'self'` is **not a relaxation**: it is what `script-src 'self'`
///   already allows, written down. Monaco's workers are files beside the bundle,
///   created from `new URL(…, import.meta.url)` (`src/shims/monaco.ts`), so there
///   is no `blob:`.
/// - **No `frame-src`, and no inline script, for CKEditor.** CKEditor 4's
///   iframe editor writes an inline `<script id="cke_actscrpt">` into its frame,
///   which this policy would block. Every editor the builder mounts is
///   `type="inline"` (`elements/Text.js`), a `contenteditable` element with no
///   frame.
/// - **No `font-src data:`.** Every font in `builder.css` is a file in the
///   bundle; its `data:` URLs are images, which `img-src data:` already allows.
/// - **No `'unsafe-eval'`** and no third-party origin. The boot data is a JSON
///   `<script type="application/json">`, never executed.
///
/// What it cannot be sure of from here is what a browser reports at run time.
/// The milestone's definition of done is run by hand with the console open, and
/// a violation report there counts as a failure (TODO "The builder" §13).
pub const BUILDER_CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
script-src 'self'; \
style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; \
font-src 'self'; \
connect-src 'self'; \
worker-src 'self'; \
base-uri 'none'; \
form-action 'self'; \
frame-ancestors 'none'; \
object-src 'none'";

/// [`BUILDER_CONTENT_SECURITY_POLICY`] for a document building a layout of the
/// application served at `application_origin`: its images may come from there.
///
/// An origin with anything in it but a scheme, a host and a port is left out
/// rather than written into a header.
pub fn builder_content_security_policy(application_origin: Option<&str>) -> String {
    let origin = application_origin.filter(|origin| {
        (origin.starts_with("http://") || origin.starts_with("https://"))
            && origin.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '/' | '-' | '[' | ']')
            })
    });
    match origin {
        Some(origin) => BUILDER_CONTENT_SECURITY_POLICY.replacen(
            "img-src 'self' data:;",
            &format!("img-src 'self' data: {origin};"),
            1,
        ),
        None => BUILDER_CONTENT_SECURITY_POLICY.to_owned(),
    }
}

/// A fresh, unguessable CSRF token (256 bits from two v4 UUIDs, hex-encoded).
pub(crate) fn new_csrf_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Build a cookie with the shared security attributes.
pub(crate) fn build_cookie(
    name: &'static str,
    value: String,
    http_only: bool,
    secure: bool,
) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/")
        .same_site(SameSite::Strict)
        .http_only(http_only)
        .secure(secure)
        .build()
}

/// Whether a request comes from a **native app**: the generated client sends
/// [`CLIENT_KIND_HEADER`]: [`NATIVE_CLIENT`] wherever there is no `document`.
///
/// Its only effect is on the session cookie a login sets: a native app's gets a
/// `Max-Age` of the session's lifetime, because a React Native cookie store may
/// drop a cookie without one when the *app* closes, signing its user out every
/// time. A browser's stays a session cookie, which a shared or kiosk machine
/// relies on. A page on another site cannot make a victim's browser send this
/// header, and all it changes is how long the sender's own cookie lasts.
pub(crate) fn is_native_client(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(CLIENT_KIND_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case(NATIVE_CLIENT))
}

/// Whether a method may change server state (and so needs CSRF protection).
fn is_mutating(method: &Method) -> bool {
    !matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    )
}

/// Whether this request authenticates by a bearer credential rather than by the
/// session cookie — and is therefore exempt from the CSRF check below.
///
/// **The exemption is written as a property of the request, not as a path**
/// (design §13.6). `/mcp` is the one route that has it today, and a
/// `path == "/mcp"` test here would be one refactor away from being wrong: it
/// would still exempt a route that had since started honouring cookies, and it
/// would not exempt the next bearer-authenticated route somebody adds.
///
/// It is safe for exactly one reason, and the reason is the whole of it: a page
/// a user visits **cannot set an `Authorization` header on a cross-origin
/// request** without a preflight this server does not answer. A forged
/// cross-site request therefore cannot carry one, so a request that does carry
/// one was not forged — which is the property the double-submit cookie is
/// standing in for everywhere else. Nothing is given up on the routes that do
/// honour cookies, because they ignore this header and are still checked.
fn is_bearer_authenticated(request: &Request) -> bool {
    crate::mcp::bearer_token(request.headers()).is_some()
}

/// The v1 spelling of the CSRF header: what `saltcorn.js` sends on every ajax
/// POST a Saltcorn UI view makes (v1's `"CSRF-Token": _sc_globalCsrf`).
pub const V1_CSRF_HEADER: &str = "csrf-token";

/// The form field a server-rendered form carries the token in — v1's
/// `renderForm(form, req.csrfToken())` writes `<input name="_csrf">`.
pub const CSRF_FORM_FIELD: &str = "_csrf";

/// The largest form body the CSRF check reads to find [`CSRF_FORM_FIELD`]. A
/// form is fields, not files; an upload is not form-encoded and is never read
/// here.
const MAX_CSRF_FORM_BODY: usize = 2 * 1024 * 1024;

/// This browser's CSRF token, as the request's handler sees it: the cookie's
/// value, or the one minted for it on first contact — which the response then
/// sets, so a page rendered on first contact carries the token its cookie will
/// hold (TODO "Saltcorn UI" 6.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsrfToken(pub String);

/// What [`csrf_middleware`] is configured with.
#[derive(Clone)]
pub(crate) struct CsrfPolicy {
    /// Whether the minted cookie carries the `Secure` flag.
    pub secure: bool,
    /// Whether a request goes to an endpoint anybody may call with no session
    /// — an application API endpoint whose role floor is the public role. Asked
    /// only of a mutating request that failed the check.
    pub open_to_public: Arc<dyn Fn(&Request) -> bool + Send + Sync>,
}

/// Marks a mutating request [`csrf_middleware`] let through **without** a valid
/// token, because it is going to an endpoint open to the public role. The
/// dispatcher must serve it as nobody: no session read, none started or ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AnonymousCaller;

/// CSRF middleware implementing the double-submit-cookie check.
///
/// On a mutating request the token must equal the existing `sc_csrf` cookie,
/// else the request is rejected `403`. The token may come in any of three
/// places, all the same check against the same cookie:
///
/// - the `x-csrf-token` header — the admin SPA and the generated client;
/// - the `csrf-token` header — v1's `saltcorn.js`, in a Saltcorn UI view;
/// - an `application/x-www-form-urlencoded` body's `_csrf` field — a Saltcorn UI
///   form submitted by the browser, which cannot set a header.
///
/// A cross-site page can make the browser send the cookie but cannot read it,
/// so it can put the token in none of them. Every response ensures the cookie is
/// set (minting one on first contact) and the handler is told the token in a
/// [`CsrfToken`] extension. [`CsrfPolicy`] carries the `Secure` cookie flag.
///
/// **Bearer requests are exempt** (see [`is_bearer_authenticated`]).
///
/// **So is a request to an endpoint open to the public role**
/// ([`CsrfPolicy::open_to_public`]) — but only by being served as the anonymous
/// caller it would be without a cookie: it passes on with an [`AnonymousCaller`]
/// marker, and the dispatcher reads no session for it. CSRF is a defence of the
/// authority a cookie carries; a request that is given none has nothing for a
/// cross-site page to borrow, and anybody may make it anyway.
pub(crate) async fn csrf_middleware(
    State(policy): State<CsrfPolicy>,
    jar: CookieJar,
    request: Request,
    next: Next,
) -> Response {
    let secure = policy.secure;
    let existing = jar.get(CSRF_COOKIE).map(|c| c.value().to_owned());
    let mut request = request;

    if is_mutating(request.method()) && !is_bearer_authenticated(&request) {
        let header = [CSRF_HEADER, V1_CSRF_HEADER]
            .iter()
            .find_map(|name| request.headers().get(*name))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut valid = matches!((&existing, &header), (Some(cookie), Some(hdr)) if cookie == hdr);
        if !valid && header.is_none() && is_form(&request) {
            let (parts, body) = request.into_parts();
            let bytes = match axum::body::to_bytes(body, MAX_CSRF_FORM_BODY).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    return (StatusCode::PAYLOAD_TOO_LARGE, "form body too large").into_response();
                }
            };
            valid = existing.as_deref().is_some_and(|cookie| {
                form_field(&bytes, CSRF_FORM_FIELD).as_deref() == Some(cookie)
            });
            request = Request::from_parts(parts, axum::body::Body::from(bytes));
        }
        // A native app that is signed in but holds no token — its session
        // cookie outlives the app, the token it remembered does not — is refused
        // even here, so its client learns the token and retries as itself rather
        // than writing as nobody while it shows the user signed in. A browser
        // sends no such header, so for it the public-endpoint rule is unchanged.
        let signed_in_native =
            is_native_client(request.headers()) && jar.get(SESSION_COOKIE).is_some();
        if !valid && !signed_in_native && (policy.open_to_public)(&request) {
            request.extensions_mut().insert(AnonymousCaller);
        } else if !valid {
            // Reject, but still hand out a token — as the cookie and as the
            // header — so a first-contact client can read it and retry.
            let token = existing.clone().unwrap_or_else(new_csrf_token);
            let jar = match existing {
                Some(_) => jar,
                None => jar.add(build_cookie(CSRF_COOKIE, token.clone(), false, secure)),
            };
            let mut refused =
                (StatusCode::FORBIDDEN, jar, "CSRF token missing or invalid").into_response();
            expose_csrf_token_on_refusal(&mut refused, &token);
            return refused;
        }
    }

    let token = existing.clone().unwrap_or_else(new_csrf_token);
    request.extensions_mut().insert(CsrfToken(token.clone()));
    let response = next.run(request).await;
    let jar = match existing {
        Some(_) => jar,
        None => jar.add(build_cookie(CSRF_COOKIE, token.clone(), false, secure)),
    };
    let mut response = (jar, response).into_response();
    expose_csrf_token(&mut response, &token);
    response
}

/// Name the request's CSRF token in the response's `x-csrf-token` header — how a
/// client with no `document.cookie` (a native app) learns what to echo — on the
/// responses where that is safe and useful, and nowhere else.
///
/// The token belongs to one browser, so it must never reach a **shared cache**:
/// a CDN that stored a hashed asset served `Cache-Control: public, immutable`
/// with the token on it would hand that user's token to everyone who fetched the
/// asset. So the header goes only on API answers — JSON, which is what a native
/// client calls — and never on one a shared cache may keep. A JSON answer that
/// states no caching of its own is marked `private` as it gains the header, which
/// is what it is anyway: one user's data.
fn expose_csrf_token(response: &mut Response, token: &str) {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));
    if !is_json {
        return;
    }
    let cache = response
        .headers()
        .get(CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_ascii_lowercase);
    match cache.as_deref() {
        None => {
            response
                .headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("private"));
        }
        Some(rule) if rule.contains("private") || rule.contains("no-store") => {}
        // Anything a shared cache may store gets no token.
        Some(_) => return,
    }
    if let Ok(value) = HeaderValue::from_str(token) {
        response.headers_mut().insert(CSRF_HEADER, value);
    }
}

/// Name the token on a refusal: always, because the refusal is exactly where a
/// first-contact client needs it, and never cacheable, because it is one
/// browser's answer.
fn expose_csrf_token_on_refusal(response: &mut Response, token: &str) {
    use axum::http::header::CACHE_CONTROL;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(value) = HeaderValue::from_str(token) {
        response.headers_mut().insert(CSRF_HEADER, value);
    }
}

/// Whether the request's body is a URL-encoded form.
fn is_form(request: &Request) -> bool {
    request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"))
}

/// The first value of `name` in a URL-encoded form body.
fn form_field(body: &[u8], name: &str) -> Option<String> {
    crate::router::parse_form(&String::from_utf8_lossy(body))
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deployment_with_no_applications_is_served_the_strict_policy_unchanged() {
        // No base domain is no applications (the router refuses to start with
        // mounts and no base domain), so there is nothing to frame.
        assert_eq!(admin_content_security_policy(None), CONTENT_SECURITY_POLICY);
        assert_eq!(
            admin_content_security_policy(Some("  ")),
            CONTENT_SECURITY_POLICY
        );
    }

    #[test]
    fn the_applications_subdomains_are_the_only_thing_the_admin_may_frame() {
        let policy = admin_content_security_policy(Some("example.com"));
        assert!(
            policy.contains("frame-src 'self' *.example.com *.example.com:*;"),
            "{policy}"
        );
        // ...and that is the *whole* of the difference: a directive that gained
        // a source here would relax the admin UI, which the strict policy is
        // there to keep from happening by accident.
        assert_eq!(
            policy.replace("frame-src 'self' *.example.com *.example.com:*; ", ""),
            CONTENT_SECURITY_POLICY,
        );
        // Nothing the IDE's policy relaxes arrives with it.
        for forbidden in ["unsafe-eval", "blob:", "worker-src"] {
            assert!(!policy.contains(forbidden), "{forbidden} in {policy}");
        }
    }
}

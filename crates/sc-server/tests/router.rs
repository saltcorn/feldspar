//! End-to-end tests for the server's dispatch, auth, CSRF, session, and static
//! serving — driven through the assembled router with `tower`'s `oneshot`, so no
//! network or database is required. The concrete admin handlers and their
//! DB-backed tests arrive in the second half of the Phase 6 server subphase.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::{AuthRequirement, Endpoint, EndpointSet, Method, PathSpec};
use sc_auth::{ROLE_ADMIN, SessionStore, User};
use sc_server::{
    CONTENT_SECURITY_POLICY, CSRF_COOKIE, CSRF_HEADER, HandlerRegistry, HandlerResponse,
    SESSION_COOKIE, ServerConfig, build_router,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// A small endpoint set exercising every dispatch path.
fn test_endpoints() -> EndpointSet {
    let mut set = EndpointSet::new();
    set.register(
        Endpoint::new("ping", Method::Get, PathSpec::root().lit("api/ping"))
            .auth(AuthRequirement::Public),
    );
    set.register(
        Endpoint::new("echo", Method::Post, PathSpec::root().lit("api/echo"))
            .auth(AuthRequirement::Public),
    );
    set.register(
        Endpoint::new("bulk", Method::Get, PathSpec::root().lit("api/bulk"))
            .auth(AuthRequirement::Public),
    );
    set.register(
        Endpoint::new("secret", Method::Get, PathSpec::root().lit("api/secret"))
            .auth(AuthRequirement::admin()),
    );
    set.register(
        Endpoint::new("login", Method::Post, PathSpec::root().lit("api/login"))
            .auth(AuthRequirement::Public),
    );
    set.register(
        Endpoint::new("logout", Method::Post, PathSpec::root().lit("api/logout"))
            .auth(AuthRequirement::LoggedIn),
    );
    set
}

fn test_registry() -> HandlerRegistry {
    let mut reg = HandlerRegistry::new();
    reg.register("ping", |_ctx| async {
        Ok(HandlerResponse::ok(json!({ "pong": true })))
    });
    reg.register(
        "echo",
        |ctx| async move { Ok(HandlerResponse::ok(ctx.body)) },
    );
    reg.register("bulk", |_ctx| async {
        let rows: Vec<Value> = (0..500)
            .map(|i| json!({ "id": i, "title": "A slide of the presentation" }))
            .collect();
        Ok(HandlerResponse::ok(json!(rows)))
    });
    reg.register("secret", |_ctx| async {
        Ok(HandlerResponse::ok(json!({ "ok": true })))
    });
    reg.register("login", |_ctx| async {
        let admin = User::new(Uuid::new_v4(), ROLE_ADMIN)?;
        Ok(HandlerResponse::start_session(
            admin,
            json!({ "role": ROLE_ADMIN }),
        ))
    });
    reg.register("logout", |_ctx| async {
        Ok(HandlerResponse::end_session(json!({ "ok": true })))
    });
    reg
}

/// Build a router over the test endpoints, returning it alongside the shared
/// session store so tests can seed sessions directly.
fn test_router() -> (Router, Arc<SessionStore>) {
    let sessions = Arc::new(SessionStore::default());
    let router = build_router(
        &test_endpoints(),
        test_registry(),
        sessions.clone(),
        &ServerConfig::default(),
    )
    .expect("build router");
    (router, sessions)
}

/// Run one request and return status, the response's Set-Cookie values, and the
/// body as text.
async fn call(router: &Router, request: Request<Body>) -> (StatusCode, Vec<String>, String) {
    let response = router.clone().oneshot(request).await.expect("dispatch");
    let status = response.status();
    let cookies: Vec<String> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_owned)
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    (
        status,
        cookies,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Extract a cookie value by name from a set of Set-Cookie header values.
fn cookie_value(cookies: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    cookies.iter().find_map(|c| {
        c.strip_prefix(&prefix)
            .map(|rest| rest.split(';').next().unwrap_or("").to_owned())
    })
}

/// A temp "built bundle" — an `index.html` linking a content-hashed entry, the
/// shape `vite build` produces — served via `--static-dir`. Cleaned up by the
/// caller.
fn admin_bundle(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-admin-bundle-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("assets/index-a1b2c3.js"), "export const x = 1;\n").unwrap();
    std::fs::write(
        dir.join("index.html"),
        "<!doctype html><div id=\"root\"></div>\n\
         <script type=\"module\" src=\"/assets/index-a1b2c3.js\"></script>\n",
    )
    .unwrap();
    dir
}

fn static_router(dir: &std::path::Path) -> Router {
    let sessions = Arc::new(SessionStore::default());
    let config = ServerConfig {
        static_dir: Some(dir.to_path_buf()),
        ..ServerConfig::default()
    };
    build_router(&test_endpoints(), test_registry(), sessions, &config).unwrap()
}

#[tokio::test]
async fn serves_a_static_bundle_and_falls_back_to_its_document() {
    let dir = admin_bundle("fallback");
    let router = static_router(&dir);

    // The bundle asset is served from the static dir.
    let (status, _, body) = call(
        &router,
        Request::get("/assets/index-a1b2c3.js")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("export const x"));

    // An unknown (client-routed) path falls back to the bundle's *own* document,
    // which is the only thing that knows the hashed name of the entry to load.
    // A constant here could only guess it, and would boot nothing.
    let (status, _, body) = call(
        &router,
        Request::get("/some/spa/route").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<div id=\"root\"></div>"));
    assert!(body.contains("/assets/index-a1b2c3.js"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A rebuilt bundle must be a bundle the browser actually fetches again.
///
/// The hashed name is half of that; these headers are the other half. The entry
/// may be cached forever because changing it changes its URL, while the document
/// — whose URL never changes, and which names the entry — must be revalidated on
/// every load. Get the second wrong and a reload keeps booting the old build
/// until someone empties the cache by hand.
#[tokio::test]
async fn hashed_assets_are_immutable_and_the_document_is_not() {
    let dir = admin_bundle("cache");
    let router = static_router(&dir);

    for (path, expected) in [
        (
            "/assets/index-a1b2c3.js",
            "public, max-age=31536000, immutable",
        ),
        ("/index.html", "no-cache"),
        ("/", "no-cache"),
        ("/some/spa/route", "no-cache"),
    ] {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            expected,
            "{path}"
        );
        // A cached asset or document never carries a user's CSRF token: a
        // shared cache would hand it to everyone who asked for the same URL.
        assert!(
            response.headers().get(CSRF_HEADER).is_none(),
            "{path} carried the CSRF token"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn serves_bootstrap_document_with_security_headers() {
    let (router, _) = test_router();
    let response = router
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    // Strict CSP and hardening headers on every response (design §16).
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap(),
        CONTENT_SECURITY_POLICY
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(
        response.headers().get(header::X_FRAME_OPTIONS).unwrap(),
        "DENY"
    );

    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);
    // This router has no bundle, so what comes back says so rather than linking
    // assets that are not there. Guessing a name would produce the blank page
    // with a MIME-type error in the console instead of the sentence.
    assert!(html.contains("not built"));
    // No server-rendered admin markup and no inline script or style, so the
    // strict CSP holds here too.
    assert!(!html.contains("<script"));
    assert!(!html.contains("onclick"));
}

#[tokio::test]
async fn public_endpoint_reaches_its_handler() {
    let (router, _) = test_router();
    let (status, cookies, body) = call(
        &router,
        Request::get("/api/ping").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({ "pong": true })
    );
    // The CSRF double-submit cookie is minted on first contact.
    assert!(cookie_value(&cookies, CSRF_COOKIE).is_some());
}

/// A client that says it accepts gzip or brotli gets a compressed body, and one
/// that says nothing gets plain JSON — the content snapshot an application loads
/// on its first visit is mostly repeated keys, which is what compression is for.
#[tokio::test]
async fn responses_are_compressed_for_a_client_that_accepts_it() {
    use std::io::Read;

    let (router, _) = test_router();
    let plain = router
        .clone()
        .oneshot(Request::get("/api/bulk").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
    let plain = axum::body::to_bytes(plain.into_body(), 1 << 20)
        .await
        .unwrap();

    let gzipped = router
        .clone()
        .oneshot(
            Request::get("/api/bulk")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(gzipped.status(), StatusCode::OK);
    assert_eq!(gzipped.headers()[header::CONTENT_ENCODING], "gzip");
    assert!(
        gzipped.headers()[header::VARY]
            .to_str()
            .unwrap()
            .contains("accept-encoding")
    );
    let compressed = axum::body::to_bytes(gzipped.into_body(), 1 << 20)
        .await
        .unwrap();
    assert!(
        compressed.len() * 10 < plain.len(),
        "{} compressed vs {} plain",
        compressed.len(),
        plain.len()
    );
    let mut inflated = Vec::new();
    flate2::read::GzDecoder::new(&compressed[..])
        .read_to_end(&mut inflated)
        .unwrap();
    assert_eq!(inflated, plain);

    let brotli = router
        .clone()
        .oneshot(
            Request::get("/api/bulk")
                .header(header::ACCEPT_ENCODING, "br, gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(brotli.headers()[header::CONTENT_ENCODING], "br");
}

#[tokio::test]
async fn unregistered_handler_is_not_implemented() {
    // Admin endpoint set with an *empty* registry: routes mount, but handlers 501.
    let sessions = Arc::new(SessionStore::default());
    let router = build_router(
        &sc_api::admin_endpoints(),
        HandlerRegistry::new(),
        sessions,
        &ServerConfig::default(),
    )
    .unwrap();
    let (status, _, _) = call(
        &router,
        Request::get("/api/auth/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn unknown_method_on_route_is_405() {
    let (router, _) = test_router();
    // /api/echo is POST-only; a GET matches the path but no endpoint's method,
    // so it is method-not-allowed (GET is CSRF-safe, so it reaches dispatch).
    let (status, _, _) = call(
        &router,
        Request::get("/api/echo").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn admin_route_requires_authentication() {
    let (router, _) = test_router();
    let (status, _, _) = call(
        &router,
        Request::get("/api/secret").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn insufficient_role_is_forbidden() {
    let (router, sessions) = test_router();
    // Seed a session for a non-admin user (role 40) directly in the store.
    let editor = User::new(Uuid::new_v4(), 40).unwrap();
    let token = sessions.login(editor).await.unwrap();

    let request = Request::get("/api/secret")
        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = call(&router, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn csrf_blocks_mutations_without_a_token() {
    let (router, _) = test_router();
    // POST with neither cookie nor header is rejected before reaching the handler.
    let request = Request::post("/api/echo")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{\"a\":1}"))
        .unwrap();
    let (status, _, _) = call(&router, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn csrf_allows_mutations_with_a_matching_token() {
    let (router, _) = test_router();
    // 1. A GET mints the CSRF cookie.
    let (_, cookies, _) = call(
        &router,
        Request::get("/api/ping").body(Body::empty()).unwrap(),
    )
    .await;
    let csrf = cookie_value(&cookies, CSRF_COOKIE).expect("csrf cookie");

    // 2. The mutation echoes it in both cookie and header, and reaches the handler.
    let request = Request::post("/api/echo")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("{CSRF_COOKIE}={csrf}"))
        .header(CSRF_HEADER, &csrf)
        .body(Body::from("{\"a\":1}"))
        .unwrap();
    let (status, _, body) = call(&router, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({ "a": 1 })
    );
}

/// A client that cannot read cookies — a React Native app, whose cookie store is
/// native and invisible to its JavaScript — learns the token from the
/// `x-csrf-token` response header, which names the same value as the cookie on
/// every response, the refusal included.
#[tokio::test]
async fn every_response_names_the_csrf_token_for_a_client_that_cannot_read_cookies() {
    let (router, _) = test_router();
    let header_of = |response: &axum::http::Response<Body>| {
        response
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    // A refused write hands out the token it wanted, so the client can retry.
    let refused = router
        .clone()
        .oneshot(
            Request::post("/api/echo")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let minted = header_of(&refused).expect("the refusal names the token");
    let cookies: Vec<String> = refused
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect();
    assert_eq!(
        cookie_value(&cookies, CSRF_COOKIE).as_deref(),
        Some(minted.as_str())
    );

    // An ordinary response names the token the request's cookie carries.
    let ping = router
        .clone()
        .oneshot(
            Request::get("/api/ping")
                .header(header::COOKIE, format!("{CSRF_COOKIE}={minted}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(header_of(&ping).as_deref(), Some(minted.as_str()));
    // …and marks itself as one user's answer, so no shared cache keeps it.
    assert_eq!(
        ping.headers().get(header::CACHE_CONTROL).unwrap(),
        "private"
    );
    assert_eq!(
        refused.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );

    // And echoing what the header said — with the cookie the native store sends
    // on its own — is accepted.
    let (status, _, _) = call(
        &router,
        Request::post("/api/echo")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, format!("{CSRF_COOKIE}={minted}"))
            .header(CSRF_HEADER, &minted)
            .body(Body::from("{\"a\":1}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// A native app — the generated client with no `document` says so — gets a
/// session cookie that lasts as long as the session, so closing the app does not
/// sign its user out. A browser's login is unchanged (see the test above).
#[tokio::test]
async fn a_native_apps_login_outlives_the_app_being_closed() {
    let (router, _) = test_router();
    let (_, cookies, _) = call(
        &router,
        Request::get("/api/ping").body(Body::empty()).unwrap(),
    )
    .await;
    let csrf = cookie_value(&cookies, CSRF_COOKIE).expect("csrf cookie");
    let login = Request::post("/api/login")
        .header(header::COOKIE, format!("{CSRF_COOKIE}={csrf}"))
        .header(CSRF_HEADER, &csrf)
        .header(
            sc_api::auth::CLIENT_KIND_HEADER,
            sc_api::auth::NATIVE_CLIENT,
        )
        .body(Body::empty())
        .unwrap();
    let (status, cookies, _) = call(&router, login).await;
    assert_eq!(status, StatusCode::OK);
    let set_session = cookies
        .iter()
        .find(|c| c.starts_with(&format!("{SESSION_COOKIE}=")))
        .unwrap();
    assert!(
        set_session.contains(&format!("Max-Age={}", sc_auth::DEFAULT_TTL_HOURS * 3600)),
        "{set_session}"
    );
    // The CSRF cookie is not the session, and is left as it was.
    assert!(
        cookies
            .iter()
            .filter(|c| c.starts_with(&format!("{CSRF_COOKIE}=")))
            .all(|c| !c.contains("Max-Age"))
    );
}

/// TODO "Saltcorn UI" 6.4: the same double-submit check, with the token where
/// v1's browser code puts it — `saltcorn.js`'s `CSRF-Token` header, and a
/// rendered form's `_csrf` field — and still refused when it is wrong.
#[tokio::test]
async fn csrf_accepts_the_token_in_v1s_header_and_in_a_forms_field() {
    let (router, _) = test_router();
    let (_, cookies, _) = call(
        &router,
        Request::get("/api/ping").body(Body::empty()).unwrap(),
    )
    .await;
    let csrf = cookie_value(&cookies, CSRF_COOKIE).expect("csrf cookie");

    // v1's header spelling.
    let request = Request::post("/api/echo")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("{CSRF_COOKIE}={csrf}"))
        .header("CSRF-Token", &csrf)
        .body(Body::from("{\"a\":1}"))
        .unwrap();
    let (status, _, _) = call(&router, request).await;
    assert_eq!(status, StatusCode::OK);

    // A browser's form post: no header, the token in the body. It gets past the
    // check (the echo handler then has a form where it wanted JSON).
    let form = |token: &str| {
        Request::post("/api/echo")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::COOKIE, format!("{CSRF_COOKIE}={csrf}"))
            .body(Body::from(format!("title=Dune&_csrf={token}")))
            .unwrap()
    };
    let (status, _, body) = call(&router, form(&csrf)).await;
    assert_ne!(status, StatusCode::FORBIDDEN, "{body}");

    // The wrong token, in either place, and a form with none.
    let (status, _, _) = call(&router, form("forged")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let request = Request::post("/api/echo")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("{CSRF_COOKIE}={csrf}"))
        .body(Body::from("title=Dune"))
        .unwrap();
    let (status, _, _) = call(&router, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let request = Request::post("/api/echo")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("{CSRF_COOKIE}={csrf}"))
        .header("CSRF-Token", "forged")
        .body(Body::from("{\"a\":1}"))
        .unwrap();
    let (status, _, _) = call(&router, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_starts_a_session_that_unlocks_admin_routes() {
    let (router, _) = test_router();
    // 1. GET to obtain the CSRF token.
    let (_, cookies, _) = call(
        &router,
        Request::get("/api/ping").body(Body::empty()).unwrap(),
    )
    .await;
    let csrf = cookie_value(&cookies, CSRF_COOKIE).expect("csrf cookie");

    // 2. POST login → sets a session cookie.
    let login = Request::post("/api/login")
        .header(header::COOKIE, format!("{CSRF_COOKIE}={csrf}"))
        .header(CSRF_HEADER, &csrf)
        .body(Body::empty())
        .unwrap();
    let (status, cookies, _) = call(&router, login).await;
    assert_eq!(status, StatusCode::OK);
    let session = cookie_value(&cookies, SESSION_COOKIE).expect("session cookie");
    // A browser's session cookie has no lifetime of its own: it ends with the
    // browser, which is what a shared machine relies on.
    let set_session = cookies
        .iter()
        .find(|c| c.starts_with(&format!("{SESSION_COOKIE}=")))
        .unwrap();
    assert!(
        !set_session.to_ascii_lowercase().contains("max-age"),
        "{set_session}"
    );

    // 3. The session grants access to the admin-only route.
    let secret = Request::get("/api/secret")
        .header(header::COOKIE, format!("{SESSION_COOKIE}={session}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = call(&router, secret).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({ "ok": true })
    );
}

/// The request log wraps every route, so the thing it must not do is change any
/// of them — and, at `trace`, the thing it must not do is bury the level that
/// carries the model transcripts under a header dump per request.
///
/// Driven at `trace`, the loudest level: a request there logs its arrival and
/// its completion and **nothing else**. A body consumed or a header moved by a
/// logger would be a bug visible only when somebody turned logging on — which is
/// to say, only in production, only while debugging something else.
#[tokio::test]
async fn logging_every_request_leaves_the_request_alone() {
    let (router, _sessions) = test_router();
    let _log = sc_log::capture::guard(sc_log::Verbosity::Trace);

    let (status, _, body) = call(
        &router,
        Request::builder()
            .method("POST")
            .uri("/api/echo?logged=yes")
            .header(header::CONTENT_TYPE, "application/json")
            // Credentials the trace must not print, and a header it has to hand
            // on regardless.
            .header(header::COOKIE, format!("{SESSION_COOKIE}=not-a-session"))
            .header(CSRF_HEADER, "csrf-secret-value")
            .header(header::COOKIE, format!("{CSRF_COOKIE}=csrf-secret-value"))
            .body(Body::from(json!({ "hello": "world" }).to_string()))
            .unwrap(),
    )
    .await;

    let log = sc_log::capture::take();

    assert_eq!(status, StatusCode::OK, "{body}");
    // The handler echoes its body, so this is the whole request arriving intact
    // through the logging layer.
    let echoed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(echoed, json!({ "hello": "world" }));

    // And it was logged: the request as it arrived, and the line that says how
    // it ended. Two lines, at the loudest level there is.
    assert_eq!(log.len(), 2, "{log:?}");
    let logged = log.join("\n");
    assert!(logged.contains("→ POST /api/echo?logged=yes"), "{logged}");
    assert!(
        logged.contains("POST /api/echo?logged=yes → 200 in"),
        "{logged}"
    );
    // The headers are not in it — not the interesting ones and not the boring
    // ones, which is the point: `trace` is where the model transcripts are, and
    // a dozen lines of `sec-fetch-mode` per request is what made that
    // unreadable. Nothing that authenticates the caller can leak from a log
    // that never prints a header.
    assert!(!logged.contains("content-type"), "{logged}");
    assert!(!logged.contains("not-a-session"), "{logged}");
    assert!(!logged.contains("csrf-secret-value"), "{logged}");
}

/// The rung the setting promises requests at, and the one below it: `info` logs
/// every request, `warning` logs none.
#[tokio::test]
async fn requests_are_logged_at_info_and_not_below_it() {
    let (router, _sessions) = test_router();

    let mut logged_at = Vec::new();
    for level in [sc_log::Verbosity::Warning, sc_log::Verbosity::Info] {
        let _log = sc_log::capture::guard(level);
        let (status, _, _) = call(
            &router,
            Request::builder()
                .uri("/api/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let lines = sc_log::capture::take();
        assert_eq!(status, StatusCode::OK);
        logged_at.push(lines);
    }

    assert!(logged_at[0].is_empty(), "{:?}", logged_at[0]);
    assert_eq!(logged_at[1].len(), 1, "{:?}", logged_at[1]);
    assert!(
        logged_at[1][0].ends_with("ms") && logged_at[1][0].contains("GET /api/ping → 200 in"),
        "{:?}",
        logged_at[1]
    );
}

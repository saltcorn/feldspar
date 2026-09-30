//! The Analytics UI's route (analytics TODO A1.14): served under `/analytics/`
//! to an admin, under its own Content-Security-Policy, and to nobody else.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::{AuthRequirement, Endpoint, EndpointSet, Method, PathSpec};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, SessionStore, User};
use sc_server::{
    ANALYTICS_CONTENT_SECURITY_POLICY, HandlerRegistry, SESSION_COOKIE, ServerConfig, build_router,
};
use tower::ServiceExt;
use uuid::Uuid;

fn endpoints() -> EndpointSet {
    let mut set = EndpointSet::new();
    set.register(
        Endpoint::new("ping", Method::Get, PathSpec::root().lit("api/ping"))
            .auth(AuthRequirement::Public),
    );
    set
}

/// A directory standing in for `ui/analytics/dist`.
fn bundle(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sc-analytics-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(
        dir.join("assets/index-a1b2.js"),
        "export const analytics = 1;\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("index.html"),
        "<!doctype html><div id=\"root\"></div>\n\
         <script type=\"module\" src=\"/analytics/assets/index-a1b2.js\"></script>\n",
    )
    .unwrap();
    dir
}

fn router(dir: Option<std::path::PathBuf>) -> (Router, Arc<SessionStore>) {
    let sessions = Arc::new(SessionStore::default());
    let config = ServerConfig {
        analytics_dir: dir,
        ..ServerConfig::default()
    };
    let router = build_router(
        &endpoints(),
        HandlerRegistry::new(),
        sessions.clone(),
        &config,
    )
    .expect("build router");
    (router, sessions)
}

async fn session_for(sessions: &SessionStore, role: u8) -> String {
    sessions
        .login(User::new(Uuid::new_v4(), role).unwrap())
        .await
        .unwrap()
}

fn get(path: &str, session: Option<&str>, html: bool) -> Request<Body> {
    let mut request = Request::get(path);
    if let Some(token) = session {
        request = request.header(header::COOKIE, format!("{SESSION_COOKIE}={token}"));
    }
    if html {
        request = request.header(header::ACCEPT, "text/html,*/*");
    }
    request.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn an_admin_gets_the_bundle_under_its_own_policy() {
    let dir = bundle("admin");
    let (router, sessions) = router(Some(dir.clone()));
    let token = session_for(&sessions, ROLE_ADMIN).await;
    for path in ["/analytics", "/analytics/"] {
        let response = router
            .clone()
            .oneshot(get(path, Some(&token), true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .unwrap(),
            ANALYTICS_CONTENT_SECURITY_POLICY
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("id=\"root\""));
    }
    let response = router
        .clone()
        .oneshot(get("/analytics/assets/index-a1b2.js", Some(&token), false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // `/analyticsx` is not the Analytics UI's.
    let response = router
        .clone()
        .oneshot(get("/analyticsx", Some(&token), false))
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("/analytics/assets/"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_non_admin_is_refused_and_a_visitor_is_sent_to_sign_in() {
    let dir = bundle("denied");
    let (router, sessions) = router(Some(dir.clone()));

    let response = router
        .clone()
        .oneshot(get("/analytics/", None, true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/");

    let response = router
        .clone()
        .oneshot(get("/analytics/assets/index-a1b2.js", None, false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A signed-in user who is not an admin is refused, even navigating.
    let public = session_for(&sessions, ROLE_PUBLIC).await;
    for html in [true, false] {
        let response = router
            .clone()
            .oneshot(get("/analytics/", Some(&public), html))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("id=\"root\""));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn without_a_bundle_the_prefix_says_how_to_build_one() {
    let (router, sessions) = router(None);
    let token = session_for(&sessions, ROLE_ADMIN).await;
    let response = router
        .oneshot(get("/analytics/", Some(&token), true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("ui/analytics"));
}

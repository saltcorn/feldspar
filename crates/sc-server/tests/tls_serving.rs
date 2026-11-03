//! Serving HTTPS end to end (design §13.5).
//!
//! Everything else about TLS can be asserted without a socket — the settings,
//! the PEM parsing, the redirect's status line. The one thing that cannot is the
//! **handshake**, because it happens below the service a `tower::oneshot` would
//! drive: whether the certificate an admin pasted actually terminates a
//! connection is a question only a client can answer.
//!
//! So this test stands the real serving path up on an ephemeral port —
//! `TlsSettings::from_ssl` over stored-shaped settings, then
//! [`sc_server::serve_https`] — points a client at it that trusts exactly the
//! certificate it generated, and asks for a response. It uses a self-signed
//! certificate made in-process: a test must not carry a key in the repository
//! (it would expire) and must not reach a CA.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use sc_config::{SslMode, SslSettings};
use sc_server::{TlsNames, TlsSettings};

/// A self-signed certificate for `localhost`, and its key.
fn self_signed() -> (String, String) {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    (key.cert.pem(), key.key_pair.serialize_pem())
}

/// Stored settings in `custom` mode, as `_fd_config` would yield them.
fn custom_settings(certificate: String, private_key: String) -> SslSettings {
    SslSettings {
        mode: SslMode::Custom,
        certificate,
        private_key: private_key.into(),
        ..SslSettings::default()
    }
}

/// The application under test: one route, because what is being tested is
/// everything *underneath* the route.
fn app() -> Router {
    Router::new().route("/ping", get(|| async { "pong" }))
}

/// A listener on an ephemeral port, and the address it got.
fn ephemeral() -> (std::net::TcpListener, SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

#[tokio::test]
async fn a_pasted_certificate_terminates_a_real_handshake() {
    let (certificate, private_key) = self_signed();
    let (listener, addr) = ephemeral();

    let tls = TlsSettings::from_ssl(
        &custom_settings(certificate.clone(), private_key),
        addr.port(),
        TlsNames::default(),
        None,
    )
    .expect("a pasted certificate is a serving plan");
    assert!(tls.enabled());

    let handle = axum_server::Handle::new();
    let server = tokio::spawn({
        let handle = handle.clone();
        async move { sc_server::serve_https(listener, app(), &tls, handle).await }
    });

    // A client that trusts *this* certificate and nothing else — the assertion
    // is that the server presents it, so accepting any certificate would assert
    // nothing. `resolve` pins the hostname the certificate is for to the
    // ephemeral port, so the name in the certificate is the name requested.
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(certificate.as_bytes()).unwrap())
        .resolve("localhost", addr)
        .build()
        .unwrap();

    let response = client
        .get(format!("https://localhost:{}/ping", addr.port()))
        .send()
        .await
        .expect("the handshake completes and the request is answered");
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "pong");

    // The handle is what a `SIGTERM` pulls, so the server stops rather than
    // outliving the test.
    handle.graceful_shutdown(Some(std::time::Duration::from_millis(50)));
    server.await.unwrap().expect("a clean shutdown");
}

/// A client that does not trust the certificate must not be served — otherwise
/// the test above would pass against a server presenting anything at all.
#[tokio::test]
async fn a_client_that_does_not_trust_the_certificate_is_refused() {
    let (certificate, private_key) = self_signed();
    let (listener, addr) = ephemeral();

    let tls = TlsSettings::from_ssl(
        &custom_settings(certificate, private_key),
        addr.port(),
        TlsNames::default(),
        None,
    )
    .unwrap();
    let handle = axum_server::Handle::new();
    let server = tokio::spawn({
        let handle = handle.clone();
        async move { sc_server::serve_https(listener, app(), &tls, handle).await }
    });

    let client = reqwest::Client::builder()
        .resolve("localhost", addr)
        .build()
        .unwrap();
    let outcome = client
        .get(format!("https://localhost:{}/ping", addr.port()))
        .send()
        .await;
    assert!(outcome.is_err(), "an untrusted certificate must not verify");

    handle.graceful_shutdown(Some(std::time::Duration::from_millis(50)));
    let _ = server.await.unwrap();
}

/// The other half of a TLS deployment: the plain-HTTP listener that stays bound
/// and sends callers to HTTPS.
#[tokio::test]
async fn the_plain_listener_redirects_to_https() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, sc_server::redirect_router(8443, Router::new())).await
    });

    // No redirect following: the redirect *is* the response under test, and its
    // target is not listening.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client
        .get(format!("http://{addr}/api/tables?limit=2"))
        .header("host", "example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 308);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "https://example.com:8443/api/tables?limit=2"
    );
    server.abort();
}

/// An IDE terminal's MCP traffic is the exception: it comes from this machine to
/// the plain listener, which is where the session's config points, so it is
/// served there rather than redirected. Everything else still is.
#[tokio::test]
async fn the_plain_listener_still_serves_a_terminals_mcp() {
    use axum::routing::{any, post};
    use tower::ServiceExt;

    let app = Router::new()
        .route("/mcp", post(|| async { "mcp" }))
        .route("/ide/mcp-proxy/{server}", any(|| async { "proxy" }))
        .route("/api/tables", get(|| async { "tables" }));
    let router = sc_server::redirect_router(8443, app);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let router = router.clone();
        async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
        }
    });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let send = |method: reqwest::Method, path: &str, token: Option<&str>| {
        let mut request = client
            .request(method, format!("http://{addr}{path}"))
            .header("host", "example.com");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send()
    };

    let mcp = send(reqwest::Method::POST, "/mcp", Some("fdterm_abc"))
        .await
        .unwrap();
    assert_eq!(mcp.status(), 200);
    assert_eq!(mcp.text().await.unwrap(), "mcp");
    let proxied = send(
        reqwest::Method::GET,
        "/ide/mcp-proxy/crm",
        Some("fdterm_abc"),
    )
    .await
    .unwrap();
    assert_eq!(proxied.text().await.unwrap(), "proxy");

    for (method, path, token) in [
        (reqwest::Method::POST, "/mcp", None),
        (reqwest::Method::POST, "/mcp", Some("fd_a_table_token")),
        (reqwest::Method::GET, "/api/tables", Some("fdterm_abc")),
    ] {
        let response = send(method, path, token).await.unwrap();
        assert_eq!(response.status(), 308, "{path} with {token:?}");
    }

    // From another machine, a session token is redirected like anything else.
    let mut request = axum::http::Request::post("/mcp")
        .header("host", "example.com")
        .header("authorization", "Bearer fdterm_abc")
        .body(axum::body::Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [192, 168, 1, 5],
            50000,
        ))));
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 308);

    server.abort();
}

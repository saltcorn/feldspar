//! A `PublicClient` never goes through a proxy from the environment: the proxy
//! would resolve the target name itself, bypassing the public-only resolver
//! (security review 2026-11, #4 / A1). Its own binary, since it sets the
//! process environment.

use std::time::Duration;

use sc_net::{PublicClient, Redirects};
use tokio::net::TcpListener;

#[tokio::test]
async fn a_public_client_ignores_proxy_variables() {
    // A "proxy" on loopback that records whether anything connected to it.
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    // SAFETY: this test binary has a single test, and the variables are set
    // before any client (or other thread that reads the environment) exists.
    unsafe {
        for var in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
        ] {
            std::env::set_var(var, &proxy_url);
        }
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
    }
    let client = PublicClient::new(
        reqwest::Client::builder().timeout(Duration::from_secs(3)),
        ", which this test may not reach",
        Redirects::None,
    )
    .unwrap();
    // `.invalid` never resolves, so without a proxy the request fails at the
    // resolver; with one it would be handed to the proxy.
    let url = "http://internal-admin.invalid/".parse().unwrap();
    let request = client.request(reqwest::Method::GET, url).unwrap();
    let accepted = tokio::spawn(async move { proxy.accept().await.is_ok() });
    let result = request.send().await;
    assert!(result.is_err(), "the name must not resolve: {result:?}");
    let reached_proxy = tokio::time::timeout(Duration::from_millis(500), accepted).await;
    assert!(
        reached_proxy.is_err(),
        "the request went to the proxy from the environment"
    );
}

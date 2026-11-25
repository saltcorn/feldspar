//! The identity provider end to end with an off-the-shelf relying party
//! (TODO IP1.9): the assembled router on a real port, and the `openidconnect`
//! crate doing what any OpenID Connect library does — discovery, an
//! authorization request with PKCE and a nonce, the code exchange, the
//! `id_token` checked against the JWKS (signature, issuer, audience, nonce,
//! `at_hash`), userinfo, a refresh, revocation and RP-initiated logout. A
//! browser in between signs in and consents over HTTP.
//!
//! Nothing here is Feldspar-specific on the relying party's side: if this
//! passes, a library-built relying party works with no code of its own.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    // A test client to a server on this machine's loopback, with its name
    // resolved there: the SSRF guard `sc_net` puts on outbound calls is what
    // would refuse it.
    clippy::disallowed_methods
)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreGenderClaim, CoreRevocableToken, CoreUserInfoClaims,
};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EmptyAdditionalClaims,
    IssuerUrl, LogoutRequest, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PostLogoutRedirectUrl,
    ProviderMetadataWithLogout, RedirectUrl, RevocationUrl, Scope, TokenResponse, UserInfoClaims,
};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{NewOAuthClient, SessionStore, create_user};
use sc_catalog::{SecretKey, bootstrap_table_meta};
use sc_server::{
    AppMounts, MountedApp, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator,
};
use sc_test_harness::TestServer;
use sc_types::Role;
use serde_json::Value;

const BASE_DOMAIN: &str = "example.com";
const IDP: &str = "staff.example.com";
const REDIRECT: &str = "https://rp.example.net/callback";
const LOGGED_OUT: &str = "https://rp.example.net/bye";
const EMAIL: &str = "staff@example.com";
const PASSWORD: &str = "correct-horse";

/// An HTTP client that reaches the provider's host on the test's port, and
/// follows no redirect: the relying party's and the browser's.
fn http_client(addr: SocketAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(IDP, addr)
        .build()
        .unwrap()
}

/// A browser: cookies kept, one request at a time.
struct Browser {
    http: reqwest::Client,
    origin: String,
    cookies: HashMap<String, String>,
}

struct Page {
    status: u16,
    location: Option<String>,
    body: String,
}

impl Page {
    /// Where a "Continuing…" page goes on to.
    fn continues_to(&self) -> Option<String> {
        let start = self.body.find("url=")? + 4;
        let end = start + self.body[start..].find('"')?;
        Some(self.body[start..end].replace("&amp;", "&"))
    }

    /// A page's hidden fields, as it wrote them.
    fn hidden(&self) -> Vec<(String, String)> {
        self.body
            .split("<input type=\"hidden\" ")
            .skip(1)
            .map(|input| {
                let attr = |name: &str| {
                    let start = input.find(&format!("{name}=\"")).unwrap() + name.len() + 2;
                    let end = start + input[start..].find('"').unwrap();
                    input[start..end]
                        .replace("&amp;", "&")
                        .replace("&quot;", "\"")
                };
                (attr("name"), attr("value"))
            })
            .collect()
    }
}

impl Browser {
    async fn request(
        &mut self,
        method: reqwest::Method,
        url: &str,
        form: Option<&[(String, String)]>,
    ) -> Page {
        let url = if url.starts_with('/') {
            format!("{}{url}", self.origin)
        } else {
            url.to_owned()
        };
        let mut request = self.http.request(method, &url);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            request = request.header("cookie", jar);
        }
        if let Some(form) = form {
            let body = form
                .iter()
                .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            request = request
                .header("content-type", "application/x-www-form-urlencoded")
                .body(body);
        }
        let response = request.send().await.unwrap();
        for raw in response.headers().get_all("set-cookie") {
            if let Some((k, v)) = raw
                .to_str()
                .ok()
                .and_then(|t| t.split(';').next())
                .and_then(|p| p.split_once('='))
            {
                if v.is_empty() {
                    self.cookies.remove(k);
                } else {
                    self.cookies.insert(k.to_owned(), v.to_owned());
                }
            }
        }
        Page {
            status: response.status().as_u16(),
            location: response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            body: response.text().await.unwrap(),
        }
    }

    async fn get(&mut self, url: &str) -> Page {
        self.request(reqwest::Method::GET, url, None).await
    }

    /// Post `page`'s form to `path` with `extra`.
    async fn submit(&mut self, path: &str, page: &Page, extra: &[(&str, &str)]) -> Page {
        let mut fields = page.hidden();
        fields.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        self.request(reqwest::Method::POST, path, Some(&fields))
            .await
    }

    /// Follow "Continuing…" pages on this host; stop at one that leaves it.
    async fn follow(&mut self, mut page: Page) -> (Page, Option<String>) {
        loop {
            match page.continues_to() {
                Some(next) if next.starts_with('/') => page = self.get(&next).await,
                Some(away) => return (page, Some(away)),
                None => return (page, None),
            }
        }
    }
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn query_of(url: &str) -> HashMap<String, String> {
    openidconnect::url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[tokio::test]
async fn an_openidconnect_relying_party_signs_in_refreshes_revokes_and_logs_out()
-> sc_error::Result<()> {
    let TestServer {
        db: _db, catalog, ..
    } = TestServer::builder().build().await?;
    catalog.set_secret_key(SecretKey::generate()?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    bootstrap_table_meta(&catalog).await?;
    sc_auth::save_role(&catalog, &sc_auth::RoleDef::new(Role::of(40), "Staff")).await?;
    let staff = create_user(&catalog, EMAIL, PASSWORD, Role::of(40)).await?;

    let framework = FrameworkRef::new("none")
        .with("store", "site")
        .with("source", "staff");
    let app = sc_app::save_application(
        &catalog,
        &Application::new("Staff", "staff", framework)
            .with_api(ApiConfig::new("rest", "/api"))
            .identity_provider(true),
    )
    .await?;
    let registered = sc_auth::register_oauth_client(
        &catalog,
        NewOAuthClient {
            app_id: app.id.0,
            name: "Relying party".into(),
            confidential: true,
            redirect_uris: vec![REDIRECT.into()],
            scopes: vec![],
            skip_consent: false,
            created_by: None,
            claim_fields: vec![],
            post_logout_redirect_uris: vec![LOGGED_OUT.into()],
        },
    )
    .await?;
    let apps = Arc::new(AppMounts::new(catalog.clone()).with_evaluator(default_js_evaluator()));
    let loaded = sc_app::load_application(&catalog, app.id).await?.unwrap();
    apps.mount(MountedApp::new_with(
        loaded,
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html>"),
        )),
        &catalog,
        apps.evaluator(),
        apps.triggers(),
    )?)?;
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig {
            base_domain: Some(BASE_DOMAIN.to_owned()),
            ..ServerConfig::default()
        },
        apps,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    let issuer = format!("http://{IDP}:{}", addr.port());

    // The relying party's HTTP client, as `openidconnect` takes one.
    let http = http_client(addr);
    let rp_http = |request: openidconnect::HttpRequest| {
        let http = http.clone();
        async move {
            let (parts, body) = request.into_parts();
            // The library insists on an `https` revocation endpoint (RFC
            // 7009 §2: a client MUST check that it is), and this test serves
            // plain http: so that one address is given as `https`, and sent
            // as http here, as a TLS-terminating proxy in front would.
            let uri = parts.uri.to_string().replacen("https://", "http://", 1);
            let response = http
                .request(parts.method, uri)
                .headers(parts.headers)
                .body(body)
                .send()
                .await?;
            let mut out = axum::http::Response::builder().status(response.status());
            for (k, v) in response.headers() {
                out = out.header(k, v);
            }
            Ok::<_, reqwest::Error>(out.body(response.bytes().await?.to_vec()).unwrap())
        }
    };

    // Discovery.
    let metadata = ProviderMetadataWithLogout::discover_async(
        IssuerUrl::new(issuer.clone()).unwrap(),
        &rp_http,
    )
    .await
    .expect("the discovery document and the JWKS are what a library expects");
    let end_session = metadata
        .additional_metadata()
        .end_session_endpoint
        .clone()
        .expect("an end_session_endpoint");
    let raw_doc: Value = http
        .get(format!("{issuer}/.well-known/openid-configuration"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(registered.client.client_id.clone()),
        Some(ClientSecret::new(registered.secret.clone().unwrap())),
    )
    .set_redirect_uri(RedirectUrl::new(REDIRECT.into()).unwrap())
    .set_revocation_url(
        RevocationUrl::new(
            raw_doc["revocation_endpoint"]
                .as_str()
                .unwrap()
                .replacen("http://", "https://", 1),
        )
        .unwrap(),
    );

    // Authorize, with PKCE and a nonce.
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .add_scope(Scope::new("profile".into()))
        .add_scope(Scope::new("offline_access".into()))
        .set_pkce_challenge(challenge)
        .url();

    // The browser: arrives from the relying party, signs in, consents.
    let mut browser = Browser {
        http: http_client(addr),
        origin: issuer.clone(),
        cookies: HashMap::new(),
    };
    let first = browser.get(auth_url.as_str()).await;
    let (sign_in, _) = browser.follow(first).await;
    assert!(
        sign_in.body.contains("Sign in to Staff"),
        "{}",
        sign_in.body
    );
    let signed = browser
        .submit(
            "/_feldspar/oidc/authorize",
            &sign_in,
            &[("email", EMAIL), ("password", PASSWORD)],
        )
        .await;
    let (consent, _) = browser.follow(signed).await;
    assert!(
        consent.body.contains("Allow Relying party"),
        "{}",
        consent.body
    );
    let allowed = browser
        .submit(
            "/_feldspar/oidc/authorize",
            &consent,
            &[("decision", "allow")],
        )
        .await;
    let (_, back) = browser.follow(allowed).await;
    let back = back.expect("sent back to the relying party");
    assert!(back.starts_with(REDIRECT), "{back}");
    let answer = query_of(&back);
    assert_eq!(&answer["state"], state.secret(), "state comes back");
    assert_eq!(answer["iss"], issuer, "RFC 9207's iss");

    // The code exchange, and the id_token checked as a library checks it.
    let tokens = client
        .exchange_code(AuthorizationCode::new(answer["code"].clone()))
        .unwrap()
        .set_pkce_verifier(verifier)
        .request_async(&rp_http)
        .await
        .expect("the token response is what a library expects");
    let id_token = tokens.id_token().expect("an id_token");
    let verifier = client.id_token_verifier();
    let claims = id_token
        .claims(&verifier, &nonce)
        .expect("signature, issuer, audience, expiry and nonce check out");
    assert_eq!(claims.subject().as_str(), staff.id().to_string());
    assert_eq!(claims.email().map(|e| e.as_str()), Some(EMAIL));
    let expected = claims.access_token_hash().expect("an at_hash");
    let actual = AccessTokenHash::from_token(
        tokens.access_token(),
        id_token.signing_alg().unwrap(),
        id_token.signing_key(&verifier).unwrap(),
    )
    .unwrap();
    assert_eq!(&actual, expected);

    // Userinfo.
    let info: CoreUserInfoClaims = client
        .user_info(
            tokens.access_token().clone(),
            Some(claims.subject().clone()),
        )
        .unwrap()
        .request_async(&rp_http)
        .await
        .expect("userinfo is what a library expects");
    assert_eq!(info.email().map(|e| e.as_str()), Some(EMAIL));
    let _: &UserInfoClaims<EmptyAdditionalClaims, CoreGenderClaim> = &info;

    // A refresh: a new pair, and a new id_token that still checks.
    let refresh = tokens.refresh_token().expect("a refresh token").clone();
    let refreshed = client
        .exchange_refresh_token(&refresh)
        .unwrap()
        .request_async(&rp_http)
        .await
        .expect("the refresh is what a library expects");
    assert_ne!(
        refreshed.refresh_token().unwrap().secret(),
        refresh.secret(),
        "rotated"
    );
    let refreshed_id = refreshed.id_token().expect("an id_token on refresh");
    // A refreshed id_token carries no nonce (OIDC Core §12.2).
    refreshed_id
        .claims(&verifier, |n: Option<&Nonce>| {
            n.is_none()
                .then_some(())
                .ok_or_else(|| "a nonce on a refresh".to_owned())
        })
        .expect("the refreshed id_token checks");

    // Revocation: the refresh token, and with it everything from the sign-in.
    client
        .revoke_token(CoreRevocableToken::from(
            refreshed.refresh_token().unwrap().clone(),
        ))
        .unwrap()
        .request_async(&rp_http)
        .await
        .expect("the revocation is what a library expects");
    let after: Result<CoreUserInfoClaims, _> = client
        .user_info(refreshed.access_token().clone(), None)
        .unwrap()
        .request_async(&rp_http)
        .await;
    assert!(
        after.is_err(),
        "the access token is revoked with its refresh token"
    );
    let again = client
        .exchange_refresh_token(refreshed.refresh_token().unwrap())
        .unwrap()
        .request_async(&rp_http)
        .await;
    assert!(again.is_err(), "and the refresh token is gone");

    // RP-initiated logout, with the id_token as the hint.
    let logout = LogoutRequest::from(end_session)
        .set_id_token_hint(id_token)
        .set_post_logout_redirect_uri(PostLogoutRedirectUrl::new(LOGGED_OUT.into()).unwrap())
        .set_state(CsrfToken::new("bye".into()))
        .http_get_url();
    let out = browser.get(logout.as_str()).await;
    assert_eq!(out.status, 303, "{}", out.body);
    assert_eq!(
        out.location.as_deref(),
        Some(format!("{LOGGED_OUT}?state=bye").as_str())
    );
    assert!(
        !browser.cookies.contains_key(sc_server::SESSION_COOKIE),
        "signed out"
    );
    Ok(())
}

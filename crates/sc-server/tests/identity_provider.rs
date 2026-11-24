//! An application as an OpenID Connect identity provider (TODO IP1.3–IP1.5),
//! through the assembled router: a browser on the application's host, and a
//! relying party's server at the token endpoint.
//!
//! - Discovery and the JWKS answer on an application that turned the provider
//!   on, and are 404 on one that did not and on the admin host.
//! - `/authorize` checks the client and its exact redirect URI first (an error
//!   page, never a redirect), then everything else (an error back to the
//!   client); signs people in through the one session path, refusing a
//!   disabled account and one the application's access settings refuse; asks
//!   for consent with a CSRF-checked form; and answers `prompt=none`.
//! - `/token` ignores cookies, authenticates the client, checks PKCE, answers
//!   each RFC 6749 §5.2 error, and issues an `id_token` that verifies against
//!   the JWKS. A code used twice and a refresh token used twice revoke what
//!   they were exchanged for.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{NewOAuthClient, RoleDef, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, SecretKey, bootstrap_table_meta};
use sc_server::{
    AppMounts, MountedApp, SESSION_COOKIE, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator,
};
use sc_test_harness::{TestDb, TestServer};
use sc_types::Role;
use serde_json::Value;
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
/// The identity provider: `min_role` 40.
const IDP_HOST: &str = "staff.example.com";
const ISSUER: &str = "http://staff.example.com";
/// An application that has not turned the provider on.
const PLAIN_HOST: &str = "plain.example.com";
const REDIRECT: &str = "https://grafana.example.net/login/generic_oauth";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

const STAFF: &str = "staff@example.com";
const MEMBER: &str = "member@example.com";
const DISABLED: &str = "gone@example.com";
const PASSWORD: &str = "correct-horse";
const ROLE_STAFF: Role = Role::of(40);
const ROLE_MEMBER: Role = Role::of(80);

struct Server {
    router: Router,
    catalog: Arc<Catalog>,
    client_id: String,
    secret: String,
    other_client_id: String,
    other_secret: String,
    _db: TestDb,
}

/// One answer: status, headers by lower-case name, body as text.
struct Answer {
    status: StatusCode,
    headers: HashMap<String, String>,
    set_cookies: Vec<String>,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn cookie(&self, name: &str) -> Option<String> {
        self.set_cookies.iter().find_map(|raw| {
            let (k, v) = raw.split(';').next()?.split_once('=')?;
            (k == name && !v.is_empty()).then(|| v.to_owned())
        })
    }

    /// Where a "Continuing…" page goes on to.
    fn continues_to(&self) -> String {
        let start = self.body.find("url=").expect("a page that goes on") + 4;
        let end = start + self.body[start..].find('"').unwrap();
        self.body[start..end].replace("&amp;", "&")
    }
}

async fn send(
    router: &Router,
    host: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<String>,
) -> Answer {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host);
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    let request = match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let set_cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    Answer {
        status,
        headers,
        set_cookies,
        body: String::from_utf8_lossy(&bytes).into_owned(),
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

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The query of a URL, decoded.
fn query_of(url: &str) -> HashMap<String, String> {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (decode_component(k), decode_component(v)))
        .collect()
}

fn decode_component(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

async fn setup() -> sc_error::Result<Server> {
    let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
    catalog.set_secret_key(SecretKey::generate()?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    bootstrap_table_meta(&catalog).await?;
    save_role(&catalog, &RoleDef::new(ROLE_STAFF, "Staff")).await?;
    save_role(&catalog, &RoleDef::new(ROLE_MEMBER, "Member")).await?;
    create_user(&catalog, STAFF, PASSWORD, ROLE_STAFF).await?;
    create_user(&catalog, MEMBER, PASSWORD, ROLE_MEMBER).await?;
    let gone = create_user(&catalog, DISABLED, PASSWORD, ROLE_STAFF).await?;
    let _disabled = sc_auth::set_user_disabled(&catalog, gone.id(), true).await?;

    let app = |name: &str, subdomain: &str| {
        let framework = FrameworkRef::new("none")
            .with("store", "site")
            .with("source", subdomain);
        Application::new(name, subdomain, framework).with_api(ApiConfig::new("rest", "/api"))
    };
    let staff = sc_app::save_application(
        &catalog,
        &app("Staff", "staff")
            .min_role(ROLE_STAFF)
            .identity_provider(true),
    )
    .await?;
    let plain = sc_app::save_application(&catalog, &app("Plain", "plain")).await?;
    let register = |name: &str| NewOAuthClient {
        app_id: staff.id.0,
        name: name.to_owned(),
        confidential: true,
        redirect_uris: vec![REDIRECT.to_owned()],
        scopes: vec![],
        skip_consent: false,
        created_by: None,
    };
    let client = sc_auth::register_oauth_client(&catalog, register("Grafana")).await?;
    let other = sc_auth::register_oauth_client(&catalog, register("Wiki")).await?;

    let apps = Arc::new(AppMounts::new(catalog.clone()).with_evaluator(default_js_evaluator()));
    for stored in [staff, plain] {
        let loaded = sc_app::load_application(&catalog, stored.id)
            .await?
            .unwrap();
        let framework = Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html>"),
        ));
        apps.mount(MountedApp::new_with(
            loaded,
            framework,
            &catalog,
            apps.evaluator(),
            apps.triggers(),
        )?)?;
    }
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps,
    )?;
    Ok(Server {
        router,
        catalog,
        client_id: client.client.client_id,
        secret: client.secret.unwrap(),
        other_client_id: other.client.client_id,
        other_secret: other.secret.unwrap(),
        _db: db,
    })
}

/// The authorization request's parameters for `client_id`.
fn authorize_params<'a>(
    client_id: &'a str,
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut params = vec![
        ("client_id", client_id),
        ("redirect_uri", REDIRECT),
        ("response_type", "code"),
        ("scope", "openid email profile"),
        ("state", "af0ifjsldkj"),
        ("nonce", "n-0S6_WzA2Mj"),
        ("code_challenge", CHALLENGE),
        ("code_challenge_method", "S256"),
    ];
    params.extend_from_slice(extra);
    params
}

fn authorize_path(params: &[(&str, &str)]) -> String {
    format!("/_feldspar/oidc/authorize?{}", form(params))
}

/// A browser on the provider's host.
struct Browser<'a> {
    server: &'a Server,
    cookies: HashMap<String, String>,
}

impl Browser<'_> {
    fn new(server: &Server) -> Browser<'_> {
        Browser {
            server,
            cookies: HashMap::new(),
        }
    }

    async fn request(&mut self, method: &str, path: &str, body: Option<String>) -> Answer {
        let mut headers = Vec::new();
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            headers.push(("cookie", jar));
        }
        let answer = send(&self.server.router, IDP_HOST, method, path, &headers, body).await;
        for raw in &answer.set_cookies {
            if let Some((k, v)) = raw.split(';').next().and_then(|p| p.split_once('=')) {
                if v.is_empty() {
                    self.cookies.remove(k);
                } else {
                    self.cookies.insert(k.to_owned(), v.to_owned());
                }
            }
        }
        answer
    }

    /// Post a page's form: its hidden fields, as the page wrote them, and
    /// `extra`.
    async fn submit(&mut self, page: &Answer, extra: &[(&str, &str)]) -> Answer {
        let mut fields: Vec<(String, String)> = Vec::new();
        for input in page.body.split("<input type=\"hidden\" ").skip(1) {
            let attr = |name: &str| {
                let start = input.find(&format!("{name}=\"")).unwrap() + name.len() + 2;
                let end = start + input[start..].find('"').unwrap();
                input[start..end]
                    .replace("&amp;", "&")
                    .replace("&quot;", "\"")
            };
            fields.push((attr("name"), attr("value")));
        }
        for (k, v) in extra {
            fields.push(((*k).to_owned(), (*v).to_owned()));
        }
        let pairs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        self.request("POST", "/_feldspar/oidc/authorize", Some(form(&pairs)))
            .await
    }

    /// Start an authorization request, arriving from another site: the
    /// first answer is the page that goes round once; this follows it.
    async fn authorize(&mut self, params: &[(&str, &str)]) -> Answer {
        let first = self.request("GET", &authorize_path(params), None).await;
        if first.status == StatusCode::OK && first.body.contains("Continuing") {
            let next = first.continues_to();
            return self.request("GET", &next, None).await;
        }
        first
    }

    /// Sign `email` in on the sign-in page `page`, and follow on.
    async fn sign_in(&mut self, page: &Answer, email: &str) -> Answer {
        assert!(page.body.contains("Sign in to Staff"), "{}", page.body);
        let signed = self
            .submit(page, &[("email", email), ("password", PASSWORD)])
            .await;
        if signed.status == StatusCode::OK && signed.body.contains("Continuing") {
            let next = signed.continues_to();
            if next.starts_with('/') {
                return self.request("GET", &next, None).await;
            }
        }
        signed
    }

    /// Sign in, allow, and return the code the client was sent.
    async fn code_for(&mut self, client_id: &str) -> String {
        let params = authorize_params(client_id, &[]);
        let page = self.authorize(&params).await;
        let consent = if page.body.contains("Sign in to Staff") {
            self.sign_in(&page, STAFF).await
        } else {
            page
        };
        let target = if consent.body.contains("Allow") {
            self.submit(&consent, &[("decision", "allow")])
                .await
                .continues_to()
        } else {
            consent.header("location").unwrap().to_owned()
        };
        assert!(target.starts_with(REDIRECT), "{target}");
        let query = query_of(&target);
        assert_eq!(query["state"], "af0ifjsldkj");
        assert_eq!(query["iss"], ISSUER);
        query
            .get("code")
            .cloned()
            .unwrap_or_else(|| panic!("no code in {target}"))
    }
}

/// A token request with `client_secret_basic`.
async fn token(server: &Server, client: (&str, &str), pairs: &[(&str, &str)]) -> Answer {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!(
        "{}:{}",
        encode(client.0),
        encode(client.1)
    ));
    send(
        &server.router,
        IDP_HOST,
        "POST",
        "/_feldspar/oidc/token",
        &[("authorization", format!("Basic {basic}"))],
        Some(form(pairs)),
    )
    .await
}

async fn exchange(server: &Server, code: &str) -> Answer {
    token(
        server,
        (&server.client_id, &server.secret),
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
        ],
    )
    .await
}

#[tokio::test]
async fn discovery_and_keys_answer_only_on_an_application_that_is_a_provider()
-> sc_error::Result<()> {
    let server = setup().await?;
    for path in [
        "/.well-known/openid-configuration",
        "/.well-known/oauth-authorization-server",
    ] {
        let doc = send(&server.router, IDP_HOST, "GET", path, &[], None).await;
        assert_eq!(doc.status, StatusCode::OK, "{}", doc.body);
        let doc = doc.json();
        assert_eq!(doc["issuer"], ISSUER);
        assert_eq!(
            doc["authorization_endpoint"],
            format!("{ISSUER}/_feldspar/oidc/authorize")
        );
        assert_eq!(
            doc["token_endpoint"],
            format!("{ISSUER}/_feldspar/oidc/token")
        );
        assert_eq!(doc["jwks_uri"], format!("{ISSUER}/_feldspar/oidc/jwks"));
        assert_eq!(
            doc["code_challenge_methods_supported"],
            serde_json::json!(["S256"])
        );
        assert_eq!(
            doc["id_token_signing_alg_values_supported"],
            serde_json::json!(["RS256", "ES256"])
        );
    }
    let jwks = send(
        &server.router,
        IDP_HOST,
        "GET",
        "/_feldspar/oidc/jwks",
        &[],
        None,
    )
    .await;
    assert_eq!(jwks.status, StatusCode::OK);
    let keys = jwks.json()["keys"].as_array().unwrap().clone();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|k| k.get("d").is_none()), "no private half");

    for host in [PLAIN_HOST, BASE_DOMAIN] {
        for path in [
            "/.well-known/openid-configuration",
            "/.well-known/oauth-authorization-server",
            "/_feldspar/oidc/jwks",
            &authorize_path(&authorize_params(&server.client_id, &[])),
        ] {
            let answer = send(&server.router, host, "GET", path, &[], None).await;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{host}{path}");
        }
        let elsewhere = send(
            &server.router,
            host,
            "POST",
            "/_feldspar/oidc/token",
            &[],
            Some(String::new()),
        )
        .await;
        assert_eq!(elsewhere.status, StatusCode::NOT_FOUND, "{host}");
    }
    Ok(())
}

#[tokio::test]
async fn a_bad_client_or_redirect_uri_is_an_error_page_and_never_a_redirect() -> sc_error::Result<()>
{
    let server = setup().await?;
    let mut browser = Browser::new(&server);
    for (client, uri) in [
        (server.client_id.as_str(), "https://evil.example/steal"),
        (
            server.client_id.as_str(),
            "https://grafana.example.net/login/other",
        ),
        (
            server.client_id.as_str(),
            "https://grafana.example.net/login/generic_oauth/",
        ),
        ("fdc_nobody", REDIRECT),
    ] {
        let mut params = authorize_params(client, &[]);
        params.retain(|(k, _)| *k != "redirect_uri");
        params.push(("redirect_uri", uri));
        let answer = browser.authorize(&params).await;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{uri}: {}",
            answer.body
        );
        assert!(answer.header("location").is_none(), "{uri}");
        assert!(
            !answer.body.contains("evil.example"),
            "the page does not link to it"
        );
    }

    // A good client and URI, and a bad request: the error goes back to it.
    let mut params = authorize_params(&server.client_id, &[]);
    params.retain(|(k, _)| !k.starts_with("code_challenge"));
    let answer = browser.authorize(&params).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    let back = query_of(answer.header("location").unwrap());
    assert_eq!(back["error"], "invalid_request");
    assert_eq!(back["state"], "af0ifjsldkj");

    let plain = authorize_params(&server.client_id, &[("code_challenge_method", "plain")]);
    let plain: Vec<_> = plain
        .into_iter()
        .filter(|(k, v)| *k != "code_challenge_method" || *v == "plain")
        .collect();
    let answer = browser.authorize(&plain).await;
    assert_eq!(
        query_of(answer.header("location").unwrap())["error"],
        "invalid_request"
    );

    let mut wider = authorize_params(&server.client_id, &[]);
    wider.retain(|(k, _)| *k != "scope");
    wider.push(("scope", "openid admin"));
    let answer = browser.authorize(&wider).await;
    assert_eq!(
        query_of(answer.header("location").unwrap())["error"],
        "invalid_scope"
    );
    Ok(())
}

#[tokio::test]
async fn prompt_none_with_nobody_signed_in_is_login_required() -> sc_error::Result<()> {
    let server = setup().await?;
    let mut browser = Browser::new(&server);
    let answer = browser
        .authorize(&authorize_params(&server.client_id, &[("prompt", "none")]))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    let target = answer.header("location").unwrap();
    assert!(target.starts_with(REDIRECT));
    assert_eq!(query_of(target)["error"], "login_required");

    // Signed in and consented, `prompt=none` gets the code with no page.
    let _ = browser.code_for(&server.client_id).await;
    let answer = browser
        .authorize(&authorize_params(&server.client_id, &[("prompt", "none")]))
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert!(query_of(answer.header("location").unwrap()).contains_key("code"));
    // …and a client nobody consented to is `consent_required`.
    let answer = browser
        .authorize(&authorize_params(
            &server.other_client_id,
            &[("prompt", "none")],
        ))
        .await;
    assert_eq!(
        query_of(answer.header("location").unwrap())["error"],
        "consent_required"
    );
    Ok(())
}

#[tokio::test]
async fn sign_in_refuses_a_disabled_account_and_one_the_application_does_not_admit()
-> sc_error::Result<()> {
    let server = setup().await?;

    let mut browser = Browser::new(&server);
    let page = browser
        .authorize(&authorize_params(&server.client_id, &[]))
        .await;
    let refused = browser.sign_in(&page, DISABLED).await;
    assert_eq!(refused.status, StatusCode::OK);
    assert!(refused.body.contains("is wrong"), "{}", refused.body);
    assert!(
        refused.cookie(SESSION_COOKIE).is_none(),
        "no session for it"
    );

    // Role 80 on an application whose `min_role` is 40.
    let mut browser = Browser::new(&server);
    let page = browser
        .authorize(&authorize_params(&server.client_id, &[]))
        .await;
    let refused = browser.sign_in(&page, MEMBER).await;
    let target = refused.continues_to();
    assert!(target.starts_with(REDIRECT), "{target}");
    assert_eq!(query_of(&target)["error"], "access_denied");
    assert!(
        refused.cookie(SESSION_COOKIE).is_none(),
        "no session for it"
    );
    Ok(())
}

#[tokio::test]
async fn consent_is_a_csrf_checked_form_and_a_denial_goes_back_to_the_client()
-> sc_error::Result<()> {
    let server = setup().await?;
    let mut browser = Browser::new(&server);
    let page = browser
        .authorize(&authorize_params(&server.client_id, &[]))
        .await;
    let consent = browser.sign_in(&page, STAFF).await;
    assert!(consent.body.contains("Allow Grafana"), "{}", consent.body);
    assert!(consent.body.contains("see your email address"));

    // Posted cross-site: the session cookie and no token. (The browser would
    // not even send a `Strict` cookie; this is the check behind that.)
    let session = browser.cookies[SESSION_COOKIE].clone();
    let mut fields = form(&authorize_params(&server.client_id, &[]));
    fields.push_str("&action=consent&decision=allow");
    let forged = send(
        &server.router,
        IDP_HOST,
        "POST",
        "/_feldspar/oidc/authorize",
        &[("cookie", format!("{SESSION_COOKIE}={session}"))],
        Some(fields),
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN, "{}", forged.body);

    let denied = browser.submit(&consent, &[("decision", "deny")]).await;
    let target = denied.continues_to();
    assert_eq!(query_of(&target)["error"], "access_denied");

    // Once allowed, the next sign-in asks nothing.
    let _ = browser.code_for(&server.client_id).await;
    let again = browser
        .authorize(&authorize_params(&server.client_id, &[]))
        .await;
    assert_eq!(again.status, StatusCode::SEE_OTHER, "{}", again.body);
    // `prompt=consent` asks again anyway.
    let asked = browser
        .authorize(&authorize_params(
            &server.client_id,
            &[("prompt", "consent")],
        ))
        .await;
    assert!(asked.body.contains("Allow Grafana"));
    // `prompt=login` signs in again however recent the session.
    let login = browser
        .authorize(&authorize_params(&server.client_id, &[("prompt", "login")]))
        .await;
    assert!(login.body.contains("Sign in to Staff"));
    Ok(())
}

/// Verify an `id_token` against the provider's JWKS, as a relying party does.
async fn verify_id_token(server: &Server, id_token: &str) -> Value {
    let jwks = send(
        &server.router,
        IDP_HOST,
        "GET",
        "/_feldspar/oidc/jwks",
        &[],
        None,
    )
    .await;
    let jwks: JwkSet = serde_json::from_str(&jwks.body).unwrap();
    let header = decode_header(id_token).unwrap();
    assert_eq!(format!("{:?}", header.alg), "RS256");
    let jwk = jwks.find(header.kid.as_deref().unwrap()).unwrap();
    let mut validation = Validation::new(header.alg);
    validation.set_audience(&[&server.client_id]);
    validation.set_issuer(&[ISSUER]);
    decode::<Value>(id_token, &DecodingKey::from_jwk(jwk).unwrap(), &validation)
        .unwrap()
        .claims
}

#[tokio::test]
async fn the_token_endpoint_issues_a_verifiable_id_token_and_ignores_cookies()
-> sc_error::Result<()> {
    let server = setup().await?;
    let mut browser = Browser::new(&server);
    let code = browser.code_for(&server.client_id).await;

    // A session cookie and no CSRF token: the route ignores the cookie, so
    // the CSRF check has nothing to defend and the client's credentials decide.
    let session = browser.cookies[SESSION_COOKIE].clone();
    let basic = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", server.client_id, server.secret));
    let answer = send(
        &server.router,
        IDP_HOST,
        "POST",
        "/_feldspar/oidc/token",
        &[
            ("authorization", format!("Basic {basic}")),
            ("cookie", format!("{SESSION_COOKIE}={session}")),
        ],
        Some(form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
        ])),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.header("cache-control"), Some("no-store"));
    assert!(
        answer.set_cookies.is_empty(),
        "no cookie is set on the token endpoint"
    );
    let tokens = answer.json();
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 600);
    assert!(
        tokens["access_token"]
            .as_str()
            .unwrap()
            .starts_with("fdat_")
    );
    assert!(
        tokens["refresh_token"]
            .as_str()
            .unwrap()
            .starts_with("fdrt_")
    );

    let claims = verify_id_token(&server, tokens["id_token"].as_str().unwrap()).await;
    let staff = sc_auth::load_user_by_email(&server.catalog, STAFF)
        .await?
        .unwrap();
    assert_eq!(
        claims["sub"],
        staff.id().to_string(),
        "the user's id, not the email"
    );
    assert_eq!(claims["nonce"], "n-0S6_WzA2Mj");
    assert_eq!(claims["email"], STAFF);
    assert_eq!(claims["email_verified"], true);
    assert!(claims["auth_time"].as_i64().is_some());
    assert_eq!(
        claims["at_hash"],
        sc_auth::left_half_sha256(tokens["access_token"].as_str().unwrap())
    );

    // `client_secret_post` works too, for a fresh code.
    let code = browser.code_for(&server.client_id).await;
    let posted = send(
        &server.router,
        IDP_HOST,
        "POST",
        "/_feldspar/oidc/token",
        &[],
        Some(form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
            ("client_id", &server.client_id),
            ("client_secret", &server.secret),
        ])),
    )
    .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    Ok(())
}

#[tokio::test]
async fn the_token_endpoint_answers_each_rfc_6749_error() -> sc_error::Result<()> {
    let server = setup().await?;
    let mine = (server.client_id.as_str(), server.secret.as_str());
    let error = |answer: &Answer| answer.json()["error"].as_str().unwrap_or("").to_owned();

    // invalid_client: a wrong secret, with the Basic challenge.
    let answer = token(
        &server,
        (mine.0, "fdcs_wrong"),
        &[("grant_type", "authorization_code")],
    )
    .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(error(&answer), "invalid_client");
    assert!(answer.header("www-authenticate").is_some());

    // invalid_request: no grant type; a parameter twice; no verifier.
    let answer = token(&server, mine, &[]).await;
    assert_eq!(
        (answer.status, error(&answer).as_str()),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    let answer = token(
        &server,
        mine,
        &[
            ("grant_type", "refresh_token"),
            ("grant_type", "refresh_token"),
        ],
    )
    .await;
    assert_eq!(error(&answer), "invalid_request");
    let mut browser = Browser::new(&server);
    let code = browser.code_for(&server.client_id).await;
    let answer = token(
        &server,
        mine,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
        ],
    )
    .await;
    assert_eq!(error(&answer), "invalid_request");

    // unsupported_grant_type.
    let answer = token(&server, mine, &[("grant_type", "password")]).await;
    assert_eq!(error(&answer), "unsupported_grant_type");
    let answer = token(&server, mine, &[("grant_type", "client_credentials")]).await;
    assert_eq!(error(&answer), "unsupported_grant_type");

    // invalid_grant: a wrong verifier spends the code.
    let code = browser.code_for(&server.client_id).await;
    let answer = token(
        &server,
        mine,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", "x".repeat(43).as_str()),
        ],
    )
    .await;
    assert_eq!(error(&answer), "invalid_grant");
    assert_eq!(error(&exchange(&server, &code).await), "invalid_grant");

    // invalid_grant: another client's code (left for its own client).
    let theirs = browser.code_for(&server.other_client_id).await;
    let answer = exchange(&server, &theirs).await;
    assert_eq!(error(&answer), "invalid_grant");
    let answer = token(
        &server,
        (&server.other_client_id, &server.other_secret),
        &[
            ("grant_type", "authorization_code"),
            ("code", &theirs),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
        ],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);

    // invalid_grant: an expired code.
    let code = browser.code_for(&server.client_id).await;
    server
        .catalog
        .primary(sc_catalog::Unchecked::system("test: age a code"))
        .query(&sc_query::Statement::from(
            sc_query::Update::new(
                sc_auth::OAUTH_CODES_TABLE,
                vec![sc_query::Assignment::new(
                    "expires_at".to_owned(),
                    sc_query::Expr::Lit(sc_query::Value::Timestamp(
                        chrono::Utc::now() - chrono::Duration::seconds(1),
                    )),
                )],
            )
            .filter(
                sc_query::Expr::col("hash")
                    .eq(sc_query::Expr::lit(sc_auth::bearer_secret_hash(&code))),
            ),
        ))
        .await?
        .try_collect()
        .await?;
    let answer = exchange(&server, &code).await;
    assert_eq!(error(&answer), "invalid_grant");
    assert!(
        answer.json()["error_description"]
            .as_str()
            .unwrap()
            .contains("expired")
    );

    // invalid_grant: a code used twice, which revokes what the first use got.
    let code = browser.code_for(&server.client_id).await;
    let first = exchange(&server, &code).await.json();
    let access = first["access_token"].as_str().unwrap().to_owned();
    let app = sc_app::load_application_by_subdomain(&server.catalog, "staff")
        .await?
        .unwrap();
    assert!(
        sc_auth::authenticate_oauth_access_token(&server.catalog, app.id.0, &access)
            .await?
            .is_some()
    );
    assert_eq!(error(&exchange(&server, &code).await), "invalid_grant");
    assert!(
        sc_auth::authenticate_oauth_access_token(&server.catalog, app.id.0, &access)
            .await?
            .is_none(),
        "the reused code's tokens are revoked"
    );

    // invalid_scope: a refresh asking for more than was granted.
    let code = browser.code_for(&server.client_id).await;
    let tokens = exchange(&server, &code).await.json();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_owned();
    let answer = token(
        &server,
        mine,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("scope", "openid offline_access admin"),
        ],
    )
    .await;
    assert_eq!(error(&answer), "invalid_scope", "{}", answer.body);
    Ok(())
}

#[tokio::test]
async fn a_refresh_token_rotates_and_a_reused_one_revokes_the_sign_in() -> sc_error::Result<()> {
    let server = setup().await?;
    let mine = (server.client_id.as_str(), server.secret.as_str());
    let mut browser = Browser::new(&server);
    let code = browser.code_for(&server.client_id).await;
    let first = exchange(&server, &code).await.json();
    let refresh = |token: String| {
        let server = &server;
        async move { token_answer(server, mine, &token).await }
    };
    let second = refresh(first["refresh_token"].as_str().unwrap().to_owned()).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    let second = second.json();
    assert_ne!(second["refresh_token"], first["refresh_token"]);
    // A refreshed id_token verifies, and carries no nonce.
    let claims = verify_id_token(&server, second["id_token"].as_str().unwrap()).await;
    assert!(claims.get("nonce").is_none());

    let app = sc_app::load_application_by_subdomain(&server.catalog, "staff")
        .await?
        .unwrap();
    let live = |token: &Value| {
        let catalog = server.catalog.clone();
        let token = token.as_str().unwrap().to_owned();
        async move {
            sc_auth::authenticate_oauth_access_token(&catalog, app.id.0, &token)
                .await
                .unwrap()
                .is_some()
        }
    };
    assert!(live(&second["access_token"]).await);

    // The first refresh token again: refused, and the whole sign-in ends.
    let reused = refresh(first["refresh_token"].as_str().unwrap().to_owned()).await;
    assert_eq!(reused.json()["error"], "invalid_grant");
    assert!(!live(&second["access_token"]).await);
    let after = refresh(second["refresh_token"].as_str().unwrap().to_owned()).await;
    assert_eq!(after.json()["error"], "invalid_grant");
    Ok(())
}

async fn token_answer(server: &Server, client: (&str, &str), refresh: &str) -> Answer {
    token(
        server,
        client,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
    )
    .await
}

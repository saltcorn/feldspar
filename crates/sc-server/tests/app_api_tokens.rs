//! An application's users calling its API with an API token (TODO AT1), through
//! the assembled router against a real database.
//!
//! - A token runs as **its user**: REST and GraphQL see that user's rows under
//!   the table's ownership formula, and a write needs no CSRF token.
//! - A cookie beside a bearer header lends it nothing.
//! - A token is refused — a `401` saying why, never the public — when the
//!   application does not take tokens, when it is another application's or an
//!   administration token, and when it is revoked or its user disabled.
//! - The application's own admission applies to a token's user as to a session's.
//! - A bearer token that is not one of ours (a proxy's) is left alone, and the
//!   request is served by its cookie as before.

//! - A user mints, lists and revokes their own at `/_feldspar/api-tokens`
//!   with a session the application issued — not a visiting one, not a token,
//!   not where the application does not take them or their role may not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_types::Role;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation, TableSettings};
use sc_app::{
    API_TOKEN_MIN_ROLE_ATTRIBUTE, API_TOKENS_ATTRIBUTE, ApiConfig, Application, AssetBundle,
    CodeFramework, FrameworkRef,
};
use sc_auth::{
    NewApiToken, NewAppApiToken, RoleDef, SessionStore, User, create_user, mint_api_token,
    mint_app_api_token, save_role,
};
use sc_catalog::Catalog;
use sc_server::{
    AppMounts, MountedApp, SESSION_COOKIE, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator,
};
use sc_test_harness::{TestDb, TestServer};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const BASE_DOMAIN: &str = "example.com";
/// Takes tokens.
const NOTES_HOST: &str = "notes.example.com";
/// Takes tokens too: a second application, for "another application's".
const OTHER_HOST: &str = "other.example.com";
/// Does not take tokens.
const CLOSED_HOST: &str = "closed.example.com";
/// Takes tokens, and admits staff only.
const STAFF_HOST: &str = "staff.example.com";

const ADMIN: &str = "admin@example.com";
const ANN: &str = "ann@example.com";
const BOB: &str = "bob@example.com";
const PASSWORD: &str = "correct-horse";
const ROLE_STAFF: Role = Role::of(40);
const ROLE_MEMBER: Role = Role::of(80);

/// What one request answered.
pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Value,
}

/// One request to `host`, with exactly the headers given.
pub(crate) async fn send(
    router: &Router,
    host: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Answer {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let request = match body {
        Some(b) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&b).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Answer {
        status,
        headers,
        body,
    }
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn field(name: &str, type_name: &str) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        type_name: type_name.into(),
        ..FieldSpec::default()
    }
}

/// `note`: each row its owner's by `owner_email`, nobody else's (admin-only
/// floors, so only the formula lets a user in).
async fn create_notes(catalog: &Catalog) -> sc_error::Result<()> {
    schema_edit::apply(
        catalog,
        &[Operation::CreateTable {
            name: "note".into(),
            database: String::new(),
            settings: TableSettings {
                min_role_read: Some(Role::ADMIN),
                min_role_write: Some(Role::ADMIN),
                ownership_formula: Some("owner_email === user.email".into()),
                ..TableSettings::default()
            },
            fields: vec![
                FieldSpec {
                    primary_key: true,
                    ..field("id", "int")
                },
                FieldSpec {
                    required: true,
                    ..field("body", "text")
                },
                field("owner_email", "text"),
            ],
        }],
        &ApplyOptions::default(),
    )
    .await?;
    Ok(())
}

fn app(name: &str, subdomain: &str, tokens: bool) -> Application {
    let framework = FrameworkRef::new("none")
        .with("store", "site")
        .with("source", subdomain);
    let mut app = Application::new(name, subdomain, framework)
        .with_table("note".to_owned())
        .with_api(ApiConfig::new("rest", "/api"))
        .with_api(ApiConfig::new("graphql", "/graphql"));
    if tokens {
        app.attributes
            .insert(API_TOKENS_ATTRIBUTE.to_owned(), Value::Bool(true));
    }
    app
}

pub(crate) struct Server {
    pub(crate) router: Router,
    pub(crate) sessions: Arc<SessionStore>,
    pub(crate) catalog: Arc<Catalog>,
    /// Application ids by subdomain.
    pub(crate) notes: Uuid,
    pub(crate) other: Uuid,
    pub(crate) staff: Uuid,
    _db: TestDb,
}

impl Server {
    pub(crate) async fn user(&self, email: &str) -> User {
        sc_auth::load_user_by_email(&self.catalog, email)
            .await
            .unwrap()
            .unwrap()
    }

    /// An application token for `email` on `application`, minted directly.
    pub(crate) async fn token(&self, application: Uuid, email: &str) -> (Uuid, String) {
        let user = self.user(email).await;
        let minted = mint_app_api_token(
            &self.catalog,
            NewAppApiToken {
                application_id: application,
                user_id: user.id(),
                label: format!("{email}'s script"),
                expires_at: None,
            },
        )
        .await
        .unwrap();
        (minted.token.id, minted.secret)
    }

    pub(crate) async fn call(
        &self,
        host: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: &str,
    ) -> Answer {
        let auth = bearer(token);
        send(
            &self.router,
            host,
            method,
            path,
            body,
            &[(header::AUTHORIZATION.as_str(), &auth)],
        )
        .await
    }
}

pub(crate) async fn setup() -> sc_error::Result<Server> {
    let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    create_notes(&catalog).await?;
    save_role(&catalog, &RoleDef::new(ROLE_STAFF, "Staff")).await?;
    save_role(&catalog, &RoleDef::new(ROLE_MEMBER, "Member")).await?;
    create_user(&catalog, ADMIN, PASSWORD, Role::ADMIN).await?;
    create_user(&catalog, ANN, PASSWORD, ROLE_MEMBER).await?;
    create_user(&catalog, BOB, PASSWORD, ROLE_STAFF).await?;
    db.client()
        .await?
        .batch_execute(&format!(
            "INSERT INTO note (id, body, owner_email) VALUES \
             (1, 'ann one', '{ANN}'), (2, 'ann two', '{ANN}'), (3, 'bob''s', '{BOB}');"
        ))
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let staff_app = app("Staff", "staff", true).min_role(ROLE_STAFF);
    let stored = [
        sc_app::save_application(&catalog, &app("Notes", "notes", true)).await?,
        sc_app::save_application(&catalog, &{
            // Tokens for staff and better only.
            let mut other = app("Other", "other", true);
            other.attributes.insert(
                API_TOKEN_MIN_ROLE_ATTRIBUTE.to_owned(),
                json!(ROLE_STAFF.get()),
            );
            other
        })
        .await?,
        sc_app::save_application(&catalog, &app("Closed", "closed", false)).await?,
        sc_app::save_application(&catalog, &staff_app).await?,
    ];

    let evaluator = default_js_evaluator();
    let apps = Arc::new(AppMounts::new(catalog.clone()).with_evaluator(evaluator));
    for stored in &stored {
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
    let sessions = Arc::new(SessionStore::default());
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        sessions.clone(),
        &config,
        apps,
    )?;
    Ok(Server {
        router,
        sessions,
        catalog,
        notes: stored[0].id.0,
        other: stored[1].id.0,
        staff: stored[3].id.0,
        _db: db,
    })
}

/// The bodies of the rows a list answered, sorted.
fn bodies(answer: &Answer) -> Vec<String> {
    let rows = answer
        .body
        .as_array()
        .or_else(|| answer.body.get("rows").and_then(Value::as_array))
        .unwrap_or_else(|| panic!("a list of rows: {}", answer.body));
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| r["body"].as_str().unwrap().to_owned())
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn a_token_calls_rest_and_graphql_as_its_user() -> sc_error::Result<()> {
    let server = setup().await?;
    let (_, ann) = server.token(server.notes, ANN).await;

    let me = server
        .call(NOTES_HOST, "GET", "/api/whoami", None, &ann)
        .await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.body["email"], ANN, "{}", me.body);

    // Her rows and nobody else's: the ownership formula applies to her.
    let listed = server
        .call(NOTES_HOST, "GET", "/api/note", None, &ann)
        .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(bodies(&listed), ["ann one", "ann two"]);

    let graphql = server
        .call(
            NOTES_HOST,
            "POST",
            "/graphql",
            Some(json!({ "query": "query { note { body } }" })),
            &ann,
        )
        .await;
    assert_eq!(graphql.status, StatusCode::OK, "{}", graphql.body);
    let mut seen: Vec<&str> = graphql.body["data"]["note"]
        .as_array()
        .unwrap_or_else(|| panic!("{}", graphql.body))
        .iter()
        .map(|r| r["body"].as_str().unwrap())
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, ["ann one", "ann two"]);

    // A write with no CSRF cookie or header: a bearer request needs neither.
    let created = server
        .call(
            NOTES_HOST,
            "POST",
            "/api/note",
            Some(json!({ "id": 10, "body": "from a script", "owner_email": ANN })),
            &ann,
        )
        .await;
    assert!(created.status.is_success(), "{}", created.body);
    // …and a write that her ownership does not cover is refused as for her.
    let forged = server
        .call(
            NOTES_HOST,
            "POST",
            "/api/note",
            Some(json!({ "id": 11, "body": "as bob", "owner_email": BOB })),
            &ann,
        )
        .await;
    assert!(forged.status.is_client_error(), "{}", forged.body);
    Ok(())
}

#[tokio::test]
async fn a_cookie_beside_a_bearer_token_lends_it_nothing() -> sc_error::Result<()> {
    let server = setup().await?;
    let (_, ann) = server.token(server.notes, ANN).await;
    // Bob's session, on the same request as Ann's token.
    let bob = server.user(BOB).await;
    let session = server.sessions.login(bob).await?;
    let cookie = format!("{SESSION_COOKIE}={session}");
    let auth = bearer(&ann);
    let me = send(
        &server.router,
        NOTES_HOST,
        "GET",
        "/api/whoami",
        None,
        &[("authorization", &auth), ("cookie", &cookie)],
    )
    .await;
    assert_eq!(me.body["email"], ANN, "{}", me.body);

    // A bad token with a good cookie is a 401, not the cookie's user.
    let bad = bearer("fsat_not-a-real-token");
    let refused = send(
        &server.router,
        NOTES_HOST,
        "POST",
        "/api/note",
        Some(json!({ "id": 20, "body": "x", "owner_email": BOB })),
        &[("authorization", &bad), ("cookie", &cookie)],
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{}", refused.body);
    assert!(
        refused
            .headers
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("Bearer ")),
        "an RFC 6750 challenge"
    );
    Ok(())
}

#[tokio::test]
async fn a_foreign_bearer_token_is_left_to_the_cookie() -> sc_error::Result<()> {
    let server = setup().await?;
    let bob = server.user(BOB).await;
    let session = server.sessions.login(bob).await?;
    let cookie = format!("{SESSION_COOKIE}={session}");
    // What oauth2-proxy forwards: an access token of its own provider's.
    let me = send(
        &server.router,
        CLOSED_HOST,
        "GET",
        "/api/whoami",
        None,
        &[
            ("authorization", "Bearer eyJhbGciOi.proxy.token"),
            ("cookie", &cookie),
        ],
    )
    .await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.body["email"], BOB, "{}", me.body);
    Ok(())
}

#[tokio::test]
async fn a_token_is_refused_where_it_does_not_belong() -> sc_error::Result<()> {
    let server = setup().await?;
    let (_, ann) = server.token(server.notes, ANN).await;

    let refused = |answer: &Answer, says: &str| {
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{}", answer.body);
        let error = answer.body["error"].as_str().unwrap_or_default().to_owned();
        assert!(error.contains(says), "{error:?} should say {says:?}");
    };

    // Another application's.
    let other = server
        .call(OTHER_HOST, "GET", "/api/whoami", None, &ann)
        .await;
    refused(&other, "another application");

    // An application that does not take tokens says so, whatever the token.
    let (_, closed_token) = server.token(Uuid::new_v4(), ANN).await;
    let closed = server
        .call(CLOSED_HOST, "GET", "/api/whoami", None, &closed_token)
        .await;
    refused(&closed, "does not accept API tokens");

    // An administration token on an application, and an application token on
    // the MCP server.
    let admin = server.user(ADMIN).await;
    let mcp = mint_api_token(
        &server.catalog,
        NewApiToken {
            user_id: admin.id(),
            label: "claude".into(),
            grants: sc_types::Attrs::new(),
            expires_at: None,
        },
    )
    .await?;
    let on_app = server
        .call(NOTES_HOST, "GET", "/api/whoami", None, &mcp.secret)
        .await;
    refused(&on_app, "administration");

    // Revoked.
    let (id, revoked) = server.token(server.notes, ANN).await;
    assert!(sc_auth::revoke_app_api_token(&server.catalog, server.notes, id, None).await?);
    let answer = server
        .call(NOTES_HOST, "GET", "/api/note", None, &revoked)
        .await;
    refused(&answer, "revoked");

    // A disabled account.
    let (_, bobs) = server.token(server.notes, BOB).await;
    let bob = server.user(BOB).await;
    let _changed = sc_auth::set_user_disabled(&server.catalog, bob.id(), true).await?;
    let answer = server
        .call(NOTES_HOST, "GET", "/api/note", None, &bobs)
        .await;
    refused(&answer, "disabled");
    Ok(())
}

#[tokio::test]
async fn the_application_admits_a_token_user_as_it_admits_a_session() -> sc_error::Result<()> {
    let server = setup().await?;
    // The staff application admits role 40 and better; Ann is 80.
    let (_, ann) = server.token(server.staff, ANN).await;
    let answer = server
        .call(STAFF_HOST, "GET", "/api/whoami", None, &ann)
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN, "{}", answer.body);

    let (_, bob) = server.token(server.staff, BOB).await;
    let answer = server
        .call(STAFF_HOST, "GET", "/api/whoami", None, &bob)
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body["email"], BOB);
    Ok(())
}

/// The CSRF cookie and header are not needed by a bearer client, and still
/// demanded of a cookie one — the exemption is the token's, not the path's.
#[tokio::test]
async fn a_cookie_write_still_needs_its_csrf_token() -> sc_error::Result<()> {
    let server = setup().await?;
    let ann = server.user(ANN).await;
    let session = server.sessions.login(ann).await?;
    let cookie = format!("{SESSION_COOKIE}={session}");
    let answer = send(
        &server.router,
        NOTES_HOST,
        "POST",
        "/api/note",
        Some(json!({ "id": 30, "body": "x", "owner_email": ANN })),
        &[("cookie", &cookie)],
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN, "{}", answer.body);
    Ok(())
}

/// The value `name` is set to among a response's `Set-Cookie` headers.
fn set_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|raw| {
            let (k, v) = raw.split(';').next()?.split_once('=')?;
            (k == name && !v.is_empty()).then(|| v.to_owned())
        })
}

/// A browser signed in to one application through its own login: the
/// cookies it sends, and the CSRF token it echoes.
struct Browser {
    host: &'static str,
    cookies: String,
    csrf: String,
}

impl Server {
    async fn sign_in(&self, host: &'static str, email: &str) -> Browser {
        let primed = send(&self.router, host, "GET", "/", None, &[]).await;
        let csrf = set_cookie(&primed.headers, CSRF_COOKIE).expect("a CSRF cookie");
        let cookie = format!("{CSRF_COOKIE}={csrf}");
        let login = send(
            &self.router,
            host,
            "POST",
            "/api/login",
            Some(json!({ "email": email, "password": PASSWORD })),
            &[("cookie", &cookie), (CSRF_HEADER, &csrf)],
        )
        .await;
        assert_eq!(login.status, StatusCode::OK, "{}", login.body);
        let session = set_cookie(&login.headers, SESSION_COOKIE).expect("a session");
        Browser {
            host,
            cookies: format!("{CSRF_COOKIE}={csrf}; {SESSION_COOKIE}={session}"),
            csrf,
        }
    }

    async fn as_browser(
        &self,
        browser: &Browser,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Answer {
        send(
            &self.router,
            browser.host,
            method,
            path,
            body,
            &[("cookie", &browser.cookies), (CSRF_HEADER, &browser.csrf)],
        )
        .await
    }
}

#[tokio::test]
async fn a_user_mints_uses_and_revokes_their_own_token() -> sc_error::Result<()> {
    let server = setup().await?;
    let ann = server.sign_in(NOTES_HOST, ANN).await;

    let empty = server
        .as_browser(&ann, "GET", "/_feldspar/api-tokens", None)
        .await;
    assert_eq!(empty.status, StatusCode::OK, "{}", empty.body);
    assert_eq!(empty.body["enabled"], true);
    assert_eq!(empty.body["may_create"], true);
    assert_eq!(empty.body["tokens"], json!([]));

    let made = server
        .as_browser(
            &ann,
            "POST",
            "/_feldspar/api-tokens",
            Some(json!({ "label": "nightly export", "expires_in_days": 30 })),
        )
        .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    let secret = made.body["secret"].as_str().unwrap().to_owned();
    assert!(secret.starts_with(sc_auth::APP_API_TOKEN_PREFIX));
    assert_eq!(made.body["token"]["state"], "live");
    assert!(made.body["token"]["expires_at"].is_string());
    let id = made.body["token"]["id"].as_str().unwrap().to_owned();

    // The token works, as her.
    let me = server
        .call(NOTES_HOST, "GET", "/api/whoami", None, &secret)
        .await;
    assert_eq!(me.body["email"], ANN, "{}", me.body);

    // Her list names it and never shows the secret again.
    let listed = server
        .as_browser(&ann, "GET", "/_feldspar/api-tokens", None)
        .await;
    assert_eq!(listed.body["tokens"][0]["label"], "nightly export");
    assert!(!listed.body.to_string().contains(&secret));

    // Bob cannot revoke it.
    let bob = server.sign_in(NOTES_HOST, BOB).await;
    let path = format!("/_feldspar/api-tokens/{id}/revoke");
    let theirs = server
        .as_browser(&bob, "POST", &path, Some(json!({})))
        .await;
    assert_eq!(theirs.body["revoked"], false, "{}", theirs.body);
    assert_eq!(
        server
            .call(NOTES_HOST, "GET", "/api/whoami", None, &secret)
            .await
            .status,
        StatusCode::OK
    );

    // She can.
    let revoked = server
        .as_browser(&ann, "POST", &path, Some(json!({})))
        .await;
    assert_eq!(revoked.body["revoked"], true, "{}", revoked.body);
    let refused = server
        .call(NOTES_HOST, "GET", "/api/whoami", None, &secret)
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);

    // The admin's list shows it, with whose it was.
    let listed = sc_auth::list_app_api_tokens(&server.catalog, server.notes, None).await?;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].revoked_at.is_some());
    Ok(())
}

#[tokio::test]
async fn minting_takes_an_own_session_and_no_token() -> sc_error::Result<()> {
    let server = setup().await?;

    // Nobody.
    let anonymous = send(
        &server.router,
        NOTES_HOST,
        "GET",
        "/_feldspar/api-tokens",
        None,
        &[],
    )
    .await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        anonymous.body
    );

    // A session issued elsewhere (the admin host), carried here as a shared
    // sign-in cookie would carry it.
    let ann = server.user(ANN).await;
    let visiting = server.sessions.login(ann).await?;
    let cookie = format!("{SESSION_COOKIE}={visiting}");
    let answer = send(
        &server.router,
        NOTES_HOST,
        "GET",
        "/_feldspar/api-tokens",
        None,
        &[("cookie", &cookie)],
    )
    .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{}", answer.body);

    // A token, even beside a good session: a token cannot mint tokens.
    let (_, token) = server.token(server.notes, ANN).await;
    let browser = server.sign_in(NOTES_HOST, ANN).await;
    let auth = bearer(&token);
    let answer = send(
        &server.router,
        NOTES_HOST,
        "POST",
        "/_feldspar/api-tokens",
        Some(json!({ "label": "more" })),
        &[("cookie", &browser.cookies), ("authorization", &auth)],
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN, "{}", answer.body);
    assert_eq!(
        sc_auth::list_app_api_tokens(&server.catalog, server.notes, None)
            .await?
            .len(),
        1,
        "nothing was minted"
    );
    Ok(())
}

#[tokio::test]
async fn minting_is_held_to_the_switch_and_the_role() -> sc_error::Result<()> {
    let server = setup().await?;

    // An application that does not take tokens: listed, not minted.
    let closed = server.sign_in(CLOSED_HOST, ANN).await;
    let listed = server
        .as_browser(&closed, "GET", "/_feldspar/api-tokens", None)
        .await;
    assert_eq!(listed.body["enabled"], false, "{}", listed.body);
    assert_eq!(listed.body["may_create"], false);
    let refused = server
        .as_browser(
            &closed,
            "POST",
            "/_feldspar/api-tokens",
            Some(json!({ "label": "x" })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);

    // Staff and better only: Ann (80) may not, Bob (40) may.
    let ann = server.sign_in(OTHER_HOST, ANN).await;
    let refused = server
        .as_browser(
            &ann,
            "POST",
            "/_feldspar/api-tokens",
            Some(json!({ "label": "x" })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(refused.body["error"].as_str().unwrap().contains("role"));
    let bob = server.sign_in(OTHER_HOST, BOB).await;
    let made = server
        .as_browser(
            &bob,
            "POST",
            "/_feldspar/api-tokens",
            Some(json!({ "label": "x" })),
        )
        .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    let made_for = sc_auth::list_app_api_tokens(&server.catalog, server.other, None).await?;
    assert_eq!(made_for.len(), 1);

    // A label is required, and an expiry is a sane number of days.
    for body in [
        json!({ "label": " " }),
        json!({ "label": "x", "expires_in_days": 0 }),
    ] {
        let bad = server
            .as_browser(&bob, "POST", "/_feldspar/api-tokens", Some(body))
            .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    }
    Ok(())
}

#[tokio::test]
async fn the_page_mints_with_a_form_and_shows_the_secret_once() -> sc_error::Result<()> {
    let server = setup().await?;
    let ann = server.sign_in(NOTES_HOST, ANN).await;

    let page = server
        .as_browser(&ann, "GET", "/_feldspar/api-tokens/page", None)
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let html = page.body.as_str().unwrap();
    assert!(html.contains("<h1>API tokens</h1>"), "{html}");
    assert!(html.contains("name=\"_csrf\""), "{html}");

    // The form posts are CSRF-checked by their `_csrf` field, not a header.
    let form = format!("_csrf={}&label=from+the+page&expires_in_days=", ann.csrf);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/_feldspar/api-tokens")
        .header(header::HOST, NOTES_HOST)
        .header("cookie", &ann.cookies)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let response = server.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("from the page"), "{html}");
    let secret = html
        .split(sc_auth::APP_API_TOKEN_PREFIX)
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .map(|rest| format!("{}{rest}", sc_auth::APP_API_TOKEN_PREFIX))
        .expect("the secret is on the page");
    let me = server
        .call(NOTES_HOST, "GET", "/api/whoami", None, &secret)
        .await;
    assert_eq!(me.body["email"], ANN, "{}", me.body);

    // And never again.
    let again = server
        .as_browser(&ann, "GET", "/_feldspar/api-tokens/page", None)
        .await;
    assert!(!again.body.as_str().unwrap().contains(&secret));
    Ok(())
}

#[tokio::test]
async fn an_admin_lists_and_revokes_an_applications_tokens() -> sc_error::Result<()> {
    let server = setup().await?;
    let (_, anns) = server.token(server.notes, ANN).await;
    server.token(server.other, BOB).await;

    let admin = server.sign_in(BASE_DOMAIN, ADMIN).await;
    let base = format!("/api/applications/{}/api-tokens", server.notes);
    let listed = server.as_browser(&admin, "GET", &base, None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let rows = listed.body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "this application's only: {}", listed.body);
    assert_eq!(rows[0]["email"], ANN);
    assert_eq!(rows[0]["state"], "live");
    assert!(!listed.body.to_string().contains(&anns));

    let id = rows[0]["id"].as_str().unwrap();
    let revoked = server
        .as_browser(&admin, "POST", &format!("{base}/{id}/revoke"), None)
        .await;
    assert_eq!(revoked.body["revoked"], true, "{}", revoked.body);
    let refused = server
        .call(NOTES_HOST, "GET", "/api/whoami", None, &anns)
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);

    // A non-admin may not (they cannot sign in to the admin host, so their
    // session is made directly).
    let bob = server.user(BOB).await;
    let session = server.sessions.login(bob).await?;
    let cookie = format!("{SESSION_COOKIE}={session}");
    let answer = send(
        &server.router,
        BASE_DOMAIN,
        "GET",
        &base,
        None,
        &[("cookie", &cookie)],
    )
    .await;
    assert!(answer.status.is_client_error(), "{}", answer.body);
    Ok(())
}

/// The MCP server takes no application token, even an administrator's, and an
/// application takes no expired one.
#[tokio::test]
async fn an_application_token_does_not_reach_the_mcp_server() -> sc_error::Result<()> {
    let server = setup().await?;
    sc_config::bootstrap(&server.catalog).await?;
    sc_config::set_config(&server.catalog, sc_config::MCP_ENABLED, json!(true)).await?;
    let (_, admins) = server.token(server.notes, ADMIN).await;

    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(sc_server::MCP_ROUTE)
        .header(header::HOST, BASE_DOMAIN)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, bearer(&admins))
        .body(Body::from(
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/list"
            }))
            .unwrap(),
        ))
        .unwrap();
    let peer: std::net::SocketAddr = "127.0.0.1:51234".parse().unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = server.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let said = String::from_utf8_lossy(&bytes);
    assert!(said.contains("application"), "refused as an application's: {said}");

    // Expired.
    let (id, lapsing) = server.token(server.notes, ANN).await;
    server
        ._db
        .client()
        .await?
        .batch_execute(&format!(
            "UPDATE _fd_api_tokens SET expires_at = now() - interval '1 minute' WHERE id = '{id}'"
        ))
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let answer = server
        .call(NOTES_HOST, "GET", "/api/note", None, &lapsing)
        .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{}", answer.body);
    assert!(answer.body["error"].as_str().unwrap().contains("expired"));
    Ok(())
}

//! **Guest sessions** end to end (TODO-payments P1.5 and P1.6, design C9): a
//! visitor who has not signed in keeps a basket as rows they own, through the
//! ordinary REST API, and keeps it when they sign in.
//!
//! What is asserted, against a real database and the assembled router:
//!
//! - two guests each see only their own rows, under the runtime ownership
//!   check and under row-level security, with `session_id` filled in from a
//!   `session.id` default;
//! - the basket is read again by a fresh request carrying only the cookie — a
//!   reload — and the cookie is persistent and set on the application's host
//!   alone, even where signed-in sessions share a cookie domain;
//! - a write without the CSRF token does not get the guest session;
//! - a lapsed guest session reads nothing;
//! - the success page after a cross-site return arrives without the cookie
//!   (`SameSite=Strict`) and its API call, which carries it, sees the order;
//! - signing in from the same browser ends the guest session, and the `login`
//!   event names it so a trigger moves the guest's rows to the account (P1.6).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_action::{EventKind, Trigger, save_trigger};
use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation, TableSettings};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{SessionStore, create_user};
use sc_catalog::{Catalog, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, GUEST_COOKIE, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const HOST: &str = "shop.example.com";

/// A browser on the shop: its cookie jar, and whether it echoes the CSRF token.
#[derive(Clone)]
struct Browser {
    router: Router,
    cookies: HashMap<String, String>,
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Browser {
    fn new(router: Router) -> Browser {
        Browser {
            router,
            cookies: HashMap::new(),
        }
    }

    async fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
        csrf: bool,
    ) -> Answer {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, HOST);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if csrf
            && method != "GET"
            && let Some(token) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, token);
        }
        let request = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        for raw in headers.get_all(header::SET_COOKIE) {
            let text = raw.to_str().unwrap();
            let pair = text.split(';').next().unwrap_or("");
            if let Some((name, value)) = pair.split_once('=') {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        Answer {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> Answer {
        self.request(method, path, body, true).await
    }

    /// The first request a page makes, which hands it the CSRF cookie.
    async fn prime(&mut self) {
        self.send("GET", "/", None).await;
    }

    /// `api.startGuestSession()`.
    async fn start_guest_session(&mut self) -> (String, Answer) {
        let answer = self.send("POST", "/api/guest-session", None).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        (answer.body["id"].as_str().unwrap().to_owned(), answer)
    }

    async fn items(&mut self, table: &str) -> Vec<String> {
        let answer = self.send("GET", &format!("/api/{table}"), None).await;
        assert_eq!(answer.status, StatusCode::OK, "{table}: {}", answer.body);
        let mut items: Vec<String> = answer
            .body
            .as_array()
            .unwrap_or_else(|| panic!("{table}: {}", answer.body))
            .iter()
            .map(|r| r["item"].as_str().unwrap().to_owned())
            .collect();
        items.sort();
        items
    }
}

fn field(name: &str, type_name: &str) -> FieldSpec {
    FieldSpec {
        name: name.into(),
        type_name: type_name.into(),
        ..FieldSpec::default()
    }
}

/// A basket table: its rows are a guest's by `session_id`, which defaults to
/// the guest session's id, or a signed-in user's by `owner_email`.
async fn create_basket(catalog: &Catalog, name: &str, rls: bool) -> sc_error::Result<()> {
    schema_edit::apply(
        catalog,
        &[Operation::CreateTable {
            name: name.into(),
            database: String::new(),
            settings: TableSettings {
                // Admin-only floors: only the ownership formula lets a guest
                // or a buyer in at all.
                min_role_read: Some(1),
                min_role_write: Some(1),
                ownership_formula: Some(
                    "user ? owner_email === user.email : session_id === session.id".into(),
                ),
                rls_enabled: Some(rls),
                ..TableSettings::default()
            },
            fields: vec![
                FieldSpec {
                    primary_key: true,
                    ..field("id", "int")
                },
                FieldSpec {
                    required: true,
                    ..field("item", "text")
                },
                FieldSpec {
                    required: true,
                    default_expression: Some("session.id".into()),
                    ..field("session_id", "text")
                },
                field("owner_email", "text"),
                field("note", "text"),
            ],
        }],
        &ApplyOptions::default(),
    )
    .await?;
    Ok(())
}

struct Server {
    router: Router,
    apps: Arc<AppMounts>,
    db: TestDb,
}

async fn setup() -> sc_error::Result<Server> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    let evaluator = default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    create_basket(&catalog, "baskets", false).await?;
    create_basket(&catalog, "rls_baskets", true).await?;
    sc_auth::save_role(&catalog, &sc_auth::Role::new(80, "User")).await?;
    create_user(&catalog, "ada@example.com", "ada-password", 80).await?;
    // P1.6: the application's own trigger, moving a guest's basket to the
    // account that signed in from the same browser.
    let adopt = Trigger::new("adopt_basket", EventKind::Login, "update_rows")
        .config("table", "baskets")
        .config("where", "session_id === payload.guest_session")
        .config("assignments", json!({ "owner_email": "user.email" }));
    save_trigger(&catalog, &dispatcher.registry(), &adopt).await?;
    // A trigger on a guest's insert sees the guest session too: in its
    // `only_if`, and in its code body.
    let noted = Trigger::new("note_guest", EventKind::Insert, "run_js_code")
        .on("baskets")
        .only_if("session !== null && row.session_id === session.id")
        .config(
            "code",
            "await db.baskets.where({ id: row.id }).update({ note: `seen ${session.id}` });",
        );
    save_trigger(&catalog, &dispatcher.registry(), &noted).await?;
    dispatcher.reload(&catalog).await?;

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(evaluator)
            .with_triggers(dispatcher),
    );
    let app = Application::new("Shop", "shop", FrameworkRef::new("code"))
        .with_table(TableId("baskets".to_owned()))
        .with_table(TableId("rls_baskets".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"))
        .attribute(sc_app::GUEST_SESSIONS_ATTRIBUTE, true)
        .attribute(sc_app::GUEST_SESSION_DAYS_ATTRIBUTE, 30);
    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html><h1>Shop</h1>"),
    ));
    apps.mount(MountedApp::new_with(
        app,
        framework,
        &catalog,
        apps.evaluator(),
        apps.triggers(),
    )?)?;
    let config = ServerConfig {
        base_domain: Some("example.com".to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;
    Ok(Server { router, apps, db })
}

async fn guest_session_rows(db: &TestDb, public_id: &str) -> i64 {
    db.client()
        .await
        .unwrap()
        .query_one(
            "SELECT count(*) FROM _fd_guest_sessions WHERE public_id = $1",
            &[&public_id],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn two_guests_each_keep_their_own_basket() -> sc_error::Result<()> {
    let server = setup().await?;
    // Signed-in sessions share a cookie domain here; a guest session's cookie
    // still belongs to the application's host alone.
    server.apps.set_shared_session_cookie(true);

    let mut ann = Browser::new(server.router.clone());
    let mut bob = Browser::new(server.router.clone());
    ann.prime().await;
    bob.prime().await;

    // Looking at pages starts nothing.
    assert!(!ann.cookies.contains_key(GUEST_COOKIE));

    let (ann_id, started) = ann.start_guest_session().await;
    let cookie = started.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cookie.starts_with(&format!("{GUEST_COOKIE}=")), "{cookie}");
    assert!(
        cookie.contains("Max-Age=2592000"),
        "persistent for 30 days: {cookie}"
    );
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(
        !cookie.contains("Domain="),
        "the application's host only: {cookie}"
    );
    // The cookie is not the public id: what a table stores is no credential.
    assert_ne!(ann.cookies[GUEST_COOKIE], ann_id);
    // Asking again answers the same session rather than starting another.
    let (again, _) = ann.start_guest_session().await;
    assert_eq!(again, ann_id);

    let (bob_id, _) = bob.start_guest_session().await;
    assert_ne!(ann_id, bob_id);

    for (browser, item) in [(&mut ann, "tea"), (&mut bob, "coffee")] {
        for table in ["baskets", "rls_baskets"] {
            let answer = browser
                .send(
                    "POST",
                    &format!("/api/{table}"),
                    Some(json!({ "item": item })),
                )
                .await;
            assert!(answer.status.is_success(), "{table}: {}", answer.body);
        }
    }
    // The default filled the guest's own id in, and the insert trigger saw it.
    let rows = server
        .db
        .client()
        .await?
        .query(
            "SELECT item, session_id, note FROM baskets ORDER BY item",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let stored: Vec<(String, String, Option<String>)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(
        stored,
        [
            (
                "coffee".to_owned(),
                bob_id.clone(),
                Some(format!("seen {bob_id}"))
            ),
            (
                "tea".to_owned(),
                ann_id.clone(),
                Some(format!("seen {ann_id}"))
            ),
        ]
    );

    // Each sees their own, by the runtime check and by the database's policy.
    for table in ["baskets", "rls_baskets"] {
        assert_eq!(ann.items(table).await, ["tea"], "{table}");
        assert_eq!(bob.items(table).await, ["coffee"], "{table}");
    }
    // Nobody with no guest session sees either.
    let mut stranger = Browser::new(server.router.clone());
    assert!(stranger.items("baskets").await.is_empty());
    assert!(stranger.items("rls_baskets").await.is_empty());

    // A reload: a fresh request carrying nothing but the cookies reads the
    // same basket.
    let mut reloaded = Browser {
        router: server.router.clone(),
        cookies: ann.cookies.clone(),
    };
    assert_eq!(reloaded.items("baskets").await, ["tea"]);

    // A write without the CSRF token does not get the guest session.
    let forged = ann
        .request(
            "POST",
            "/api/baskets",
            Some(json!({ "item": "forged" })),
            false,
        )
        .await;
    assert!(!forged.status.is_success(), "{}", forged.body);
    assert_eq!(ann.items("baskets").await, ["tea"]);

    // The success page after a cross-site return from the provider's checkout
    // arrives without the cookie (`SameSite=Strict`), and is the page anyway;
    // its API call is same-site, carries the cookie, and sees the order.
    let mut returning = Browser::new(server.router.clone());
    let page = returning.send("GET", "/?checkout=done", None).await;
    assert_eq!(page.status, StatusCode::OK);
    returning.cookies.extend(ann.cookies.clone());
    assert_eq!(returning.items("baskets").await, ["tea"]);

    // A guest session that has lapsed is nobody.
    server
        .db
        .client()
        .await?
        .execute(
            "UPDATE _fd_guest_sessions SET expires_at = now() - interval '1 minute' \
             WHERE public_id = $1",
            &[&bob_id],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert!(bob.items("baskets").await.is_empty());
    assert!(bob.items("rls_baskets").await.is_empty());
    Ok(())
}

#[tokio::test]
async fn signing_in_from_the_same_browser_moves_the_basket_to_the_account() -> sc_error::Result<()>
{
    let server = setup().await?;
    let mut ada = Browser::new(server.router.clone());
    ada.prime().await;
    let (guest_id, _) = ada.start_guest_session().await;
    let answer = ada
        .send("POST", "/api/baskets", Some(json!({ "item": "teapot" })))
        .await;
    assert!(answer.status.is_success(), "{}", answer.body);
    // Somebody else's basket, which must stay theirs.
    let mut other = Browser::new(server.router.clone());
    other.prime().await;
    let (other_id, _) = other.start_guest_session().await;
    other
        .send("POST", "/api/baskets", Some(json!({ "item": "kettle" })))
        .await;

    let login = ada
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "ada@example.com", "password": "ada-password" })),
        )
        .await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.body);
    // The guest session is over: its cookie is cleared and its row is gone.
    assert!(!ada.cookies.contains_key(GUEST_COOKIE), "{:?}", ada.cookies);
    assert_eq!(guest_session_rows(&server.db, &guest_id).await, 0);
    assert_eq!(guest_session_rows(&server.db, &other_id).await, 1);

    // The trigger on `login` moved the basket to the account, and the account
    // reads it by `user` now.
    assert_eq!(ada.items("baskets").await, ["teapot"]);
    let owners = server
        .db
        .client()
        .await?
        .query("SELECT item, owner_email FROM baskets ORDER BY item", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let owners: Vec<(String, Option<String>)> =
        owners.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        owners,
        [
            ("kettle".to_owned(), None),
            ("teapot".to_owned(), Some("ada@example.com".to_owned())),
        ]
    );
    assert_eq!(other.items("baskets").await, ["kettle"]);

    // A signed-in caller has no guest session to start.
    let refused = ada.send("POST", "/api/guest-session", None).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    Ok(())
}

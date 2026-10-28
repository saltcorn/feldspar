//! Saltcorn UI, the admin API (TODO "Saltcorn UI" 9.1, 9.4): an application's
//! views and pages listed, read, saved and deleted through the endpoints the
//! Views and Pages tabs call, over HTTP against a real Postgres.
//!
//! The claims:
//!
//! - **The round trip.** A view and a page saved through the API are listed and
//!   read back as they were sent, the configuration unchanged; saving again
//!   under the same name updates the one row; a body with another name renames
//!   it; deleting removes it, and deleting it again is a 404 naming it.
//! - **A save is live.** Every write moves the generation of the view set the
//!   mounted application renders from, so there is no build to follow it with —
//!   which is also why the application's row says `builds: false`.
//! - **Refusals name what is wrong**: a pattern this server does not have, and a
//!   table outside the application's subset.
//! - **A name with a space in it** — every v1 view name — is addressed through
//!   its percent-encoded path.
//! - The pattern list is the registry's six, and every endpoint is admin-only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator, install_agents, install_triggers,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// A cookie-carrying client over the router, echoing CSRF on mutations.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
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
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str() {
                let pair = text.split(';').next().unwrap_or("");
                if let Some((name, value)) = pair.split_once('=') {
                    if value.is_empty() {
                        self.cookies.remove(name);
                    } else {
                        self.cookies.insert(name.to_owned(), value.to_owned());
                    }
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

struct Server {
    client: Client,
    router: Router,
    _catalog: Arc<Catalog>,
    _db: TestDb,
}

/// A server with every platform table bootstrapped and an admin signed in.
async fn setup() -> sc_error::Result<Server> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_viewpattern::bootstrap(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_agents(agents)
            .with_triggers(dispatcher),
    );
    let router = build_router_with_apps(
        &admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;

    let mut client = Client {
        router: router.clone(),
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    Ok(Server {
        client,
        router,
        _catalog: catalog,
        _db: db,
    })
}

/// A table with one text field.
async fn table(client: &mut Client, name: &str) {
    let (status, body) = client
        .send("POST", "/api/tables", Some(json!({ "name": name })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = client
        .send(
            "POST",
            &format!("/api/tables/{name}/fields"),
            Some(json!({ "name": "title", "type": "text" })),
        )
        .await;
    assert!(status.is_success(), "{body}");
}

/// A Saltcorn UI application over `books`, created as the application form
/// creates one. Its id.
async fn application(client: &mut Client) -> String {
    let (status, body) = client
        .send(
            "POST",
            "/api/applications",
            Some(json!({
                "name": "Library",
                "description": "",
                "subdomain": "library",
                "framework": { "name": "saltcorn-ui", "config": {} },
                "extra_frameworks": [],
                "tables": ["books"],
                "file_stores": [],
                "triggers": [],
                "apis": [],
                "static_dirs": [],
                "csp": {},
                "attributes": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

fn generation(app: &str) -> u64 {
    let id = sc_app::AppId(uuid::Uuid::parse_str(app).unwrap());
    sc_viewpattern::view_sets()
        .generation(id)
        .unwrap()
        .expect("a write caches the application's view set")
}

#[tokio::test]
async fn an_applications_views_and_pages_are_managed_over_http() -> sc_error::Result<()> {
    let mut server = setup().await?;
    let client = &mut server.client;
    table(client, "books").await;
    table(client, "authors").await;
    let app = application(client).await;
    let views = format!("/api/applications/{app}/views");
    let pages = format!("/api/applications/{app}/pages");

    // Nothing to build: the list offers no Build button for it (9.3).
    let (_, listed) = client.send("GET", "/api/applications", None).await;
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == app)
        .unwrap();
    assert_eq!(row["builds"], false, "{row}");
    assert_eq!(row["has_views"], true, "{row}");

    // The framework says so too, which is what keeps its settings off the
    // create form and on the App settings tab.
    let (_, frameworks) = client.send("GET", "/api/frameworks", None).await;
    let has_views = |name: &str| {
        frameworks
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed: {frameworks}"))["has_views"]
            .clone()
    };
    assert_eq!(has_views("saltcorn-ui"), true);
    assert_eq!(has_views("code"), false);

    // The patterns a view may be saved with.
    let (status, patterns) = client.send("GET", "/api/view-patterns", None).await;
    assert_eq!(status, StatusCode::OK, "{patterns}");
    let names: Vec<&str> = patterns
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["List", "Show", "Edit", "Feed", "Filter", "ListShowList"]
    );
    assert!(patterns[0]["table_required"].as_bool().unwrap());

    // An application with no views lists none.
    let (status, empty) = client.send("GET", &views, None).await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty, json!([]));

    // Save two views; the configuration crosses unchanged.
    let configuration = json!({
        "columns": [{ "type": "Field", "field_name": "title", "state_field": "on" }],
        "default_state": { "_order_field": "title" },
    });
    let (status, saved) = client
        .send(
            "PUT",
            &format!("{views}/List%20Books"),
            Some(json!({
                "name": "List Books",
                "description": "Every book",
                "viewpattern": "List",
                "table_name": "books",
                "configuration": configuration,
                "min_role": 1,
                "slug": null,
                "attributes": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let list_id = saved["id"].as_str().unwrap().to_owned();
    let after_save = generation(&app);
    let (status, saved) = client
        .send(
            "PUT",
            &format!("{views}/ShowBook"),
            Some(json!({
                "name": "ShowBook",
                "description": "",
                "viewpattern": "Show",
                "table_name": "books",
                "configuration": {},
                "min_role": 100,
                "attributes": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    let (_, listed) = client.send("GET", &views, None).await;
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["List Books", "ShowBook"]);

    let (status, got) = client
        .send("GET", &format!("{views}/List%20Books"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["id"], list_id);
    assert_eq!(got["viewpattern"], "List");
    assert_eq!(got["table_name"], "books");
    assert_eq!(got["min_role"], 1);
    assert_eq!(got["configuration"], configuration);

    // Saving again under the name updates the row rather than adding one, and a
    // body with another name renames it.
    let mut renamed = got.clone();
    renamed["name"] = json!("All Books");
    renamed["description"] = json!("Every book, renamed");
    let (status, saved) = client
        .send("PUT", &format!("{views}/List%20Books"), Some(renamed))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["id"], list_id);
    let (_, listed) = client.send("GET", &views, None).await;
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["All Books", "ShowBook"]);
    let (status, _) = client
        .send("GET", &format!("{views}/List%20Books"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The refusals name what is wrong, and save nothing.
    let (status, err) = client
        .send(
            "PUT",
            &format!("{views}/Chat"),
            Some(json!({
                "name": "Chat", "description": "", "viewpattern": "Room",
                "table_name": "books", "configuration": {}, "min_role": 100, "attributes": {},
            })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {err}");
    assert!(err["error"].as_str().unwrap().contains("Room"), "{err}");
    let (status, err) = client
        .send(
            "PUT",
            &format!("{views}/Authors"),
            Some(json!({
                "name": "Authors", "description": "", "viewpattern": "List",
                "table_name": "authors", "configuration": {}, "min_role": 100, "attributes": {},
            })),
        )
        .await;
    assert!(status.is_client_error(), "{status} {err}");
    assert!(err["error"].as_str().unwrap().contains("authors"), "{err}");
    let (_, listed) = client.send("GET", &views, None).await;
    assert_eq!(listed.as_array().unwrap().len(), 2);

    // Delete, live on the next request: the generation moved.
    let (status, deleted) = client
        .send("DELETE", &format!("{views}/ShowBook"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted, json!({ "deleted": true }));
    assert!(generation(&app) > after_save);
    let (status, err) = client
        .send("DELETE", &format!("{views}/ShowBook"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{err}");
    assert!(err["error"].as_str().unwrap().contains("ShowBook"), "{err}");

    // Pages: the same four.
    let layout = json!({ "type": "view", "view": "All Books", "state": "shared" });
    let (status, saved) = client
        .send(
            "PUT",
            &format!("{pages}/Home"),
            Some(json!({
                "name": "Home",
                "title": "Welcome",
                "description": "",
                "layout": layout,
                "min_role": 100,
                "attributes": { "root_page_for_roles": [100] },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (_, listed) = client.send("GET", &pages, None).await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let (status, got) = client.send("GET", &format!("{pages}/Home"), None).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["title"], "Welcome");
    assert_eq!(got["layout"], layout);
    assert_eq!(got["attributes"]["root_page_for_roles"], json!([100]));
    let (status, deleted) = client.send("DELETE", &format!("{pages}/Home"), None).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let (_, listed) = client.send("GET", &pages, None).await;
    assert_eq!(listed, json!([]));

    // An application that does not exist.
    let missing = uuid::Uuid::new_v4();
    let (status, _) = client
        .send("GET", &format!("/api/applications/{missing}/views"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Admin only: a visitor with no session is refused every one of them.
    let mut visitor = Client {
        router: server.router.clone(),
        cookies: HashMap::new(),
    };
    for (method, path) in [
        ("GET", views.clone()),
        ("GET", format!("{views}/All%20Books")),
        ("GET", pages.clone()),
        ("GET", format!("{pages}/Home")),
        ("GET", "/api/view-patterns".to_owned()),
    ] {
        let (status, _) = visitor.send(method, &path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
    Ok(())
}

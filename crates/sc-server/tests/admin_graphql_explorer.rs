//! The admin UI's GraphQL explorer, from the server's side (TODO "GraphQL API"
//! Phase 9).
//!
//! The screen is React and its model is tested with `vitest`
//! (`ui/admin/src/graphqlExplorer.test.ts`); what cannot be tested there is the
//! endpoint it drives, which is the whole reason the explorer exists as an
//! *admin* endpoint rather than as a `fetch` to the application's subdomain: the
//! admin SPA is served under `connect-src 'self'`.
//!
//! What is asserted here is that it is a window onto the application's own API
//! rather than a second one:
//!
//! - the same document, through the explorer and through the app's own mount in
//!   the same admin's session, gives the **same answer** — because it is the
//!   same [`ApiProvider::handle`](sc_api::ApiProvider) call;
//! - introspection through it describes exactly what the application exposes,
//!   so a table outside its declared subset is neither browsable nor answerable;
//! - a write through it is the row layer's write, readable back over REST;
//! - it is admin-only, and an application that is not mounted or does not enable
//!   the provider is told so in those words rather than answering emptily.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_app::{
    ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, bootstrap, save_application,
};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "staff.example.com";
const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// The milestone's departments and employees — plus a `secrets` table the
/// application does **not** declare, which is what makes "the explorer sees the
/// application's schema" a claim that can fail.
const FIXTURE: &str = "
    CREATE TABLE departments (id bigint primary key, name text not null);
    CREATE TABLE employees (
        id bigint primary key,
        name text not null,
        salary bigint,
        department bigint references departments(id));
    CREATE TABLE secrets (id bigint primary key, body text not null);
    INSERT INTO departments VALUES (1, 'Engineering'), (2, 'Sales'), (3, 'Empty');
    INSERT INTO employees VALUES
        (1, 'Ada',  90000, 1),
        (2, 'Bror', 45000, 1),
        (3, 'Cleo', 30000, 1),
        (4, 'Dag',  40000, 2);
    INSERT INTO secrets VALUES (1, 'the combination is 1234');
";

/// A cookie-jar client over the router (session + CSRF), as a browser is.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router, host: &str) -> Client {
        Client {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
        }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.host);
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
            && method != "HEAD"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(ref b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(b).unwrap()))
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
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024)
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

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-gql-explorer-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

struct Harness {
    /// The signed-in admin, on the base domain — the explorer's own client.
    admin: Client,
    router: Router,
    /// The id of the application the explorer is pointed at.
    app_id: String,
    /// The live mount registry, so a test can add a second application without
    /// running a bundler.
    apps: Arc<AppMounts>,
    catalog: Arc<Catalog>,
    _db: TestDb,
    _dir: TempDir,
}

impl Harness {
    /// Run a document through the **explorer**: the admin endpoint the screen
    /// posts to.
    async fn explore(&mut self, body: Value) -> (StatusCode, Value) {
        let path = format!("/api/applications/{}/graphql", self.app_id);
        self.admin.send("POST", &path, Some(body)).await
    }
}

/// The application the explorer is pointed at: REST at `/api`, GraphQL at
/// `/graphql`, over two of the schema's three tables.
fn staff_app() -> Application {
    Application::new(
        "Staff",
        "staff",
        FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "sh build.sh"),
    )
    .with_table(TableId("departments".to_owned()))
    .with_table(TableId("employees".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"))
    // Aggregates switched on: they are absent from the schema unless an
    // application asks for them, and the queries below ask for them.
    .with_api(ApiConfig::new("graphql", "/graphql").with(sc_api::GRAPHQL_CFG_AGGREGATES, true))
}

/// A server with that app mounted and an admin signed in.
async fn setup(tag: &str) -> sc_error::Result<Harness> {
    let db = TestDb::new().await?;
    let dir = TempDir::new(tag);
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$;",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    db.client()
        .await?
        .batch_execute(FIXTURE)
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &dir.0)?))?;

    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    admin.send("GET", "/api/auth/status", None).await;
    admin
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;

    let app = staff_app();
    let app_id = app.id.to_string();
    save_application(&catalog, &app).await?;
    // An empty bundle: this is about the API, and a bundler here would only be a
    // slower way of serving nothing.
    let framework = Arc::new(CodeFramework::new("code", AssetBundle::new()));
    apps.mount(MountedApp::new(app, framework, &catalog)?)?;

    Ok(Harness {
        admin,
        router,
        app_id,
        apps,
        catalog,
        _db: db,
        _dir: dir,
    })
}

/// Sign in to the *application* as the same admin, through its REST provider's
/// own `login` — the session a request to the app's own mount carries.
async fn app_client(h: &Harness) -> Client {
    let mut client = Client::new(h.router.clone(), APP_HOST);
    client.send("GET", "/graphql/schema.graphql", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    client
}

#[tokio::test]
async fn the_explorer_answers_exactly_what_the_apps_own_mount_answers() -> sc_error::Result<()> {
    let mut h = setup("same").await?;
    let mut app = app_client(&h).await;

    // The milestone's own query, with its constrained aggregate — and a
    // *variable*, because the explorer has a variables pane and a document that
    // ignores it would not exercise it.
    let document = r#"query staff($floor: BigInt) {
        departments(order_by: [{ name: asc }]) {
          name
          employees(where: { salary: { gte: $floor } }, limit: 5) { name salary }
          employees_aggregate(where: { salary: { lt: 50000 } }) { count }
        }
      }"#;
    let body = json!({
        "query": document,
        "variables": { "floor": 40000 },
        "operationName": "staff",
    });

    let (status, through_explorer) = h.explore(body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{through_explorer}");
    assert_eq!(through_explorer.get("errors"), None, "{through_explorer}");

    let departments = &through_explorer["data"]["departments"];
    assert_eq!(departments.as_array().unwrap().len(), 3);
    assert_eq!(departments[1]["name"], json!("Engineering"));
    assert_eq!(
        departments[1]["employees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Ada", "Bror"],
        "the variable reached the child list's filter: {through_explorer}"
    );
    assert_eq!(departments[1]["employees_aggregate"]["count"], json!(2));

    // The same document, in the same admin's session, through the application's
    // own mount. The explorer is a window onto that API, so the two answers are
    // the same answer — not merely similar numbers.
    let (status, through_mount) = app.send("POST", "/graphql", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{through_mount}");
    assert_eq!(through_explorer, through_mount);
    Ok(())
}

#[tokio::test]
async fn introspection_describes_the_applications_schema_and_nothing_else() -> sc_error::Result<()>
{
    let mut h = setup("introspect").await?;

    // What the explorer's schema pane asks for: the query root's fields and the
    // object types, which is how it browses a schema it was not compiled
    // against.
    let (status, body) = h
        .explore(json!({
            "query": "query { __schema { queryType { name } types { kind name } } }",
        }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.get("errors"), None, "{body}");
    let names: Vec<&str> = body["data"]["__schema"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&"Departments"), "{names:?}");
    assert!(names.contains(&"Employees"), "{names:?}");
    // `secrets` is a table of the database that this application does not
    // declare. The explorer runs against the application's schema, so it is not
    // there to browse...
    assert!(
        !names.iter().any(|n| n.eq_ignore_ascii_case("secrets")),
        "a table outside the app's subset must not be in its schema: {names:?}"
    );

    // ...and not there to ask for either.
    let (status, body) = h
        .explore(json!({ "query": "query { secrets { body } }" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"].is_null(), "{body}");
    assert!(
        format!("{}", body["errors"]).contains("secrets"),
        "the refusal should name the field asked for: {body}"
    );
    Ok(())
}

#[tokio::test]
async fn a_write_through_the_explorer_is_the_row_layers_write() -> sc_error::Result<()> {
    let mut h = setup("mutate").await?;

    let (status, body) = h
        .explore(json!({
            "query": r#"mutation { insert_departments(object: { id: 4, name: "Legal" }) { id name } }"#,
        }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.get("errors"), None, "{body}");
    assert_eq!(body["data"]["insert_departments"]["name"], json!("Legal"));

    // Read back over the application's REST provider: one row layer, so the row
    // an explorer wrote is a row the application serves.
    let mut app = app_client(&h).await;
    let (status, rows) = app.send("GET", "/api/departments", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows.as_array().unwrap().len(), 4, "{rows}");
    Ok(())
}

#[tokio::test]
async fn the_explorer_is_admin_only() -> sc_error::Result<()> {
    let h = setup("auth").await?;
    // No session: the endpoint is `AuthRequirement::admin()`, and the admin
    // surface admits nobody else (`authenticate_admin`), so this is the whole of
    // "not the signed-in admin".
    let mut anon = Client::new(h.router.clone(), BASE_DOMAIN);
    let (status, body) = anon
        .send(
            "POST",
            &format!("/api/applications/{}/graphql", h.app_id),
            Some(json!({ "query": "query { departments { name } }" })),
        )
        .await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "an anonymous caller must not reach the explorer: {status} {body}"
    );
    Ok(())
}

#[tokio::test]
async fn an_application_with_nothing_to_explore_says_which_it_is() -> sc_error::Result<()> {
    let mut h = setup("no-graphql").await?;

    // A second application, REST only — what an admin hits by opening the
    // explorer on the wrong app. Saved *and mounted*, so what is missing is the
    // provider and not the mount.
    let rest_only = Application::new(
        "Reports",
        "reports",
        FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "sh build.sh"),
    )
    .with_table(TableId("departments".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    let rest_only_id = rest_only.id.to_string();
    save_application(&h.catalog, &rest_only).await?;
    let framework = Arc::new(CodeFramework::new("code", AssetBundle::new()));
    h.apps
        .mount(MountedApp::new(rest_only, framework, &h.catalog)?)?;

    let (status, body) = h
        .admin
        .send(
            "POST",
            &format!("/api/applications/{rest_only_id}/graphql"),
            Some(json!({ "query": "query { departments { name } }" })),
        )
        .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    let text = format!("{body}");
    assert!(
        text.contains("Reports") && text.contains("graphql"),
        "the refusal should name the application and the provider: {text}"
    );

    // And a third: enabled, saved, never built — so nothing is serving it. A
    // "no such provider" here would send the admin looking at the wrong thing.
    let unbuilt = Application::new(
        "Warehouse",
        "warehouse",
        FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "sh build.sh"),
    )
    .with_table(TableId("departments".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("graphql", "/graphql"));
    let unbuilt_id = unbuilt.id.to_string();
    save_application(&h.catalog, &unbuilt).await?;

    let (status, body) = h
        .admin
        .send(
            "POST",
            &format!("/api/applications/{unbuilt_id}/graphql"),
            Some(json!({ "query": "query { departments { name } }" })),
        )
        .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    let text = format!("{body}");
    assert!(
        text.contains("Warehouse") && text.contains("build"),
        "an unbuilt application should be told to build: {text}"
    );
    Ok(())
}

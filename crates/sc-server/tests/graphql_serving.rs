//! An application serving **REST and GraphQL at once**, through the assembled
//! router (TODO "GraphQL API" Phase 8).
//!
//! The milestone's claim is that the `ApiProvider` seam carries a second
//! protocol whose shape is nothing like the first — not instead of REST, but
//! beside it, on one subdomain, over one table subset, with one authorization
//! layer. Everything below is that claim in the only form that can fail:
//! requests to a real server.
//!
//! - `POST /graphql` answers the milestone's own query — a department, its
//!   employees, and a *constrained* aggregate over them — while `GET /api/…`
//!   answers the REST call for the same rows in the same session.
//! - `GET /graphql/schema.graphql` serves the SDL of what is mounted.
//! - A column added through the admin API is in that SDL on the **next request**,
//!   with no restart and no rebuild: the [`SchemaObserver`](sc_catalog::SchemaObserver)
//!   seam re-projects both providers, and the GraphQL schema is built at mount
//!   like the REST endpoint set is.
//! - And the gate is the *table's*, not the endpoint's: the anonymous caller who
//!   can reach the endpoint is refused the rows by name.
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

/// Three departments and their employees — the milestone's example, small
/// enough that every number below can be checked by eye.
const FIXTURE: &str = "
    CREATE TABLE departments (id bigint primary key, name text not null);
    CREATE TABLE employees (
        id bigint primary key,
        name text not null,
        salary bigint,
        department bigint references departments(id));
    INSERT INTO departments VALUES (1, 'Engineering'), (2, 'Sales'), (3, 'Empty');
    INSERT INTO employees VALUES
        (1, 'Ada',  90000, 1),
        (2, 'Bror', 45000, 1),
        (3, 'Cleo', 30000, 1),
        (4, 'Dag',  40000, 2);
";

/// A cookie-jar client over the router (session + CSRF), as a browser would be.
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

    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Vec<u8>) {
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
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
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
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
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
        (status, content_type, bytes.to_vec())
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, _, bytes) = self.raw(method, path, body).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// POST a GraphQL document to the app's mount.
    async fn graphql(&mut self, query: &str) -> Value {
        let (status, body) = self
            .send("POST", "/graphql", Some(json!({ "query": query })))
            .await;
        // The legacy `application/json` rule: 200, with any errors in the body.
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-graphql-{}-{tag}-{:?}",
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
    /// The admin, on the base domain.
    admin: Client,
    router: Router,
    _catalog: Arc<Catalog>,
    _db: TestDb,
    _dir: TempDir,
}

/// The application both providers are enabled on: REST at `/api`, GraphQL at
/// `/graphql`, over the same two tables.
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

/// A server with the app mounted, an admin logged in, and both tables readable
/// by a signed-in user.
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

    // `admin_handlers` installs the schema observer, exactly as a real server's
    // boot does — which is the seam the re-projection test below rides on.
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
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;

    let app = staff_app();
    save_application(&catalog, &app).await?;
    // The bundle is empty: this test is about the APIs, and a bundler here would
    // only be a slower way of serving nothing.
    let framework = Arc::new(CodeFramework::new("code", AssetBundle::new()));
    apps.mount(MountedApp::new(app, framework, &catalog)?)?;

    Ok(Harness {
        admin,
        router,
        _catalog: catalog,
        _db: db,
        _dir: dir,
    })
}

/// Sign in to the app itself, through the REST provider's own `login` — the
/// session a GraphQL request then carries.
async fn app_client(h: &Harness) -> Client {
    let mut client = Client::new(h.router.clone(), APP_HOST);
    // One GET first, for the CSRF cookie the login POST has to echo — a browser
    // has loaded the page before it submits the form.
    client.raw("GET", "/graphql/schema.graphql", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    client
}

#[tokio::test]
async fn one_application_serves_rest_and_graphql_on_one_subdomain() -> sc_error::Result<()> {
    let h = setup("both").await?;
    let mut app = app_client(&h).await;

    // --- the SDL, served where the provider says it is ----------------------
    let (status, ct, bytes) = app.raw("GET", "/graphql/schema.graphql", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct, "text/plain; charset=utf-8");
    let sdl = String::from_utf8(bytes).unwrap();
    assert!(sdl.contains("type Departments {"), "{sdl}");
    assert!(sdl.contains("type Employees {"), "{sdl}");

    // --- the milestone's own query, in one round trip -----------------------
    let body = app
        .graphql(
            r#"query {
                 departments(order_by: [{ name: asc }]) {
                   name
                   employees(where: { salary: { gte: 40000 } }, limit: 5) { name salary }
                   employees_aggregate(where: { salary: { lt: 50000 } }) { count }
                 }
               }"#,
        )
        .await;
    assert_eq!(body.get("errors"), None, "{body}");
    let departments = &body["data"]["departments"];
    assert_eq!(departments.as_array().unwrap().len(), 3, "{body}");
    assert_eq!(departments[0]["name"], json!("Empty"));
    assert_eq!(departments[0]["employees_aggregate"]["count"], json!(0));
    assert_eq!(departments[1]["name"], json!("Engineering"));
    // Ada and Bror earn 40 000 or more; Bror and Cleo earn under 50 000. The two
    // constraints are different, and each applies where it was written.
    assert_eq!(
        departments[1]["employees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Ada", "Bror"]
    );
    assert_eq!(departments[1]["employees_aggregate"]["count"], json!(2));
    assert_eq!(departments[2]["name"], json!("Sales"));
    assert_eq!(departments[2]["employees_aggregate"]["count"], json!(1));

    // --- and REST is still there, in the same session -----------------------
    let (status, rows) = app.send("GET", "/api/departments", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows.as_array().unwrap().len(), 3, "{rows}");

    // A mutation through GraphQL is the row layer's, so the row it writes is the
    // row REST reads back.
    let body = app
        .graphql(
            r#"mutation {
                 insert_departments(object: { id: 4, name: "Legal" }) { id name }
               }"#,
        )
        .await;
    assert_eq!(body.get("errors"), None, "{body}");
    assert_eq!(body["data"]["insert_departments"]["name"], json!("Legal"));
    let (_, rows) = app.send("GET", "/api/departments", None).await;
    assert_eq!(rows.as_array().unwrap().len(), 4, "{rows}");
    Ok(())
}

#[tokio::test]
async fn the_endpoint_is_open_and_the_tables_are_not() -> sc_error::Result<()> {
    // Decision 4: one schema per application, authorized at resolve time. So an
    // anonymous caller reaches the endpoint and is refused the *rows*, by name —
    // rather than being told the API does not exist.
    let h = setup("anon").await?;
    let mut anon = Client::new(h.router.clone(), APP_HOST);

    let (status, _, _) = anon.raw("GET", "/graphql/schema.graphql", None).await;
    assert_eq!(status, StatusCode::OK, "introspection stays on");

    let body = anon.graphql("query { departments { name } }").await;
    let errors = body["errors"].as_array().cloned().unwrap_or_default();
    assert!(!errors.is_empty(), "{body}");
    assert!(
        format!("{}", body["errors"]).contains("departments"),
        "the refusal should name the table: {body}"
    );
    assert!(body["data"]["departments"].is_null(), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_column_added_in_the_admin_ui_is_in_the_schema_on_the_next_request()
-> sc_error::Result<()> {
    // The `SchemaObserver` path: no restart, no rebuild, no `refresh_table` call
    // in any handler. The GraphQL schema is built at mount exactly as the REST
    // endpoint set is, so it moves for the same reason and at the same moment.
    let mut h = setup("reproject").await?;
    let mut app = app_client(&h).await;

    let (_, _, before) = app.raw("GET", "/graphql/schema.graphql", None).await;
    let before = String::from_utf8(before).unwrap();
    assert!(!before.contains("started"), "{before}");

    let (status, body) = h
        .admin
        .send(
            "POST",
            "/api/tables/employees/fields",
            Some(json!({ "name": "started", "type": "date" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (_, _, after) = app.raw("GET", "/graphql/schema.graphql", None).await;
    let after = String::from_utf8(after).unwrap();
    assert!(after.contains("started: Date"), "{after}");
    // Selectable *and* filterable: the SDL is one contract, re-derived whole.
    assert!(after.contains("started: DateComparison"), "{after}");

    // And it is answerable, not merely describable.
    let body = app
        .graphql("query { employees(limit: 1) { name started } }")
        .await;
    assert_eq!(body.get("errors"), None, "{body}");
    assert!(
        body["data"]["employees"][0].get("started").is_some(),
        "{body}"
    );

    // The REST projection moved with it, from the same notification.
    let (status, rows) = app.send("GET", "/api/employees", None).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert!(rows[0].get("started").is_some(), "{rows}");
    Ok(())
}

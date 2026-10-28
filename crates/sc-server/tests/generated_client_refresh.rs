//! Phase 6 integration test: an application's **generated code follows its API
//! definition** — automatically when the schema moves, and on demand from the
//! admin screen's button (§13.1/§13.3, decision 10).
//!
//! Two claims that only a running server can carry:
//!
//! - A column added through the admin API rewrites the mounted app's
//!   `src/feldspar/client.ts` **and** its `schema.sql`, with no build, no
//!   restart, and nobody asking for it. The seam is the same
//!   [`SchemaObserver`](sc_catalog::SchemaObserver) that re-projects the app's
//!   providers: the projection in memory and the client on disk must not be
//!   allowed to disagree, which is the drift §13.1 exists to prevent.
//! - `updateApplicationClient` does it on demand, is admin-only, and reports
//!   *which* of its two outcomes happened.
//!
//! The re-emit is deliberately a **background** task (the observer is
//! synchronous, and a file write must never fail somebody's schema change), so
//! the assertions below wait for the file rather than reading it once.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_app::{
    ApiConfig, Application, CodeFramework, FrameworkRef, bootstrap, save_application, scaffold_app,
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

/// A cookie-jar client over the router (session + CSRF), as a browser SPA would.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, BASE_DOMAIN);
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
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024)
            .await
            .unwrap();
        let value = match bytes.is_empty() {
            true => Value::Null,
            false => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        };
        (status, value)
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-genclient-{}-{tag}-{:?}",
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
    client: Client,
    app: Application,
    /// The scaffolded project directory on disk.
    project: PathBuf,
    /// Held for the test's lifetime: the catalog the observer re-projects
    /// against, the registry the app is mounted in, the database, and the store
    /// root. Dropping any of them would take the thing under test with it.
    _catalog: Arc<Catalog>,
    _apps: Arc<AppMounts>,
    _db: TestDb,
    _dir: TempDir,
}

/// An admin router with a signed-in admin, a `tasks` table, an `apps` store, and
/// a **scaffolded, mounted** React application over that table.
///
/// The observer is not installed by hand: `admin_handlers` installs it, exactly
/// as a real server assembling its admin API does.
async fn setup(tag: &str) -> sc_error::Result<Harness> {
    let db = TestDb::new().await?;
    let dir = TempDir::new(tag);
    db.client()
        .await?
        .batch_execute(
            "CREATE TABLE tasks (\
               id bigint generated by default as identity primary key, \
               title text not null)",
        )
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
    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;

    let app = Application::new(
        "Todo",
        "todo",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "todo"),
    )
    .with_table(TableId("tasks".to_owned()))
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"));
    save_application(&catalog, &app).await?;
    scaffold_app(&catalog, &app, None).await?;
    // Mounted with an empty bundle: nothing here serves the app's UI, and
    // building it would need a Node toolchain the Rust suite does not have. What
    // matters is that it is *mounted*, so the schema observer re-projects it.
    let framework = Arc::new(CodeFramework::new("react", sc_app::AssetBundle::new()));
    apps.remount(MountedApp::new(app.clone(), framework, &catalog)?);

    let project = dir.0.join("todo");
    Ok(Harness {
        client,
        app,
        project,
        _catalog: catalog,
        _apps: apps,
        _db: db,
        _dir: dir,
    })
}

/// Wait for `path` to contain `needle`, up to a couple of seconds.
///
/// The re-emit runs in the background on purpose (decision 10: it must not be
/// able to fail an admin's schema change), so "has it happened yet?" is a real
/// question rather than a testing convenience. Waiting for the content beats
/// sleeping for a guessed interval: a machine slower than this one still passes,
/// and a regression that stops writing the file still fails.
async fn wait_for(path: &Path, needle: &str) -> String {
    for _ in 0..200 {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{} never gained `{needle}`:\n{}",
        path.display(),
        std::fs::read_to_string(path).unwrap_or_default()
    );
}

/// A column added in the admin UI reaches the app's generated files by itself —
/// no build, no restart, nobody pressing anything.
#[tokio::test]
async fn adding_a_column_rewrites_the_apps_generated_directory() -> sc_error::Result<()> {
    let mut h = setup("column").await?;
    // The row type the app's components are written against lives in the
    // generated client, beside the methods that answer with it.
    let client_ts = h.project.join("src/feldspar/client.ts");
    let schema_sql = h.project.join("src/feldspar/schema.sql");

    // What the scaffold wrote describes the table as it was.
    assert!(!std::fs::read_to_string(&client_ts)?.contains("done"));
    assert!(!std::fs::read_to_string(&schema_sql)?.contains("done"));

    let (status, body) = h
        .client
        .send(
            "POST",
            "/api/tables/tasks/fields",
            Some(json!({ "name": "done", "type": "bool" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The typed row the app's components are written against has the new
    // column...
    let client = wait_for(&client_ts, "done").await;
    assert!(client.contains("done: boolean | null"), "{client}");
    // ...and `schema.sql` has the column a custom SQL query may now name. Both,
    // from one change: one is what the app's code holds and the other is what an
    // agent writes SQL against, and a change that updated only one of them would
    // leave the other quietly lying. (`hooks.ts` and `store.ts` are rewritten on
    // the same pass; their text moves when the *endpoint set* moves, which the
    // next test does.)
    let schema = wait_for(&schema_sql, "done").await;
    assert!(schema.contains(r#""done" bool"#), "{schema}");
    assert!(schema.contains(r#"CREATE TABLE "tasks""#), "{schema}");
    Ok(())
}

/// Saving an application is an API-definition change, so its client follows:
/// a custom SQL query added through the admin API appears as a typed method in
/// `client.ts` with no build and nobody pressing regenerate.
#[tokio::test]
async fn saving_an_application_rewrites_its_typed_client() -> sc_error::Result<()> {
    let mut h = setup("save").await?;
    let client_ts = h.project.join("src/feldspar/client.ts");
    assert!(!std::fs::read_to_string(&client_ts)?.contains("countTasks"));

    // The app as the admin form would post it back, with one query added to the
    // REST API's config.
    let (status, list) = h.client.send("GET", "/api/applications", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let mut body = list
        .as_array()
        .and_then(|apps| {
            apps.iter()
                .find(|a| a["id"] == json!(h.app.id.to_string()))
                .cloned()
        })
        .unwrap_or_else(|| panic!("the saved application should be listed: {list}"));
    body["apis"][0]["config"] = json!({
        "queries": [{
            "name": "countTasks",
            "method": "GET",
            "path": "/reports/count",
            "min_role": 1,
            "code": "SELECT count(*) AS n FROM tasks WHERE title = :title",
            "params": [{ "name": "title", "type": "text", "required": true }],
        }],
    });
    let (status, saved) = h
        .client
        .send(
            "PUT",
            &format!("/api/applications/{}", h.app.id),
            Some(body),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    // The client gained the method, typed from the columns Postgres reported
    // when the query was prepared on the way in.
    let client = wait_for(&client_ts, "countTasks").await;
    assert!(client.contains("countTasks(query:"), "{client}");
    assert!(client.contains("n?: number | null"), "{client}");
    Ok(())
}

/// The admin screen's button: `updateApplicationClient` regenerates on demand,
/// and rescaffolds a directory that has been emptied. (That it is admin-only is
/// asserted with every other application endpoint, in `admin_applications_api`.)
#[tokio::test]
async fn the_update_client_endpoint_regenerates_and_rescaffolds() -> sc_error::Result<()> {
    let mut h = setup("button").await?;
    let path = format!("/api/applications/{}/client", h.app.id);

    // A populated project is regenerated, and says so — the generated files and
    // not one thing outside them.
    let mine = "// mine\n";
    std::fs::write(h.project.join("src/App.tsx"), mine)?;
    let (status, body) = h.client.send("POST", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scaffolded"], json!(false), "{body}");
    let files: Vec<String> = serde_json::from_value(body["files"].clone()).unwrap();
    assert_eq!(
        files,
        [
            "todo/src/feldspar/hooks.ts",
            "todo/src/feldspar/store.ts",
            "todo/src/feldspar/i18n.tsx",
            "todo/src/feldspar/live-react.ts",
            "todo/src/feldspar/README.md",
            "todo/src/feldspar/client.ts",
            "todo/src/feldspar/helper.ts",
            "todo/src/feldspar/schema.sql",
            "todo/src/feldspar/messages.ts",
            "todo/src/feldspar/SKILL.md",
        ],
        "{body}"
    );
    assert_eq!(
        std::fs::read_to_string(h.project.join("src/App.tsx"))?,
        mine
    );

    // An emptied project directory is scaffolded instead: re-emitting into it
    // would leave generated files with no project around them, which cannot
    // build. And the response distinguishes the two, because writing a whole
    // project is not the same news as rewriting four files.
    std::fs::remove_dir_all(&h.project)?;
    let (status, body) = h.client.send("POST", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scaffolded"], json!(true), "{body}");
    assert!(
        body["log"].as_str().unwrap_or_default().contains("empty"),
        "the log should say why it scaffolded: {body}"
    );
    assert!(h.project.join("package.json").is_file());
    assert!(h.project.join("AGENTS.md").is_file());
    assert!(h.project.join("src/feldspar/README.md").is_file());
    Ok(())
}

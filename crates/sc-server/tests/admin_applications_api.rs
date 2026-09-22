//! Phase 10 "admin API" integration test: the application-management endpoints
//! driven end-to-end over HTTP against a **real Postgres**, through a router whose
//! live [`AppMounts`] the build endpoint mounts into (design §13.1/§13.2/§16).
//!
//! This is the configuration path GOALS asks for — "applications are created in
//! the admin UI" — exercised as the SPA would drive it: list frameworks, create
//! an application, build (+mount) it and watch it serve on its subdomain with no
//! restart, edit + rebuild, then delete and watch the subdomain stop resolving. A
//! failing build comes back as an Application error (a `422`, not a `500`)
//! carrying the bundler's diagnostics, with the previous bundle still serving.
//! Non-admins are rejected throughout.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, SessionStore, create_user};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "blog.example.com";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-adminapp-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A cookie-carrying client addressing one host, echoing CSRF on mutations.
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
    ) -> (StatusCode, Vec<u8>) {
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
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, bytes) = self.raw(method, path, body).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn login(&mut self, email: &str, password: &str) {
        // Load a page first so the CSRF cookie is minted before any mutation.
        self.raw("GET", "/api/auth/status", None).await;
        let (status, _) = self
            .send(
                "POST",
                "/api/login",
                Some(json!({ "email": email, "password": password })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "login should succeed");
    }
}

/// Install a bundler that stamps `marker` into the served `index.html`; when
/// `succeed` is false it exits non-zero with a bundler-style diagnostic instead.
fn write_bundler(root: &Path, marker: &str, succeed: bool) {
    let web = root.join("web");
    std::fs::create_dir_all(web.join("src")).unwrap();
    let script = if succeed {
        format!(
            "#!/bin/sh\n\
             set -e\n\
             mkdir -p dist\n\
             printf '<!doctype html><div id=root>{marker}</div>' > dist/index.html\n"
        )
    } else {
        "#!/bin/sh\necho 'TS2304: Cannot find name Widget' >&2\nexit 2\n".to_owned()
    };
    let path = web.join("build.sh");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The create/update body for the blog app, built from the code framework config.
fn blog_body() -> Value {
    json!({
        "name": "Blog",
        "description": "The company blog",
        "subdomain": "blog",
        "framework": {
            "name": "code",
            "config": {
                "store": "apps",
                "source": "web",
                "output": "web/dist",
                "command": "sh build.sh"
            }
        },
        "extra_frameworks": [],
        "tables": ["posts"],
        "file_stores": ["apps"],
        "apis": [{ "provider": "rest", "mount": "/api" }],
        "static_dirs": [],
        "attributes": {}
    })
}

/// Bring up a catalog (posts table + `apps` store) and the router over a shared,
/// empty live [`AppMounts`] the admin build endpoint mounts into.
async fn setup(tmp: &TempDir) -> sc_error::Result<(Router, Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$; \
             CREATE TABLE posts (\
               id bigint generated by default as identity primary key, \
               title text not null)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    // A static directory reads its store's floor off this table before it serves
    // a byte, so an application that mounts one needs it bootstrapped.
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    // With the base domain the real server gives it: it is what a preview host
    // is derived from, and what an application's default policy lets frame it.
    let apps =
        Arc::new(AppMounts::new(catalog.clone()).with_base_domain(Some(BASE_DOMAIN.to_owned())));
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
    Ok((router, catalog, db))
}

/// **Creating an application deploys it**: the server runs its first build and
/// mounts it, so the subdomain serves without anybody pressing Build and without
/// a restart (the boot path built every stored application, which is what made
/// restarting look like part of creating one).
///
/// The build is deliberately in the background — a first build is `npm install`
/// plus a bundler — so this waits for the subdomain rather than reading it once.
#[tokio::test]
async fn a_created_application_builds_itself_and_serves_without_a_restart() -> sc_error::Result<()>
{
    let tmp = TempDir::new("first-build");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "correct-horse", ROLE_ADMIN).await?;
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    admin.login("admin@example.com", "correct-horse").await;

    // The project is on disk before the application exists — the state a
    // scaffold leaves behind for a framework whose project the server writes.
    write_bundler(tmp.path(), "first", true);

    let (status, created) = admin
        .send("POST", "/api/applications", Some(blog_body()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // Said on the response, because the build outlives it.
    assert_eq!(created["building"], json!(true), "{created}");

    let mut app = Client::new(router.clone(), APP_HOST);
    let mut serving = false;
    for _ in 0..200 {
        let (status, body) = app.raw("GET", "/", None).await;
        if status == StatusCode::OK && body == b"<!doctype html><div id=root>first</div>" {
            serving = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        serving,
        "the created application built itself and serves on its subdomain"
    );
    Ok(())
}

#[tokio::test]
async fn applications_are_managed_over_http_and_serve_without_a_restart() -> sc_error::Result<()> {
    let tmp = TempDir::new("story");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "correct-horse", ROLE_ADMIN).await?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    admin.login("admin@example.com", "correct-horse").await;

    // --- list frameworks: the UI can render a form it knows nothing about -----
    let (status, frameworks) = admin.send("GET", "/api/frameworks", None).await;
    assert_eq!(status, StatusCode::OK);
    let code = frameworks
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("code"))
        .expect("the code framework is listed");
    let spec = code["config_spec"].as_array().unwrap();
    let store = spec
        .iter()
        .find(|f| f["name"] == json!("store"))
        .expect("the `store` setting is described");
    assert_eq!(store["type"], json!("text"));
    assert_eq!(store["required"], json!(true));
    assert_eq!(store["label"], json!("File store"));

    // --- create the application (a saved-but-unbuilt row) ---------------------
    let (status, created) = admin
        .send("POST", "/api/applications", Some(blog_body()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["subdomain"], json!("blog"));
    assert_eq!(created["tables"], json!(["posts"]));
    // The body carries no `triggers` at all — the shape a client written before
    // applications could expose them posts — and the app comes back exposing
    // none, rather than being refused for a field it never heard of.
    assert_eq!(created["triggers"], json!([]));
    let id = created["id"].as_str().expect("a minted id").to_owned();

    // It is listed, and CSP defaults to strict on the round-trip.
    let (_, list) = admin.send("GET", "/api/applications", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["csp"]["default-src"], json!(["'self'"]));
    // A `code` app states its source in five settings; the listing still reports
    // it in one shape, which is the whole point of deriving it server-side.
    assert_eq!(list[0]["source"], json!({ "store": "apps", "path": "web" }));

    // Saved but unbuilt: the subdomain does not serve the app yet.
    let mut app = Client::new(router.clone(), APP_HOST);
    let (_, body) = app.raw("GET", "/", None).await;
    assert_eq!(body, sc_server::BOOTSTRAP_HTML.as_bytes());

    // --- build (+ mount): it serves on its subdomain, no restart --------------
    write_bundler(tmp.path(), "v1", true);
    let (status, report) = admin
        .send("POST", &format!("/api/applications/{id}/build"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["built"], json!(true));

    let (status, body) = app.raw("GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"<!doctype html><div id=root>v1</div>");

    // --- edit + rebuild: the new bundle is what serves ------------------------
    let mut edited = blog_body();
    edited["description"] = json!("Updated blog");
    let (status, _) = admin
        .send("PUT", &format!("/api/applications/{id}"), Some(edited))
        .await;
    assert_eq!(status, StatusCode::OK);

    write_bundler(tmp.path(), "v2", true);
    let (status, _) = admin
        .send("POST", &format!("/api/applications/{id}/build"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = app.raw("GET", "/", None).await;
    assert_eq!(body, b"<!doctype html><div id=root>v2</div>");

    // --- a failing build is an Application error (422), old bundle stays -------
    write_bundler(tmp.path(), "v3", false);
    let (status, err) = admin
        .send("POST", &format!("/api/applications/{id}/build"), None)
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a build failure is a client-fixable Application error, not a 500"
    );
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("TS2304: Cannot find name Widget"),
        "the bundler's diagnostics reach the admin: {err}"
    );
    // The previously built v2 is still serving.
    let (_, body) = app.raw("GET", "/", None).await;
    assert_eq!(body, b"<!doctype html><div id=root>v2</div>");

    // --- delete: the row goes and the subdomain stops resolving ---------------
    let (status, deleted) = admin
        .send("DELETE", &format!("/api/applications/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["deleted"], json!(true));

    let (_, list) = admin.send("GET", "/api/applications", None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    let (_, body) = app.raw("GET", "/", None).await;
    assert_eq!(
        body,
        sc_server::BOOTSTRAP_HTML.as_bytes(),
        "a deleted app's subdomain falls back to the admin"
    );

    // Deleting again is a 404.
    let (status, _) = admin
        .send("DELETE", &format!("/api/applications/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// The `react` framework as the admin API presents it (TODO §2.2): offered
/// first, described by two settings, and choosing its own default CSP.
///
/// Driven over HTTP because that is the only place the claims meet: the admin SPA
/// renders whatever `/api/frameworks` says, and the app row it posts back is
/// where a default policy is or is not applied.
#[tokio::test]
async fn the_react_framework_is_offered_first_and_brings_its_own_defaults() -> sc_error::Result<()>
{
    let tmp = TempDir::new("react");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "correct-horse", ROLE_ADMIN).await?;
    let mut admin = Client::new(router, BASE_DOMAIN);
    admin.login("admin@example.com", "correct-horse").await;

    // --- the pick-list: React first, `code` as the escape hatch --------------
    let (status, frameworks) = admin.send("GET", "/api/frameworks", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = frameworks
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    // Saltcorn UI follows them: compiled in, and constructed rather than built.
    assert_eq!(names, ["react", "code", "saltcorn-ui"]);

    // Each carries the label and the sentence the picker shows. Without these the
    // admin UI could only distinguish the two by special-casing the name `react`,
    // which is exactly the per-framework knowledge §13.3 keeps out of the screens.
    for f in frameworks.as_array().unwrap() {
        assert!(!f["label"].as_str().unwrap_or_default().is_empty(), "{f}");
        assert!(
            f["description"].as_str().unwrap_or_default().len() > 20,
            "{f}"
        );
    }
    assert_eq!(frameworks[0]["label"], json!("React"));
    // The escape hatch says so in its own name.
    assert!(
        frameworks[1]["label"]
            .as_str()
            .unwrap()
            .contains("bring your own")
    );

    // Two settings — the short form §2.4 renders. Only the store must be
    // answered: the project directory defaults to the store root, so an admin
    // whose store holds one application can leave the box empty and save (§2.2).
    let react = &frameworks[0];
    let spec = react["config_spec"].as_array().unwrap();
    let setting_names: Vec<&Value> = spec.iter().map(|f| &f["name"]).collect();
    assert_eq!(setting_names, [&json!("store"), &json!("project")]);
    assert_eq!(spec[0]["required"], json!(true));
    assert_eq!(spec[1]["required"], json!(false));
    assert_eq!(spec[1]["default"], json!(""));
    // The store arrives already resolved to the stores that exist, so the UI
    // renders a select with no query evaluator of its own (§1.6).
    assert_eq!(spec[0]["options"], json!(["apps"]));

    // --- a react app: two settings, and no CSP stated ------------------------
    let body = json!({
        "name": "Todo",
        "subdomain": "todo",
        "framework": {
            "name": "react",
            "config": { "store": "apps", "project": "todo" }
        },
        "tables": ["posts"],
        "file_stores": ["apps"],
        "apis": [{ "provider": "rest", "mount": "/api" }],
    });
    let (status, created) = admin
        .send("POST", "/api/applications", Some(body.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    // The framework's own policy, not the bare strict baseline: an admin who no
    // longer picks the bundler is not asked to work out the policy it needs.
    let csp = &created["csp"];
    assert_eq!(csp["default-src"], json!(["'self'"]));
    assert_eq!(csp["img-src"], json!(["'self'", "data:"]));
    assert_eq!(csp["connect-src"], json!(["'self'"]));
    // Framable by the admin on the base domain, and by nobody else: that is
    // the preview pane beside the builder agent's chat (TODO "The preview
    // pane"), which is an iframe on the admin's own origin.
    assert_eq!(
        csp["frame-ancestors"],
        json!(["'self'", "example.com", "example.com:*"])
    );
    // Nothing unsafe: the tooling decision (§2.1) is what earns this.
    assert!(!created["csp"].to_string().contains("unsafe-"));

    // A stated policy still wins — this is a default, not a fixture.
    let mut strict_only = body.clone();
    strict_only["subdomain"] = json!("todo2");
    strict_only["csp"] = json!({ "default-src": ["'self'"] });
    let (status, plain) = admin
        .send("POST", "/api/applications", Some(strict_only))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(plain["csp"], json!({ "default-src": ["'self'"] }));

    // --- creating it scaffolded the project, with no shell step --------------
    // This is §2.3's whole point: the admin filled in two fields in a browser and
    // a complete Vite project now exists on the server.
    assert!(
        created["scaffolded"]
            .as_str()
            .unwrap_or_default()
            .contains("scaffolded"),
        "{created}"
    );
    assert!(created["scaffold_error"].is_null(), "{created}");
    // The row says where its source is, derived server-side — which is what lets
    // the applications list link into the file manager at an app's source with no
    // framework-specific code in the screen (§2.4).
    assert_eq!(created["source"]["store"], json!("apps"));
    assert_eq!(created["source"]["path"], json!("todo"));

    let project = tmp.path().join("todo");
    assert!(project.join("package.json").is_file());
    assert!(project.join("src/feldspar/hooks.ts").is_file());
    // Generated against the app's declared table.
    assert!(project.join("src/pages/Posts.tsx").is_file());

    // A second app pointed at the *same* project directory is created (the row is
    // valid and saved) but reports why nothing was generated — scaffolding never
    // overwrites, and losing the app the admin just configured over it would be
    // the wrong trade.
    let mut collide = body.clone();
    collide["subdomain"] = json!("todo-again");
    let (status, second) = admin.send("POST", "/api/applications", Some(collide)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(second["scaffolded"].is_null(), "{second}");
    let scaffold_error = second["scaffold_error"].as_str().unwrap_or_default();
    assert!(scaffold_error.contains("not empty"), "{second}");

    // --- the settings a spec cannot describe are still checked on save -------
    let mut traversal = body.clone();
    traversal["subdomain"] = json!("todo3");
    traversal["framework"]["config"]["project"] = json!("../../etc");
    let (status, err) = admin
        .send("POST", "/api/applications", Some(traversal))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Named as the setting the admin typed, not as a build path that escaped the
    // store several layers later.
    let message = err["error"].as_str().unwrap_or_default();
    assert!(message.contains("project"), "{err}");
    assert!(message.contains("directory name"), "{err}");

    // And a `code` setting on a react app is refused rather than ignored.
    let mut extra = body;
    extra["subdomain"] = json!("todo4");
    extra["framework"]["config"]["command"] = json!("make");
    let (status, err) = admin.send("POST", "/api/applications", Some(extra)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    Ok(())
}

#[tokio::test]
async fn an_api_that_would_swallow_the_apps_ui_is_refused_on_save() -> sc_error::Result<()> {
    let tmp = TempDir::new("mounts");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "correct-horse", ROLE_ADMIN).await?;
    let mut admin = Client::new(router, BASE_DOMAIN);
    admin.login("admin@example.com", "correct-horse").await;

    // A provider at `/` claims every path, so the app would answer its own pages
    // from an API that has no endpoint there. Refused where the admin is looking
    // at the form, not discovered in a browser.
    let mut root = blog_body();
    root["apis"] = json!([{ "provider": "rest", "mount": "/" }]);
    let (status, err) = admin.send("POST", "/api/applications", Some(root)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let message = err["error"].as_str().unwrap_or_default();
    assert!(message.contains("claims every path"), "{err}");
    assert!(message.contains("/api"), "{err}");

    // A blank mount normalises to `/`, which is the same trap by accident: the
    // field is required rather than defaulted.
    let mut blank = blog_body();
    blank["apis"] = json!([{ "provider": "rest", "mount": "" }]);
    let (status, err) = admin.send("POST", "/api/applications", Some(blank)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("`mount`"),
        "{err}"
    );

    // Nothing was stored by either attempt.
    let (_, list) = admin.send("GET", "/api/applications", None).await;
    assert_eq!(list.as_array().unwrap().len(), 0, "{list}");

    // The same app on a sub-path saves.
    let (status, _) = admin
        .send("POST", "/api/applications", Some(blog_body()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    Ok(())
}

/// A custom SQL query, added through the admin API exactly as the editor will
/// (TODO "API improvements" Phase 4).
///
/// The claim: the query survives the round trip, it comes back **described** —
/// with the columns Postgres reported, which is what the editor shows the admin
/// and what the client is typed with — and a broken one is a `400` carrying
/// Postgres's own message rather than a stored endpoint that fails on its first
/// call.
#[tokio::test]
async fn a_custom_sql_query_is_saved_described_and_returned() -> sc_error::Result<()> {
    let tmp = TempDir::new("customsql");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "correct-horse", ROLE_ADMIN).await?;
    let mut admin = Client::new(router, BASE_DOMAIN);
    admin.login("admin@example.com", "correct-horse").await;

    let query = json!({
        "name": "titlesLike",
        "description": "Posts whose title matches a pattern",
        "method": "GET",
        "path": "/reports/titles",
        "sql": "SELECT id, title FROM posts WHERE title LIKE :pattern ORDER BY id",
        "params": [{ "name": "pattern", "type": "text" }],
        "min_role": 40
    });
    let mut body = blog_body();
    body["apis"] = json!([{
        "provider": "rest",
        "mount": "/api",
        "config": { "queries": [query.clone()] }
    }]);

    let (status, created) = admin
        .send("POST", "/api/applications", Some(body.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    // The response carries what was *stored*, columns and all — an admin who
    // just saved a query is shown what their client method will return.
    let stored = &created["apis"][0]["config"]["queries"][0];
    assert_eq!(stored["name"], json!("titlesLike"), "{created}");
    assert_eq!(stored["min_role"], json!(40), "{created}");
    assert_eq!(
        stored["columns"],
        json!([
            { "name": "id", "type": "int" },
            { "name": "title", "type": "text" },
        ]),
        "{created}"
    );

    // It survives a reload, and it is on the application's endpoint set.
    let (_, list) = admin.send("GET", "/api/applications", None).await;
    assert_eq!(
        list[0]["apis"][0]["config"]["queries"][0]["columns"], stored["columns"],
        "{list}"
    );
    let app = sc_app::list_applications(&catalog).await?.remove(0);
    let endpoints = sc_app::app_endpoints(&app, &catalog)?;
    assert!(endpoints.find("titlesLike").is_some());

    // A query that will not prepare is refused with Postgres's own message, and
    // the application it was posted with keeps the query it had.
    let mut broken = query.clone();
    broken["sql"] = json!("SELECT titel FROM posts WHERE title LIKE :pattern");
    let mut bad = body;
    bad["apis"][0]["config"]["queries"] = json!([broken]);
    let id = created["id"].as_str().expect("a minted id").to_owned();
    let (status, err) = admin
        .send("PUT", &format!("/api/applications/{id}"), Some(bad))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["error"].as_str().unwrap_or_default().contains("titel"),
        "{err}"
    );
    let (_, list) = admin.send("GET", "/api/applications", None).await;
    assert_eq!(
        list[0]["apis"][0]["config"]["queries"][0]["sql"], query["sql"],
        "the stored query is untouched: {list}"
    );
    Ok(())
}

/// The editor's **Check** button (TODO "API improvements" Phase 5):
/// `describeCustomQuery` prepares a query and reports its columns without
/// storing anything.
///
/// Why it exists, and therefore what this asserts: the columns are what the
/// admin's generated client method will return, and finding them out by saving
/// the whole application means finding out about a typo in one `SELECT` by
/// having every other edit on the screen refused with it. So the same two
/// judgements a save makes — the model's rules, then Postgres's — have to arrive
/// from this one call, and nothing may be written by it.
#[tokio::test]
async fn a_custom_sql_query_is_described_without_being_saved() -> sc_error::Result<()> {
    let tmp = TempDir::new("describe");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "correct-horse", ROLE_ADMIN).await?;
    let mut admin = Client::new(router, BASE_DOMAIN);
    admin.login("admin@example.com", "correct-horse").await;

    let query = json!({
        "name": "titlesLike",
        "method": "GET",
        "path": "/reports/titles",
        "sql": "SELECT id, title FROM posts WHERE title LIKE :pattern ORDER BY id",
        "params": [{ "name": "pattern", "type": "text" }],
        "min_role": 40,
        "tables": ["posts"]
    });
    let (status, described) = admin
        .send("POST", "/api/custom-queries/describe", Some(query.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{described}");
    assert_eq!(
        described["columns"],
        json!([
            { "name": "id", "type": "int" },
            { "name": "title", "type": "text" },
        ]),
        "{described}"
    );
    // Nothing was stored: this is a question, not a save.
    assert!(sc_app::list_applications(&catalog).await?.is_empty());

    // A statement that will not prepare comes back as Postgres's own message.
    let mut broken = query.clone();
    broken["sql"] = json!("SELECT titel FROM posts");
    broken["params"] = json!([]);
    let (status, err) = admin
        .send("POST", "/api/custom-queries/describe", Some(broken))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["error"].as_str().unwrap_or_default().contains("titel"),
        "{err}"
    );

    // …and so does a name the app's own table endpoints already hold, which is
    // decided without a database — the whole refusal a save would give, from
    // the check button, so it takes one round trip rather than two.
    let mut colliding = query;
    colliding["name"] = json!("listPosts");
    let (status, err) = admin
        .send("POST", "/api/custom-queries/describe", Some(colliding))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("listPosts"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn non_admins_are_rejected_from_every_application_endpoint() -> sc_error::Result<()> {
    let tmp = TempDir::new("authz");
    let (router, catalog, _db) = setup(&tmp).await?;
    // A real, non-admin user.
    create_user(&catalog, "reader@example.com", "correct-horse", ROLE_PUBLIC).await?;

    // Anonymous: every endpoint requires authentication.
    let mut anon = Client::new(router.clone(), BASE_DOMAIN);
    for (method, path) in [
        ("GET", "/api/applications"),
        ("POST", "/api/applications"),
        (
            "PUT",
            "/api/applications/00000000-0000-0000-0000-000000000000",
        ),
        (
            "DELETE",
            "/api/applications/00000000-0000-0000-0000-000000000000",
        ),
        (
            "POST",
            "/api/applications/00000000-0000-0000-0000-000000000000/build",
        ),
        // Rewriting an application's source tree is administration too, even
        // though it builds nothing.
        (
            "POST",
            "/api/applications/00000000-0000-0000-0000-000000000000/client",
        ),
        ("GET", "/api/frameworks"),
        // Preparing arbitrary SQL is as much an admin's business as saving it.
        ("POST", "/api/custom-queries/describe"),
    ] {
        // Prime CSRF for mutations.
        anon.raw("GET", "/api/auth/status", None).await;
        let (status, _) = anon.send(method, path, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} must require auth"
        );
    }

    // Logged in but not an admin: the role gate forbids the admin API. (A public
    // user cannot even log into the admin UI, so create it, then confirm the
    // endpoints stay closed to the anonymous session it never got.)
    let mut reader = Client::new(router, BASE_DOMAIN);
    reader.raw("GET", "/api/auth/status", None).await;
    let (status, _) = reader
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "reader@example.com", "password": "correct-horse" })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a non-admin cannot log into the admin UI"
    );
    let (status, _) = reader.send("GET", "/api/applications", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}

/// **A static directory added on the Applications screen serves at once** —
/// without a rebuild and without a restart (TODO "Static directories" §2, design
/// §13.2).
///
/// Everything the router reads off an application's row — its static
/// directories, its CSP, its locales — belongs to the `Application` the mount
/// was made with, and until an edit reached the mount it changed nothing a
/// browser could see. Worse than nothing, in this case: the framework's SPA
/// fallback claims `/*`, so the new mount's path answered **200 with
/// `index.html`**, which looks like it worked. That is what this asserts
/// against, which is why it checks the bytes rather than the status.
#[tokio::test]
async fn a_static_directory_added_by_an_edit_serves_without_a_rebuild() -> sc_error::Result<()> {
    let tmp = TempDir::new("edit-static-dir");
    let (router, catalog, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "correct-horse", ROLE_ADMIN).await?;
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    admin.login("admin@example.com", "correct-horse").await;

    write_bundler(tmp.path(), "before", true);
    let (status, created) = admin
        .send("POST", "/api/applications", Some(blog_body()))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();

    // The image an admin drops into the store, and the SPA that is serving.
    const HERO: &[u8] = b"\x89PNG\r\n\x1a\nhero-pixels";
    std::fs::create_dir_all(tmp.path().join("media")).map_err(sc_error::Error::from)?;
    std::fs::write(tmp.path().join("media/hero.png"), HERO).map_err(sc_error::Error::from)?;

    let mut app = Client::new(router.clone(), APP_HOST);
    let mut serving = false;
    for _ in 0..200 {
        let (status, body) = app.raw("GET", "/", None).await;
        if status == StatusCode::OK && body == b"<!doctype html><div id=root>before</div>" {
            serving = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(serving, "the created application serves its bundle");

    // Nothing claims the path yet, so the SPA fallback answers it: a 200 that is
    // the wrong file.
    let (status, body) = app.raw("GET", "/img/hero.png", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(&body[..], HERO, "no directory is mounted yet");

    // --- the edit -------------------------------------------------------------
    let mut edited = blog_body();
    edited["static_dirs"] = json!([{ "mount": "/img", "store": "apps", "path": "media" }]);
    let (status, updated) = admin
        .send("PUT", &format!("/api/applications/{id}"), Some(edited))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert!(
        updated.get("mount_error").is_none(),
        "the running mount took the edit: {updated}"
    );

    // No build was run and no signal was sent: the next request is the one that
    // sees it.
    let (status, body) = app.raw("GET", "/img/hero.png", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        &body[..],
        HERO,
        "the directory an admin just added serves the file"
    );

    // The bundle is still the one that was built — a record refresh is not a
    // build, and re-running the bundler on every save would be the bug this is
    // the cheap alternative to.
    let (status, body) = app.raw("GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"<!doctype html><div id=root>before</div>");

    // --- and it goes away the same way ----------------------------------------
    let (status, _) = admin
        .send("PUT", &format!("/api/applications/{id}"), Some(blog_body()))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = app.raw("GET", "/img/hero.png", None).await;
    assert_ne!(&body[..], HERO, "the directory an admin removed is gone");

    Ok(())
}

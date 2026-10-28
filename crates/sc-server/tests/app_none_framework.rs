//! An application with **no framework** (`none`): its APIs, its static
//! directories and its streams, and nothing to build — driven over HTTP against
//! a real Postgres and a real file store.
//!
//! What is asserted, end to end:
//!
//! - `none` is offered in the picker, with two settings — the file store its
//!   coding agent works in, and a directory;
//! - creating one **is** its deployment: nothing to build, mounted on save, and
//!   the subdomain serves at once;
//! - its static directory's `index.html` answers `/`, and a path nothing claims
//!   is a 404 rather than somebody's SPA fallback;
//! - it still gets a coding agent, which the trait registry accepts, naming the
//!   application even though there is no build for `check` to run;
//! - a run's preview of it is the application as served, with no bundle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_agent::{AppPreviewer, RunId};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::{FileStore, LocalFileStore};
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "landing.example.com";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-nonefw-{}-{tag}-{:?}",
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
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
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
        let headers = response.headers().clone();
        for raw in headers.get_all(header::SET_COOKIE) {
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
        (status, headers, bytes.to_vec())
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

    async fn text(&mut self, path: &str) -> (StatusCode, HeaderMap, String) {
        let (status, headers, bytes) = self.raw("GET", path, None).await;
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }
}

/// A router over the platform tables, with agents installed, an LLM provider
/// connected (so a builder agent can be created), a `site` store holding a
/// small static site, and an admin signed in on the base domain.
async fn setup(
    tmp: &TempDir,
) -> sc_error::Result<(Client, Router, Arc<AppMounts>, Arc<Catalog>, TestDb)> {
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
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    let agents =
        sc_server::install_agents_on(&catalog, sc_agent::HostCapabilities::default()).await?;

    let site = Arc::new(LocalFileStore::new("site", tmp.path())?);
    catalog.connect_file_store(site.clone())?;
    site.write(
        "public/index.html",
        bytes::Bytes::from_static(b"<!doctype html><h1>Landing</h1>"),
    )
    .await?;
    site.write(
        "public/style.css",
        bytes::Bytes::from_static(b"h1 { color: teal }"),
    )
    .await?;

    sc_llm::save_llm_provider(
        &catalog,
        &sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-test"),
    )
    .await?;
    let provider = sc_llm::require_llm_provider(&catalog, "house").await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_agents(agents),
    );
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    admin.send("GET", "/api/auth/status", None).await;
    let (status, _) = admin
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok((admin, router, apps, catalog, db))
}

#[tokio::test]
async fn an_application_with_no_framework_serves_its_files_and_has_a_coding_agent()
-> sc_error::Result<()> {
    let tmp = TempDir::new("serve");
    let (mut admin, router, apps, _catalog, _db) = setup(&tmp).await?;

    // --- the picker offers it, with a store and a directory -------------------
    let (status, frameworks) = admin.send("GET", "/api/frameworks", None).await;
    assert_eq!(status, StatusCode::OK);
    let none = frameworks
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("none"))
        .unwrap_or_else(|| panic!("`none` is not offered: {frameworks}"))
        .clone();
    assert_eq!(none["label"], json!("None"));
    let settings: Vec<&str> = none["config_spec"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(settings, ["store", "source"]);
    assert_eq!(none["file_store_settings"], json!(["store"]));

    // --- creating it is deploying it ------------------------------------------
    let (status, created) = admin
        .send(
            "POST",
            "/api/applications",
            Some(json!({
                "name": "Landing",
                "description": "",
                "subdomain": "landing",
                "framework": { "name": "none", "config": { "store": "site", "source": "public" } },
                "extra_frameworks": [],
                "tables": [],
                "file_stores": ["site"],
                "triggers": [],
                // With no UI to lose, the API may claim a sub-path of its own
                // or the root; here it has one, so `/` is the site's.
                "apis": [{ "provider": "rest", "mount": "/api" }],
                "static_dirs": [{ "mount": "/", "store": "site", "path": "public" }],
                "attributes": {}
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["builds"], json!(false), "{created}");
    assert_eq!(created["mounted"], json!(true), "{created}");
    assert_eq!(created.get("building"), None, "{created}");
    assert_eq!(created.get("scaffold_error"), None, "{created}");
    // Where its files are, for the file manager and the IDE.
    assert_eq!(
        created["source"],
        json!({ "store": "site", "path": "public" })
    );

    // --- and it serves ---------------------------------------------------------
    let mut visitor = Client::new(router.clone(), APP_HOST);
    let (status, headers, body) = visitor.text("/").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("<h1>Landing</h1>"), "{body}");
    assert!(
        headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let (status, _, body) = visitor.text("/style.css").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "h1 { color: teal }");
    // The API beside it.
    let (status, _, body) = visitor.text("/api/whoami").await;
    assert_ne!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        !body.contains("Landing"),
        "the site answered the API: {body}"
    );

    // A path nothing claims is not found — there is no framework behind the
    // directory to fall back to.
    let (status, _, _) = visitor.text("/nope.html").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // --- the coding agent it was created with ----------------------------------
    assert_eq!(created["agent"], json!("build-landing"), "{created}");
    assert_eq!(created.get("agent_error"), None, "{created}");
    let (status, agents) = admin.send("GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    let agent = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!("build-landing"))
        .unwrap()
        .clone();
    // Usable: `coding` accepted an `application` with no build step.
    assert_eq!(agent["error"], Value::Null, "{agent}");
    let coding = &agent["traits"][0];
    assert_eq!(coding["trait"], json!("coding"));
    assert_eq!(coding["config"]["store"], json!("site"));
    assert_eq!(coding["config"]["root"], json!("public"));
    assert_eq!(coding["config"]["application"], json!("landing"));
    assert_eq!(coding["config"]["may_edit"], json!(true));
    let prompt = agent["system_prompt"].as_str().unwrap();
    assert!(prompt.contains("no build"), "{prompt}");
    assert!(prompt.contains("`/` serves `public`"), "{prompt}");

    // --- a run's preview is the application as served --------------------------
    let run = RunId::new();
    let preview = AppPreviewer::mount_preview(apps.as_ref(), run, "landing", Path::new("")).await?;
    assert_eq!(preview.subdomain, "landing");
    let token = admin.cookies["sc_session"].clone();
    apps.allow_preview_token(run, &token);
    let mut previewer = Client::new(router, &preview.host);
    previewer
        .cookies
        .insert("sc_session".to_owned(), token.clone());
    let (status, _, body) = previewer.text("/").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("<h1>Landing</h1>"), "{body}");
    Ok(())
}

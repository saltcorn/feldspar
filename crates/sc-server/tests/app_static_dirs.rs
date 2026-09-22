//! An application's **static directories** are served (design §13.2, TODO
//! "Static directories" §2/§3).
//!
//! `Application::static_dirs` has been stored, edited and validated since the
//! MVP, and until this milestone nothing served it: an admin filled the form in
//! and got a 404. What is asserted here, through the real router against a real
//! Postgres, is the promise being kept and the four ways it must not be kept too
//! generously:
//!
//! - a file under the mount is served with the framework's own content type, an
//!   ETag, and a 304 on the second request;
//! - a path that walks out of the directory — spelled plainly or percent-encoded
//!   — is the same 404 an unknown path gets;
//! - **a mount is not a grant**: a file closed to a guest is 404 for a guest and
//!   served to an admin, through the same `sc_files::check_access` every other
//!   reader goes through;
//! - an **API provider** on the same prefix still wins, and the framework's SPA
//!   fallback still answers a path no directory claims.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_app::{
    ApiConfig, Application, CodeFramework, FrameworkRef, StaticDir, app_source_from_config,
    build_application,
};
use sc_auth::{ROLE_ADMIN, SessionStore, create_user};
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::{FileMeta, FileStore, LocalFileStore};
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::json;
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "blog.example.com";

/// The bytes of the image the application's landing page points at. A real PNG
/// header, so the content type served is checkable against something.
const HERO: &[u8] = b"\x89PNG\r\n\x1a\nhero-pixels";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-staticdir-{}-{tag}-{:?}",
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

/// A cookie-carrying client addressing one host, able to read raw bytes and the
/// response headers a cache would.
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
        extra: &[(&str, String)],
        body: Option<Vec<u8>>,
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
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
        }
        for (name, value) in extra {
            builder = builder.header(*name, value);
        }
        let request = builder.body(Body::from(body.unwrap_or_default())).unwrap();
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
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
    }

    async fn get(&mut self, path: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
        self.raw("GET", path, &[], None).await
    }

    /// A conditional GET — what a browser sends on the second page load.
    async fn get_if_none_match(
        &mut self,
        path: &str,
        etag: &str,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        self.raw("GET", path, &[("if-none-match", etag.to_owned())], None)
            .await
    }

    async fn login(&mut self, email: &str, password: &str) -> StatusCode {
        self.raw("GET", "/", &[], None).await;
        let body = serde_json::to_vec(&json!({ "email": email, "password": password })).unwrap();
        let (status, _, _) = self
            .raw(
                "POST",
                "/api/login",
                &[("content-type", "application/json".to_owned())],
                Some(body),
            )
            .await;
        status
    }
}

/// The app's source tree: a git repo whose stand-in bundler emits an SPA.
fn write_app_source(root: &Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let web = root.join("web");
    std::fs::create_dir_all(web.join("src")).unwrap();
    let script = web.join("build.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         set -e\n\
         test -f src/client.ts\n\
         mkdir -p dist/assets\n\
         printf '<!doctype html><div id=root></div>' > dist/index.html\n\
         cp src/client.ts dist/assets/client.js\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn code_framework() -> FrameworkRef {
    FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts")
}

/// The application: an API at `/api`, images at `/img`, and — deliberately — a
/// second directory mounted **over the API's own prefix**, which the router must
/// never let win (§2).
fn blog_app() -> Application {
    Application::new("Blog", "blog", code_framework())
        .with_table(TableId("posts".to_owned()))
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_file_store(FileStoreId("assets".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_static_dir(StaticDir::new(
            "/img",
            FileStoreId("assets".to_owned()),
            "media",
        ))
        .with_static_dir(StaticDir::new(
            "/api",
            FileStoreId("assets".to_owned()),
            "media",
        ))
}

/// Build and mount the app and return the router, plus the `assets` store the
/// static directories read from.
async fn setup(
    tmp: &TempDir,
) -> sc_error::Result<(Router, Arc<Catalog>, Arc<LocalFileStore>, TestDb)> {
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
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;

    write_app_source(tmp.path());
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    // The store the images live in — not the app-source store, which is the
    // point of a static directory naming one.
    let assets_dir = tmp.path().join("assets-store");
    std::fs::create_dir_all(&assets_dir).map_err(sc_error::Error::from)?;
    let assets = Arc::new(LocalFileStore::new("assets", &assets_dir)?);
    catalog.connect_file_store(assets.clone())?;

    assets
        .write("media/hero.png", bytes::Bytes::from_static(HERO))
        .await?;
    assets
        .write(
            "media/private/plans.txt",
            bytes::Bytes::from_static(b"the secret plans"),
        )
        .await?;
    // What the `/api` directory would serve if it were ever reached: the API
    // provider answers `/api/posts`, so these bytes must never appear.
    assets
        .write("media/posts", bytes::Bytes::from_static(b"STATIC"))
        .await?;
    // And what a directory traversal would reach if `..` escaped the mount.
    assets
        .write(
            "outside.txt",
            bytes::Bytes::from_static(b"not under the mount"),
        )
        .await?;

    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &blog_app(), &source, None).await?;
    let framework = Arc::new(CodeFramework::new("code", report.bundle));
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    apps.mount(MountedApp::new(blog_app(), framework, &catalog)?)?;

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
    Ok((router, catalog, assets, db))
}

#[tokio::test]
async fn a_static_directory_serves_its_files_and_nothing_else() -> sc_error::Result<()> {
    let tmp = TempDir::new("serve");
    let (router, catalog, assets, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;

    let mut visitor = Client::new(router.clone(), APP_HOST);

    // --- the promise ----------------------------------------------------------
    let (status, headers, body) = visitor.get("/img/hero.png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], HERO);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "image/png",
        "the framework's own answer for a .png"
    );
    // On the application's own origin, under the application's own CSP.
    assert_eq!(
        headers
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap(),
        "default-src 'self'"
    );

    // --- the second page load -------------------------------------------------
    let etag = headers
        .get(header::ETAG)
        .expect("an image is served with an ETag")
        .to_str()
        .unwrap()
        .to_owned();
    let (status, _, body) = visitor.get_if_none_match("/img/hero.png", &etag).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty(), "a 304 carries no bytes");
    // A tag that is not this file's is not a match.
    let (status, _, _) = visitor
        .get_if_none_match("/img/hero.png", "\"0000000000000000\"")
        .await;
    assert_eq!(status, StatusCode::OK);

    // --- the mount is a window, not a door ------------------------------------
    for escape in [
        "/img/../outside.txt",
        "/img/a/../../outside.txt",
        "/img/%2e%2e/outside.txt",
    ] {
        let (status, _, body) = visitor.get(escape).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{escape} must not escape");
        assert!(
            !String::from_utf8_lossy(&body).contains("not under the mount"),
            "{escape} served the file above the directory"
        );
    }
    // A file the directory simply does not have is the same answer.
    let (status, _, _) = visitor.get("/img/nope.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // --- an API on the same prefix still wins ---------------------------------
    let (status, headers, body) = visitor.get("/api/posts").await;
    assert_ne!(
        &body[..],
        b"STATIC",
        "the static directory answered the API"
    );
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .contains("json"),
        "the provider answered: {status}"
    );

    // --- the framework still answers what no directory claims -----------------
    let (status, _, body) = visitor.get("/about").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains("id=root"),
        "the SPA fallback still claims /*"
    );

    // --- a mount is not a grant -----------------------------------------------
    // The folder is closed to everyone below role 40; the mount does not open it.
    assets
        .set_meta(
            "media/private",
            &FileMeta {
                min_role: Some(40),
                ..Default::default()
            },
        )
        .await?;

    let (status, _, body) = visitor.get("/img/private/plans.txt").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a closed file is not found, not forbidden — a 403 would confirm it exists"
    );
    assert!(!String::from_utf8_lossy(&body).contains("secret plans"));

    let mut admin = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let (status, headers, body) = admin.get("/img/private/plans.txt").await;
    assert_eq!(status, StatusCode::OK, "role 1 clears the folder's rule");
    assert_eq!(&body[..], b"the secret plans");
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8"
    );

    // --- a static directory is read, not written ------------------------------
    let (status, _, _) = admin
        .raw("PUT", "/img/hero.png", &[], Some(b"replacement".to_vec()))
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(&assets.read("media/hero.png").await?[..], HERO);

    Ok(())
}

/// The store a static directory names must be one the application declares
/// (§5): the declared subset is the whole truth about which stores an
/// application touches, and the router enforces it as well as the save does.
#[tokio::test]
async fn a_directory_naming_an_undeclared_store_serves_nothing() -> sc_error::Result<()> {
    let tmp = TempDir::new("undeclared");
    let (router, catalog, _assets, _db) = setup(&tmp).await?;

    // Remount the same application with `assets` taken out of its subset, the
    // static directories left pointing at it.
    let app = {
        let mut app = blog_app();
        app.file_stores
            .retain(|s| s != &FileStoreId("assets".to_owned()));
        app
    };
    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &app, &source, None).await?;
    let framework = Arc::new(CodeFramework::new("code", report.bundle));
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    apps.mount(MountedApp::new(app, framework, &catalog)?)?;
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router2 = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps,
    )?;
    drop(router);

    let mut visitor = Client::new(router2, APP_HOST);
    let (status, _, body) = visitor.get("/img/hero.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ne!(&body[..], HERO);
    Ok(())
}

/// **The tool and the router agree, over the same fixture** (TODO "Static
/// directories" §6, 3.3).
///
/// This is the half of the milestone's definition of done that `cargo test`
/// can reach: the application's coding agent calls `list_assets_*`, is told
/// `/img/hero.png`, and that URL — fetched through the real router with no
/// human having typed it — is the file. The two halves share this file's
/// fixture deliberately: a URL the tool invents and a URL the router answers
/// cannot drift apart while one test asserts both.
///
/// And the access rule travels with it. The folder closed to a guest is not
/// *named* to a guest either, because listing goes through the same
/// `check_access` the serving does — a mount is not a grant on the way out or
/// on the way in.
#[tokio::test]
async fn list_assets_returns_the_urls_the_router_serves() -> sc_error::Result<()> {
    use sc_agent::{RunCaller, RunId, RunMode, ToolsContext, TraitContext};
    use sc_core_traits::builtin_traits;
    use sc_files::ROLE_PUBLIC;
    use sc_types::Attrs;

    let tmp = TempDir::new("list-assets");
    let (router, catalog, assets, _db) = setup(&tmp).await?;
    // The tool resolves the application from its stored row, as `check` does.
    // Stored without the fixture's deliberately-shadowed `/api` directory,
    // because `save_application` refuses that one (§5) — which is the point:
    // the only directories a *saved* application has are ones the router can
    // reach, so the tool cannot be handed a mount that an API has taken.
    sc_app::bootstrap(&catalog).await?;
    let stored = {
        let mut app = blog_app();
        app.static_dirs.retain(|d| d.mount != "/api");
        app
    };
    sc_app::save_application(&catalog, &stored).await?;
    // Closed to everyone below role 40 — the same folder the serving half uses.
    assets
        .set_meta(
            "media/private",
            &FileMeta {
                min_role: Some(40),
                ..Default::default()
            },
        )
        .await?;

    let traits = builtin_traits()?;
    let coding = traits.require("coding")?.clone();
    let config: Attrs = [
        ("store".to_owned(), json!("apps")),
        ("root".to_owned(), json!("web")),
        (sc_core_traits::CFG_APPLICATION.to_owned(), json!("blog")),
    ]
    .into_iter()
    .collect();

    // Offered because the trait names an application — and named after the
    // scope, like every other tool this trait contributes.
    let tool = sc_core_traits::tool_names::list_assets(&sc_core_traits::FileScope {
        store: "apps".to_owned(),
        root: "web".to_owned(),
    });
    assert_eq!(tool, "list_assets_apps_web");
    static CAPABILITIES: std::sync::LazyLock<sc_llm::ModelCapabilities> =
        std::sync::LazyLock::new(|| sc_llm::ModelCapabilities::built_in("", ""));
    let offered: Vec<String> = coding
        .tools(
            &ToolsContext::new(&catalog, RunMode::Act, &CAPABILITIES),
            &config,
        )
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(offered.contains(&tool), "{offered:?}");

    // …and absent from a trait that names no application: there is nothing for
    // it to list.
    let no_app: Attrs = [
        ("store".to_owned(), json!("apps")),
        ("root".to_owned(), json!("web")),
    ]
    .into_iter()
    .collect();
    let without: Vec<String> = coding
        .tools(
            &ToolsContext::new(&catalog, RunMode::Act, &CAPABILITIES),
            &no_app,
        )
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(!without.contains(&tool), "{without:?}");

    let call = async |args: serde_json::Value, role: u8| -> sc_error::Result<serde_json::Value> {
        let caller = RunCaller {
            role,
            ..RunCaller::system()
        };
        let mut state = serde_json::Value::Null;
        let mut ctx = TraitContext {
            catalog: &catalog,
            caller: &caller,
            agent: "builder",
            run: RunId::new(),
            mode: RunMode::Act,
            trait_state: &mut state,
            evaluator: None,
            triggers: None,
            delegate: None,
            previews: None,
            browser: None,
            signals: Vec::new(),
            images: Vec::new(),
        };
        coding.call(&config, &tool, &args, &mut ctx).await
    };

    // --- what a guest is told, and what the router then serves ----------------
    let listed = call(json!({}), ROLE_PUBLIC).await?;
    let rows = listed["assets"].as_array().unwrap().clone();
    assert_eq!(listed["truncated"], json!(false), "{listed}");
    let urls: Vec<&str> = rows.iter().map(|r| r["url"].as_str().unwrap()).collect();
    assert!(urls.contains(&"/img/hero.png"), "{listed}");
    // The file behind the closed folder is not named to somebody who may not
    // read it, and neither is the one under the `/api` mount's own store path
    // by any URL that is not that mount's.
    assert!(
        !urls.iter().any(|u| u.contains("plans.txt")),
        "a closed file was listed: {listed}"
    );

    let hero = rows
        .iter()
        .find(|r| r["url"] == json!("/img/hero.png"))
        .expect("the hero image");
    assert_eq!(hero["path"], json!("media/hero.png"));
    assert_eq!(hero["store"], json!("assets"));
    assert_eq!(hero["content_type"], json!("image/png"));
    assert_eq!(hero["size"], json!(HERO.len()));

    // **The point of the milestone**: every URL the tool handed the model is a
    // URL the router answers with the bytes the tool described.
    let mut visitor = Client::new(router.clone(), APP_HOST);
    for row in &rows {
        let url = row["url"].as_str().unwrap();
        let (status, headers, body) = visitor.get(url).await;
        assert_eq!(status, StatusCode::OK, "the tool named {url}");
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            row["content_type"].as_str().unwrap(),
            "{url}"
        );
        assert_eq!(body.len() as u64, row["size"].as_u64().unwrap(), "{url}");
    }
    assert_eq!(
        visitor.get("/img/hero.png").await.2,
        HERO.to_vec(),
        "the URL the agent would write into the JSX"
    );

    // --- an admin sees the closed file, and its URL works for them -------------
    let listed = call(json!({}), ROLE_ADMIN).await?;
    let urls: Vec<&str> = listed["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["url"].as_str().unwrap())
        .collect();
    assert!(
        urls.contains(&"/img/private/plans.txt"),
        "role 1 clears the folder's rule: {listed}"
    );

    // --- the glob and the cap behave as `find_files`' do -----------------------
    let pngs = call(json!({"pattern": "*.png"}), ROLE_ADMIN).await?;
    let urls: Vec<&str> = pngs["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["url"].as_str().unwrap())
        .collect();
    assert_eq!(urls, ["/img/hero.png"], "{pngs}");
    // A directory narrows it the same way.
    let under = call(json!({"dir": "private"}), ROLE_ADMIN).await?;
    let urls: Vec<&str> = under["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["url"].as_str().unwrap())
        .collect();
    assert_eq!(urls, ["/img/private/plans.txt"], "{under}");

    // The cap says how many there were and how to narrow, as `find_files` does.
    let mut capped = config.clone();
    capped.insert("max_results".to_owned(), json!(1));
    let caller = RunCaller {
        role: ROLE_ADMIN,
        ..RunCaller::system()
    };
    let mut state = serde_json::Value::Null;
    let mut ctx = TraitContext {
        catalog: &catalog,
        caller: &caller,
        agent: "builder",
        run: RunId::new(),
        mode: RunMode::Act,
        trait_state: &mut state,
        evaluator: None,
        triggers: None,
        delegate: None,
        previews: None,
        browser: None,
        signals: Vec::new(),
        images: Vec::new(),
    };
    let one = coding.call(&capped, &tool, &json!({}), &mut ctx).await?;
    assert_eq!(one["assets"].as_array().unwrap().len(), 1, "{one}");
    assert_eq!(one["truncated"], json!(true), "{one}");
    assert!(
        one["note"].as_str().unwrap().contains("Narrow the list"),
        "{one}"
    );
    Ok(())
}

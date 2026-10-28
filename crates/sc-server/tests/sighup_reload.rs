//! The signal itself: `SIGHUP` reaches the reload, in its own test binary.
//!
//! [`live_mounting`](../live_mounting.rs) tests what a reload *does* by calling
//! [`reload_all`](sc_server::reload_all) directly. What that cannot test is the
//! wiring — that a listener is installed, that it survives the first signal, and
//! that `kill -HUP` therefore does anything at all — which is exactly the part
//! that would break silently.
//!
//! It is a **separate binary** on purpose: a signal is process-wide, so raising
//! one inside a test binary shared with other tests would fire their reloads too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use sc_app::{Application, FrameworkRef, bootstrap, save_application};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, FileStoreId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, ServerConfig, admin_handlers, build_router_with_apps, mount_all, spawn_sighup_reload,
};
use sc_test_harness::TestDb;
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "blog.example.com";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let dir = std::env::temp_dir().join(format!("sc-server-sighup-{}", std::process::id()));
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

/// A project whose bundle is already built: a `dist/` holding `marker`, and a
/// build command that does nothing, so the initial mount succeeds without a real
/// bundler and every later change to `dist/` is one this test made by hand — the
/// developer's `npm run build`.
fn write_project(root: &Path, marker: &str) {
    let web = root.join("web");
    std::fs::create_dir_all(web.join("dist")).unwrap();
    std::fs::write(web.join("build.sh"), "#!/bin/sh\ntrue\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(web.join("build.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    write_dist(root, marker);
}

/// Overwrite the built `index.html` — what running the bundler by hand amounts
/// to, as far as the server is concerned.
fn write_dist(root: &Path, marker: &str) {
    std::fs::write(
        root.join("web/dist/index.html"),
        format!("<!doctype html><div id=root>{marker}</div>"),
    )
    .unwrap();
}

async fn get(router: &Router, path: &str) -> Vec<u8> {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::HOST, APP_HOST)
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn a_hangup_reloads_the_bundle_a_build_left_on_disk() -> sc_error::Result<()> {
    let tmp = TempDir::new();
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    bootstrap(&catalog).await?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    write_project(tmp.path(), "before");
    save_application(
        &catalog,
        &Application::new(
            "Blog",
            "blog",
            FrameworkRef::new("code")
                .with("store", "apps")
                .with("source", "web")
                .with("output", "web/dist")
                .with("command", "sh build.sh"),
        )
        .with_file_store(FileStoreId("apps".to_owned())),
    )
    .await?;

    let apps = Arc::new(AppMounts::new(catalog.clone()));
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
    mount_all(&apps).await;
    assert_eq!(
        get(&router, "/").await,
        b"<!doctype html><div id=root>before</div>"
    );

    // What `serve` does at boot, and the whole point of this test.
    spawn_sighup_reload(apps.clone());

    // The developer builds. Nothing changes yet — and `SIGHUP`'s default
    // disposition is to *terminate*, so if the listener above were not installed
    // this test would not fail, it would disappear.
    write_dist(tmp.path(), "after");
    let pid = std::process::id().to_string();
    let status = std::process::Command::new("kill")
        .args(["-HUP", &pid])
        .status()
        .expect("kill");
    assert!(status.success());

    // The reload is asynchronous — a signal is not a request — so this is the
    // wait an agent's script does, bounded so a failure is a failure rather than
    // a hang.
    let mut served = Vec::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        served = get(&router, "/").await;
        if served == b"<!doctype html><div id=root>after</div>" {
            return Ok(());
        }
    }
    panic!(
        "SIGHUP should have reloaded the bundle within two seconds; served {}",
        String::from_utf8_lossy(&served)
    );
}

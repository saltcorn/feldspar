//! A Saltcorn UI application on a server without the Saltcorn UI bundle fails
//! **on mount**, with a sentence naming the bundle (TODO "Saltcorn UI" 2.6) — and
//! is therefore not mounted, so no request ever reaches a half-working app.

use std::sync::Arc;

use sc_app::{Application, FrameworkRef};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{AppMounts, build_and_mount};
use sc_test_harness::TestDb;

#[tokio::test]
async fn a_saltcorn_ui_app_fails_to_mount_naming_the_missing_bundle() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    let app = Application::new("Books", "books", FrameworkRef::new("saltcorn-ui"));

    // A binary built with SC_BUILD_ADMIN=0 records no directory at all.
    let apps = AppMounts::new(catalog.clone());
    let msg = build_and_mount(&apps, app.clone())
        .await
        .expect_err("a Saltcorn UI app cannot mount without the bundle")
        .to_string();
    assert!(msg.contains("`books`"), "the application is named: {msg}");
    assert!(
        msg.contains("ui/saltcorn-ui/dist"),
        "the bundle is named: {msg}"
    );
    assert!(
        msg.contains("SC_BUILD_ADMIN"),
        "the way out is named: {msg}"
    );
    assert!(apps.get("books").is_none(), "nothing was mounted");

    // A packaged tree whose directory lost the runtime names the file.
    let empty = std::env::temp_dir().join(format!("sc-saltcorn-ui-empty-{}", std::process::id()));
    std::fs::create_dir_all(&empty)?;
    let apps = AppMounts::new(catalog).with_saltcorn_ui_dir(Some(empty.clone()));
    let msg = build_and_mount(&apps, app)
        .await
        .expect_err("a Saltcorn UI app cannot mount without the runtime")
        .to_string();
    std::fs::remove_dir_all(&empty).ok();
    assert!(
        msg.contains("view-runtime.js"),
        "the missing file is named: {msg}"
    );
    assert!(apps.get("books").is_none(), "nothing was mounted");
    Ok(())
}

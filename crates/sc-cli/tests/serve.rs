//! Smoke test for `feldspar serve` (Phase 7): the CLI's boot path brings the
//! data layer up against a **real Postgres database** and the assembled server
//! answers its health route.
//!
//! This drives the same steps the binary does — [`DbConfig`] → [`connect_catalog`]
//! (connect + introspect + ensure the users table) → the admin router — but
//! stops short of binding a TCP port, exercising the router with a `oneshot`
//! request instead.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sc_auth::SessionStore;
use sc_cli::{DbConfig, connect_catalog, connect_file_stores, connect_stored_file_stores};
use sc_server::{AppMounts, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use tower::ServiceExt;

#[tokio::test]
async fn serve_boots_against_a_db_and_answers_health() -> sc_error::Result<()> {
    let db = TestDb::new().await?;

    // The full CLI boot path: parse a URL, connect, init the catalog, bootstrap.
    let cfg = DbConfig::from_url(db.url());
    let catalog = connect_catalog(&cfg).await?;

    // The users table now exists (bootstrap ran as part of connect_catalog).
    assert!(
        catalog.get(sc_auth::USERS_TABLE)?.is_some(),
        "connect_catalog should have bootstrapped the users table"
    );

    // ...and so does the applications table. The admin UI lists applications on
    // load — the exact `SELECT * FROM _fd_applications` that failed with
    // `relation "_fd_applications" does not exist` before this bootstrap was
    // wired into the boot path. It must now succeed (an empty list, not an error).
    assert!(
        catalog.get(sc_app::APPLICATIONS_TABLE)?.is_some(),
        "connect_catalog should have bootstrapped the applications table"
    );
    assert!(
        sc_app::list_applications(&catalog).await?.is_empty(),
        "a freshly bootstrapped applications table lists no applications"
    );

    // Assemble the server exactly as `serve` does, then hit /health.
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog, apps),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
    )?;

    let response = router
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["status"], "ok");

    Ok(())
}

#[tokio::test]
async fn connect_to_an_unreachable_database_fails_loudly() {
    // A port nothing listens on: connecting must return an error, and it must
    // carry the redacted target for context (no silent half-boot).
    let cfg = DbConfig::from_url("postgres://saltcorn:secret@127.0.0.1:1/saltcorn_test");
    // `Arc<Catalog>` is not `Debug`, so match rather than `expect_err`.
    let msg = match connect_catalog(&cfg).await {
        Ok(_) => panic!("connecting to a dead port must fail"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("127.0.0.1:1/saltcorn_test"),
        "error should name the target: {msg}"
    );
    assert!(!msg.contains("secret"), "error leaked the password: {msg}");
}

/// Phase 1.3: the boot path connects stored file stores, and `--file-store`
/// stays an ephemeral convenience that must never silently shadow one.
#[tokio::test]
async fn boot_connects_stored_stores_and_refuses_a_flag_that_shadows_one() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let cfg = DbConfig::from_url(db.url());
    let catalog = connect_catalog(&cfg).await?;

    // `connect_catalog` bootstraps the file-stores table alongside the others,
    // so a legacy database gains it on first serve with no migration step.
    assert!(
        catalog.get(sc_catalog::FILE_STORES_TABLE)?.is_some(),
        "connect_catalog should have bootstrapped the file stores table"
    );

    let dir = std::env::temp_dir().join(format!("sc-cli-store-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.to_string_lossy().into_owned();

    sc_catalog::save_file_store(&catalog, &sc_files::FileStoreDef::local("docs", &path)).await?;

    // Boot order: stored stores first …
    let report = connect_stored_file_stores(&catalog).await?;
    assert_eq!(report.connected, ["docs"]);
    assert!(
        catalog
            .file_store("docs", sc_catalog::Unchecked::system("test"))?
            .is_some()
    );

    // … then the flags. A flag naming a store that is already configured is a
    // startup error, not a silent override: `connect_file_store` replaces on a
    // repeated name, so without this the flag would shadow the admin's store and
    // nothing anywhere would say why their edits had no effect.
    let clash = connect_file_stores(&catalog, &["docs=/tmp".to_owned()]).unwrap_err();
    assert!(clash.to_string().contains("docs"), "{clash}");

    // A flag with its own name still works, and is not persisted — it exists
    // only for this process.
    connect_file_stores(&catalog, &[format!("scratch={path}")])?;
    assert!(
        catalog
            .file_store("scratch", sc_catalog::Unchecked::system("test"))?
            .is_some()
    );
    assert_eq!(
        sc_catalog::list_file_stores(&catalog).await?.len(),
        1,
        "the flag must not have written a definition"
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

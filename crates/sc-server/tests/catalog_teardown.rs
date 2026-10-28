//! A server test's catalog, and everything installed on it, is freed when the
//! test ends — `Catalog::teardown`, run by the harness's `on_teardown`.
//!
//! The services a server installs on its catalog hold the catalog back, so
//! without the teardown a test's catalog, its formula isolate (two threads) and
//! the runtime handle that isolate anchors to outlived the test, and
//! `sc-server`'s integration binary ran out of file descriptors at a desktop
//! session's soft limit of 1024.

use std::sync::{Arc, Weak};

use axum::body::Body;
use axum::http::Request;
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_server::{
    AppMounts, ServerConfig, admin_handlers, build_router_with_apps, default_js_evaluator,
    install_triggers, start_workflow_engine,
};
use sc_test_harness::TestDb;
use tower::ServiceExt;

/// What one test would build — the agents, models and triggers, the workflow
/// engine, the mount registry, the router — exercised once, and the weak ends
/// of the catalog and the evaluator handed back.
async fn a_server_test(teardown: bool) -> Result<(Weak<Catalog>, Weak<dyn sc_expr::JsEvaluator>)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    if teardown {
        db.on_teardown(catalog.teardown_hook());
    }
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let evaluator = default_js_evaluator();
    let weak_evaluator = Arc::downgrade(&evaluator);
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    let (_engine, _handle) = start_workflow_engine(&catalog, &dispatcher);
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(evaluator)
            .with_triggers(dispatcher.clone())
            .with_agents(agents)
            .with_models(models),
    );
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;
    // A formula evaluated, so the isolate thread really is running.
    let request = Request::get("/api/auth/status")
        .body(Body::empty())
        .map_err(|e| sc_error::Error::msg(e.to_string()))?;
    let _ = router.oneshot(request).await;
    let _ = catalog.formula_evaluator();
    Ok((Arc::downgrade(&catalog), weak_evaluator))
}

/// Run one test on its own runtime, as `#[tokio::test]` does, and report what
/// is still alive once that runtime has shut down.
fn after_the_test(teardown: bool) -> (bool, bool) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (catalog, evaluator) = runtime.block_on(a_server_test(teardown)).unwrap();
    drop(runtime);
    (catalog.upgrade().is_some(), evaluator.upgrade().is_some())
}

#[test]
fn a_torn_down_catalog_and_its_evaluator_are_freed_when_the_test_ends() {
    assert_eq!(
        after_the_test(true),
        (false, false),
        "(catalog, evaluator) alive"
    );
}

/// The control: without the teardown both survive, so the test above is
/// measuring the teardown and not something that frees them anyway.
#[test]
fn without_the_teardown_the_catalog_and_its_evaluator_survive_the_test() {
    assert_eq!(
        after_the_test(false),
        (true, true),
        "(catalog, evaluator) alive"
    );
}

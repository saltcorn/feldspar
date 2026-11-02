//! `feldspar build-agent` against a real server on a real port
//! (`TODO-build-hosts.md` B1.3): the agent hears its targets, checks them on
//! this machine and reports back; a removed host's agent is told to stop.

use std::sync::Arc;

use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_cli::build_agent::{Agent, HeartbeatError};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{AppMounts, ServerConfig, admin_handlers, build_router_with_apps};
use sc_test_harness::TestDb;

#[tokio::test]
async fn an_agent_hears_its_targets_and_reports_on_them() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| sc_error::Error::msg(e.to_string()))?;
    let server = format!(
        "http://{}",
        listener
            .local_addr()
            .map_err(|e| sc_error::Error::msg(e.to_string()))?
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let (host, token) = sc_app::add_build_host(&catalog, "Mac mini", &["ios".into()]).await?;
    let mut agent = Agent::new(&server, token)?;

    // The first heartbeat hears the targets, the second reports on them.
    let first = agent.heartbeat().await.expect("the server answers");
    assert_eq!(first.name, "Mac mini");
    assert_eq!(first.targets[0].name, "ios");
    agent.heartbeat().await.expect("the server answers again");
    let report = sc_app::list_build_hosts(&catalog).await?[0]
        .report
        .clone()
        .expect("a report is kept");
    assert_eq!(report["os"], std::env::consts::OS);
    assert!(report["readiness"]["ios"]["missing"].is_array(), "{report}");
    assert!(agent.readiness_lines()[0].starts_with("  ios ("));

    // Removed, the host's agent is told to stop rather than to retry.
    sc_app::delete_build_host(&catalog, host.id).await?;
    match agent.heartbeat().await {
        Err(HeartbeatError::Fatal(e)) => {
            assert!(e.to_string().contains("not recognised"), "{e}")
        }
        other => panic!("expected a fatal refusal, got {other:?}"),
    }

    // A server that is not there is worth retrying.
    let mut lost = Agent::new("http://127.0.0.1:9", "fsbh_x".into())?;
    assert!(matches!(
        lost.heartbeat().await,
        Err(HeartbeatError::Retry(_))
    ));
    Ok(())
}

//! Build hosts (`TODO-build-hosts.md` B1): the registry a Mac is added to, and
//! the token its agent authenticates with.

use std::sync::Arc;

use sc_app::{
    BUILD_HOST_TOKEN_PREFIX, add_build_host, authenticate_build_host, delete_build_host,
    list_build_hosts, record_heartbeat, update_build_host,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use serde_json::json;

async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    sc_app::bootstrap(&cat).await?;
    Ok(cat)
}

#[tokio::test]
async fn a_build_host_is_added_heard_from_renamed_and_removed() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    let (host, token) =
        add_build_host(&cat, " Mac mini ", &["ios".into(), "ios_simulator".into()]).await?;
    assert_eq!(host.name, "Mac mini");
    assert!(token.starts_with(BUILD_HOST_TOKEN_PREFIX), "{token}");

    // Only the hash is kept: the token is the one way back to the host.
    let stored: String = db
        .client()
        .await?
        .query_one("SELECT token_hash FROM _fd_build_hosts", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?
        .get(0);
    assert_ne!(stored, token);
    assert_eq!(authenticate_build_host(&cat, &token).await?.id, host.id);
    let wrong = authenticate_build_host(&cat, &format!("{token}x"))
        .await
        .unwrap_err();
    assert!(wrong.to_string().contains("not recognised"), "{wrong}");
    let foreign = authenticate_build_host(&cat, "fspk_abc").await.unwrap_err();
    assert!(
        foreign.to_string().contains("not a build host token"),
        "{foreign}"
    );

    // A second host by the same name is refused, whatever its case.
    let twice = add_build_host(&cat, "mac MINI", &["ios".into()])
        .await
        .unwrap_err();
    assert!(
        twice.to_string().contains("already a build host"),
        "{twice}"
    );

    record_heartbeat(&cat, host.id, &json!({ "protocol": 1, "os": "macos" })).await?;
    let listed = list_build_hosts(&cat).await?;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].is_online(chrono::Utc::now()));
    assert_eq!(listed[0].report.as_ref().unwrap()["os"], "macos");

    let renamed = update_build_host(&cat, host.id, "Studio", &["ios".into()])
        .await?
        .expect("the host is there");
    assert_eq!(
        (renamed.name.as_str(), renamed.targets.as_slice()),
        ("Studio", &["ios".to_owned()][..])
    );
    assert_eq!(authenticate_build_host(&cat, &token).await?.name, "Studio");

    // Removing it is revoking it.
    assert!(delete_build_host(&cat, host.id).await?);
    assert!(!delete_build_host(&cat, host.id).await?);
    assert!(authenticate_build_host(&cat, &token).await.is_err());
    assert!(
        update_build_host(&cat, host.id, "Gone", &["ios".into()])
            .await?
            .is_none()
    );
    Ok(())
}

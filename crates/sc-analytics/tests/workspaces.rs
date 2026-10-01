//! `_fd_workspaces` on both backends (analytics TODO A1.12, A1.21).

use std::sync::Arc;

use sc_analytics::{
    Workspace, WorkspaceKind, bootstrap_workspaces, create_workspace, delete_workspace,
    list_workspaces, load_workspace, rename_workspace, save_workspace_state,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use serde_json::json;

async fn round_trip(cat: &Catalog) -> Result<()> {
    bootstrap_workspaces(cat).await?;
    let me = uuid::Uuid::new_v4();
    // The store keeps any kind; the API is what refuses one not here yet.
    let ws = Workspace::new("House plots", WorkspaceKind::DataExplorer, Some(me));
    create_workspace(cat, &ws).await?;
    let back = load_workspace(cat, ws.id).await?.expect("stored");
    assert_eq!(back.name, "House plots");
    assert_eq!(back.kind, WorkspaceKind::DataExplorer);
    assert_eq!(back.state, json!({}));
    assert_eq!(back.created_by, Some(me));

    // The state is the screen's, stored whole and handed back as it was.
    let state = json!({ "dataset": "abc", "x": ["area"], "scroll": 120 });
    let saved = save_workspace_state(cat, ws.id, state.clone()).await?;
    assert_eq!(saved.state, state);
    assert!(save_workspace_state(cat, ws.id, json!([1])).await.is_err());

    let renamed = rename_workspace(cat, ws.id, " Houses ").await?;
    assert_eq!(renamed.name, "Houses");
    assert_eq!(renamed.state, state);
    assert!(rename_workspace(cat, ws.id, "").await.is_err());
    assert!(
        create_workspace(cat, &Workspace::new(" ", WorkspaceKind::Map, None))
            .await
            .is_err()
    );

    assert_eq!(list_workspaces(cat).await?.len(), 1);
    assert!(delete_workspace(cat, ws.id).await?);
    assert!(!delete_workspace(cat, ws.id).await?);
    assert!(list_workspaces(cat).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_workspace_round_trips_on_postgres() -> Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    round_trip(&Catalog::init(driver as Arc<dyn DatabaseDriver>).await?).await
}

#[tokio::test]
async fn a_workspace_round_trips_on_sqlite() -> Result<()> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    round_trip(&Catalog::init(driver).await?).await
}

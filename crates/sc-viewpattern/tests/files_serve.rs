//! A Saltcorn UI application's `/files/serve/*` (TODO F2.1): a file in an
//! admin-only folder is served to its **owner**, and is the same not-found a
//! missing file is to another user and to anonymous.
//!
//! What is asserted is the access decision and the answer it produces — a
//! [`ServeFile`](sc_files::ServeFile) for the router's file builder, or a 404 —
//! through [`serve_store_file`], the function the framework's route calls.

use std::sync::Arc;

use bytes::Bytes;
use sc_app::{Application, FrameworkRef};
use sc_auth::User;
use sc_catalog::{Catalog, FileStoreId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::{FileMeta, FileStore, LocalFileStore};
use sc_test_harness::TestDb;
use sc_viewpattern::serve_store_file;

#[tokio::test]
async fn an_owned_file_is_served_to_its_owner_and_not_found_to_anyone_else() -> Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    let dir = std::env::temp_dir().join(format!(
        "sc-viewpattern-files-serve-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(LocalFileStore::new("media", &dir)?);
    catalog.connect_file_store(store.clone())?;

    store.mkdir("private").await?;
    store
        .set_meta(
            "private",
            &FileMeta {
                min_role: Some(1),
                ..Default::default()
            },
        )
        .await?;
    store
        .write("private/ada.png", Bytes::from_static(b"png"))
        .await?;
    let ada = User::new(uuid::Uuid::new_v4(), 80)?;
    let bob = User::new(uuid::Uuid::new_v4(), 80)?;
    sc_files::record_owner(store.as_ref(), "private/ada.png", &ada.id.to_string()).await;

    let app = Application::new("Books", "books", FrameworkRef::new("saltcorn-ui"))
        .with_file_store(FileStoreId("media".to_owned()));
    let path = ["media", "private", "ada.png"];

    let served = serve_store_file(&app, &catalog, Some(&ada), &path).await?;
    let file = served.file.expect("the owner is served the file");
    assert_eq!(file.path, "private/ada.png");
    assert_eq!(file.store.name(), "media");

    for (who, user) in [("another user", Some(&bob)), ("anonymous", None)] {
        let answer = serve_store_file(&app, &catalog, user, &path).await?;
        assert_eq!(answer.status, 404, "{who}");
        assert!(answer.file.is_none(), "{who}");
    }
    // A missing file is decided by the builder, as a 404 too; a traversal never
    // gets that far.
    let answer = serve_store_file(&app, &catalog, Some(&ada), &["media", "..", "x"]).await?;
    assert_eq!(answer.status, 404);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

//! Phase 1.3 integration test: connecting stored file stores live, against a
//! real database.
//!
//! The MVP could only connect a store by restarting the process with a different
//! `--file-store` flag. These assert the four things that replace that: stored
//! stores come up at boot, one that fails does not take the others (or the
//! server) down, an edit takes effect without a restart, and a disconnected
//! store stops resolving.
//!
//! `allow-unwrap-in-tests` covers `#[test]` bodies but not the free helper
//! functions here, so this follows the other integration tests in allowing both
//! at crate level.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_catalog::{
    Catalog, bootstrap_file_stores, connect_all_file_stores, connect_file_store_def,
    delete_file_store, save_file_store,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::{CFG_PATH, FileStoreDef};
use sc_test_harness::TestDb;

async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_file_stores(&cat).await?;
    Ok(cat)
}

/// A fresh directory that exists, and its path as a string.
fn temp_dir(tag: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!(
        "sc-live-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.to_string_lossy().into_owned();
    (dir, path)
}

#[tokio::test]
async fn stored_stores_are_connected_and_become_resolvable() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let (dir, path) = temp_dir("boot");

    save_file_store(&cat, &FileStoreDef::local("docs", &path)).await?;

    // Nothing is connected until the stores are brought up: saving a definition
    // and connecting it are separate operations (§1.1).
    assert!(
        cat.file_store("docs", sc_catalog::Unchecked::system("test"))?
            .is_none()
    );

    let report = connect_all_file_stores(&cat).await?;
    assert_eq!(report.connected, ["docs"]);
    assert!(report.all_connected());
    assert!(
        cat.file_store("docs", sc_catalog::Unchecked::system("test"))?
            .is_some()
    );
    assert_eq!(cat.file_store_names()?, ["docs"]);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// The rule that keeps a server bootable: one broken store is the admin's
/// problem to fix in the UI, not a reason for everything else to stay down.
#[tokio::test]
async fn one_store_that_fails_does_not_stop_the_others() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let (dir_a, path_a) = temp_dir("ok-a");
    let (dir_b, path_b) = temp_dir("ok-b");

    save_file_store(&cat, &FileStoreDef::local("apps", &path_a)).await?;
    save_file_store(&cat, &FileStoreDef::local("gone", "/definitely/not/here")).await?;
    save_file_store(&cat, &FileStoreDef::local("uploads", &path_b)).await?;

    // Reading the table succeeded, so this is `Ok` even though a store failed —
    // only unreachable *metadata* is an error.
    let report = connect_all_file_stores(&cat).await?;

    assert_eq!(report.connected, ["apps", "uploads"]);
    assert!(!report.all_connected());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "gone");

    // The two good stores are usable …
    assert!(
        cat.file_store("apps", sc_catalog::Unchecked::system("test"))?
            .is_some()
    );
    assert!(
        cat.file_store("uploads", sc_catalog::Unchecked::system("test"))?
            .is_some()
    );
    // … and the bad one is absent but *explained*, which is what lets the admin
    // UI show a defined-but-unusable store with the reason rather than dropping
    // it silently or pretending it works.
    assert!(
        cat.file_store("gone", sc_catalog::Unchecked::system("test"))?
            .is_none()
    );
    let reason = cat
        .file_store_error("gone")?
        .expect("a store that failed to connect must record why");
    assert!(reason.contains("gone"), "{reason}");

    // A store that connected has no stale error against it.
    assert_eq!(cat.file_store_error("apps")?, None);

    std::fs::remove_dir_all(&dir_a).ok();
    std::fs::remove_dir_all(&dir_b).ok();
    Ok(())
}

#[tokio::test]
async fn editing_a_store_repoints_it_without_a_restart() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let (dir_old, path_old) = temp_dir("old");
    let (dir_new, path_new) = temp_dir("new");
    std::fs::write(dir_old.join("marker.txt"), b"old").unwrap();
    std::fs::write(dir_new.join("marker.txt"), b"new").unwrap();

    let mut def = FileStoreDef::local("docs", &path_old);
    save_file_store(&cat, &def).await?;
    connect_all_file_stores(&cat).await?;

    let before = cat
        .require_file_store("docs", sc_catalog::Unchecked::system("test"))?
        .read("marker.txt")
        .await?;
    assert_eq!(&before[..], b"old");

    // Edit the path and reconnect — no process restart, which is the whole point
    // of §1.3.
    def.config.insert(CFG_PATH.into(), path_new.clone().into());
    save_file_store(&cat, &def).await?;
    connect_file_store_def(&cat, &def)?;

    let after = cat
        .require_file_store("docs", sc_catalog::Unchecked::system("test"))?
        .read("marker.txt")
        .await?;
    assert_eq!(&after[..], b"new", "the edit must take effect immediately");
    // Still exactly one store: re-connecting a name replaces it rather than
    // accumulating a second handle.
    assert_eq!(cat.file_store_names()?, ["docs"]);

    std::fs::remove_dir_all(&dir_old).ok();
    std::fs::remove_dir_all(&dir_new).ok();
    Ok(())
}

#[tokio::test]
async fn disconnecting_stops_a_store_resolving() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let (dir, path) = temp_dir("disconnect");

    let def = FileStoreDef::local("scratch", &path);
    save_file_store(&cat, &def).await?;
    connect_all_file_stores(&cat).await?;
    assert!(
        cat.file_store("scratch", sc_catalog::Unchecked::system("test"))?
            .is_some()
    );

    // Deleting the definition and disconnecting the handle are separate steps —
    // without the second, the file manager would happily go on browsing a store
    // the admin had just deleted.
    assert!(delete_file_store(&cat, def.id, &[]).await?);
    assert!(
        cat.file_store("scratch", sc_catalog::Unchecked::system("test"))?
            .is_some(),
        "deleting the row alone must not disturb the live registry"
    );

    assert!(cat.disconnect_file_store("scratch")?);
    assert!(
        cat.file_store("scratch", sc_catalog::Unchecked::system("test"))?
            .is_none()
    );
    assert!(cat.file_store_names()?.is_empty());
    // Disconnecting something that was never connected is not an error.
    assert!(!cat.disconnect_file_store("scratch")?);

    // And the bytes are untouched by any of it (§1.1: a delete removes the row,
    // never the data).
    assert!(dir.is_dir());

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn a_repaired_store_loses_its_recorded_error() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let (dir, path) = temp_dir("repair");

    // Defined pointing somewhere that does not exist: connects nowhere, and the
    // reason is recorded.
    let mut def = FileStoreDef::local("docs", "/definitely/not/here");
    save_file_store(&cat, &def).await?;
    assert!(connect_file_store_def(&cat, &def).is_err());
    assert!(cat.file_store_error("docs")?.is_some());

    // The admin repoints it at a directory that exists — the exact repair the
    // "still editable when broken" rule exists to allow.
    def.config.insert(CFG_PATH.into(), path.clone().into());
    save_file_store(&cat, &def).await?;
    connect_file_store_def(&cat, &def)?;

    assert!(
        cat.file_store("docs", sc_catalog::Unchecked::system("test"))?
            .is_some()
    );
    assert_eq!(
        cat.file_store_error("docs")?,
        None,
        "a stale reason would be shown to the admin as if it were current"
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

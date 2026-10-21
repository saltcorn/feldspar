//! File writes from JavaScript modules (TODO-imap M6.2): a fixture module whose
//! declared file field names the store `fm_files`, folder `docs/`, writes
//! there from its service — a small file with a type and a name, 20 MB from a
//! stream and 20 MB from one buffer, within the worker's heap bound — and from
//! an action, and is refused everywhere else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "deno-host")]
mod deno {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use sc_action::{ActionContext, ActionRegistry, Event, EventKind};
    use sc_catalog::{Catalog, bootstrap_field_meta, bootstrap_table_meta};
    use sc_db::DatabaseDriver;
    use sc_db_postgres::PgDriver;
    use sc_files::{CONTENT_TYPE_ATTRIBUTE, FILENAME_ATTRIBUTE};
    use sc_module::{Backoff, Module, ModuleSet, ModuleSource, ServiceSupervisor, save_module};
    use sc_test_harness::TestDb;
    use sc_types::Attrs;
    use serde_json::json;

    use crate::common::{have_npm, installed, surfaces};

    const MB: u64 = 1024 * 1024;

    async fn catalog(db: &TestDb) -> Arc<Catalog> {
        let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
        let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>)
            .await
            .unwrap();
        sc_auth::bootstrap(&cat).await.unwrap();
        sc_auth::bootstrap_roles(&cat).await.unwrap();
        bootstrap_table_meta(&cat).await.unwrap();
        bootstrap_field_meta(&cat).await.unwrap();
        sc_catalog::bootstrap_file_stores(&cat).await.unwrap();
        sc_module::bootstrap_modules(&cat).await.unwrap();
        Arc::new(cat)
    }

    /// A local store of that name in a temporary directory, made before the
    /// module loads so its table sync uses it.
    async fn store(cat: &Catalog, name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sc-module-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut def = sc_module::tables::local_store(name).unwrap();
        def.config
            .insert("path".into(), json!(dir.display().to_string()));
        sc_catalog::save_file_store(cat, &def).await.unwrap();
        sc_catalog::connect_file_store_def(cat, &def).unwrap();
        dir
    }

    /// The service's notes by step, once it has written `done` or `failed`.
    async fn notes(db: &TestDb) -> BTreeMap<String, String> {
        for _ in 0..600 {
            let rows = db
                .client()
                .await
                .unwrap()
                .query("SELECT step, note FROM fm_results ORDER BY id", &[])
                .await
                .map(|rows| {
                    rows.iter()
                        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            if let Some(failed) = rows.get("failed") {
                panic!("the service failed: {failed}");
            }
            if rows.contains_key("done") {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the service never finished");
    }

    #[tokio::test]
    async fn a_module_keeps_files_in_its_declared_area_and_nowhere_else() {
        skip_without!(have_npm(), "npm is not on the PATH");
        let db = TestDb::new().await.unwrap();
        let cat = catalog(&db).await;
        let files_dir = store(&cat, "fm_files").await;
        let other_dir = store(&cat, "fm_other").await;
        let (installer, host, names) = installed("module-files", &["files-module"]).await;
        save_module(
            &cat,
            &Module::new(&names[0], ModuleSource::Local, &names[0]),
        )
        .await
        .unwrap();
        let mut registry = ActionRegistry::new();
        let set = ModuleSet::load(&cat, &host, &installer, &surfaces(), &mut registry)
            .await
            .unwrap();
        assert!(set.issues().is_empty(), "{:?}", set.issues());

        // --- From the service --------------------------------------------------
        let supervisor = ServiceSupervisor::new(&cat, &host, Backoff::default()).unwrap();
        supervisor.start(&set).await;
        let notes = notes(&db).await;

        // The path is answered normalised: what a File field holds.
        assert_eq!(notes["small_path"], "docs/a/hello.txt");
        assert_eq!(notes["small_read"], "hello, files");
        // 20 MB twice, and read back intact — inside the 256 MB heap.
        assert_eq!(notes["stream_size"], (20 * MB).to_string());
        assert_eq!(notes["buffer_size"], (20 * MB).to_string());
        assert_eq!(notes["stream_read"], "true");
        for step in ["outside", "sibling", "store", "folder"] {
            let said = &notes[step];
            assert!(
                said.contains("may keep files only where its declared file fields do")
                    || said.contains("names a file, not the store's root"),
                "{step}: {said}"
            );
            assert!(said.contains("docs/"), "{step} names the area: {said}");
        }
        assert!(
            notes["climb"].contains("leaves the file store"),
            "{}",
            notes["climb"]
        );
        assert_eq!(notes["remove"], "true");
        assert_eq!(notes["stat_after"], "null");

        // In the store: the type and the name kept, no temporary left behind,
        // and nothing written anywhere else.
        let files = cat.require_file_store("fm_files").unwrap();
        let big = files.stat("docs/big-stream.bin").await.unwrap().unwrap();
        assert_eq!(big.size, 20 * MB);
        let meta = files.get_meta("docs/a/hello.txt").await.unwrap();
        assert_eq!(meta.attributes[CONTENT_TYPE_ATTRIBUTE], "text/x-greeting");
        assert_eq!(meta.attributes[FILENAME_ATTRIBUTE], "Hello.txt");
        let listed: Vec<String> = files
            .list("docs")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(
            listed.iter().all(|n| !n.contains(".part-")),
            "no temporary is left: {listed:?}"
        );
        assert!(files.stat("other/x.txt").await.unwrap().is_none());
        assert!(files.stat("docs-x/x.txt").await.unwrap().is_none());
        let other = cat.require_file_store("fm_other").unwrap();
        assert!(other.stat("docs/x.txt").await.unwrap().is_none());

        // --- From an action -----------------------------------------------------
        let mut event = Event::new(EventKind::None);
        event.payload = json!({ "path": "docs/notes/n.txt", "text": "a note" });
        let config = Attrs::new();
        let action = registry.require("store_doc").unwrap();
        let mut ctx = ActionContext::new(&cat, &event, &config, "store a doc");
        let result = action.run(&mut ctx).await.unwrap();
        assert_eq!(result["path"], json!("docs/notes/n.txt"));
        assert_eq!(result["back"], json!("a note"));
        assert!(
            result["outside"]
                .as_str()
                .unwrap()
                .contains("may keep files only where its declared file fields do"),
            "{result}"
        );

        supervisor.stop_all().await;
        host.shutdown().await;
        let _ = std::fs::remove_dir_all(installer.root());
        let _ = std::fs::remove_dir_all(files_dir);
        let _ = std::fs::remove_dir_all(other_dir);
    }
}

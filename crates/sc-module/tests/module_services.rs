//! Module services (TODO-imap M5.4): a fixture module whose service inserts a
//! row into its own table every 100 ms, run by a [`ServiceSupervisor`] over a
//! real catalog — the rows arrive, they are table writes a listener sees, a
//! table the module did not declare is refused, a crash is restarted, and an
//! unload stops it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "deno-host")]
mod deno {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sc_action::ActionRegistry;
    use sc_api::code_host::TableHost;
    use sc_catalog::{
        Catalog, TableEvents, TableWrite, WriteOp, bootstrap_field_meta, bootstrap_table_meta,
    };
    use sc_db::DatabaseDriver;
    use sc_db_postgres::PgDriver;
    use sc_error::Result;
    use sc_expr::CodeHost;
    use sc_module::{
        Backoff, Module, ModuleSet, ModuleSource, ServiceState, ServiceSupervisor, save_module,
    };
    use sc_test_harness::TestDb;
    use serde_json::{Value as Json, json};

    use crate::common::{have_npm, installed, surfaces};

    const MODULE: &str = "@saltcorn-test/service";

    /// A listener on the catalog's write seam — the one the live hub and the
    /// trigger dispatcher are — recording the user each `svc_ticks` insert was
    /// caused by.
    #[derive(Default)]
    struct Watcher {
        inserts: Mutex<Vec<Json>>,
    }

    #[async_trait::async_trait]
    impl TableEvents for Watcher {
        fn observes(&self, table: &str, op: WriteOp) -> bool {
            table == "svc_ticks" && op == WriteOp::Insert
        }

        async fn emit(&self, _catalog: &Catalog, write: TableWrite<'_>) -> Result<()> {
            let user = write
                .caller
                .and_then(|c| c.user.clone())
                .unwrap_or(Json::Null);
            self.inserts.lock().unwrap().push(user);
            Ok(())
        }
    }

    async fn catalog(db: &TestDb) -> Arc<Catalog> {
        let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
        let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await.unwrap();
        sc_auth::bootstrap(&cat).await.unwrap();
        sc_auth::bootstrap_roles(&cat).await.unwrap();
        bootstrap_table_meta(&cat).await.unwrap();
        bootstrap_field_meta(&cat).await.unwrap();
        sc_module::bootstrap_modules(&cat).await.unwrap();
        Arc::new(cat)
    }

    /// `(n, instance, note)` of every tick.
    async fn ticks(db: &TestDb) -> Vec<(i32, String, Option<String>)> {
        db.client()
            .await
            .unwrap()
            .query("SELECT n::int4, instance, note FROM svc_ticks ORDER BY id", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect()
    }

    /// Wait up to fifteen seconds for `ready` to hold of the ticks.
    async fn until(
        db: &TestDb,
        what: &str,
        ready: impl Fn(&[(i32, String, Option<String>)]) -> bool,
    ) -> Vec<(i32, String, Option<String>)> {
        for _ in 0..300 {
            let rows = ticks(db).await;
            if ready(&rows) {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}: {:?}", ticks(db).await);
    }

    /// Write a command through the row layer, so it is an event the service's
    /// `onTableChange` hears.
    async fn command(cat: &Catalog, command: &str) {
        TableHost::new(cat)
            .call(json!({ "op": "insert", "table": "svc_control", "values": { "command": command } }))
            .await
            .unwrap();
    }

    fn note(rows: &[(i32, String, Option<String>)], n: i32) -> Option<String> {
        rows.iter()
            .find(|(m, _, _)| *m == n)
            .and_then(|(_, _, note)| note.clone())
    }

    #[tokio::test]
    async fn a_service_writes_its_own_tables_is_refused_others_and_is_restarted_and_stopped() {
        skip_without!(have_npm(), "npm is not on the PATH");
        let db = TestDb::new().await.unwrap();
        let cat = catalog(&db).await;
        let (installer, host, names) =
            installed("module-services", &["service-module", "echo-module"]).await;
        for name in &names {
            save_module(&cat, &Module::new(name, ModuleSource::Local, name))
                .await
                .unwrap();
        }
        let set = ModuleSet::load(&cat, &host, &installer, &surfaces(), &mut ActionRegistry::new())
            .await
            .unwrap();
        assert!(set.issues().is_empty(), "{:?}", set.issues());
        assert_eq!(set.get(MODULE).unwrap().service_names(), ["ticker"]);

        // M5.3: a module that runs a service has a worker of its own.
        let service_worker = host.worker_of(MODULE).await.unwrap();
        let echo = names.iter().find(|n| n.as_str() != MODULE).unwrap();
        assert_ne!(Some(service_worker), host.worker_of(echo).await);

        let watcher = Arc::new(Watcher::default());
        cat.add_table_events("test-watcher", Arc::clone(&watcher) as Arc<dyn TableEvents>)
            .unwrap();
        let supervisor = ServiceSupervisor::new(
            &cat,
            &host,
            Backoff {
                first: Duration::from_millis(50),
                max: Duration::from_millis(200),
                reset_after: Duration::from_secs(60),
            },
        )
        .unwrap();
        supervisor.start(&set).await;

        // The rows arrive, every 100 ms, as writes a listener sees — caused by
        // the module.
        let rows = until(&db, "five ticks", |rows| rows.len() >= 5).await;
        let first = rows[0].1.clone();
        let seen = watcher.inserts.lock().unwrap().clone();
        assert!(seen.len() >= 5, "{seen:?}");
        assert_eq!(seen[0]["module"], json!(MODULE));
        assert_eq!(seen[0]["service"], json!("ticker"));

        // Undeclared tables are refused, its own SQL is, and so is watching a
        // table it does not own. A write by somebody else is not its own.
        command(&cat, "trespass").await;
        let rows = until(&db, "the trespass report", |rows| {
            rows.iter().any(|(n, _, _)| *n == -4)
        })
        .await;
        for n in [-1, -2, -3] {
            let said = note(&rows, n).unwrap();
            assert!(
                said.contains("only the tables its module declared")
                    || said.contains("cannot run its own SQL"),
                "{n}: {said}"
            );
        }
        assert!(note(&rows, -2).unwrap().contains("SQL"));
        assert_eq!(note(&rows, -4).unwrap(), "own=false");

        // A crash: stopped, logged, restarted with backoff — on the same
        // worker, so with the same module state.
        command(&cat, "crash").await;
        let before = ticks(&db).await.len();
        until(&db, "ticks after the crash", |rows| {
            rows.len() > before + 5 && rows.iter().any(|(_, _, n)| n.as_deref() == Some("stopped"))
        })
        .await;
        let status = &supervisor.statuses_of(MODULE)[0];
        assert!(status.restarts >= 1, "{status:?}");
        assert!(
            status
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("asked to crash")),
            "{status:?}"
        );
        assert!(
            ticks(&db).await.iter().all(|(_, instance, _)| *instance == first),
            "a restarted service keeps its module's state"
        );

        // Its worker dies: restarted on a fresh one, loaded afresh.
        command(&cat, "exit").await;
        let rows = until(&db, "ticks from a new worker", |rows| {
            rows.iter().any(|(n, instance, _)| *n > 0 && *instance != first)
        })
        .await;
        assert!(rows.len() > 5);
        let status = &supervisor.statuses_of(MODULE)[0];
        assert!(status.restarts >= 2, "{status:?}");
        assert!(
            status
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("process.exit(3)")),
            "{status:?}"
        );

        // Uninstalled: the host unloads it, its stop runs, and it stays
        // stopped — no restart.
        host.unload(MODULE).await;
        let stopped = until(&db, "the stop", |rows| {
            rows.iter()
                .filter(|(_, _, n)| n.as_deref() == Some("stopped"))
                .count()
                >= 2
        })
        .await
        .len();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            ticks(&db).await.len() <= stopped + 1,
            "no ticks after the stop"
        );
        assert_eq!(supervisor.statuses_of(MODULE)[0].state, ServiceState::Stopped);

        supervisor.stop_all().await;
        host.shutdown().await;
        let _ = std::fs::remove_dir_all(installer.root());
    }

    /// `stop_all` — what a server does before every module reload — calls the
    /// service's `stop` and waits for it.
    #[tokio::test]
    async fn stop_all_awaits_the_services_stop() {
        skip_without!(have_npm(), "npm is not on the PATH");
        let db = TestDb::new().await.unwrap();
        let cat = catalog(&db).await;
        let (installer, host, names) = installed("module-services-stop", &["service-module"]).await;
        save_module(&cat, &Module::new(&names[0], ModuleSource::Local, &names[0]))
            .await
            .unwrap();
        let set = ModuleSet::load(&cat, &host, &installer, &surfaces(), &mut ActionRegistry::new())
            .await
            .unwrap();
        let supervisor = ServiceSupervisor::new(&cat, &host, Backoff::default()).unwrap();
        supervisor.start(&set).await;
        until(&db, "two ticks", |rows| rows.len() >= 2).await;

        supervisor.stop_all().await;
        let rows = ticks(&db).await;
        assert_eq!(
            rows.last().and_then(|(_, _, n)| n.clone()).as_deref(),
            Some("stopped"),
            "the stop's own write landed before stop_all returned: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(ticks(&db).await.len(), rows.len());
        assert_eq!(supervisor.statuses()[0].state, ServiceState::Stopped);

        host.shutdown().await;
        let _ = std::fs::remove_dir_all(installer.root());
    }

    /// A module reload while its service runs — what the Modules tab's Reload
    /// and saving a module's settings do on a serving server: the services are
    /// stopped, the module is loaded again, and they start over the new load.
    /// The load must not wait out the call timeout on the old service.
    #[tokio::test]
    async fn a_module_reloads_promptly_while_its_service_runs() {
        skip_without!(have_npm(), "npm is not on the PATH");
        let db = TestDb::new().await.unwrap();
        let cat = catalog(&db).await;
        let (installer, host, names) =
            installed("module-services-reload", &["service-module"]).await;
        save_module(
            &cat,
            &Module::new(&names[0], ModuleSource::Local, &names[0]),
        )
        .await
        .unwrap();
        let load = || async {
            ModuleSet::load(
                &cat,
                &host,
                &installer,
                &surfaces(),
                &mut ActionRegistry::new(),
            )
            .await
            .unwrap()
        };
        let set = load().await;
        let supervisor = ServiceSupervisor::new(&cat, &host, Backoff::default()).unwrap();
        for round in 0..2 {
            supervisor.start(&set).await;
            if round == 1 {
                until(&db, "ticks", |rows| rows.len() >= 2).await;
            }
            supervisor.stop_all().await;
            // Nothing was left running: a stop that overtook its start (round
            // 0 stops at once) would leave the service registered in the host,
            // running with nobody to stop it.
            let left =
                tokio::time::timeout(Duration::from_secs(5), host.stop_service(MODULE, "ticker"))
                    .await
                    .unwrap_or_else(|_| panic!("round {round}: a stop is still waiting"))
                    .unwrap();
            assert_eq!(left, json!({ "stopped": false }), "round {round}");
            let began = std::time::Instant::now();
            let again = tokio::time::timeout(Duration::from_secs(20), load())
                .await
                .unwrap_or_else(|_| {
                    panic!("round {round}: the reload waited on the stopped service")
                });
            assert!(again.issues().is_empty(), "{:?}", again.issues());
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "{:?}",
                began.elapsed()
            );
        }
        supervisor.stop_all().await;
        host.shutdown().await;
        let _ = std::fs::remove_dir_all(installer.root());
    }
}

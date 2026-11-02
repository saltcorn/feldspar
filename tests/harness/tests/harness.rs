//! Integration tests for the test harness itself, run against a real Postgres.
//!
//! These require a reachable Postgres — from `DATABASE_URL`, from the `test`
//! environment of `feldspar.toml`, or the local default. They are the reference
//! for how downstream crates use the harness.

use sc_test_harness::TestDb;

/// A fresh database is created and CRUD/DDL works against it.
#[tokio::test]
async fn creates_a_usable_database() {
    let db = TestDb::new().await.expect("create test db");
    let client = db.client().await.expect("checkout client");

    client
        .batch_execute("create table item (id int primary key, name text)")
        .await
        .expect("create table");
    client
        .execute(
            "insert into item (id, name) values ($1, $2)",
            &[&1i32, &"a"],
        )
        .await
        .expect("insert row");

    let row = client
        .query_one("select name from item where id = $1", &[&1i32])
        .await
        .expect("select row");
    let name: &str = row.get(0);
    assert_eq!(name, "a");
}

/// Two handles get distinct databases that cannot see each other's tables — the
/// isolation the "reset per test" requirement depends on.
#[tokio::test]
async fn databases_are_isolated_from_each_other() {
    let a = TestDb::new().await.expect("create db a");
    let b = TestDb::new().await.expect("create db b");
    assert_ne!(a.name(), b.name(), "each test db has a unique name");

    let ca = a.client().await.expect("client a");
    ca.batch_execute("create table only_in_a (id int)")
        .await
        .expect("create table in a");

    // Introspecting b's `information_schema` must not see a's table.
    let cb = b.client().await.expect("client b");
    let count: i64 = cb
        .query_one(
            "select count(*) from information_schema.tables where table_name = 'only_in_a'",
            &[],
        )
        .await
        .expect("introspect b")
        .get(0);
    assert_eq!(count, 0, "database b must not see database a's tables");
}

/// After a handle is dropped its database is gone — the reset that keeps state
/// from leaking between tests.
#[tokio::test]
async fn database_is_dropped_on_teardown() {
    let name = {
        let db = TestDb::new().await.expect("create db");
        // Prove it exists while the handle is alive.
        let client = db.client().await.expect("client");
        let exists: bool = client
            .query_one(
                "select exists(select 1 from pg_database where datname = $1)",
                &[&db.name()],
            )
            .await
            .expect("probe existence")
            .get(0);
        assert!(exists, "database should exist while handle is alive");
        db.name().to_string()
    }; // db dropped here -> database dropped on a teardown thread

    // Poll from a separate probe database until the drop lands (teardown runs on
    // another thread, so it is not instantaneous).
    let probe = TestDb::new().await.expect("create probe db");
    let client = probe.client().await.expect("probe client");
    let mut gone = false;
    for _ in 0..50 {
        let exists: bool = client
            .query_one(
                "select exists(select 1 from pg_database where datname = $1)",
                &[&name],
            )
            .await
            .expect("probe dropped")
            .get(0);
        if !exists {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(gone, "database {name} should be dropped on teardown");
}

/// What a test registers with `on_teardown` runs when its database is dropped,
/// latest first — and a bare `Teardown` guard does the same when it goes.
#[tokio::test]
async fn teardowns_run_when_the_test_ends_latest_first() {
    use std::sync::{Arc, Mutex};
    let ran = Arc::new(Mutex::new(Vec::new()));
    let db = TestDb::new().await.expect("create test db");
    for n in [1, 2] {
        let ran = Arc::clone(&ran);
        db.on_teardown(move || ran.lock().unwrap().push(n));
    }
    assert!(ran.lock().unwrap().is_empty(), "ran before the test ended");
    drop(db);
    assert_eq!(*ran.lock().unwrap(), vec![2, 1]);

    let guard = sc_test_harness::Teardown::new();
    {
        let ran = Arc::clone(&ran);
        guard.add(move || ran.lock().unwrap().push(3));
    }
    drop(guard);
    assert_eq!(*ran.lock().unwrap(), vec![2, 1, 3]);
}

//! [`SharedTx`] against a real database (§10.3, decision 6).
//!
//! The transaction a **workflow step** runs in is shared by everything the step
//! does — its action, the row layer beneath it, and the triggers its writes
//! cascade into — so the three properties that make that safe are pinned here,
//! at the layer that owns them rather than through the engine that uses them:
//!
//! - it **begins when it is first used**, so a step that writes nothing holds no
//!   transaction while it waits;
//! - what it writes is **invisible until it commits** and gone if it rolls back,
//!   through every handle on it;
//! - each statement carries **its own caller** to the policies, because the
//!   writers sharing it are not the same caller and a `SET LOCAL` outlives its
//!   statement;
//! - and an **after-commit** callback runs once the commit has succeeded, in
//!   order, and never for a rollback, a failed commit or a savepoint rolled
//!   back to (TODO.md "Live updates" L3.1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use sc_catalog::{
    CallerContext, Catalog, SharedTx, TableMeta, bootstrap_table_meta, enable_rls, save_table_meta,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Insert, Projection, Select, Source, Statement};
use sc_test_harness::TestDb;

async fn catalog(db: &TestDb, schema: &str) -> Result<Catalog> {
    db.client()
        .await?
        .batch_execute(schema)
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_table_meta(&catalog).await?;
    catalog.reload().await?;
    Ok(catalog)
}

/// `SELECT * FROM posts`.
fn select() -> Statement {
    Statement::Select(Box::new(
        Select::from(Source::table("posts")).columns(vec![Projection::all()]),
    ))
}

/// `INSERT INTO posts VALUES (id, owner)`.
fn insert(id: i64, owner: &str) -> Statement {
    Statement::from(Insert::row(
        "posts".to_owned(),
        vec!["id".to_owned(), "owner".to_owned()],
        vec![
            sc_query::Expr::lit(sc_query::Value::Int(id)),
            sc_query::Expr::lit(sc_query::Value::Text(owner.to_owned())),
        ],
    ))
}

/// How many rows a pooled reader — anybody but this transaction — can see.
async fn committed(db: &TestDb) -> usize {
    let client = db.client().await.unwrap();
    let rows = client.query("SELECT id FROM posts", &[]).await.unwrap();
    rows.len()
}

/// Nothing is begun until something needs it, and committing a transaction that
/// never began is the success it describes.
#[tokio::test]
async fn a_shared_transaction_begins_when_it_is_first_used() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(
        &db,
        "CREATE TABLE posts (id bigint primary key, owner text)",
    )
    .await?;

    let tx = SharedTx::begin_primary(&catalog)?;
    assert!(tx.is_open().await);
    assert!(!tx.has_begun().await, "nothing has needed the database yet");
    tx.commit().await?;

    // And a handle whose transaction is finished refuses by name rather than
    // quietly writing somewhere else.
    let err = tx.run(None, &select()).await.expect_err("finished");
    assert!(err.to_string().contains("already been committed"), "{err}");
    Ok(())
}

/// A write on a shared transaction is invisible outside it, visible to every
/// handle on it, and undone by a rollback.
#[tokio::test]
async fn what_it_writes_is_private_until_it_commits() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(
        &db,
        "CREATE TABLE posts (id bigint primary key, owner text)",
    )
    .await?;

    let tx = SharedTx::begin_primary(&catalog)?;
    tx.run(None, &insert(1, "alice@example.com")).await?;
    assert!(tx.has_begun().await);
    // A clone is a handle on the *same* transaction: it sees the row, and what
    // it writes joins it — which is what lets the row layer, an action and a
    // cascading trigger share one.
    let also = tx.clone();
    assert_eq!(also.run(None, &select()).await?.len(), 1);
    also.run(None, &insert(2, "bob@example.com")).await?;
    assert_eq!(tx.run(None, &select()).await?.len(), 2);
    // Nobody else can see either of them yet.
    assert_eq!(committed(&db).await, 0);

    tx.rollback().await?;
    assert_eq!(committed(&db).await, 0, "a rollback undid both writes");

    // The same again, committed: both land at once.
    let tx = SharedTx::begin_primary(&catalog)?;
    tx.run(None, &insert(1, "alice@example.com")).await?;
    tx.run(None, &insert(2, "bob@example.com")).await?;
    tx.commit().await?;
    assert_eq!(committed(&db).await, 2);
    Ok(())
}

/// Each statement reaches the policies as **its own** caller, and a statement
/// with no caller reaches them as no access — even when the statement before it
/// on the same transaction was an admin's.
#[tokio::test]
async fn every_statement_carries_its_own_caller_to_the_policies() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(
        &db,
        "CREATE TABLE posts (id bigint primary key, owner text); \
         INSERT INTO posts VALUES (1, 'alice@example.com'), (2, 'bob@example.com')",
    )
    .await?;
    let mut meta = TableMeta::new("posts");
    meta.set_ownership_formula(Some("owner === user.email"));
    meta.set_rls_enabled(true);
    save_table_meta(&catalog, &meta).await?;
    let table = catalog.require("posts")?;
    enable_rls(&catalog, &table).await?;

    let tx = SharedTx::begin_primary(&catalog)?;
    let admin = CallerContext::anonymous(1);
    assert_eq!(tx.run(Some(&admin), &select()).await?.len(), 2);

    // Alice, on the same transaction the admin just used: her row only. Without
    // re-applying the GUCs per statement this would still be the admin's read.
    let alice = CallerContext::new(
        80,
        Some(serde_json::json!({ "email": "alice@example.com" })),
    );
    let rows = tx.run(Some(&alice), &select()).await?;
    assert_eq!(rows.len(), 1);

    // And no caller at all is no access, not "whoever went last".
    assert!(tx.run(None, &select()).await?.is_empty());
    tx.rollback().await?;
    Ok(())
}

/// What a list of after-commit callbacks has said, shared with them.
#[derive(Clone, Default)]
struct Heard(Arc<Mutex<Vec<String>>>);

impl Heard {
    fn note(&self, what: &str) -> impl FnOnce() + Send + 'static {
        let heard = self.clone();
        let what = what.to_owned();
        move || heard.0.lock().unwrap().push(what)
    }

    fn said(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// A callback runs **after** the commit — not when it is added, not before
/// the commit returns — once, in the order callbacks were added; a rollback
/// drops them unrun; and one added after the end follows the end's outcome.
#[tokio::test]
async fn an_after_commit_callback_runs_only_once_the_commit_has_succeeded() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(
        &db,
        "CREATE TABLE posts (id bigint primary key, owner text)",
    )
    .await?;

    let heard = Heard::default();
    let tx = SharedTx::begin_primary(&catalog)?;
    tx.run(None, &insert(1, "alice@example.com")).await?;
    tx.after_commit(heard.note("first"));
    // From a clone, as a listener the write was handed to would.
    tx.clone().after_commit(heard.note("second"));
    assert_eq!(tx.after_commit_pending(), 2);
    assert!(heard.said().is_empty(), "nothing runs before the commit");
    tx.commit().await?;
    assert_eq!(heard.said(), ["first", "second"]);
    assert_eq!(committed(&db).await, 1);
    // Added after the commit, it is already true: it runs at once.
    tx.after_commit(heard.note("late"));
    assert_eq!(heard.said(), ["first", "second", "late"]);

    let heard = Heard::default();
    let tx = SharedTx::begin_primary(&catalog)?;
    tx.run(None, &insert(2, "bob@example.com")).await?;
    tx.after_commit(heard.note("rolled back"));
    tx.rollback().await?;
    tx.after_commit(heard.note("after the rollback"));
    assert!(heard.said().is_empty(), "{:?}", heard.said());
    assert_eq!(committed(&db).await, 1);
    Ok(())
}

/// A commit the database refuses — here a deferred foreign key, checked only
/// at the commit — runs none of the callbacks: the write they announce never
/// happened.
#[tokio::test]
async fn a_commit_that_fails_runs_no_callbacks() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(
        &db,
        "CREATE TABLE owners (name text primary key);          CREATE TABLE posts (id bigint primary key,              owner text references owners(name) deferrable initially immediate)",
    )
    .await?;

    let heard = Heard::default();
    let tx = SharedTx::begin_primary(&catalog)?;
    tx.defer_constraints().await?;
    tx.run(None, &insert(1, "nobody@example.com")).await?;
    tx.after_commit(heard.note("announced"));
    tx.commit()
        .await
        .expect_err("the deferred key fails the commit");
    assert!(heard.said().is_empty(), "{:?}", heard.said());
    assert_eq!(committed(&db).await, 0);
    Ok(())
}

/// Rolling back to a savepoint drops the callbacks added since it, and only
/// those; releasing one keeps them. That is what lets an import that skips a
/// bad row announce only the rows it kept.
#[tokio::test]
async fn rolling_back_to_a_savepoint_drops_the_callbacks_added_since() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(
        &db,
        "CREATE TABLE posts (id bigint primary key, owner text)",
    )
    .await?;

    let heard = Heard::default();
    let tx = SharedTx::begin_primary(&catalog)?;
    tx.run(None, &insert(1, "alice@example.com")).await?;
    tx.after_commit(heard.note("1"));

    tx.savepoint("row").await?;
    tx.run(None, &insert(2, "bob@example.com")).await?;
    tx.after_commit(heard.note("2"));
    tx.release_savepoint("row").await?;

    tx.savepoint("row").await?;
    tx.run(None, &insert(3, "carol@example.com")).await?;
    tx.after_commit(heard.note("3"));
    tx.rollback_to_savepoint("row").await?;
    assert_eq!(tx.after_commit_pending(), 2);

    // The savepoint survives being rolled back to, as in SQL.
    tx.run(None, &insert(4, "dave@example.com")).await?;
    tx.after_commit(heard.note("4"));
    tx.release_savepoint("row").await?;

    tx.commit().await?;
    assert_eq!(heard.said(), ["1", "2", "4"]);
    assert_eq!(committed(&db).await, 3);
    Ok(())
}

/// A snapshot transaction reads one moment: a row another writer commits after
/// its first read is not seen by the next one, and it refuses to write.
#[tokio::test]
async fn a_snapshot_transaction_reads_one_moment_and_writes_nothing() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(
        &db,
        "CREATE TABLE posts (id bigint primary key, owner text);
         INSERT INTO posts VALUES (1, 'ada');",
    )
    .await?;

    let tx = SharedTx::begin_snapshot(&catalog).await?;
    assert_eq!(tx.run(None, &select()).await?.len(), 1);

    // Committed elsewhere, after the snapshot was taken.
    db.client()
        .await?
        .batch_execute("INSERT INTO posts VALUES (2, 'bob')")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert_eq!(committed(&db).await, 2);
    assert_eq!(
        tx.run(None, &select()).await?.len(),
        1,
        "the second read sees the moment of the first"
    );

    let err = tx
        .run(None, &insert(3, "cy"))
        .await
        .expect_err("a snapshot transaction is read-only");
    assert!(err.to_string().contains("read-only"), "{err}");
    tx.rollback().await?;
    assert_eq!(committed(&db).await, 2);
    Ok(())
}

//! The migration runner against real databases, SQLite and Postgres: a new
//! database is recorded rather than migrated, an older one runs what it lacks
//! exactly once, a failure leaves neither its change nor its record, a newer
//! database is refused, and two runners at once apply a migration once.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_migrate::{
    Body, BoxFuture, Event, Migration, MigrationCtx, Report, Value, check_registry, run_one,
    run_with,
};
use sc_query::Statement;
use sc_test_harness::TestDb;

const CREATE_BOOKS: Migration = Migration {
    id: "2026-01-01-create-books",
    description: "a books table",
    body: Body::Sql {
        postgres: Some("CREATE TABLE books (id int8 PRIMARY KEY, title text NOT NULL)"),
        sqlite: Some("CREATE TABLE books (id int8 PRIMARY KEY, title text NOT NULL)"),
    },
};

fn add_book(ctx: &mut MigrationCtx) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        assert!(ctx.table_exists("books").await?);
        assert!(ctx.column_exists("books", "title").await?);
        assert!(!ctx.column_exists("books", "author").await?);
        assert!(!ctx.column_exists("no_such_table", "title").await?);
        let sql = format!(
            "INSERT INTO books (id, title) VALUES ({}, {})",
            ctx.placeholder(1),
            ctx.placeholder(2)
        );
        ctx.execute(&sql, vec![Value::Int(1), Value::Text("Dune".into())])
            .await
    })
}

const ADD_BOOK: Migration = Migration {
    id: "2026-01-02-add-book",
    description: "a first book, written from Rust",
    body: Body::Rust(add_book),
};

/// Writes a row, then fails: the row must not survive.
const BROKEN: Migration = Migration {
    id: "2026-01-03-broken",
    description: "fails half way",
    body: Body::Sql {
        postgres: Some(
            "INSERT INTO books (id, title) VALUES (2, 'x'); SELECT * FROM no_such_table",
        ),
        sqlite: Some("INSERT INTO books (id, title) VALUES (2, 'x'); SELECT * FROM no_such_table"),
    },
};

/// Postgres only: SQLite is recorded with nothing to do.
const PG_ONLY: Migration = Migration {
    id: "2026-01-04-pg-only",
    description: "Postgres only",
    body: Body::Sql {
        postgres: Some("INSERT INTO books (id, title) VALUES (4, 'pg')"),
        sqlite: None,
    },
};

/// A database Feldspar has started against before the framework: it has the
/// marker table.
async fn make_pre_framework(driver: &dyn DatabaseDriver) {
    let mut tx = driver.begin().await.unwrap();
    tx.batch("CREATE TABLE \"_fd_config\" (\"key\" text PRIMARY KEY, \"value\" text)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn scalar(driver: &dyn DatabaseDriver, sql: &str) -> Value {
    let rows = driver
        .query(&Statement::raw(sql, Vec::new()))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    rows[0].values()[0].clone()
}

async fn count(driver: &dyn DatabaseDriver, table: &str) -> i64 {
    match scalar(driver, &format!("SELECT count(*) FROM \"{table}\"")).await {
        Value::Int(n) => n,
        other => panic!("count(*) is {other:?}"),
    }
}

async fn recorded(driver: &dyn DatabaseDriver) -> Vec<String> {
    driver
        .query(&Statement::raw(
            "SELECT id FROM \"_fd_migrations\" ORDER BY id",
            Vec::new(),
        ))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap()
        .into_iter()
        .map(|row| match row.values()[0].clone() {
            Value::Text(id) => id,
            other => panic!("id is {other:?}"),
        })
        .collect()
}

async fn table_exists(driver: &dyn DatabaseDriver, name: &str) -> bool {
    driver
        .introspect()
        .await
        .unwrap()
        .iter()
        .any(|t| t.name == name)
}

async fn quietly(driver: &dyn DatabaseDriver, migrations: &[Migration]) -> Result<Report> {
    run_with(driver, migrations, &mut |_| {}).await
}

// --- the scenarios, each run on both backends -------------------------------

async fn new_database_is_recorded_not_migrated(driver: Arc<dyn DatabaseDriver>) {
    let mut events = Vec::new();
    let report = run_with(driver.as_ref(), &[CREATE_BOOKS, ADD_BOOK], &mut |e| {
        events.push(format!("{e:?}"))
    })
    .await
    .unwrap();
    assert_eq!(report.stamped, 2);
    assert!(report.applied.is_empty());
    assert_eq!(events, vec!["Stamped { count: 2 }".to_owned()]);
    assert!(!table_exists(driver.as_ref(), "books").await, "nothing ran");
    assert_eq!(
        recorded(driver.as_ref()).await,
        vec!["2026-01-01-create-books", "2026-01-02-add-book"]
    );

    // The next start finds everything recorded.
    let again = quietly(driver.as_ref(), &[CREATE_BOOKS, ADD_BOOK])
        .await
        .unwrap();
    assert_eq!(again, Report::default());
}

async fn older_database_runs_each_once(driver: Arc<dyn DatabaseDriver>) {
    make_pre_framework(driver.as_ref()).await;
    let mut applied = Vec::new();
    let report = run_with(
        driver.as_ref(),
        &[CREATE_BOOKS, ADD_BOOK, PG_ONLY],
        &mut |e| {
            if let Event::Applied { migration, .. } = e {
                applied.push(migration.id);
            }
        },
    )
    .await
    .unwrap();
    let all = vec![
        "2026-01-01-create-books",
        "2026-01-02-add-book",
        "2026-01-04-pg-only",
    ];
    assert_eq!(report.applied, all);
    assert_eq!(applied, all);
    let sqlite = driver.dialect().is_sqlite();
    assert_eq!(
        count(driver.as_ref(), "books").await,
        if sqlite { 1 } else { 2 }
    );
    assert_eq!(recorded(driver.as_ref()).await, all);

    // A second start runs nothing, though none of them is idempotent.
    let again = quietly(driver.as_ref(), &[CREATE_BOOKS, ADD_BOOK, PG_ONLY])
        .await
        .unwrap();
    assert!(again.applied.is_empty());

    // A later release's migration runs on its own.
    let later = Migration {
        id: "2026-02-01-second-book",
        description: "another",
        body: Body::Sql {
            postgres: Some("INSERT INTO books (id, title) VALUES (5, 'b')"),
            sqlite: Some("INSERT INTO books (id, title) VALUES (5, 'b')"),
        },
    };
    let report = quietly(driver.as_ref(), &[CREATE_BOOKS, ADD_BOOK, PG_ONLY, later])
        .await
        .unwrap();
    assert_eq!(report.applied, vec!["2026-02-01-second-book"]);

    // An older binary, which knows fewer migrations, refuses this database.
    let err = quietly(driver.as_ref(), &[CREATE_BOOKS, ADD_BOOK])
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("newer release"), "{err}");
    assert!(
        err.contains("`2026-01-04-pg-only`, `2026-02-01-second-book`"),
        "{err}"
    );
}

async fn failure_leaves_neither_change_nor_record(driver: Arc<dyn DatabaseDriver>) {
    make_pre_framework(driver.as_ref()).await;
    let err = quietly(driver.as_ref(), &[CREATE_BOOKS, ADD_BOOK, BROKEN])
        .await
        .unwrap_err();
    let chain = sc_error::format_chain(&err);
    assert!(
        chain.contains("migration `2026-01-03-broken` (fails half way) failed"),
        "{chain}"
    );
    assert!(chain.contains("no_such_table"), "{chain}");
    // The two before it stand; its own insert does not.
    assert_eq!(
        recorded(driver.as_ref()).await,
        vec!["2026-01-01-create-books", "2026-01-02-add-book"]
    );
    assert_eq!(count(driver.as_ref(), "books").await, 1);
}

async fn concurrent_runners_apply_once(driver: Arc<dyn DatabaseDriver>) {
    make_pre_framework(driver.as_ref()).await;
    let list = [CREATE_BOOKS, ADD_BOOK];
    let (a, b) = tokio::join!(
        quietly(driver.as_ref(), &list),
        quietly(driver.as_ref(), &list)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let mut applied: Vec<_> = a.applied.into_iter().chain(b.applied).collect();
    applied.sort_unstable();
    assert_eq!(
        applied,
        vec!["2026-01-01-create-books", "2026-01-02-add-book"]
    );
    assert_eq!(count(driver.as_ref(), "books").await, 1);
}

async fn run_one_runs_just_that(driver: Arc<dyn DatabaseDriver>) {
    let mut tx = driver.begin().await.unwrap();
    tx.batch("CREATE TABLE books (id int8 PRIMARY KEY, title text NOT NULL)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    run_one(
        driver.as_ref(),
        &[CREATE_BOOKS, ADD_BOOK],
        "2026-01-02-add-book",
    )
    .await
    .unwrap();
    assert_eq!(count(driver.as_ref(), "books").await, 1);
    assert_eq!(recorded(driver.as_ref()).await, vec!["2026-01-02-add-book"]);
    let err = run_one(
        driver.as_ref(),
        &[CREATE_BOOKS, ADD_BOOK],
        "2026-01-02-add-book",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("already been applied"), "{err}");
}

// --- backends -----------------------------------------------------------------

/// A SQLite file of its own (a file rather than memory, so that the pool's
/// connections share it), removed when dropped.
struct SqliteFile(PathBuf);

impl SqliteFile {
    fn new(name: &str) -> (SqliteFile, Arc<dyn DatabaseDriver>) {
        let dir = std::env::temp_dir().join(format!(
            "sc-migrate-{}-{name}-{}",
            std::process::id(),
            uuid_like()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let driver = SqliteDriver::open(dir.join("db.sqlite")).unwrap();
        (SqliteFile(dir), Arc::new(driver))
    }
}

impl Drop for SqliteFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

async fn postgres() -> (TestDb, Arc<dyn DatabaseDriver>) {
    let db = TestDb::new().await.expect("a test database");
    let driver = PgDriver::from_pool(db.pool().clone());
    (db, Arc::new(driver))
}

macro_rules! on_both {
    ($($scenario:ident),* $(,)?) => {
        mod sqlite {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $scenario() {
                    let (_file, driver) = super::SqliteFile::new(stringify!($scenario));
                    super::$scenario(driver).await;
                }
            )*
        }
        mod postgres {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $scenario() {
                    let (_db, driver) = super::postgres().await;
                    super::$scenario(driver).await;
                }
            )*
        }
    };
}

on_both!(
    new_database_is_recorded_not_migrated,
    older_database_runs_each_once,
    failure_leaves_neither_change_nor_record,
    concurrent_runners_apply_once,
    run_one_runs_just_that,
);

#[test]
fn the_registry_is_in_order_and_well_formed() {
    check_registry(sc_migrate::MIGRATIONS).unwrap();
    check_registry(&[CREATE_BOOKS, ADD_BOOK, BROKEN]).unwrap();

    let err = check_registry(&[ADD_BOOK, CREATE_BOOKS])
        .unwrap_err()
        .to_string();
    assert!(err.contains("listed after"), "{err}");
    let err = check_registry(&[CREATE_BOOKS, CREATE_BOOKS])
        .unwrap_err()
        .to_string();
    assert!(err.contains("listed after"), "{err}");

    for bad in [
        "2026-1-01-x",
        "2026-01-01",
        "2026-01-01-",
        "2026-01-01-Books",
        "2026-01-01-a--b",
        "add-books",
    ] {
        let migration = Migration {
            id: bad,
            ..CREATE_BOOKS
        };
        assert!(check_registry(&[migration]).is_err(), "{bad} was accepted");
    }
}

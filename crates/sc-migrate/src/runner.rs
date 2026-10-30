//! Applying migrations: [`run`], and [`status`] for the CLI.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use sc_db::{DatabaseDriver, Transaction};
use sc_error::{Context, Error, Result};
use sc_query::{Statement, Value};

use crate::ctx::{MigrationCtx, placeholder, table_exists_query};
use crate::{Body, MIGRATIONS, MIGRATIONS_TABLE, Migration};

/// A table every database Feldspar has started against has: the bootstrap
/// creates it on every start. A database without it, and without
/// [`MIGRATIONS_TABLE`], is new to Feldspar — whatever else it holds — and is
/// built in the current shape by the bootstrap, so it is recorded as having
/// every migration rather than running them.
const FELDSPAR_MARKER_TABLE: &str = "_fd_config";

/// The key of the Postgres advisory lock every migration transaction takes, so
/// two processes starting together ("_fd_mig" in ASCII). SQLite needs none:
/// its transactions begin `IMMEDIATE`, holding the write lock throughout.
const PG_LOCK_KEY: i64 = 0x005f_6664_5f6d_6967;

/// The record, created when it is first needed. The types are `sc-types`'
/// names, which both backends declare as written.
const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS \"_fd_migrations\" (\
     \"id\" text NOT NULL PRIMARY KEY, \
     \"applied_at\" timestamptz NOT NULL, \
     \"version\" text NOT NULL, \
     \"duration_ms\" int8 NOT NULL)";

/// The release recording a migration: the workspace's version, which is
/// `feldspar`'s.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What happened during [`run_with`], as it happens — so a long migration is
/// announced before it starts rather than after.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// A database new to Feldspar was recorded as having every migration.
    Stamped {
        /// How many were recorded.
        count: usize,
    },
    /// A migration is about to run.
    Applying(&'a Migration),
    /// A migration ran and was recorded.
    Applied {
        /// Which.
        migration: &'a Migration,
        /// How long it took.
        duration: Duration,
    },
}

/// What [`run_with`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// How many migrations were recorded without running, on a new database.
    pub stamped: usize,
    /// The migrations that ran, in order.
    pub applied: Vec<&'static str>,
}

/// One row of [`MIGRATIONS_TABLE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRecord {
    /// The migration's id.
    pub id: String,
    /// When it was applied (or recorded, on a new database), RFC 3339.
    pub applied_at: String,
    /// The release that applied it.
    pub version: String,
    /// How long it took; 0 for one recorded without running.
    pub duration_ms: i64,
}

/// Where a database stands, for `feldspar migrate status`.
#[derive(Debug, Clone)]
pub struct Status {
    /// Whether the database is new to Feldspar: the next start records every
    /// migration rather than running them, so `pending` will not run.
    pub new_database: bool,
    /// What has been applied, by id.
    pub applied: Vec<AppliedRecord>,
    /// What the next start runs, in order.
    pub pending: Vec<Migration>,
    /// Ids recorded by a newer release that this binary does not know. The
    /// next start refuses while there are any.
    pub unknown: Vec<String>,
}

/// Apply every pending migration in [`MIGRATIONS`] to `driver`'s database,
/// calling `on_event` as each starts and finishes.
pub async fn run(
    driver: &dyn DatabaseDriver,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<Report> {
    run_with(driver, MIGRATIONS, on_event).await
}

/// [`run`] over a given list rather than [`MIGRATIONS`] — what the tests use.
///
/// - A database with neither [`MIGRATIONS_TABLE`] nor any trace of Feldspar is
///   new: the record is created with every migration in it, and nothing runs.
/// - Otherwise each migration not recorded runs, in order, in its own
///   transaction, which also records it: a crash cannot leave one without the
///   other. The transaction takes a lock and checks the record again first, so
///   two processes starting together apply each migration once.
/// - A failure rolls that migration back and is returned, naming it. Those
///   before it stay applied.
/// - A recorded id that is not in the list means a newer release migrated the
///   database, and nothing runs.
pub async fn run_with(
    driver: &dyn DatabaseDriver,
    migrations: &[Migration],
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<Report> {
    check_registry(migrations)?;
    let sqlite = driver.dialect().is_sqlite();
    let mut report = Report::default();

    if !driver_table_exists(driver, sqlite, MIGRATIONS_TABLE).await? {
        let new_database = !driver_table_exists(driver, sqlite, FELDSPAR_MARKER_TABLE).await?;
        let mut tx = begin_locked(driver, sqlite).await?;
        tx.batch(CREATE_TABLE).await?;
        if new_database {
            for migration in migrations {
                insert_record(tx.as_mut(), sqlite, migration.id, 0, true).await?;
            }
        }
        tx.commit().await?;
        if new_database {
            report.stamped = migrations.len();
            on_event(Event::Stamped {
                count: migrations.len(),
            });
            return Ok(report);
        }
    }

    let applied: HashSet<String> = read_records(driver)
        .await?
        .into_iter()
        .map(|record| record.id)
        .collect();
    refuse_unknown(&applied, migrations)?;
    for migration in migrations.iter().filter(|m| !applied.contains(m.id)) {
        if apply(driver, sqlite, migration, on_event).await? {
            report.applied.push(migration.id);
        }
    }
    Ok(report)
}

/// Run the one migration `id` out of `migrations`, whatever else is pending,
/// and record it — so a test can build the shape a migration starts from and
/// check what it leaves. Refuses one already recorded.
pub async fn run_one(
    driver: &dyn DatabaseDriver,
    migrations: &[Migration],
    id: &str,
) -> Result<()> {
    let migration = migrations
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| Error::invalid(format!("there is no migration `{id}`")))?;
    let sqlite = driver.dialect().is_sqlite();
    let mut tx = begin_locked(driver, sqlite).await?;
    tx.batch(CREATE_TABLE).await?;
    tx.commit().await?;
    if apply(driver, sqlite, migration, &mut |_| {}).await? {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "migration `{id}` has already been applied"
        )))
    }
}

/// Where `driver`'s database stands against [`MIGRATIONS`]. Reads only.
pub async fn status(driver: &dyn DatabaseDriver) -> Result<Status> {
    check_registry(MIGRATIONS)?;
    let sqlite = driver.dialect().is_sqlite();
    if !driver_table_exists(driver, sqlite, MIGRATIONS_TABLE).await? {
        let new_database = !driver_table_exists(driver, sqlite, FELDSPAR_MARKER_TABLE).await?;
        return Ok(Status {
            new_database,
            applied: Vec::new(),
            pending: MIGRATIONS.to_vec(),
            unknown: Vec::new(),
        });
    }
    let applied = read_records(driver).await?;
    let applied_ids: HashSet<&str> = applied.iter().map(|r| r.id.as_str()).collect();
    let known: HashSet<&str> = MIGRATIONS.iter().map(|m| m.id).collect();
    let unknown = applied
        .iter()
        .filter(|r| !known.contains(r.id.as_str()))
        .map(|r| r.id.clone())
        .collect();
    let pending = MIGRATIONS
        .iter()
        .filter(|m| !applied_ids.contains(m.id))
        .copied()
        .collect();
    Ok(Status {
        new_database: false,
        applied,
        pending,
        unknown,
    })
}

/// Refuse a list whose ids are malformed, repeated or out of order: the order
/// of the ids is the order migrations run in, so a list that disagrees with it
/// would run them differently on a database that skipped some.
pub fn check_registry(migrations: &[Migration]) -> Result<()> {
    for migration in migrations {
        if !is_well_formed_id(migration.id) {
            return Err(Error::invalid(format!(
                "migration id `{}` is not of the form `YYYY-MM-DD-some-words` \
                 (lowercase letters, digits and single `-`s)",
                migration.id
            )));
        }
    }
    for pair in migrations.windows(2) {
        if pair[0].id >= pair[1].id {
            return Err(Error::invalid(format!(
                "migration `{}` is listed after `{}`: ids must be unique and listed in order",
                pair[1].id, pair[0].id
            )));
        }
    }
    Ok(())
}

fn is_well_formed_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() < 12 {
        return false;
    }
    let date_ok = bytes[..10].iter().enumerate().all(|(i, b)| match i {
        4 | 7 => *b == b'-',
        _ => b.is_ascii_digit(),
    });
    let words = &id[11..];
    date_ok
        && bytes[10] == b'-'
        && words.split('-').all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Run one migration and record it, in one transaction. `false` when another
/// process recorded it first.
async fn apply(
    driver: &dyn DatabaseDriver,
    sqlite: bool,
    migration: &Migration,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<bool> {
    let mut tx = begin_locked(driver, sqlite).await?;
    let (sql, binds) = (
        format!(
            "SELECT 1 FROM \"_fd_migrations\" WHERE \"id\" = {}",
            placeholder(sqlite, 1)
        ),
        vec![Value::Text(migration.id.to_owned())],
    );
    let recorded = !tx
        .query(&Statement::raw(sql, binds))
        .await?
        .try_collect()
        .await?
        .is_empty();
    if recorded {
        tx.rollback().await?;
        return Ok(false);
    }

    on_event(Event::Applying(migration));
    let started = Instant::now();
    let mut ctx = MigrationCtx::new(tx, sqlite);
    let result = run_body(&mut ctx, migration).await;
    let mut tx = ctx.into_transaction();
    let result = match result {
        Ok(()) => {
            let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
            insert_record(tx.as_mut(), sqlite, migration.id, duration_ms, false).await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        // The error is what matters; a failed rollback still ends the
        // transaction when the connection is dropped.
        let _ = tx.rollback().await;
        return Err(e).with_context(|| {
            format!(
                "migration `{}` ({}) failed and was rolled back; the migrations before it are \
                 applied, and the next start tries it again",
                migration.id, migration.description
            )
        });
    }
    tx.commit()
        .await
        .with_context(|| format!("committing migration `{}`", migration.id))?;
    on_event(Event::Applied {
        migration,
        duration: started.elapsed(),
    });
    Ok(true)
}

async fn run_body(ctx: &mut MigrationCtx, migration: &Migration) -> Result<()> {
    match migration.body {
        Body::Sql { postgres, sqlite } => {
            let script = if ctx.is_sqlite() { sqlite } else { postgres };
            match script {
                Some(sql) if !sql.trim().is_empty() => ctx.batch(sql).await,
                _ => Ok(()),
            }
        }
        Body::Rust(step) => step(ctx).await,
    }
}

/// A transaction holding the migration lock.
async fn begin_locked(driver: &dyn DatabaseDriver, sqlite: bool) -> Result<Box<dyn Transaction>> {
    let mut tx = driver.begin().await?;
    if !sqlite {
        tx.batch(&format!("SELECT pg_advisory_xact_lock({PG_LOCK_KEY})"))
            .await?;
    }
    Ok(tx)
}

async fn insert_record(
    tx: &mut dyn Transaction,
    sqlite: bool,
    id: &str,
    duration_ms: i64,
    if_absent: bool,
) -> Result<()> {
    let p = |n| placeholder(sqlite, n);
    let sql = format!(
        "INSERT INTO \"_fd_migrations\" (\"id\", \"applied_at\", \"version\", \"duration_ms\") \
         VALUES ({}, {}, {}, {}){}",
        p(1),
        p(2),
        p(3),
        p(4),
        if if_absent {
            " ON CONFLICT (\"id\") DO NOTHING"
        } else {
            ""
        }
    );
    let binds = vec![
        Value::Text(id.to_owned()),
        Value::Timestamp(chrono::Utc::now()),
        Value::Text(VERSION.to_owned()),
        Value::Int(duration_ms),
    ];
    tx.query(&Statement::raw(sql, binds))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

async fn driver_table_exists(
    driver: &dyn DatabaseDriver,
    sqlite: bool,
    name: &str,
) -> Result<bool> {
    let (sql, binds) = table_exists_query(sqlite, name);
    Ok(!driver
        .query(&Statement::raw(sql, binds))
        .await?
        .try_collect()
        .await?
        .is_empty())
}

async fn read_records(driver: &dyn DatabaseDriver) -> Result<Vec<AppliedRecord>> {
    let rows = driver
        .query(&Statement::raw(
            "SELECT \"id\", \"applied_at\", \"version\", \"duration_ms\" \
             FROM \"_fd_migrations\" ORDER BY \"id\"",
            Vec::new(),
        ))
        .await?
        .try_collect()
        .await?;
    rows.iter()
        .map(|row| {
            let text = |column: &str| match row.get(column) {
                Some(Value::Text(s)) => Ok(s.clone()),
                Some(Value::Timestamp(t)) => {
                    Ok(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                }
                other => Err(Error::database(format!(
                    "`_fd_migrations.{column}` holds {other:?}, not text"
                ))),
            };
            Ok(AppliedRecord {
                id: text("id")?,
                applied_at: text("applied_at")?,
                version: text("version")?,
                duration_ms: match row.get("duration_ms") {
                    Some(Value::Int(n)) => *n,
                    _ => 0,
                },
            })
        })
        .collect()
}

fn refuse_unknown(applied: &HashSet<String>, migrations: &[Migration]) -> Result<()> {
    let known: HashSet<&str> = migrations.iter().map(|m| m.id).collect();
    let mut unknown: Vec<&str> = applied
        .iter()
        .map(String::as_str)
        .filter(|id| !known.contains(id))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    Err(Error::config(format!(
        "this database has been migrated by a newer release of Feldspar: it records {} that \
         this one ({VERSION}) does not know ({}). Run that release, or restore a backup taken \
         before it",
        if unknown.len() == 1 {
            "a migration"
        } else {
            "migrations"
        },
        unknown
            .iter()
            .map(|id| format!("`{id}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

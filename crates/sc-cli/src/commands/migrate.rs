//! `feldspar migrate`: the primary database's migrations.

use sc_cli::DbConfig;
use sc_error::{Context, Result};

/// `feldspar migrate [database flags]` applies the pending migrations (§5.3)
/// and says what it did; `feldspar migrate status` lists what is applied and
/// what is pending, and changes nothing.
///
/// Every command that opens the database migrates it anyway, `serve` included,
/// so this is for running them at a time of the operator's choosing — before a
/// restart, with the old server stopped — and for watching them run.
pub(crate) async fn migrate_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let status = match rest.as_slice() {
        [] => false,
        [word] if word == "status" => true,
        _ => {
            return Err(sc_error::Error::config(format!(
                "usage: feldspar migrate [status] [database flags] (got `{}`)",
                rest.join(" ")
            )));
        }
    };
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let driver = db
        .connect()
        .await
        .with_context(|| format!("connecting to database {}", db.target()))?;
    sc_log::set_log_sql(false);
    if status {
        return print_migration_status(&sc_migrate::status(driver.as_ref()).await?);
    }
    let report = sc_cli::migrate_database(driver.as_ref()).await?;
    match (report.stamped, report.applied.len()) {
        (0, 0) => println!("The database is up to date; there was nothing to migrate."),
        (0, n) => println!("Applied {n} migration{}.", if n == 1 { "" } else { "s" }),
        (n, _) => println!(
            "The database is new to Feldspar: recorded {n} migration{} as applied, since the \
             tables are created in their current shape.",
            if n == 1 { "" } else { "s" }
        ),
    }
    Ok(())
}

/// What `feldspar migrate status` prints.
pub(crate) fn print_migration_status(status: &sc_migrate::Status) -> Result<()> {
    if status.new_database {
        println!(
            "The database is new to Feldspar. The next start creates its tables in their current \
             shape and records every migration ({}) as applied.",
            status.pending.len()
        );
        return Ok(());
    }
    if status.applied.is_empty() {
        println!("Applied: none");
    } else {
        println!("Applied:");
        for record in &status.applied {
            println!(
                "  {}  {}  by {} in {} ms",
                record.id, record.applied_at, record.version, record.duration_ms
            );
        }
    }
    if status.pending.is_empty() {
        println!("Pending: none");
    } else {
        println!("Pending (the next start applies these, in this order):");
        for migration in &status.pending {
            println!("  {}  {}", migration.id, migration.description);
        }
    }
    if !status.unknown.is_empty() {
        println!(
            "Unknown: {} — applied by a newer release; this binary will not start against \
             this database.",
            status.unknown.join(", ")
        );
    }
    Ok(())
}

//! `feldspar demo …`: demo data.

use sc_cli::DbConfig;
use sc_cli::connect_catalog;
use sc_error::Result;

/// `feldspar demo analytics [--replace] [database flags]`: the Analytics UI's
/// demo tables and datasets (analytics TODO A1.18, A2.15, A5.14).
pub(crate) async fn demo_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let parsed = sc_cli::demo::DemoArgs::parse(&rest)?;
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    let report = sc_analytics::demo::demo_analytics(&catalog, parsed.replace).await?;
    for table in &report.replaced {
        println!("dropped {table}");
    }
    for (table, rows) in &report.tables {
        println!("made {table}: {rows} rows");
    }
    for name in &report.datasets {
        println!("made the dataset {name}");
    }
    for name in &report.kept {
        println!("kept the dataset {name}, which was already there");
    }
    if let Some(reason) = &report.skipped {
        println!("{reason} (docs/OPERATIONS.md §10 says how to install PostGIS)");
    }
    println!(
        "Now run `feldspar serve`, sign in, and open Analytics in the admin sidebar \
         (docs/tutorial-analytics.md)."
    );
    Ok(())
}

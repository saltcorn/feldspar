//! `feldspar backup` and `feldspar restore`.

use sc_cli::DbConfig;
use sc_cli::{connect_catalog, connect_stored_databases, connect_stored_file_stores};
use sc_error::Result;
use sc_server::{ServerConfig, ServiceManager};

use super::serve::{Services, install_services, serving_defaults};

/// The operator restoring a backup at the CLI: a person, who may restore the
/// access settings of a table with row-level security (TODO-type-hardening
/// H8.5). Listed in the system-authority inventory.
const RESTORE: sc_catalog::Operator = sc_catalog::Operator::at_cli("restore");

/// The FILE a `backup` or `restore` names, which comes first, and the
/// arguments after it.
pub(crate) fn file_argument(
    command: &str,
    args: &[String],
) -> Result<(std::path::PathBuf, Vec<String>)> {
    match args.split_first() {
        Some((first, rest)) if !first.starts_with('-') => {
            Ok((std::path::PathBuf::from(first), rest.to_vec()))
        }
        _ => Err(sc_error::Error::config(format!(
            "{command} requires the backup file: feldspar {command} FILE [database flags]"
        ))),
    }
}

/// `feldspar backup FILE [database flags]` — the zip Settings → Backup
/// downloads, with everything ticked except the TLS settings, written to FILE.
/// A FILE of `-` writes it to stdout instead, with no file on disk on the way:
/// `feldspar backup - | aws s3 cp - s3://…` streams it in the memory one page
/// of rows takes, however large the installation.
///
/// The TLS settings are left out because they belong to the machine rather than
/// to the installation: a certificate, its private key and the names it covers
/// are wrong on the staging copy or the new host a backup from a terminal is
/// usually taken for. The Backup dialog still offers them for the admin who
/// does mean to move them.
///
/// Nothing is remembered: the dialog's stored selection is the dialog's, and a
/// script's backup is not the admin saying what the next one should include.
///
/// `--password` or `FELDSPAR_BACKUP_PASSWORD` encrypts it with AES-256 (see
/// [`sc_cli::backup_password`]).
pub(crate) async fn backup_command(args: &[String]) -> Result<()> {
    let (file, rest) = match args.split_first() {
        Some((first, rest)) if first == "-" => (None, rest.to_vec()),
        _ => {
            let (file, rest) = file_argument("backup", args)?;
            (Some(file), rest)
        }
    };
    let (password, rest) = sc_cli::backup_password::extract(rest)?;
    let (db, leftover) = DbConfig::extract(rest)?;
    if let Some(unknown) = leftover.first() {
        return Err(sc_error::Error::config(format!(
            "unknown backup argument `{unknown}`"
        )));
    }
    // gzip's rule: a zip is not something to print on a terminal, and a `-`
    // whose pipe was forgotten would otherwise do it.
    if file.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Err(sc_error::Error::config(
            "backup - writes the zip to stdout, which is a terminal: \
             redirect or pipe it (feldspar backup - > site.zip)",
        ));
    }
    // Asked for before the database is connected to, so a mistyped
    // confirmation costs nothing.
    let password = sc_cli::backup_password::for_backup(
        password,
        std::env::var(sc_cli::backup_password::PASSWORD_ENV).ok(),
    )?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    // Off whatever Settings → Development says: the echo goes to stdout, which
    // under `-` is the zip.
    sc_log::set_log_sql(false);
    // A file store's bytes are read through its connection, so a store that is
    // not connected comes out as its definition with no files.
    connect_stored_file_stores(&catalog).await?;

    let available = sc_backup::available(&catalog).await?;
    let selection = sc_backup::Selection {
        ssl: false,
        ..sc_backup::Selection::everything(&available)
    };
    let size = match &file {
        Some(file) => {
            write_whole(file, |out| {
                sc_backup::write_backup_to(&catalog, &selection, password.as_deref(), out)
            })
            .await?
        }
        None => {
            sc_backup::write_backup_stream(
                &catalog,
                &selection,
                password.as_deref(),
                std::io::stdout(),
            )
            .await?
        }
    };
    eprintln!(
        "feldspar: wrote {}{} ({} tables, {} applications, {} file stores, {})",
        file.as_ref().map_or_else(
            || "the backup to stdout".to_owned(),
            |f| f.display().to_string()
        ),
        if password.is_some() {
            ", protected by a password"
        } else {
            ""
        },
        selection.tables.len(),
        selection.applications.len(),
        selection.file_stores.len(),
        human_size(size)
    );
    Ok(())
}

/// Write the file at `path` by way of a sibling `.partial` file that `write`
/// streams into, then a rename, so an interrupted backup leaves the previous
/// file at `path` rather than half of a new one — the automated backups'
/// arrangement. The partial file is removed when anything fails.
pub(crate) async fn write_whole<F, Fut>(path: &std::path::Path, write: F) -> Result<u64>
where
    F: FnOnce(std::fs::File) -> Fut,
    Fut: std::future::Future<Output = Result<u64>>,
{
    let name = path
        .file_name()
        .ok_or_else(|| sc_error::Error::config(format!("`{}` is not a file name", path.display())))?
        .to_string_lossy();
    let partial = path.with_file_name(format!(".{name}.partial"));
    let out = std::fs::File::create(&partial)
        .map_err(|e| sc_error::Error::file(format!("writing `{}`: {e}", partial.display())))?;
    let written = match write(out).await {
        Ok(size) => size,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };
    std::fs::rename(&partial, path).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        sc_error::Error::file(format!("writing `{}`: {e}", path.display()))
    })?;
    Ok(written)
}

/// A file size as a person reads it: `812 bytes`, `4.2 MB`, `1.3 GB`.
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut size = bytes as f64;
    let mut unit = "";
    for next in UNITS {
        size /= 1000.0;
        unit = next;
        if size < 1000.0 {
            break;
        }
    }
    format!("{size:.1} {unit}")
}

/// `feldspar restore FILE [database flags] [server flags]` — everything FILE
/// holds, restored as the Restore dialog restores it with nothing unticked.
///
/// The restore runs over the services `serve` assembles, because it is the
/// dialog's restore: modules are reinstalled into `--modules-dir`, applications
/// are built, triggers and agents are validated against the registries a server
/// would hold. Which is why it takes the server flags — the same ones, from the
/// same environment, the server on this database is started with.
///
/// A server already running on the database does not see the result until it
/// is restarted: its catalog, mounts and trigger set were loaded at its boot.
///
/// A password-protected FILE is opened with `--password`,
/// `FELDSPAR_BACKUP_PASSWORD`, or the password asked for at the terminal.
pub(crate) async fn restore_command(args: &[String]) -> Result<()> {
    let (file, rest) = file_argument("restore", args)?;
    let (password, rest) = sc_cli::backup_password::extract(rest)?;
    let (db, rest) = DbConfig::extract(rest)?;
    let config = ServerConfig::from_args(serving_defaults(&db).iter().chain(&rest))?;

    // The file before the database: one that is not a backup is refused before
    // anything has been connected to, let alone written — and so is a wrong
    // password. It is read from where it is as the restore goes, never into
    // memory whole.
    let password = sc_cli::backup_password::for_restore(
        password,
        std::env::var(sc_cli::backup_password::PASSWORD_ENV).ok(),
        sc_backup::is_encrypted(&file)?,
    )?;
    let (contents, manifest) = sc_backup::inspect(&file, password.as_deref())?;

    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    // A module reinstalled from a registry resolves its host's name, so the
    // resolver `serve` starts is started here too.
    sc_dns::init()?;
    let catalog = connect_catalog(&db).await?;
    sc_log::set_log_sql(false);
    connect_stored_file_stores(&catalog).await?;
    connect_stored_databases(&catalog).await?;
    let Services { apps, .. } =
        install_services(&catalog, &config, &ServiceManager::default()).await?;

    eprintln!(
        "feldspar: restoring {} ({}, written {})",
        file.display(),
        manifest
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("a backup"),
        manifest
            .get("created_at")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("at an unrecorded time"),
    );
    let selection = sc_backup::Selection::everything(&contents);
    let report = sc_backup::restore_backup(
        &catalog,
        &apps,
        &file,
        password.as_deref(),
        &selection,
        sc_catalog::Editor::Operator(RESTORE),
    )
    .await?;
    for line in &report.restored {
        println!("restored {line}");
    }
    for line in &report.warnings {
        eprintln!("warning: {line}");
    }
    eprintln!(
        "feldspar: restored {} item(s) with {} warning(s); restart any server running on \
         this database to load them",
        report.restored.len(),
        report.warnings.len()
    );
    Ok(())
}

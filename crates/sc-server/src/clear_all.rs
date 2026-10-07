//! **Clear all**: put a running installation back to the empty state
//! (Settings → Development; Saltcorn 1 had the same button under the same name).
//!
//! What goes:
//!
//! - **Every table in the primary database** the admin made — dropped, rows and
//!   all, through the schema editor, so the `_fd_tables`/`_fd_fields` overlays go
//!   with them — and every column an admin added to `users`. Tables in other
//!   connected databases are not dropped: the connection is forgotten and they
//!   leave the catalog, but they are not this installation's to destroy. A
//!   provided table is a row and goes with the rows.
//! - **Every entity**: applications (unmounted), their views, pages and library,
//!   file stores, database connections, LLM providers and models, agents and
//!   their runs, triggers and workflow versions, streams, models and their
//!   instances, datasets, workspaces, modules (unloaded and uninstalled),
//!   translations, comments, settings — every row of every `_fd_*` table, so a
//!   table a later feature adds is cleared without anyone remembering to add it
//!   here.
//! - **Every user account**, with its sessions and tokens — the admin who pressed
//!   the button included. With no user left, the admin UI shows the
//!   create-first-user screen, which is how a fresh installation starts. The two
//!   built-in roles are seeded again, as the boot bootstrap does, because the
//!   first user is created with one.
//! - **File stores' directories**, only for the stores the admin ticked.
//!
//! Settings go back to their defaults at once where this process applies them
//! from the database (localisation, development logging). **The TLS settings
//! stay**, with the ACME cache: they say how this host serves, a backup leaves
//! them out for that reason, and an admin who cleared them would find out at the
//! next restart, with the admin UI that sets them unreachable on the port the
//! proxy forwards to.
//!
//! **The tables go first, and a failure there stops everything.** Dropping is
//! the step most likely to refuse, and refusing before anything else has
//! happened leaves the installation exactly as it was. Everything after it is
//! best effort and reported line by line, like a restore: an entity row that
//! will not delete or a directory that will not go is a warning, not a reason to
//! stop half way.

use std::collections::BTreeSet;

use sc_api::schema_edit::{self, ApplyOptions, Operation};
use sc_catalog::{Catalog, DataField, DataFieldKind, DbId, Table};
use sc_error::Result;
use sc_query::{BinOp, Delete, Expr, Statement};
use serde_json::{Value as Json, json};

use crate::apps::AppMounts;

/// What the dialog shows before the admin confirms: every file store and the
/// directory it occupies on this machine.
pub async fn preview(catalog: &Catalog) -> Result<Json> {
    let mut stores = Vec::new();
    for def in sc_catalog::list_file_stores(catalog).await? {
        let directory = sc_files::store_directory(&def)
            .ok()
            .flatten()
            .map(|d| d.to_string_lossy().into_owned());
        stores.push(json!({
            "name": def.name,
            "backend": def.backend,
            "directory": directory,
        }));
    }
    Ok(json!({ "file_stores": stores }))
}

/// What one clear did: a line per thing cleared, and a line per thing that
/// would not go.
#[derive(Debug, Default)]
pub struct ClearReport {
    pub cleared: Vec<String>,
    pub warnings: Vec<String>,
}

/// Clear the installation, removing from disk the file stores named in
/// `delete_from_disk`.
pub async fn clear_all(
    catalog: &Catalog,
    apps: &AppMounts,
    delete_from_disk: &[String],
) -> Result<ClearReport> {
    let mut report = ClearReport::default();
    // Read before the rows go: the definitions say where each store is on disk,
    // and the connections are named to be disconnected.
    let stores = sc_catalog::list_file_stores(catalog).await?;
    // A server that never bootstrapped connections has none to forget.
    let connections = sc_catalog::list_db_connections(catalog)
        .await
        .unwrap_or_default();

    let dropped = drop_user_tables(catalog).await?;
    report.cleared.push(match dropped {
        0 => "no tables to drop".to_owned(),
        1 => "dropped 1 table".to_owned(),
        n => format!("dropped {n} tables"),
    });

    for subdomain in apps.subdomains() {
        apps.unmount(&subdomain);
    }

    if let Some(services) = apps.modules() {
        for module in sc_module::list_modules(catalog).await.unwrap_or_default() {
            match module.language {
                sc_module::ModuleLanguage::JavaScript => services.host().unload(&module.name).await,
                sc_module::ModuleLanguage::Python => {
                    services.python_host().unload(&module.name).await;
                }
            }
            if let Err(e) = services.uninstall_package(&module).await {
                report.warnings.push(format!(
                    "module `{}`: its package could not be uninstalled: {}",
                    module.name,
                    e.causes()
                ));
            }
        }
    }

    clear_system_rows(catalog, &mut report).await;
    if let Err(e) = sc_auth::bootstrap_roles(catalog).await {
        report.warnings.push(format!(
            "the built-in roles could not be recreated: {}",
            e.causes()
        ));
    }

    for def in &stores {
        let _ = catalog.disconnect_file_store(&def.name);
    }
    for conn in &connections {
        let _ = catalog.disconnect_database(&conn.name);
    }
    if let Err(e) = catalog.reload().await {
        report
            .warnings
            .push(format!("the catalog could not be reloaded: {}", e.causes()));
    }
    if let Some(triggers) = apps.triggers()
        && let Err(e) = triggers.reload(catalog).await
    {
        report.warnings.push(format!(
            "the trigger set could not be reloaded: {}",
            e.causes()
        ));
    }
    if let Some(streams) = apps.streams()
        && let Err(e) = streams.reload(catalog).await
    {
        report.warnings.push(format!(
            "the stream set could not be reloaded: {}",
            e.causes()
        ));
    }
    if let Some(services) = apps.modules()
        && let Err(e) = services.reload().await
    {
        report.warnings.push(format!(
            "the module set could not be reloaded: {}",
            e.causes()
        ));
    }
    // The settings this process applied from `_fd_config`, back to what an
    // empty table means.
    if let Err(e) = sc_config::apply_localisation_settings(catalog).await {
        report.warnings.push(format!(
            "the localisation settings could not be reset: {}",
            e.causes()
        ));
    }
    if let Err(e) = sc_config::apply_development_settings(catalog).await {
        report.warnings.push(format!(
            "the development settings could not be reset: {}",
            e.causes()
        ));
    }

    for def in stores.iter().filter(|d| delete_from_disk.contains(&d.name)) {
        match sc_files::remove_store_from_disk(def) {
            Ok(removed) if removed.is_empty() => report.cleared.push(format!(
                "file store `{}`: nothing on disk to remove",
                def.name
            )),
            Ok(removed) => {
                for path in removed {
                    report.cleared.push(format!(
                        "file store `{}`: removed `{}`",
                        def.name,
                        path.display()
                    ));
                }
            }
            Err(e) => report
                .warnings
                .push(format!("file store `{}`: {}", def.name, e.causes())),
        }
    }
    Ok(report)
}

/// Drop every table the admin made in the primary database, returning how many.
///
/// One schema-editor batch, which also drops the columns an admin added to
/// `users` (the table itself stays: it is the system's). A table may only be
/// dropped once nothing else points at it, so the batch is ordered: a table
/// goes once every table referencing it has gone, and a key round a cycle is
/// dropped as a field first.
async fn drop_user_tables(catalog: &Catalog) -> Result<usize> {
    let tables = catalog.tables()?;
    let (mut remaining, kept): (Vec<Table>, Vec<Table>) =
        tables.into_iter().partition(is_user_table);
    let count = remaining.len();

    let mut ops = Vec::new();
    // The admin's columns on `users`, and keys from any other table that stays
    // into a table that goes.
    let doomed: BTreeSet<String> = remaining.iter().map(|t| t.name.clone()).collect();
    for table in &kept {
        for field in &table.fields {
            let added_to_users = table.name == sc_auth::USERS_TABLE
                && !sc_auth::is_system_user_column(&field.base.name);
            if added_to_users || key_target(field).is_some_and(|t| doomed.contains(t)) {
                ops.push(Operation::DropField {
                    table: table.name.clone(),
                    field: field.base.name.clone(),
                });
            }
        }
    }

    // Then the tables, each once nothing left references it.
    while !remaining.is_empty() {
        let referenced = |name: &str| {
            remaining
                .iter()
                .any(|t| t.name != name && t.fields.iter().any(|f| key_target(f) == Some(name)))
        };
        let (free, mut held): (Vec<Table>, Vec<Table>) = remaining
            .iter()
            .cloned()
            .partition(|t| !referenced(&t.name));
        if free.is_empty() {
            // A cycle: break it at the first table by dropping its keys into
            // the others, which frees what it pointed at on the next round.
            let first = &mut held[0];
            let name = first.name.clone();
            for field in &first.fields {
                if key_target(field).is_some_and(|t| t != name) {
                    ops.push(Operation::DropField {
                        table: name.clone(),
                        field: field.base.name.clone(),
                    });
                }
            }
            first
                .fields
                .retain(|f| key_target(f).is_none_or(|t| t == name));
        }
        for table in &free {
            ops.push(Operation::DropTable {
                table: table.name.clone(),
            });
        }
        remaining = held;
    }
    if !ops.is_empty() {
        schema_edit::apply(catalog, &ops, &ApplyOptions::default()).await?;
    }
    Ok(count)
}

/// The table a `Key` field points at.
fn key_target(field: &DataField) -> Option<&str> {
    match &field.kind {
        DataFieldKind::Key { target_table, .. } => Some(target_table.0.as_str()),
        _ => None,
    }
}

/// A table the admin made in the primary database: not a system table, not
/// `users`, and not one a module provides.
fn is_user_table(table: &Table) -> bool {
    table.database == DbId::primary()
        && !table.is_system()
        && table.name != sc_auth::USERS_TABLE
        && table.provider().is_none()
}

/// Delete every row of every `_fd_*` table and of `users`.
///
/// Rows are deleted table by table in whatever order works: a table whose
/// delete is refused (a foreign key from a table not emptied yet) is tried again
/// after the others, until a round makes no progress.
async fn clear_system_rows(catalog: &Catalog, report: &mut ClearReport) {
    let mut pending: Vec<String> = match catalog.tables() {
        Ok(tables) => tables
            .into_iter()
            .filter(|t| {
                t.database == DbId::primary() && (t.is_system() || t.name == sc_auth::USERS_TABLE)
            })
            .map(|t| t.name)
            .collect(),
        Err(e) => {
            report.warnings.push(format!(
                "the system tables could not be listed: {}",
                e.causes()
            ));
            return;
        }
    };
    let mut last_errors = Vec::new();
    while !pending.is_empty() {
        let mut failed = Vec::new();
        last_errors.clear();
        for name in &pending {
            match delete_rows(catalog, name).await {
                Ok(()) => {}
                Err(e) => {
                    last_errors.push(format!("`{name}`: {}", e.causes()));
                    failed.push(name.clone());
                }
            }
        }
        if failed.len() == pending.len() {
            break;
        }
        pending = failed;
    }
    if last_errors.is_empty() {
        report.cleared.push(
            "removed every user, application, file store, connection, provider, agent, \
             trigger, stream, model, dataset, workspace, module and setting \
             (the SSL / TLS settings were kept)"
                .to_owned(),
        );
    } else {
        for line in last_errors {
            report.warnings.push(format!("could not clear {line}"));
        }
    }
}

/// Empty one table — all but the host's TLS settings.
///
/// `_fd_config` keeps the TLS section's rows and the ACME cache is left alone:
/// how this host serves is the host's, as a backup already treats it (its
/// `ssl` part is off by default). Clearing them would change nothing until the
/// next restart and then take every application *and* the admin UI off the
/// port the proxy sends traffic to, which no one can repair from a browser.
/// The ACME cache goes with them because a cleared account and certificate is
/// a fresh order against the CA's rate limits for names it already certified.
async fn delete_rows(catalog: &Catalog, table: &str) -> Result<()> {
    if table == sc_config::ACME_CACHE_TABLE {
        return Ok(());
    }
    let mut delete = Delete::from(table);
    if table == sc_config::CONFIG_TABLE {
        let kept = sc_config::ssl_keys()
            .into_iter()
            .map(|key| Expr::binary(BinOp::Ne, Expr::col("key"), Expr::lit(key)))
            .reduce(Expr::and);
        if let Some(kept) = kept {
            delete = delete.filter(kept);
        }
    }
    catalog
        .primary()
        .query(&Statement::from(delete))
        .await?
        .try_collect()
        .await?;
    Ok(())
}

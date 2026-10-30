//! Changing which tables an application reaches: `update_application`'s
//! `tables` and `api_tables` sections and `feldspar app tables` /
//! `feldspar app api-tables`, one rule for each.
//!
//! An application's tables are an **opt-in subset** of the catalog
//! ([`Application::tables`]): its custom queries, views and streams may name
//! those and no others. A table is connected only if it exists, so a typo is
//! refused rather than stored as a name nothing serves; one already connected
//! may always be disconnected, including one dropped since.
//!
//! Which of them also get a **generic row API** is a second, narrower opt-in
//! ([`Application::api_tables`]), empty unless somebody asked: a table must be
//! connected before its row API can be served, and disconnecting it stops
//! serving it.

use sc_catalog::{Catalog, TableId};
use sc_error::{Error, Result};

use crate::application::Application;

/// What a change asks of the connected list: `set` replaces it, then `add`
/// and `remove` change it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableChange {
    /// Names appended when not already connected.
    pub add: Vec<String>,
    /// Names taken out.
    pub remove: Vec<String>,
    /// The whole new list, when given.
    pub set: Option<Vec<String>>,
}

/// Apply `change` to `app.tables`, refusing a table the catalog does not have.
/// Returns whether the list changed. On an error `app` is unchanged.
pub fn change_app_tables(
    catalog: &Catalog,
    app: &mut Application,
    change: &TableChange,
) -> Result<bool> {
    let mut tables: Vec<String> = match &change.set {
        Some(set) => set.clone(),
        None => app.tables.iter().map(|t| t.0.clone()).collect(),
    };
    for add in &change.add {
        if !tables.contains(add) {
            tables.push(add.clone());
        }
    }
    tables.retain(|t| !change.remove.contains(t));
    for table in &tables {
        // A table already connected stays connectable, so a list can always
        // be shrunk past one that was dropped since.
        if app.tables.iter().any(|t| &t.0 == table) {
            continue;
        }
        if catalog.get(table)?.is_none() {
            return Err(Error::invalid(format!(
                "there is no table `{table}`; create it with `edit_schema` first. \
                 Nothing was changed."
            )));
        }
    }
    let tables: Vec<TableId> = tables.into_iter().map(TableId).collect();
    // A row API over a table the app no longer includes is not one it serves.
    let api_tables: Vec<TableId> = app
        .api_tables
        .iter()
        .filter(|t| tables.contains(t))
        .cloned()
        .collect();
    let changed = tables != app.tables || api_tables != app.api_tables;
    app.tables = tables;
    app.api_tables = api_tables;
    Ok(changed)
}

/// Apply `change` to `app.api_tables` — which connected tables serve their
/// generic row API — refusing a table the application is not connected to.
/// Returns whether the list changed. On an error `app` is unchanged.
pub fn change_app_api_tables(app: &mut Application, change: &TableChange) -> Result<bool> {
    let mut tables: Vec<String> = match &change.set {
        Some(set) => set.clone(),
        None => app.api_tables.iter().map(|t| t.0.clone()).collect(),
    };
    for add in &change.add {
        if !tables.contains(add) {
            tables.push(add.clone());
        }
    }
    tables.retain(|t| !change.remove.contains(t));
    let tables: Vec<TableId> = tables.into_iter().map(TableId).collect();
    for table in &tables {
        not_connected(app, table)?;
    }
    let changed = tables != app.api_tables;
    app.api_tables = tables;
    Ok(changed)
}

/// Refuse a row API over a table the application is not connected to.
pub fn validate_api_tables(app: &Application) -> Result<()> {
    for table in &app.api_tables {
        not_connected(app, table)?;
    }
    Ok(())
}

fn not_connected(app: &Application, table: &TableId) -> Result<()> {
    if app.tables.contains(table) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "application `{}` cannot serve the row API of `{}`: it is not connected to that \
         table. Connect it first (its `tables`), or leave it out of `api_tables`.",
        app.name, table.0
    )))
}

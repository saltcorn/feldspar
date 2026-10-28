//! Changing which tables an application reaches: `update_application`'s
//! `tables` section and `feldspar app tables`, one rule for both.
//!
//! An application's tables are an **opt-in subset** of the catalog
//! ([`Application::tables`]): its API serves those and no others. A table is
//! connected only if it exists, so a typo is refused rather than stored as a
//! name nothing serves; one already connected may always be disconnected,
//! including one dropped since.

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
    let changed = tables != app.tables;
    app.tables = tables;
    Ok(changed)
}

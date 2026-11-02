//! Creating, configuring and forgetting a **provided** table (design §8.3).
//!
//! A provided table's `_fd_tables` row is not an overlay, it is the table's only
//! definition — so the three things an admin can do to one are not schema edits
//! and none of them issues DDL. They live here rather than in
//! [`schema_edit`](crate::schema_edit) for exactly that reason, and here rather
//! than in a handler for [`schema_edit`]'s reason: the admin API is one caller,
//! and an agent building an application is the next.
//!
//! What is *not* here: adding, altering or dropping a column. A provided table's
//! columns are what `fields(cfg)` answers, so the way to change them is to
//! change the configuration or the module. `schema_edit::apply` refuses each of
//! those operations by name, which is where an admin (or an agent) meets the
//! rule.

use sc_catalog::{
    Catalog, ProvidedTableDef, Table, TableMeta, load_table_meta_by_name, save_table_meta_row,
};
use sc_error::{Error, Result};
use sc_types::Attrs;

/// Create a provided table: write its definition row and reload.
///
/// The reload is what makes the table exist — [`Catalog::reload`] is where a row
/// carrying a provider becomes a [`Table`] — and it is also what asks the module
/// for the columns, so the table this returns already has them.
///
/// **The name is checked as an identifier** even though nothing will be
/// `CREATE TABLE`d: a provided table is named in the same places a database one
/// is (a URL, a formula, an API path, a generated client's types), so a name
/// that would not do there must not do here either.
pub async fn create(
    catalog: &Catalog,
    name: &str,
    module: &str,
    provider: &str,
    configuration: Attrs,
) -> Result<Table> {
    let name = name.trim();
    crate::schema_edit::check_identifier(name, "table")?;
    if catalog.get(name)?.is_some() {
        return Err(Error::invalid(format!(
            "a table called `{name}` already exists"
        )));
    }
    if load_table_meta_by_name(catalog, name).await?.is_some() {
        return Err(Error::invalid(format!(
            "settings for a table called `{name}` are already stored; forget them before \
             creating a provided table of that name"
        )));
    }
    let (module, provider) = require_provider(catalog, module, provider)?;

    let mut meta = TableMeta::new(name);
    meta.set_provider(Some(
        &ProvidedTableDef::new(module, provider).configuration(configuration),
    ));
    save_table_meta_row(catalog, &meta).await?;
    catalog.reload().await?;
    catalog.require(name)
}

/// Change a provided table's configuration — the object handed to `fields(cfg)`
/// and `get_table(cfg)`.
///
/// Reloads, because the columns may have changed: `@saltcorn/postgres-tables`
/// asks which columns to present, and an admin who adds one has to see it.
/// Secret settings are merged by the caller, exactly as a module's own settings
/// are (`sc_types::merge_secrets`), so a form submitted with the redaction
/// sentinel keeps what is stored.
pub async fn configure(catalog: &Catalog, table: &str, configuration: Attrs) -> Result<Table> {
    let mut meta = require_definition(catalog, table).await?;
    let def = meta
        .provider()
        .ok_or_else(|| provided_only(table))?
        .configuration(configuration);
    meta.set_provider(Some(&def));
    save_table_meta_row(catalog, &meta).await?;
    catalog.reload().await?;
    catalog.require(table)
}

/// Delete a provided table: forget its definition row, and reload.
///
/// This is what "drop" means for a provided table, and it is the whole of it —
/// there is no DDL, and the data was never Saltcorn's. `false` when there was no
/// such definition, which is what makes a second delete harmless.
pub async fn forget(catalog: &Catalog, table: &str) -> Result<bool> {
    let Some(meta) = load_table_meta_by_name(catalog, table).await? else {
        return Ok(false);
    };
    if meta.provider().is_none() {
        return Err(provided_only(table));
    }
    let deleted = sc_catalog::delete_table_meta(catalog, meta.id).await?;
    catalog.reload().await?;
    // The applications the observer re-projected are of no interest here: this
    // is a table *definition* being forgotten from a screen that shows the
    // outcome itself. Only `schema_edit::apply` reports them, because only its
    // caller may be a model that has to be told to rebuild what it changed.
    catalog.notify_schema_changed(&sc_catalog::SchemaChanged::TableChanged(table.to_owned()))?;
    Ok(deleted)
}

/// The stored definition row of a provided table, or a sentence.
async fn require_definition(catalog: &Catalog, table: &str) -> Result<TableMeta> {
    let meta = load_table_meta_by_name(catalog, table)
        .await?
        .ok_or_else(|| Error::not_found(format!("no table called `{table}` is defined here")))?;
    if meta.provider().is_none() {
        return Err(provided_only(table));
    }
    Ok(meta)
}

/// The provider named, as the installed modules spell it.
///
/// Checked at creation because this is the one moment an admin *chooses* one: a
/// table pointing at a provider nothing supplies would be created empty, look
/// broken, and give no clue that the name was simply wrong. Afterwards the same
/// state is legitimate — a module can be uninstalled under a table that was fine
/// yesterday — and the catalog carries it as an issue rather than refusing to
/// load.
fn require_provider(catalog: &Catalog, module: &str, provider: &str) -> Result<(String, String)> {
    let kinds = catalog.table_provider_kinds();
    if let Some(kind) = kinds
        .iter()
        .find(|k| k.module == module && k.provider == provider)
    {
        return Ok((kind.module.clone(), kind.provider.clone()));
    }
    let available = if kinds.is_empty() {
        "no installed module supplies one".to_owned()
    } else {
        kinds
            .iter()
            .map(|k| format!("`{}` of `{}`", k.provider, k.module))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Err(Error::not_found(format!(
        "no installed module supplies the table provider `{provider}` of `{module}`; available: \
         {available}"
    )))
}

fn provided_only(table: &str) -> Error {
    Error::invalid(format!(
        "`{table}` is not a provided table: it is a table in a database, so its rows and columns \
         are the database's"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_table_is_refused_by_the_endpoints_that_only_mean_a_provided_one() {
        let err = provided_only("books").to_string();
        assert!(err.contains("books"), "{err}");
        assert!(err.contains("not a provided table"), "{err}");
    }
}

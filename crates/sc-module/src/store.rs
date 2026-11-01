//! The `_fd_modules` table: its schema, bootstrap, and the [`Module`] ⇄ row
//! mapping (design §9).
//!
//! Follows `_fd_llm_providers` and `_fd_file_stores` in every respect that
//! matters, because it is the same kind of thing: nothing in
//! `information_schema` says "this installation has an MQTT module", so the row
//! *is* the module rather than an overlay on something the database knows.
//!
//! **Reading is strict.** A column that is missing or of the wrong shape is an
//! [`Error::invalid`] naming the module and the column, never a silently
//! defaulted field: a half-understood module row is one that would be
//! reinstalled from the wrong specifier or loaded with the wrong configuration.

use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;

use crate::module::{Module, ModuleId, ModuleLanguage, ModuleSource};
use crate::permissions::ModulePermissions;

/// Name of the modules table in the primary database.
pub const MODULES_TABLE: &str = "_fd_modules";

/// The UUID primary-key column (§9).
pub const COL_ID: &str = "id";
/// The package name — the key everything resolves through.
pub const COL_NAME: &str = "name";
/// Which language the module is written in, as [`ModuleLanguage::as_str`] (§8).
pub const COL_LANGUAGE: &str = "language";
/// Where the package came from, as [`ModuleSource::as_str`].
pub const COL_SOURCE: &str = "source";
/// The npm specifier or local directory the module was installed from.
pub const COL_LOCATION: &str = "location";
/// The version actually installed, or NULL before a successful install.
pub const COL_VERSION: &str = "version";
/// The module's own configuration (JSON object) — v1's plugin configuration.
pub const COL_CONFIGURATION: &str = "configuration";
/// What the module's worker may reach (§2) — JSON, an object of allow-lists.
pub const COL_PERMISSIONS: &str = "permissions";
/// The sparse per-module values column (§9) — JSON, always an object.
pub const COL_ATTRIBUTES: &str = "attributes";

/// The fields of the `_fd_modules` table, in declaration order.
///
/// `name` carries the `UNIQUE` constraint for the reason every other stored
/// definition's does: it is the key the loader, the action registry and the
/// admin UI all resolve through, so two rows claiming one package name is not a
/// state the system can serve. The database is the authority, because two
/// admins installing concurrently cannot see each other's transaction.
fn module_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        // Not required, and NULL reads as `javascript` — the same shape
        // `permissions` has and for the same reason: a column added after a row
        // was written must have an answer for that row, and the honest answer
        // for a module installed before there was a second language is the only
        // language there was.
        DataField::plain(COL_LANGUAGE, text()),
        DataField::plain(COL_SOURCE, text()).required(),
        DataField::plain(COL_LOCATION, text()).required(),
        DataField::plain(COL_VERSION, text()),
        DataField::plain(COL_CONFIGURATION, json()).required(),
        // Not required, and that is the one thing to know about this column: a
        // module installed before it existed has NULL here, and NULL reads as
        // the **closed** set. A permission column that defaulted to anything
        // else on a row nobody had thought about would be the failure §2 warns
        // against.
        DataField::plain(COL_PERMISSIONS, json()),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure the `_fd_modules` table exists, creating it if absent.
///
/// Idempotent and safe against a database that has never seen Saltcorn — the
/// same contract every other bootstrap has. Called once at startup.
pub async fn bootstrap_modules(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(MODULES_TABLE, &module_fields())
        .await
}

/// Save a module: insert its row, or update it in place if a row with its
/// [`ModuleId`] already exists.
///
/// A name clash is reported as an [`Error::invalid`] naming the conflict rather
/// than a raw constraint violation. The database's `UNIQUE` constraint remains
/// the authority: this check and the write are not one transaction.
pub async fn save_module(catalog: &Catalog, module: &Module) -> Result<()> {
    let name = module.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("a module needs a package name"));
    }
    if module.location.trim().is_empty() {
        return Err(Error::invalid(format!(
            "module `{name}` needs a location to have been installed from"
        )));
    }
    // A language and a source that disagree is a row nothing could reinstall:
    // npm cannot fetch from PyPI and pip cannot fetch from npm, so the pair is
    // checked here rather than left to fail at the next install.
    if !module.language.allows(module.source) {
        return Err(Error::invalid(format!(
            "module `{name}` is {} and cannot be installed from `{}`; a {} module comes from {}",
            module.language.as_str(),
            module.source.as_str(),
            module.language.as_str(),
            module
                .language
                .sources()
                .iter()
                .map(|source| source.as_str())
                .collect::<Vec<_>>()
                .join(" or ")
        )));
    }
    if let Some(other) = load_module_by_name(catalog, name).await?
        && other.id != module.id
    {
        return Err(Error::invalid(format!(
            "module `{name}` is already installed; uninstall it before installing it again"
        )));
    }

    let columns = module_columns();
    let values = module_values(module);

    if load_module(catalog, module.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            // The id is the row's identity, not something to reassign.
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        let update = Update::new(MODULES_TABLE, assignments)
            .filter(Expr::col(COL_ID).eq(Expr::lit(module.id.0)));
        run(catalog, Statement::from(update)).await?;
    } else {
        let insert = Insert::row(
            MODULES_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        );
        run(catalog, Statement::from(insert)).await?;
    }
    Ok(())
}

/// Load the module with this id, if it exists.
pub async fn load_module(catalog: &Catalog, id: ModuleId) -> Result<Option<Module>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// Load the module with this package name, if it exists.
pub async fn load_module_by_name(catalog: &Catalog, name: &str) -> Result<Option<Module>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// The module with this id, or an error saying it does not exist.
pub async fn require_module(catalog: &Catalog, id: ModuleId) -> Result<Module> {
    load_module(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("no module with id {id}")))
}

/// Every installed module, ordered by name — what the Modules tab lists and
/// what the loader walks at boot.
pub async fn list_modules(catalog: &Catalog) -> Result<Vec<Module>> {
    let select = Select::from(Source::table(MODULES_TABLE));
    let mut modules: Vec<Module> = rows(catalog, select)
        .await?
        .iter()
        .map(module_from_row)
        .collect::<Result<_>>()?;
    modules.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(modules)
}

/// Delete a module's row, returning whether one was there to delete.
///
/// The row only: taking the package off the disk is [`crate::install`]'s job,
/// and the caller does both in the order that leaves nothing dangling if the
/// second fails (the package stays, the row goes, and a reinstall overwrites).
pub async fn delete_module(catalog: &Catalog, id: ModuleId) -> Result<bool> {
    if load_module(catalog, id).await?.is_none() {
        return Ok(false);
    }
    let delete = Delete::from(MODULES_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id.0)));
    run(catalog, Statement::from(delete)).await?;
    Ok(true)
}

/// The row's columns, in the order [`module_values`] produces them.
fn module_columns() -> Vec<String> {
    [
        COL_ID,
        COL_NAME,
        COL_LANGUAGE,
        COL_SOURCE,
        COL_LOCATION,
        COL_VERSION,
        COL_CONFIGURATION,
        COL_PERMISSIONS,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// The module serialised to its row's values, in [`module_columns`] order.
fn module_values(module: &Module) -> Vec<Value> {
    vec![
        Value::Uuid(module.id.0),
        Value::Text(module.name.trim().to_owned()),
        Value::Text(module.language.as_str().to_owned()),
        Value::Text(module.source.as_str().to_owned()),
        Value::Text(module.location.trim().to_owned()),
        match &module.version {
            Some(v) => Value::Text(v.clone()),
            None => Value::Null,
        },
        Value::Json(Json::Object(module.configuration.clone())),
        Value::Json(Json::Object(module.permissions.to_json())),
        Value::Json(Json::Object(module.attributes.clone())),
    ]
}

/// Rebuild a [`Module`] from its row. The strictness note in the module docs
/// applies throughout.
fn module_from_row(row: &Row) -> Result<Module> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => ModuleId(*u),
        other => return Err(bad_column(COL_ID, "a uuid", other)),
    };
    let name = text(row, COL_NAME)?;
    // NULL is `javascript`, and anything else is parsed strictly: a language
    // nothing understands must not become a module some host guesses at.
    let language = match row.get(COL_LANGUAGE) {
        Some(Value::Text(t)) => {
            ModuleLanguage::parse(t).map_err(|e| Error::invalid(format!("module `{name}`: {e}")))?
        }
        Some(Value::Null) | None => ModuleLanguage::JavaScript,
        other => return Err(bad_column(COL_LANGUAGE, "text", other)),
    };
    let source = ModuleSource::parse(&text(row, COL_SOURCE)?)
        .map_err(|e| Error::invalid(format!("module `{name}`: {e}")))?;
    // A NULL version is "not installed yet", not a broken row.
    let version = match row.get(COL_VERSION) {
        Some(Value::Text(t)) => Some(t.clone()),
        Some(Value::Null) | None => None,
        other => return Err(bad_column(COL_VERSION, "text", other)),
    };

    Ok(Module {
        id,
        name,
        language,
        source,
        location: text(row, COL_LOCATION)?,
        version,
        configuration: object(row, COL_CONFIGURATION)?,
        permissions: permissions(row)?,
        attributes: object(row, COL_ATTRIBUTES)?,
    })
}

/// A required text column.
fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

/// A JSON column that must hold an object.
fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{MODULES_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

/// The permission set, or the closed one when the row has never had it written.
///
/// The one lenient read in this file, and only about NULL: the *shape* is still
/// checked entry by entry by [`ModulePermissions::from_json`], and a set that
/// will not parse is an error naming the module rather than a module that
/// quietly reaches something.
fn permissions(row: &Row) -> Result<ModulePermissions> {
    match row.get(COL_PERMISSIONS) {
        Some(Value::Json(value)) => ModulePermissions::from_json(value),
        Some(Value::Null) | None => Ok(ModulePermissions::closed()),
        other => Err(bad_column(COL_PERMISSIONS, "json", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{MODULES_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Run a select and collect its rows.
async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

/// Load the single module matching `filter`, if any.
async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<Module>> {
    let select = Select::from(Source::table(MODULES_TABLE))
        .filter(filter)
        .limit(1);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(module_from_row(row)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_the_section_9_required_columns() {
        let fields = module_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).unwrap();

        let id = by_name(COL_ID);
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));
        assert!(by_name(COL_NAME).required && by_name(COL_NAME).unique);
        assert_eq!(
            by_name(COL_ATTRIBUTES).base.type_,
            TypeRef::Basic(BasicType::Json)
        );
        // A module that has never installed has no version, so the column is not
        // required — the one nullable column in the table.
        assert!(!by_name(COL_VERSION).required);
    }

    #[test]
    fn the_table_is_a_hidden_system_table() {
        assert!(MODULES_TABLE.starts_with("_fd_"));
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        // The insert pairs these two positionally, so a column added to one and
        // not the other would write a location into the version column.
        let module = Module::new("@saltcorn/mqtt", ModuleSource::Npm, "@saltcorn/mqtt");
        assert_eq!(module_columns().len(), module_values(&module).len());
        assert_eq!(module_columns().len(), module_fields().len());
    }

    #[test]
    fn an_uninstalled_modules_version_is_null_rather_than_empty_text() {
        let module = Module::new("@saltcorn/mqtt", ModuleSource::Npm, "@saltcorn/mqtt");
        let idx = module_columns()
            .iter()
            .position(|c| c == COL_VERSION)
            .unwrap();
        assert_eq!(module_values(&module)[idx], Value::Null);
    }
}

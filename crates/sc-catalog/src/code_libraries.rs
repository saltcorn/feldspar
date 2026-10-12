//! The `_fd_code_libraries` table (TODO F3.1, design "The code library"): named,
//! shared JavaScript that code queries and triggers import with `lib(name)`.
//!
//! A library is stored metadata in the §9 sense — there is nothing to
//! introspect it from — so its row *is* its definition, as a file store's is.
//! It lives in `sc-catalog` rather than `sc-app`, although a library may belong
//! to an application, because the **reader** is here: every code body is run
//! over a [`Catalog`] (`sc-core-actions`' hosts), and the set it may import is
//! the catalog's to hand over ([`Catalog::code_libraries_for`]).
//!
//! # The loaded set
//!
//! A body's `lib` is synchronous and the hosts are built without an `await`, so
//! the libraries are held in memory: loaded by [`bootstrap_code_libraries`] and
//! again after every [`save_code_library`] and [`delete_code_library`], each
//! time under a new **generation**. What a body can import is resolved once
//! per application and generation (the application's own over the instance's)
//! and shared by every run until the next change.
//!
//! The application id is a plain UUID, not a foreign key: `_fd_applications`
//! belongs to `sc-app`, above this crate. Deleting an application deletes its
//! libraries through [`delete_application_code_libraries`], which `sc-app`
//! calls — the cascade is ours to perform, as it is for translations.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sc_db::Row;
use sc_error::{Error, Result};
use sc_expr::CodeLibraries;
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::{BasicType, TypeRef};
use uuid::Uuid;

use crate::catalog::Catalog;
use crate::field::DataField;
use crate::table::Table;

/// Name of the code-libraries table in the primary database.
pub const CODE_LIBRARIES_TABLE: &str = "_fd_code_libraries";

const COL_ID: &str = "id";
const COL_NAME: &str = "name";
const COL_DESCRIPTION: &str = "description";
const COL_CODE: &str = "code";
const COL_APPLICATION: &str = "application_id";

/// One code library: a function body that assigns to `exports` (or
/// `module.exports`), imported by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeLibrary {
    /// Stable identity: the UUID of its row.
    pub id: Uuid,
    /// What `lib(name)` imports it by — identifier-like, and unique within its
    /// scope (one application, or the instance).
    pub name: String,
    /// What it is for; the empty string means none given.
    pub description: String,
    /// The JavaScript: statements run once per importing run, with `exports`,
    /// `module`, `lib`, `user` and the importing run's handles in scope.
    pub code: String,
    /// The application it belongs to, or `None` for an instance-wide library.
    pub application_id: Option<Uuid>,
}

impl CodeLibrary {
    /// A **new** library with a fresh id.
    pub fn new(name: impl Into<String>, code: impl Into<String>) -> CodeLibrary {
        CodeLibrary {
            id: Uuid::new_v4(),
            name: name.into(),
            description: String::new(),
            code: code.into(),
            application_id: None,
        }
    }

    /// The same library, belonging to `application`.
    #[must_use]
    pub fn in_application(mut self, application: Uuid) -> CodeLibrary {
        self.application_id = Some(application);
        self
    }

    /// The same library, described.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> CodeLibrary {
        self.description = description.into();
        self
    }

    /// The scope it is visible in, in words — for a message.
    pub fn scope(&self) -> String {
        match self.application_id {
            Some(app) => format!("application {app}"),
            None => "the instance".to_owned(),
        }
    }
}

/// What the catalog holds: every library as of one generation, and the sets
/// already resolved from it.
#[derive(Debug, Default)]
pub(crate) struct LoadedLibraries {
    generation: u64,
    libraries: Vec<CodeLibrary>,
    resolved: Mutex<HashMap<Option<Uuid>, Arc<CodeLibraries>>>,
}

impl LoadedLibraries {
    fn resolve(&self, application: Option<Uuid>) -> Arc<CodeLibraries> {
        if let Ok(guard) = self.resolved.lock()
            && let Some(set) = guard.get(&application)
        {
            return Arc::clone(set);
        }
        let pairs = |scope: Option<Uuid>| {
            self.libraries
                .iter()
                .filter(move |l| l.application_id == scope)
                .map(|l| (l.name.clone(), l.code.clone()))
        };
        let set = Arc::new(match application {
            Some(app) => CodeLibraries::resolve(pairs(None), pairs(Some(app))),
            None => CodeLibraries::new(pairs(None)),
        });
        if let Ok(mut guard) = self.resolved.lock() {
            guard.insert(application, Arc::clone(&set));
        }
        set
    }
}

impl Catalog {
    /// The libraries a code body may import (design §3.3): an application's
    /// body sees its own libraries over the instance's; `None` — a trigger,
    /// which belongs to no application — sees the instance's alone.
    ///
    /// `None` where this process never loaded the table (a build tool, a test
    /// that did not bootstrap it), and a body there imports nothing.
    pub fn code_libraries_for(&self, application: Option<Uuid>) -> Option<Arc<CodeLibraries>> {
        let loaded = self.code_libraries.read().ok()?.clone()?;
        Some(loaded.resolve(application))
    }

    /// The generation of the loaded libraries: bumped by every reload, so a
    /// caller holding a resolved set can tell it is stale. `0` before any load.
    pub fn code_libraries_generation(&self) -> u64 {
        self.code_libraries
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|loaded| loaded.generation))
            .unwrap_or(0)
    }

    fn set_code_libraries(&self, libraries: Vec<CodeLibrary>) -> Result<()> {
        let mut guard = self
            .code_libraries
            .write()
            .map_err(|_| Error::msg("catalog code-library lock poisoned"))?;
        let generation = guard.as_ref().map_or(0, |loaded| loaded.generation) + 1;
        *guard = Some(Arc::new(LoadedLibraries {
            generation,
            libraries,
            resolved: Mutex::new(HashMap::new()),
        }));
        Ok(())
    }
}

fn code_library_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        // Unique per scope, not globally — the same name may be an instance
        // library and an application's override of it — and NULL is distinct
        // from NULL in SQL, so the check is [`check_code_library_saveable`]'s.
        DataField::plain(COL_NAME, text()).required(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_CODE, text()).required(),
        DataField::plain(COL_APPLICATION, uuid()),
    ]
}

/// Ensure the `_fd_code_libraries` table exists, and load what it holds into
/// the catalog. Idempotent; call once at startup beside the other `_fd_*`
/// bootstraps.
pub async fn bootstrap_code_libraries(catalog: &Catalog) -> Result<Table> {
    let table = catalog
        .bootstrap_table(CODE_LIBRARIES_TABLE, &code_library_fields())
        .await?;
    reload_code_libraries(catalog).await?;
    Ok(table)
}

/// Load every library into the catalog under a new generation — what a save
/// and a delete do, and what a process that changed the table some other way
/// (a restore) calls.
pub async fn reload_code_libraries(catalog: &Catalog) -> Result<()> {
    let libraries = list_code_libraries(catalog).await?;
    catalog.set_code_libraries(libraries)
}

/// Every library, instance-wide first, then by application and name.
pub async fn list_code_libraries(catalog: &Catalog) -> Result<Vec<CodeLibrary>> {
    let select = Select::from(Source::table(CODE_LIBRARIES_TABLE));
    let mut libraries: Vec<CodeLibrary> = rows(catalog, select)
        .await?
        .iter()
        .map(library_from_row)
        .collect::<Result<_>>()?;
    libraries.sort_by(|a, b| (a.application_id, &a.name).cmp(&(b.application_id, &b.name)));
    Ok(libraries)
}

/// The library with this id, if any.
pub async fn load_code_library(catalog: &Catalog, id: Uuid) -> Result<Option<CodeLibrary>> {
    let select = Select::from(Source::table(CODE_LIBRARIES_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(id)))
        .limit(1);
    rows(catalog, select)
        .await?
        .first()
        .map(library_from_row)
        .transpose()
}

/// The library called `name` in exactly this scope (not falling back from an
/// application to the instance), if any.
pub async fn load_code_library_by_name(
    catalog: &Catalog,
    name: &str,
    application: Option<Uuid>,
) -> Result<Option<CodeLibrary>> {
    Ok(list_code_libraries(catalog)
        .await?
        .into_iter()
        .find(|l| l.name == name && l.application_id == application))
}

/// Whether `name` can be a library's name: what `lib("…")` is written with, so
/// identifier-like — a letter, `_` or `$`, then letters, digits, `_`, `$` or
/// `-` — and at most 64 characters.
pub fn valid_code_library_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 64
        && (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '-')
}

/// Everything [`save_code_library`] checks before it writes, apart from
/// whether the code compiles — which needs a JavaScript engine this crate does
/// not have, and which the admin API and MCP check first
/// (`sc_expr::check_code_library`).
pub async fn check_code_library_saveable(catalog: &Catalog, library: &CodeLibrary) -> Result<()> {
    if !valid_code_library_name(&library.name) {
        return Err(Error::invalid(format!(
            "`{}` is not a code library name: use letters, digits, `_`, `$` and `-`, \
             starting with a letter, `_` or `$` (at most 64 characters)",
            library.name
        )));
    }
    if let Some(other) =
        load_code_library_by_name(catalog, &library.name, library.application_id).await?
        && other.id != library.id
    {
        return Err(Error::invalid(format!(
            "{} already has a code library named `{}`",
            library.scope(),
            library.name
        )));
    }
    Ok(())
}

/// Save a library — insert it, or update the row with its id — and reload the
/// catalog's set, so the next run of every body that imports it sees the edit.
pub async fn save_code_library(catalog: &Catalog, library: &CodeLibrary) -> Result<()> {
    check_code_library_saveable(catalog, library).await?;
    let description = if library.description.is_empty() {
        Value::Null
    } else {
        Value::Text(library.description.clone())
    };
    let values = vec![
        Value::Uuid(library.id),
        Value::Text(library.name.clone()),
        description,
        Value::Text(library.code.clone()),
        library.application_id.map_or(Value::Null, Value::Uuid),
    ];
    let columns: Vec<String> = [COL_ID, COL_NAME, COL_DESCRIPTION, COL_CODE, COL_APPLICATION]
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    let statement = if load_code_library(catalog, library.id).await?.is_some() {
        let assignments = columns
            .iter()
            .zip(values)
            .filter(|(col, _)| *col != COL_ID)
            .map(|(col, value)| Assignment::new(col.clone(), Expr::Lit(value)))
            .collect();
        Statement::from(
            sc_query::Update::new(CODE_LIBRARIES_TABLE, assignments)
                .filter(Expr::col(COL_ID).eq(Expr::lit(library.id))),
        )
    } else {
        Statement::from(Insert::row(
            CODE_LIBRARIES_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        ))
    };
    run(catalog, statement).await?;
    reload_code_libraries(catalog).await
}

/// Delete a library, answering whether there was one. Code that imports it
/// fails at its `lib()` call from the next run on, naming what it can import.
pub async fn delete_code_library(catalog: &Catalog, id: Uuid) -> Result<bool> {
    if load_code_library(catalog, id).await?.is_none() {
        return Ok(false);
    }
    let delete = Delete::from(CODE_LIBRARIES_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id)));
    run(catalog, Statement::from(delete)).await?;
    reload_code_libraries(catalog).await?;
    Ok(true)
}

/// Delete every library of one application — the cascade `sc-app`'s
/// `delete_application` performs. Answers how many there were.
pub async fn delete_application_code_libraries(
    catalog: &Catalog,
    application: Uuid,
) -> Result<usize> {
    if catalog.get(CODE_LIBRARIES_TABLE)?.is_none() {
        return Ok(0);
    }
    let doomed = list_code_libraries(catalog)
        .await?
        .into_iter()
        .filter(|l| l.application_id == Some(application))
        .count();
    if doomed == 0 {
        return Ok(0);
    }
    let delete = Delete::from(CODE_LIBRARIES_TABLE)
        .filter(Expr::col(COL_APPLICATION).eq(Expr::lit(application)));
    run(catalog, Statement::from(delete)).await?;
    reload_code_libraries(catalog).await?;
    Ok(doomed)
}

fn library_from_row(row: &Row) -> Result<CodeLibrary> {
    let uuid = |column: &str| match row.get(column) {
        Some(Value::Uuid(u)) => Ok(Some(*u)),
        Some(Value::Text(t)) => Uuid::parse_str(t)
            .map(Some)
            .map_err(|_| bad(column, "a uuid")),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(bad(column, "a uuid")),
    };
    let text = |column: &str| match row.get(column) {
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(bad(column, "text")),
    };
    Ok(CodeLibrary {
        id: uuid(COL_ID)?.ok_or_else(|| bad(COL_ID, "a uuid"))?,
        name: text(COL_NAME)?.ok_or_else(|| bad(COL_NAME, "text"))?,
        description: text(COL_DESCRIPTION)?.unwrap_or_default(),
        code: text(COL_CODE)?.unwrap_or_default(),
        application_id: uuid(COL_APPLICATION)?,
    })
}

fn bad(column: &str, expected: &str) -> Error {
    Error::invalid(format!(
        "{CODE_LIBRARIES_TABLE}.{column} should be {expected}"
    ))
}

async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_what_lib_is_written_with() {
        for good in ["auth", "_util", "$x", "date-fns", "v2"] {
            assert!(valid_code_library_name(good), "{good}");
        }
        for bad in ["", "2fa", "a b", "a/b", "../x", &"x".repeat(65)] {
            assert!(!valid_code_library_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_table_is_a_hidden_system_table_with_the_section_9_columns() {
        assert!(CODE_LIBRARIES_TABLE.starts_with("_fd_"));
        let fields = code_library_fields();
        let id = fields.iter().find(|f| f.base.name == COL_ID).unwrap();
        assert!(id.primary_key && id.required);
        let app = fields
            .iter()
            .find(|f| f.base.name == COL_APPLICATION)
            .unwrap();
        assert!(!app.required, "NULL is an instance-wide library");
    }
}

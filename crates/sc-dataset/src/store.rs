//! The `_fd_datasets` table: its schema, bootstrap, and the [`DatasetDef`] ⇄
//! row mapping (analytics TODO A1.1; design §9).
//!
//! A dataset, like a model, has nothing to introspect it from, so by §9's rule
//! its row *is* its definition. The judgements §9 asks for:
//!
//! - **`base` and `operations` are JSON columns.** Each is a structured value
//!   nothing queries into; the list is edited and saved whole.
//! - **Names are unique**, because the name is what the model form, the
//!   explorer and a person pick a dataset by; a second "House prices" would be
//!   a coin toss.
//! - **Reading is strict.** A column that is missing or misshapen is an error
//!   naming the dataset and the column, never a default: a dataset read with
//!   half its operations would be fitted.
//! - **An operation that does not compile is still saved.** Marking it is the
//!   editor's job (the goals document), and refusing the save would lose the
//!   edit that was going to repair it. What *is* refused: an empty or taken
//!   name, a base that does not exist or leads back to the dataset, a changed
//!   base, and operation ids that are empty or repeated.

use std::collections::BTreeSet;

use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Assignment, Delete, Expr, Insert, Select, Source, Statement, Update, Value};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;

use crate::compile::Library;
use crate::def::{Base, DatasetDef, DatasetId, Operation};

/// Name of the datasets table in the primary database.
pub const DATASETS_TABLE: &str = "_fd_datasets";

const COL_ID: &str = "id";
const COL_NAME: &str = "name";
const COL_DESCRIPTION: &str = "description";
const COL_BASE: &str = "base";
const COL_OPERATIONS: &str = "operations";
const COL_ATTRIBUTES: &str = "attributes";

/// The fields of `_fd_datasets`, in declaration order.
fn dataset_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let json = || TypeRef::Basic(BasicType::Json);
    vec![
        DataField::plain(COL_ID, TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key(),
        DataField::plain(COL_NAME, text()).required().unique(),
        DataField::plain(COL_DESCRIPTION, text()),
        DataField::plain(COL_BASE, json()).required(),
        DataField::plain(COL_OPERATIONS, json()).required(),
        DataField::plain(COL_ATTRIBUTES, json()).required(),
    ]
}

/// Ensure `_fd_datasets` exists, creating it if absent. Idempotent.
pub async fn bootstrap_datasets(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(DATASETS_TABLE, &dataset_fields())
        .await
}

/// Save a dataset: insert it, or replace the row with its id.
///
/// See the module docs for what is refused. The operations are **not**
/// compiled here: one that does not compile is saved and marked.
pub async fn save_dataset(catalog: &Catalog, def: &DatasetDef) -> Result<()> {
    let name = def.name.trim();
    if name.is_empty() {
        return Err(Error::invalid("a dataset needs a name"));
    }
    if let Some(other) = load_dataset_by_name(catalog, name).await?
        && other.id != def.id
    {
        return Err(Error::invalid(format!(
            "a dataset called `{name}` already exists; each dataset has a name of its own"
        )));
    }
    let existing = load_dataset(catalog, def.id).await?;
    if let Some(existing) = &existing
        && existing.base != def.base
    {
        return Err(Error::invalid(format!(
            "the base of `{name}` cannot be changed: every operation is written against the \
             columns it provides. Clone the dataset or create a new one instead"
        )));
    }
    let mut ids = BTreeSet::new();
    for op in &def.operations {
        if op.id.trim().is_empty() {
            return Err(Error::invalid(format!(
                "an operation of `{name}` has no id"
            )));
        }
        if !ids.insert(op.id.as_str()) {
            return Err(Error::invalid(format!(
                "two operations of `{name}` have the id `{}`",
                op.id
            )));
        }
    }
    check_base(catalog, def).await?;

    let columns: Vec<String> = [
        COL_ID,
        COL_NAME,
        COL_DESCRIPTION,
        COL_BASE,
        COL_OPERATIONS,
        COL_ATTRIBUTES,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect();
    let values = vec![
        Value::Uuid(def.id.0),
        Value::Text(name.to_owned()),
        Value::Text(def.description.clone()),
        Value::Json(to_json(&def.base)?),
        Value::Json(to_json(&def.operations)?),
        Value::Json(Json::Object(Attrs::new())),
    ];
    let statement: Statement = if existing.is_some() {
        Update::new(
            DATASETS_TABLE,
            columns
                .iter()
                .zip(values)
                .filter(|(c, _)| *c != COL_ID && *c != COL_ATTRIBUTES)
                .map(|(c, v)| Assignment::new(c.clone(), Expr::Lit(v)))
                .collect(),
        )
        .filter(Expr::col(COL_ID).eq(Expr::lit(def.id.0)))
        .into()
    } else {
        Insert::row(
            DATASETS_TABLE,
            columns,
            values.into_iter().map(Expr::Lit).collect(),
        )
        .into()
    };
    exec(catalog, statement).await
}

/// The base must exist, and a dataset base must not lead back to this one.
async fn check_base(catalog: &Catalog, def: &DatasetDef) -> Result<()> {
    match &def.base {
        Base::Table { table } => {
            if table.starts_with("_fd_") {
                return Err(Error::invalid(format!(
                    "`{table}` is one of the server's own tables, and a dataset cannot be built on it"
                )));
            }
            if catalog.get(table)?.is_none() {
                return Err(Error::invalid(format!(
                    "there is no table called `{table}` to base `{}` on",
                    def.name.trim()
                )));
            }
            Ok(())
        }
        Base::Dataset { dataset } => {
            let mut library = load_library(catalog).await?;
            if library.get(*dataset).is_none() {
                return Err(Error::invalid(format!(
                    "the dataset `{}` is based on does not exist",
                    def.name.trim()
                )));
            }
            library.insert(def.clone());
            // Only the chain of bases: a join that leads back is an operation
            // error, marked on the operation, not a refused save.
            let mut seen = BTreeSet::from([def.id]);
            let mut at = *dataset;
            loop {
                if !seen.insert(at) {
                    return Err(Error::invalid(format!(
                        "`{}` would be based on itself, through the datasets it is based on",
                        def.name.trim()
                    )));
                }
                match library.get(at).map(|d| &d.base) {
                    Some(Base::Dataset { dataset }) => at = *dataset,
                    _ => return Ok(()),
                }
            }
        }
    }
}

/// The dataset with this id.
pub async fn load_dataset(catalog: &Catalog, id: DatasetId) -> Result<Option<DatasetDef>> {
    load_one(catalog, Expr::col(COL_ID).eq(Expr::lit(id.0))).await
}

/// The dataset called `name`.
pub async fn load_dataset_by_name(catalog: &Catalog, name: &str) -> Result<Option<DatasetDef>> {
    load_one(catalog, Expr::col(COL_NAME).eq(Expr::lit(name))).await
}

/// The dataset with this id, or not-found naming it.
pub async fn require_dataset(catalog: &Catalog, id: DatasetId) -> Result<DatasetDef> {
    load_dataset(catalog, id)
        .await?
        .ok_or_else(|| Error::not_found(format!("there is no dataset with id {id}")))
}

/// Every dataset, by name.
pub async fn list_datasets(catalog: &Catalog) -> Result<Vec<DatasetDef>> {
    let mut out: Vec<DatasetDef> = rows(catalog, Select::from(Source::table(DATASETS_TABLE)))
        .await?
        .iter()
        .map(dataset_from_row)
        .collect::<Result<_>>()?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Every dataset, as a [`Library`] a compile reads from.
pub async fn load_library(catalog: &Catalog) -> Result<Library> {
    Ok(Library::new(list_datasets(catalog).await?))
}

/// The datasets that read `id`: based on it, or joining or appending it.
pub async fn datasets_using(catalog: &Catalog, id: DatasetId) -> Result<Vec<DatasetDef>> {
    Ok(list_datasets(catalog)
        .await?
        .into_iter()
        .filter(|d| d.id != id && d.dependencies().contains(&id))
        .collect())
}

/// Delete a dataset, returning whether there was one.
///
/// Refused while other datasets read it: their bases cannot be changed, so
/// deleting it would break them for good. A model that uses it is the caller's
/// to warn about (it depends on `sc-model`, above this crate); such a model is
/// then listed with its error and stays editable.
pub async fn delete_dataset(catalog: &Catalog, id: DatasetId) -> Result<bool> {
    let Some(def) = load_dataset(catalog, id).await? else {
        return Ok(false);
    };
    let users = datasets_using(catalog, id).await?;
    if !users.is_empty() {
        return Err(Error::invalid(format!(
            "`{}` cannot be deleted while other datasets read it: {}",
            def.name,
            users
                .iter()
                .map(|d| format!("`{}`", d.name))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    exec(
        catalog,
        Delete::from(DATASETS_TABLE)
            .filter(Expr::col(COL_ID).eq(Expr::lit(id.0)))
            .into(),
    )
    .await?;
    Ok(true)
}

/// Copy a dataset under a new id and `name` (or "… (copy)", made unique).
pub async fn clone_dataset(
    catalog: &Catalog,
    id: DatasetId,
    name: Option<&str>,
) -> Result<DatasetDef> {
    let source = require_dataset(catalog, id).await?;
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_owned(),
        None => {
            let taken: BTreeSet<String> = list_datasets(catalog)
                .await?
                .into_iter()
                .map(|d| d.name)
                .collect();
            let mut candidate = format!("{} (copy)", source.name);
            let mut n = 2;
            while taken.contains(&candidate) {
                candidate = format!("{} (copy {n})", source.name);
                n += 1;
            }
            candidate
        }
    };
    let copy = DatasetDef {
        id: DatasetId::new(),
        name,
        ..source
    };
    save_dataset(catalog, &copy).await?;
    Ok(copy)
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<Json> {
    serde_json::to_value(value)
        .map_err(|e| Error::msg(format!("a dataset could not be stored: {e}")))
}

/// Rebuild a [`DatasetDef`] from its row, strictly.
fn dataset_from_row(row: &Row) -> Result<DatasetDef> {
    let id = match row.get(COL_ID) {
        Some(Value::Uuid(u)) => DatasetId(*u),
        other => return Err(bad(COL_ID, "a uuid", other)),
    };
    let name = match row.get(COL_NAME) {
        Some(Value::Text(t)) => t.clone(),
        other => return Err(bad(COL_NAME, "text", other)),
    };
    let at = |e: String| Error::invalid(format!("dataset `{name}`: {e}"));
    let description = match row.get(COL_DESCRIPTION) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        other => return Err(bad(COL_DESCRIPTION, "text", other)),
    };
    let base: Base = json_column(row, COL_BASE).map_err(at)?;
    let operations: Vec<Operation> = json_column(row, COL_OPERATIONS).map_err(at)?;
    Ok(DatasetDef {
        id,
        name,
        description,
        base,
        operations,
    })
}

fn json_column<T: serde::de::DeserializeOwned>(
    row: &Row,
    column: &str,
) -> std::result::Result<T, String> {
    match row.get(column) {
        Some(Value::Json(v)) => {
            serde_json::from_value(v.clone()).map_err(|e| format!("{column} is not readable: {e}"))
        }
        // SQLite hands JSON back as text when it cannot tell.
        Some(Value::Text(t)) => {
            serde_json::from_str(t).map_err(|e| format!("{column} is not readable: {e}"))
        }
        other => Err(bad(column, "json", other).to_string()),
    }
}

fn bad(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(v) => Error::invalid(format!("{column} should be {expected}, got {}", v.kind())),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

async fn load_one(catalog: &Catalog, filter: Expr) -> Result<Option<DatasetDef>> {
    let select = Select::from(Source::table(DATASETS_TABLE)).filter(filter);
    match rows(catalog, select).await?.first() {
        Some(row) => Ok(Some(dataset_from_row(row)?)),
        None => Ok(None),
    }
}

async fn exec(catalog: &Catalog, statement: Statement) -> Result<()> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

pub(crate) async fn rows(catalog: &Catalog, select: Select) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await
}

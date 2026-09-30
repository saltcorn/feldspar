//! Reading a stage (analytics TODO A1.7): a page of rows, the column types and
//! the total, in the stage's order.
//!
//! **As the caller**, which in this milestone means as the admin: the query is
//! the compiled dataset run on the primary database, with no ownership formula
//! or row-level policy in it. A9 is where a restricted user's reads go through
//! their table permissions.
//!
//! Paging is stable because the order is total (see
//! [`compile`](crate::compile)): the sort keys, then the row key, the group keys
//! or every column.

use sc_catalog::Catalog;
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{Statement, Value};
use serde_json::Value as Json;

use crate::compile::{Library, Options, Restriction, Stage, compile};
use crate::def::DatasetDef;
use crate::shape::{ColType, Grain, Schema, StageColumn};
use crate::store::load_library;

/// The most rows one page may ask for.
pub const MAX_PAGE: u64 = 1_000;

/// Which rows of a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// Rows to skip.
    pub offset: u64,
    /// Rows to return, at most [`MAX_PAGE`].
    pub limit: u64,
}

impl Page {
    /// The first `limit` rows.
    pub fn first(limit: u64) -> Page {
        Page { offset: 0, limit }
    }
}

/// One page of a stage.
#[derive(Debug, Clone)]
pub struct StagePage {
    /// The columns, their types filled in from the rows where the compiler
    /// could not tell.
    pub columns: Vec<StageColumn>,
    /// What a row represents.
    pub grain: Grain,
    /// The rows, each in column order.
    pub rows: Vec<Vec<Value>>,
    /// How many rows the stage has in all.
    pub total: u64,
}

/// Read one page of the stage after the first `upto` operations of `def`
/// (all of them when `None`), compiling it against the catalog as it is now.
pub async fn read_stage(
    catalog: &Catalog,
    def: &DatasetDef,
    upto: Option<usize>,
    page: Page,
) -> Result<StagePage> {
    let schema = Schema::of_catalog(catalog)?;
    let mut library = load_library(catalog).await?;
    library.insert(def.clone());
    let compiled = compile(&schema, &library, def, Options::default());
    let upto = upto.unwrap_or(def.operations.len());
    let stage = compiled
        .stage(upto)
        .map_err(|e| Error::invalid(format!("`{}` does not read here: {e}", def.name)))?;
    read_page(catalog, stage, page).await
}

/// Read one page of `stage`, and count all of it.
pub async fn read_page(catalog: &Catalog, stage: &Stage, page: Page) -> Result<StagePage> {
    let limit = page.limit.clamp(1, MAX_PAGE);
    let select = stage
        .rows_query(None, Some((page.offset, limit)), false)
        .map_err(Error::invalid)?;
    let rows = run(catalog, select.into()).await?;
    let total = count(catalog, stage, None).await?;
    let shape = stage.shape();
    let mut rows: Vec<Vec<Value>> = rows.into_iter().map(Row::into_values).collect();
    let columns = typed(shape.columns, &mut rows);
    Ok(StagePage {
        columns,
        grain: shape.grain,
        rows,
        total,
    })
}

/// How many rows `stage` has, restricted.
pub async fn count(
    catalog: &Catalog,
    stage: &Stage,
    restrict: Option<&Restriction>,
) -> Result<u64> {
    let select = stage.count_query(restrict).map_err(Error::invalid)?;
    let rows = run(catalog, select.into()).await?;
    match rows.first().and_then(|r| r.get_index(0)) {
        Some(Value::Int(n)) => Ok(u64::try_from(*n).unwrap_or(0)),
        Some(Value::Decimal(d)) => Ok(d.to_string().parse().unwrap_or(0)),
        other => Err(Error::database(format!(
            "counting a dataset's rows gave {other:?}"
        ))),
    }
}

/// Every row of `stage` — restricted, the first `limit` of them, or all of
/// them — with the row key as the last value of each row when the stage has
/// one. The caller bounds an unlimited read by counting first.
pub async fn read_rows(
    catalog: &Catalog,
    stage: &Stage,
    restrict: Option<&Restriction>,
    limit: Option<u64>,
) -> Result<Rows> {
    let select = stage
        .rows_query(restrict, limit.map(|l| (0, l)), true)
        .map_err(Error::invalid)?;
    let rows = run(catalog, select.into()).await?;
    let shape = stage.shape();
    let with_key = stage.has_row_key();
    let mut values: Vec<Vec<Value>> = rows.into_iter().map(Row::into_values).collect();
    let keys = if with_key {
        values
            .iter_mut()
            .map(|r| r.pop().unwrap_or(Value::Null))
            .collect()
    } else {
        Vec::new()
    };
    Ok(Rows {
        columns: typed(shape.columns, &mut values),
        grain: shape.grain,
        rows: values,
        keys,
    })
}

/// Rows read by [`read_rows`].
#[derive(Debug, Clone)]
pub struct Rows {
    /// The columns.
    pub columns: Vec<StageColumn>,
    /// What a row represents.
    pub grain: Grain,
    /// The rows, each in column order.
    pub rows: Vec<Vec<Value>>,
    /// Each row's base-table primary key ([`ROW_KEY`](crate::ROW_KEY)), when the stage has
    /// one; empty otherwise.
    pub keys: Vec<Value>,
}

/// The distinct values of `column` at `stage`, most frequent first.
pub async fn column_values(
    catalog: &Catalog,
    stage: &Stage,
    column: &str,
    limit: u64,
) -> Result<Vec<Value>> {
    let select = stage
        .values_query(column, limit.clamp(1, MAX_PAGE))
        .map_err(Error::invalid)?;
    Ok(run(catalog, select.into())
        .await?
        .into_iter()
        .filter_map(|r| r.get_index(0).cloned())
        .collect())
}

/// Compile `def` and give back its last stage, or the sentence saying why it
/// does not read.
pub fn last_stage(
    schema: &Schema,
    library: &Library,
    def: &DatasetDef,
    options: Options,
) -> Result<Stage> {
    let compiled = compile(schema, library, def, options);
    compiled
        .last()
        .cloned()
        .map_err(|e| Error::invalid(format!("the dataset `{}` does not read: {e}", def.name)))
}

/// A value as the JSON a grid shows: numbers as numbers, including a decimal
/// (an analysis wants arithmetic, not the string that keeps its last digit),
/// and dates and times as ISO strings.
pub fn value_json(value: &Value) -> Json {
    match value {
        Value::Decimal(d) => d
            .to_string()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(Json::Null, Json::Number),
        other => sc_types::value_to_json(other),
    }
}

/// Each value as its column's type says it is, where a backend hands back
/// another representation: SQLite has no boolean or date of its own, so a
/// computed `price > 100` comes back `1` and a `min(viewed_on)` as text, and a
/// Postgres `sum` of integers is a decimal. The same dataset then reads the
/// same on both.
fn coerce(columns: &[StageColumn], rows: &mut [Vec<Value>]) {
    for row in rows {
        for (value, column) in row.iter_mut().zip(columns) {
            let new = match (&*value, column.ty) {
                (Value::Int(n), ColType::Bool) => Some(Value::Bool(*n != 0)),
                (Value::Int(n), ColType::Float) => Some(Value::Float(*n as f64)),
                (Value::Float(f), ColType::Int) if f.fract() == 0.0 && f.abs() < 9.0e15 => {
                    Some(Value::Int(*f as i64))
                }
                (Value::Decimal(d), ColType::Int) if d.is_integer() => {
                    d.trunc().to_string().parse::<i64>().ok().map(Value::Int)
                }
                (Value::Text(t), ColType::Date) => chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d")
                    .ok()
                    .map(Value::Date),
                (Value::Text(t), ColType::Timestamp) => chrono::DateTime::parse_from_rfc3339(t)
                    .ok()
                    .map(|d| Value::Timestamp(d.with_timezone(&chrono::Utc))),
                _ => None,
            };
            if let Some(new) = new {
                *value = new;
            }
        }
    }
}

/// Columns whose type the compiler did not know, typed by the first value
/// that is not missing; then every value coerced to its column's type.
fn typed(columns: Vec<StageColumn>, rows: &mut [Vec<Value>]) -> Vec<StageColumn> {
    let columns = fill_types(columns, rows);
    coerce(&columns, rows);
    columns
}

/// Columns whose type the compiler did not know, typed by the first value
/// that is not missing.
fn fill_types(mut columns: Vec<StageColumn>, rows: &[Vec<Value>]) -> Vec<StageColumn> {
    for (i, column) in columns.iter_mut().enumerate() {
        if column.ty == ColType::Unknown
            && let Some(v) = rows.iter().filter_map(|r| r.get(i)).find(|v| !v.is_null())
        {
            column.ty = ColType::of_value(v);
        }
    }
    columns
}

async fn run(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

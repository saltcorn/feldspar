//! When each entity was last edited: the `updated_at` column, and the
//! `_fd_table_edits` table that stands in for it on tables.
//!
//! The admin UI's home page lists every entity with the most recently changed
//! ones first, so every definition an admin edits carries the instant of its
//! last save. For a trigger, an agent, a file store or an application that is a
//! column of the row that *is* the definition ([`COL_UPDATED_AT`]), written by
//! the store's own save function: whichever path saves it (the admin API, MCP,
//! the CLI) stamps it.
//!
//! **A table is the exception**, for the reason `table_meta` states at length: a
//! table's `_fd_tables` row is an overlay that may not exist, and creating one
//! just to hold a timestamp would make an unconfigured table look configured
//! (and an overlay row is the authority on access). So a table's edit time lives
//! in a table of its own, keyed by the table's name, and [`touch_tables`] is
//! called by the writers that change a table: the schema editor's batches, and
//! the overlay saves.
//!
//! Both are **bookkeeping, not definition**: nullable, never read back into the
//! definition types, and absent for anything saved before they existed.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sc_error::Result;
use sc_query::{Delete, Expr, Insert, Select, Source, Statement, Value};
use sc_types::{BasicType, TypeRef};

use crate::catalog::Catalog;
use crate::field::DataField;
use crate::table::Table;

/// The column every definition table carries for its last save.
pub const COL_UPDATED_AT: &str = "updated_at";

/// Name of the table holding each table's last edit time.
pub const TABLE_EDITS_TABLE: &str = "_fd_table_edits";

const COL_NAME: &str = "name";

/// The `updated_at` field, for a definition table's field list. Nullable: it is
/// reconciled onto tables that already have rows, and "not known" is its honest
/// value for those.
pub fn updated_at_field() -> DataField {
    DataField::plain(COL_UPDATED_AT, TypeRef::Basic(BasicType::Timestamp))
}

/// The value a save writes into [`COL_UPDATED_AT`]: now.
pub fn updated_now() -> Value {
    Value::Timestamp(Utc::now())
}

/// Ensure `_fd_table_edits` exists. Called by
/// [`bootstrap_table_meta`](crate::bootstrap_table_meta), so every installation
/// that can configure a table can also record that it did.
pub async fn bootstrap_table_edits(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(
            TABLE_EDITS_TABLE,
            &[
                DataField::plain(COL_NAME, TypeRef::Basic(BasicType::Text))
                    .required()
                    .primary_key(),
                updated_at_field().required(),
            ],
        )
        .await
}

/// Record that these tables were edited now.
///
/// A delete and an insert per name rather than an upsert, which the query
/// layer does not speak; two concurrent saves of one table can at worst both
/// write "now". On an installation that has not bootstrapped the table (a bare
/// catalog in a test), there is nothing to record into and this does nothing.
pub async fn touch_tables<S: AsRef<str>>(catalog: &Catalog, names: &[S]) -> Result<()> {
    if names.is_empty() || catalog.get(TABLE_EDITS_TABLE)?.is_none() {
        return Ok(());
    }
    let now = Utc::now();
    for name in names {
        let name = name.as_ref();
        let delete =
            Delete::from(TABLE_EDITS_TABLE).filter(Expr::col(COL_NAME).eq(Expr::lit(name)));
        run(catalog, Statement::from(delete)).await?;
        let insert = Insert::row(
            TABLE_EDITS_TABLE,
            vec![COL_NAME.to_owned(), COL_UPDATED_AT.to_owned()],
            vec![
                Expr::Lit(Value::Text(name.to_owned())),
                Expr::Lit(Value::Timestamp(now)),
            ],
        );
        run(catalog, Statement::from(insert)).await?;
    }
    Ok(())
}

/// Forget a dropped table's edit time, so a new table of the same name does not
/// inherit it.
pub async fn forget_table_edit(catalog: &Catalog, name: &str) -> Result<()> {
    if catalog.get(TABLE_EDITS_TABLE)?.is_none() {
        return Ok(());
    }
    let delete = Delete::from(TABLE_EDITS_TABLE).filter(Expr::col(COL_NAME).eq(Expr::lit(name)));
    run(catalog, Statement::from(delete)).await
}

/// Every `key` → `updated_at` in a definition table (or `_fd_table_edits`),
/// leaving out the rows that have none.
///
/// `key` is the column the caller joins on — a definition's `id` (its text
/// form), or a table's `name` in `_fd_table_edits`. A table without the column
/// (not yet bootstrapped) reads as empty rather than failing the list.
pub async fn updated_at_by(
    catalog: &Catalog,
    table: &str,
    key: &str,
) -> Result<HashMap<String, DateTime<Utc>>> {
    let Some(stored) = catalog.get(table)? else {
        return Ok(HashMap::new());
    };
    if stored.field(COL_UPDATED_AT).is_none() || stored.field(key).is_none() {
        return Ok(HashMap::new());
    }
    let select = Select::from(Source::table(table));
    let rows: Vec<sc_db::Row> = catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let key = match row.get(key)? {
                Value::Text(name) => name.clone(),
                Value::Uuid(id) => id.to_string(),
                _ => return None,
            };
            match row.get(COL_UPDATED_AT)? {
                Value::Timestamp(at) => Some((key, *at)),
                _ => None,
            }
        })
        .collect())
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

//! [`ApiRow`]: a row fit to leave the server (security review 2026-11 §3,
//! TODO-type-hardening H3.5).
//!
//! Hiding is a property of the value, not of the handler that remembered it:
//! the users table's password hash, a `collab_doc` field's state and every
//! secret's sealed value are in the rows the server reads and in the images the
//! change log records, and a reader that serialized those straight into a
//! response leaked them (#6, row history). An `ApiRow` is made only by this
//! module's constructors, each of which drops the columns no API reads and
//! turns each secret into its sentinel; it has `Serialize` and nothing else
//! does what it does. The change log's images are `sc_catalog::history::RawRow`,
//! which has no `Serialize` at all, so the only way from one to a response is
//! through here.

use std::collections::BTreeMap;

use sc_catalog::Table;
use sc_catalog::history::RawRow;
use sc_query::Value;
use serde::Serialize;
use serde_json::{Map, Value as Json};

use crate::user_rows::{is_hidden_column, visible_field};

/// A row as a response may carry it: no column an API never reads, and each
/// secret as its sentinel.
///
/// No `Deserialize`, no `Default`, no `From` a map: the constructors below
/// are the only way to make one. Serializes as the JSON object it holds.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ApiRow(Map<String, Json>);

impl ApiRow {
    /// `values` of `table` — a row read, a change-log image — without the
    /// hidden columns, with each secret as its sentinel. Keys that are not the
    /// table's columns (a column dropped since, a projected value) are kept.
    pub fn of(table: &Table, values: Map<String, Json>) -> ApiRow {
        let mut values = values;
        values.retain(|column, _| !is_hidden_column(table, column));
        let mut row = Json::Object(values);
        sc_catalog::secrets::redact_json(table, &mut row);
        match row {
            Json::Object(map) => ApiRow(map),
            _ => ApiRow(Map::new()),
        }
    }

    /// A change-log image of `table`'s row, as [`of`](ApiRow::of) shows it.
    pub fn of_image(table: &Table, image: RawRow) -> ApiRow {
        ApiRow::of(table, image.into_map())
    }

    /// A row read as values: `table`'s own visible columns in field order (a
    /// projected join value is an enforcement input, not payload), each secret
    /// as its sentinel, rendered as the REST wire shape (`value_to_json`).
    pub fn of_values(table: &Table, values: &BTreeMap<String, Value>) -> ApiRow {
        let mut values = values.clone();
        sc_catalog::secrets::redact_values(table, &mut values);
        let mut map = Map::new();
        for field in &table.fields {
            let name = &field.base.name;
            if visible_field(table, name).is_none() {
                continue;
            }
            if let Some(value) = values.get(name) {
                map.insert(name.clone(), sc_types::value_to_json(value));
            }
        }
        ApiRow(map)
    }

    /// A row read as values, rendered through `shape` — a REST `select`'s
    /// nested shape — from the values with the hidden columns already gone and
    /// each secret already its sentinel, so the shape cannot reach either.
    pub fn shaped(
        table: &Table,
        values: &BTreeMap<String, Value>,
        shape: impl FnOnce(&BTreeMap<String, Value>) -> Map<String, Json>,
    ) -> ApiRow {
        let mut values = values.clone();
        values.retain(|column, _| !is_hidden_column(table, column));
        sc_catalog::secrets::redact_values(table, &mut values);
        ApiRow(shape(&values))
    }

    /// A row as JSON — a write's answer, a live update — reduced to `table`'s
    /// own visible columns in field order, each secret as its sentinel. `None`
    /// for anything that is not an object.
    pub fn own_columns(table: &Table, row: &Json) -> Option<ApiRow> {
        let Json::Object(obj) = row else {
            return None;
        };
        let mut out = Map::with_capacity(obj.len());
        for field in &table.fields {
            let name = &field.base.name;
            if visible_field(table, name).is_none() {
                continue;
            }
            if let Some(value) = obj.get(name) {
                out.insert(name.clone(), value.clone());
            }
        }
        Some(ApiRow::of(table, out))
    }

    /// The row's columns.
    pub fn as_map(&self) -> &Map<String, Json> {
        &self.0
    }

    /// The row as the JSON object it is, for a response being built.
    pub fn into_json(self) -> Json {
        Json::Object(self.0)
    }

    /// Rows as a JSON array.
    pub fn array(rows: impl IntoIterator<Item = ApiRow>) -> Json {
        Json::Array(rows.into_iter().map(ApiRow::into_json).collect())
    }
}

//! What every table trait shares: its configuration, and the `where` object
//! (§11.3).
//!
//! `query_table` and `edit_table_rows`' insert, update and delete are four
//! grants over one table, and the parts they have in common are here rather than copied
//! four times — not to save the lines, but because each of them is a **promise
//! to the model**. The `where` vocabulary a model learns from `query_books` is
//! the one `delete_from_books` takes; a field named in the allow-list means the
//! same thing in each; and an unknown field is refused with the same message
//! listing the ones that exist. Four dialects of the same tool set would be four
//! chances for the model to get it right somewhere and wrong somewhere else.

use sc_catalog::{Catalog, DataField, DataFieldKind, Table};
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType};
use serde_json::{Map, Value as Json, json};

use sc_api::rows;

/// The `where` object's lowering, which lives in `sc_api::filter` beside the
/// comparison vocabulary it calls.
///
/// It was written here, for these tools, and it moved when a **code body** grew
/// the same argument (§10.1's `db.books.where({ … })`): one walk, one meaning,
/// one place to add `and`/`or`/`not` — which is where they were added, so a
/// model's filter and an admin's code body gained them together. The tools keep
/// naming them through this module, because "what a table trait's `where` is" is
/// still this module's promise to the model.
pub use sc_api::filter::{queryable_field, required_where, where_expr};

/// The table a trait is configured against. Every table trait has one, and
/// **only** one: "which tables may this agent reach?" must be answerable off the
/// agent's definition.
pub const CFG_TABLE: &str = "table";
/// The fields the model may use. Empty means all of them.
///
/// What "use" means is each trait's own: for `query_table` it is what may be
/// seen, filtered on and ordered by; for `edit_table_rows`' insert and update
/// it is what may be **written**. A read allow-list narrows a filter because a hidden
/// field could otherwise be read back one comparison at a time; a write
/// allow-list does not, because a filter naming a field the model may not change
/// still only reaches rows the caller may already see.
pub const CFG_FIELDS: &str = "fields";
/// The ceiling on how many rows one call may return or affect.
pub const CFG_MAX_ROWS: &str = "max_rows";

/// The filter object every table trait takes.
pub const ARG_WHERE: &str = "where";

/// The paragraph every tool that takes a `where` puts in its description.
///
/// One text, so the vocabulary a model picks up from one tool is the one the
/// next accepts.
pub const WHERE_HELP: &str = "In `where`, each entry is a field name against \
     either a value to match exactly or an object with one operator key: `eq`, \
     `ne`, `gt`, `gte`, `lt`, `lte`, `like`, `ilike` (text patterns, `%` matches \
     any run of characters), `in` (a list), `nin` (a list) or `is_null` (true or \
     false). All the entries must hold at once, and `and`, `or` and `not` take \
     filters of the same shape when that is not what you mean.";

// --- the configuration ------------------------------------------------------

/// A string setting, or the empty string.
pub fn config_str(config: &Attrs, key: &str) -> String {
    config
        .get(key)
        .and_then(Json::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// The configured table, resolved against the catalog.
pub fn configured_table(catalog: &Catalog, config: &Attrs) -> Result<Table> {
    let name = config_str(config, CFG_TABLE);
    if name.is_empty() {
        return Err(Error::invalid(format!("`{CFG_TABLE}` is required")));
    }
    catalog.require(&name)
}

/// The configured allow-list, as written. Empty means "every field".
pub fn configured_fields(config: &Attrs) -> Result<Vec<String>> {
    match config.get(CFG_FIELDS) {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        Error::invalid(format!("`{CFG_FIELDS}` should be a list of field names"))
                    })
            })
            .collect(),
        Some(other) => Err(Error::invalid(format!(
            "`{CFG_FIELDS}` should be a list of field names, got {other}"
        ))),
    }
}

/// The fields an instance exposes, in the table's own declaration order.
///
/// Table order rather than allow-list order, so a tool's description reads like
/// the table does and two instances over the same table cannot describe it
/// differently depending on how the admin typed the list.
pub fn visible_fields(table: &Table, config: &Attrs) -> Result<Vec<String>> {
    let allowed = configured_fields(config)?;
    Ok(table
        .fields
        .iter()
        .filter(|f| !f.is_collab_doc() && !f.is_secret())
        .map(|f| f.base.name.clone())
        .filter(|name| allowed.is_empty() || allowed.contains(name))
        .collect())
}

/// Every field of the table, named — what a trait passes where an allow-list
/// does not apply (a write trait's filter).
pub fn all_fields(table: &Table) -> Vec<String> {
    table
        .fields
        .iter()
        .filter(|f| !f.is_collab_doc() && !f.is_secret())
        .map(|f| f.base.name.clone())
        .collect()
}

/// The configured ceiling, or `default` when the admin set none.
pub fn max_rows(config: &Attrs, default: u64) -> Result<u64> {
    match config.get(CFG_MAX_ROWS) {
        None | Some(Json::Null) => Ok(default),
        Some(Json::Number(n)) => match n.as_i64() {
            Some(n) if n >= 0 => Ok(n as u64),
            _ => Err(Error::invalid(format!(
                "`{CFG_MAX_ROWS}` should be a whole number, got {n}"
            ))),
        },
        Some(other) => Err(Error::invalid(format!(
            "`{CFG_MAX_ROWS}` should be a number, got {other}"
        ))),
    }
}

/// The save-time checks every table trait makes: the table exists, it is
/// addressable by primary key, every field the allow-list names is real, and the
/// ceiling admits at least one row.
///
/// Returned as the table, because every caller wants it next.
pub fn check_table_config(catalog: &Catalog, config: &Attrs, default_max: u64) -> Result<Table> {
    let table = configured_table(catalog, config)?;
    // Addressable by primary key: a read whose rows nobody can then name is a
    // half-useful answer, and every write trait needs the key outright.
    rows::single_pk(&table)?;
    // Every named field is real. A stale allow-list would otherwise silently
    // narrow the tool to nothing, which reads as an empty table.
    for name in configured_fields(config)? {
        if table.field(&name).is_none() {
            return Err(Error::invalid(format!(
                "`{}` has no field `{name}`",
                table.name
            )));
        }
    }
    if max_rows(config, default_max)? == 0 {
        return Err(Error::invalid(format!(
            "`{CFG_MAX_ROWS}` must be at least 1"
        )));
    }
    Ok(table)
}

// --- describing the table to the model --------------------------------------

/// `id (int, primary key), title (text), author (int, references authors)` — the
/// named fields as one line.
pub fn field_list(table: &Table, fields: &[String]) -> String {
    table
        .fields
        .iter()
        .filter(|f| fields.contains(&f.base.name))
        .map(|f| format!("{} ({})", f.base.name, field_note(f)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One field's type and whatever else the model needs to know to use it.
pub fn field_note(field: &DataField) -> String {
    let mut note = field.base.type_.name().to_owned();
    if field.primary_key {
        note.push_str(", primary key");
    }
    if field.required && !field.primary_key {
        note.push_str(", required");
    }
    if let Some(expression) = &field.default_expression {
        note.push_str(&format!(", defaults to `{expression}` when left out"));
    }
    match &field.kind {
        DataFieldKind::Key { target_table, .. } => {
            note.push_str(&format!(", references {}", target_table.0));
        }
        // A calculated field is computed on read and has no column, so it comes
        // back with every row but cannot be filtered, ordered or written — and
        // being told that up front is cheaper than a failed call that says it.
        DataFieldKind::Calc { .. } => note.push_str(", computed; not filterable"),
        DataFieldKind::File { .. } => note.push_str(", a file path"),
        // Never offered (`visible_fields` leaves secrets out); named for
        // exhaustiveness.
        DataFieldKind::Secret => note.push_str(", a secret; never read"),
        DataFieldKind::Plain => {}
    }
    note
}

/// The JSON-Schema type a field's values take, where saying so cannot refuse a
/// value the row layer would have accepted.
///
/// Deliberately partial. A date, a UUID and a decimal all arrive as strings that
/// [`rows::column_value`] coerces, and a schema that insisted on `"string"` for
/// one and `"number"` for another would refuse valid calls at the vendor, before
/// this crate ever sees them. Three types are unambiguous, and the rest are
/// described in prose instead.
fn json_type(field: &DataField) -> Option<&'static str> {
    match field.base.type_.as_basic()? {
        BasicType::Bool => Some("boolean"),
        BasicType::Int => Some("integer"),
        BasicType::Text => Some("string"),
        _ => None,
    }
}

/// One field as a JSON-Schema property: its type where that is safe to state,
/// and its note as the description.
pub fn field_schema(field: &DataField) -> Json {
    let mut schema = Map::new();
    if let Some(type_) = json_type(field) {
        // Nullable unless the column is `NOT NULL`: writing null is how a value
        // is cleared, and a schema that could not say so would make an optional
        // field unclearable.
        schema.insert(
            "type".to_owned(),
            match field.required {
                true => json!(type_),
                false => json!([type_, "null"]),
            },
        );
    }
    schema.insert(
        "description".to_owned(),
        json!(format!("{} ({})", field.base.name, field_note(field))),
    );
    Json::Object(schema)
}

/// The `where` argument's schema, enumerating the fields that can be filtered
/// on.
///
/// A condition is a bare value *or* an operator object, which no single JSON
/// Schema type describes; what the schema is for here is telling the model which
/// **names** exist, and `additionalProperties: false` is what makes a guessed
/// one fail at the vendor rather than three tool calls later.
pub fn where_schema(table: &Table, fields: &[String]) -> Json {
    let mut conditions = Map::new();
    for field in queryable_fields(table, fields) {
        conditions.insert(
            field.base.name.clone(),
            json!({
                "description": format!("{} ({})", field.base.name, field_note(field)),
            }),
        );
    }
    json!({
        "type": "object",
        "description":
            "Which rows: field name → an exact value, or an object with one of \
             the operator keys.",
        "properties": Json::Object(conditions),
        "additionalProperties": false,
    })
}

/// The named fields that are backed by a column, in the table's order — the ones
/// a `where` or an `ORDER BY` can name.
pub fn queryable_fields<'a>(table: &'a Table, fields: &[String]) -> Vec<&'a DataField> {
    table
        .fields
        .iter()
        .filter(|f| fields.contains(&f.base.name) && !f.is_calc())
        .collect()
}

// --- the arguments ----------------------------------------------------------

/// The arguments object, or an empty one, with every key checked against the
/// ones this tool declared.
///
/// [`sc_api::mcp`]'s, re-exported rather than reimplemented: the administrative
/// tools moved down there and every other built-in trait parses its arguments
/// the same way, so one function answers "what is a well-formed tool call?" for
/// both.
pub use sc_api::mcp::arguments;

// --- rows in, rows out ------------------------------------------------------

/// One row narrowed to the named fields.
pub fn project(row: &Json, fields: &[String]) -> Json {
    let Some(obj) = row.as_object() else {
        return row.clone();
    };
    let mut out = Map::with_capacity(fields.len());
    for name in fields {
        if let Some(value) = obj.get(name) {
            out.insert(name.clone(), value.clone());
        }
    }
    Json::Object(out)
}

/// The primary key of one read-back row, as the string the row layer addresses
/// rows by and as the JSON a tool result reports.
///
/// Two forms because they are two different jobs: `sc_api`'s row functions take
/// the key as a path parameter would carry it, and the model should be told the
/// key it can use in a later `where` — which for an integer key is a number, not
/// `"3"`.
pub fn row_id(table: &Table, pk: &str, row: &Json) -> Result<(String, Json)> {
    let value = row.get(pk).cloned().unwrap_or(Json::Null);
    let id = match &value {
        Json::Null => {
            return Err(Error::msg(format!(
                "a row of `{}` came back without its `{pk}`",
                table.name
            )));
        }
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    Ok((id, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_argument_the_schema_does_not_describe_is_refused_by_name() {
        let err = arguments(&json!({"sql": "drop table books"}), &[ARG_WHERE]).unwrap_err();
        assert!(err.to_string().contains("`sql`"), "{err}");
        // …while an absent bag is the ordinary "no arguments" call.
        assert!(arguments(&Json::Null, &[ARG_WHERE]).unwrap().is_empty());
        assert!(arguments(&json!({}), &[ARG_WHERE]).unwrap().is_empty());
        assert!(arguments(&json!([]), &[ARG_WHERE]).is_err());
    }

    #[test]
    fn the_allow_list_is_read_in_the_tables_order_not_the_admins() {
        let config: Attrs = json!({"fields": ["pages", "title"]})
            .as_object()
            .cloned()
            .unwrap();
        assert_eq!(
            configured_fields(&config).unwrap(),
            vec!["pages".to_owned(), "title".to_owned()]
        );
        let empty = Attrs::new();
        assert!(configured_fields(&empty).unwrap().is_empty());
        let wrong: Attrs = json!({"fields": "title"}).as_object().cloned().unwrap();
        assert!(configured_fields(&wrong).is_err());
    }

    #[test]
    fn the_ceiling_defaults_and_refuses_nonsense() {
        assert_eq!(max_rows(&Attrs::new(), 50).unwrap(), 50);
        let cfg: Attrs = json!({"max_rows": 7}).as_object().cloned().unwrap();
        assert_eq!(max_rows(&cfg, 50).unwrap(), 7);
        let cfg: Attrs = json!({"max_rows": -1}).as_object().cloned().unwrap();
        assert!(max_rows(&cfg, 50).is_err());
    }

    #[test]
    fn a_row_is_narrowed_to_the_named_fields() {
        let row = json!({"id": 1, "title": "A", "secret": "x"});
        let fields = vec!["id".to_owned(), "title".to_owned()];
        assert_eq!(project(&row, &fields), json!({"id": 1, "title": "A"}));
    }
}

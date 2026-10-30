//! `edit_table_rows`' update grant — change the rows of the configured table
//! that a filter selects (§11.3).
//!
//! Two bounds are the whole design, and both are the admin's:
//!
//! - **A `where` is required.** An update whose filter was left out
//!   is "set this on every row of the table", and a table rewritten in one call
//!   is not something an omitted argument should be able to cause — §10.1
//!   refuses exactly this on the `update_rows` *action*, at save time, and the
//!   tool refuses it at call time for the same reason.
//! - **`max_rows` is a refusal, not a truncation.** A filter that matches more
//!   rows than the configuration allows changes **nothing** and comes back
//!   saying how many it matched. A read that returns the first 50 of 200 is a
//!   short answer; a write that changes the first 50 of 200 is a half-applied
//!   change nobody can find the other half of.
//!
//! Each selected row is then written **one at a time, by primary key**, through
//! [`sc_api::update_row_as`] — so every changed row is checked against §7.3
//! twice (granted on the row as it is, and on the row as it would become) and
//! raises its own update event carrying its own row, exactly as the action of
//! the same name does.

use sc_agent::TraitContext;
use sc_catalog::Table;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::{Map, Value as Json, json};

use super::DEFAULT_MAX_WRITE_ROWS;
use crate::table::{
    ARG_WHERE, WHERE_HELP, all_fields, arguments, field_list, field_schema, max_rows,
    required_where, visible_fields, where_schema,
};
use crate::write::matching_ids;

/// The values the model may set.
const ARG_SET: &str = "set";

/// Every argument this tool takes.
const ARGUMENTS: [&str; 2] = [ARG_WHERE, ARG_SET];

/// The tool the update grant offers, derived from its table.
pub fn tool_name(table: &str) -> String {
    format!("update_{table}")
}

/// The fields of `table` a model may set here.
pub(super) fn settable_fields(table: &Table, config: &Attrs) -> Result<Vec<String>> {
    let allowed = visible_fields(table, config)?;
    Ok(table
        .fields
        .iter()
        .filter(|f| !f.is_calc() && !f.primary_key && allowed.contains(&f.base.name))
        .map(|f| f.base.name.clone())
        .collect())
}

/// The tool, described by the table it changes and the bound it is under.
pub fn spec(table: &Table, config: &Attrs) -> ToolSpec {
    let settable = settable_fields(table, config).unwrap_or_default();
    let ceiling =
        max_rows(config, DEFAULT_MAX_WRITE_ROWS as u64).unwrap_or(DEFAULT_MAX_WRITE_ROWS as u64);
    ToolSpec::new(
        tool_name(&table.name),
        describe(table, &settable, ceiling),
        parameters(table, &settable),
    )
}

pub async fn call(
    table: &Table,
    config: &Attrs,
    args: &Json,
    ctx: &mut TraitContext<'_>,
) -> Result<Json> {
    let settable = settable_fields(table, config)?;
    let ceiling = max_rows(config, DEFAULT_MAX_WRITE_ROWS as u64)?;
    let args = arguments(args, &ARGUMENTS)?;

    // The filter may name any field of the table, not only the settable
    // ones: "the row whose title is Dune" is how a model addresses a row it
    // may not rename, and the rows it can reach are bounded by the caller's
    // own access rather than by this list.
    let filter = required_where(table, &all_fields(table), args.get(ARG_WHERE))?;
    let body = assignments(table, &settable, args.get(ARG_SET))?;
    let matched = matching_ids(ctx, table, Some(filter), ceiling, "update").await?;

    let mut ids = Vec::with_capacity(matched.len());
    for (id, id_json) in &matched {
        sc_api::update_row_as(
            ctx.catalog,
            table,
            id,
            &body,
            ctx.caller.role,
            ctx.caller.user.as_ref(),
            ctx.evaluator,
            // A tool call is not a trigger firing: the write this raises is at
            // depth 0, marked if the run's prompt came from outside the console.
            &ctx.caller.tool_chain(),
            // An agent run is its own unit of durability — its own run row,
            // persisted after every step — so its tools' writes commit as they
            // are made, even when the run was started by a workflow step
            // (§10.3, decision 6).
            &sc_api::rows::Executor::Pooled,
        )
        .await?;
        ids.push(id_json.clone());
    }
    Ok(json!({ "table": table.name, "updated": ids.len(), "ids": ids }))
}

/// The `set` object as a row body.
fn assignments(table: &Table, settable: &[String], set: Option<&Json>) -> Result<Json> {
    let set = set.filter(|v| !v.is_null()).ok_or_else(|| {
        Error::invalid(format!(
            "`{ARG_SET}` is required: name at least one field to change. \
             The fields you may change are {}",
            settable.join(", ")
        ))
    })?;
    let obj = set.as_object().ok_or_else(|| {
        Error::invalid(format!(
            "`{ARG_SET}` should be an object of field values, got {set}"
        ))
    })?;
    for key in obj.keys() {
        if !settable.contains(key) {
            return Err(Error::invalid(format!(
                "`{ARG_SET}`: `{}` has no field `{key}` you may change; \
                 the fields are {}",
                table.name,
                settable.join(", ")
            )));
        }
    }
    if obj.is_empty() {
        return Err(Error::invalid(format!(
            "`{ARG_SET}` needs at least one field; the fields you may change are {}",
            settable.join(", ")
        )));
    }
    Ok(Json::Object(obj.clone()))
}

/// What the model is told this tool does, what it may change, and how far it
/// reaches.
fn describe(table: &Table, settable: &[String], ceiling: u64) -> String {
    let mut out = format!("Change existing rows of the `{}` table", table.name);
    if !table.description.trim().is_empty() {
        out.push_str(&format!(" ({})", table.description.trim()));
    }
    out.push_str(&format!(
        ". Every row matching `where` gets every value in `set`. \
         `where` is required, and a call matching more than {ceiling} rows is \
         refused without changing anything, so narrow the filter rather than \
         widening it.\n\nFields you may change: {}.",
        field_list(table, settable)
    ));
    out.push_str("\n\n");
    out.push_str(WHERE_HELP);
    out.push_str(" It may name any field of the table, including ones you cannot change.");
    out
}

/// The tool's JSON Schema: a required `where` and a required `set`.
fn parameters(table: &Table, settable: &[String]) -> Json {
    let mut properties = Map::new();
    for field in &table.fields {
        if settable.contains(&field.base.name) {
            properties.insert(field.base.name.clone(), field_schema(field));
        }
    }
    json!({
        "type": "object",
        "properties": {
            ARG_WHERE: where_schema(table, &all_fields(table)),
            ARG_SET: {
                "type": "object",
                "description": "The new values: one key per field you are changing.",
                "properties": Json::Object(properties),
                "additionalProperties": false,
            },
        },
        "required": [ARG_WHERE, ARG_SET],
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_table() {
        assert_eq!(tool_name("books"), "update_books");
    }
}

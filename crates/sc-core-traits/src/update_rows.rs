//! `update_rows` — change the rows of a configured table that a filter selects
//! (§11.3).
//!
//! Two bounds are the whole design, and both are the admin's:
//!
//! - **A `where` is required.** An `update_rows` call whose filter was left out
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

use sc_agent::{AgentTrait, ToolsContext, TraitCheck, TraitContext};
use sc_catalog::Table;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Map, Value as Json, json};

use crate::table::{
    ARG_WHERE, CFG_FIELDS, CFG_MAX_ROWS, CFG_TABLE, WHERE_HELP, all_fields, arguments,
    check_table_config, config_str, configured_fields, configured_table, field_list, field_schema,
    max_rows, required_where, visible_fields, where_schema,
};
use crate::write::matching_ids;

/// How many rows one call may change when the admin sets no ceiling.
///
/// Smaller than a read's, and for a different reason: the cost of a read that
/// was too broad is context, and the cost of a write that was too broad is the
/// data. An admin who means "retag the whole catalogue" says so in a number.
pub const DEFAULT_MAX_WRITE_ROWS: i64 = 20;

/// The values the model may set.
const ARG_SET: &str = "set";

/// Every argument this tool takes.
const ARGUMENTS: [&str; 2] = [ARG_WHERE, ARG_SET];

/// Update the rows of one configured table that a filter selects.
pub struct UpdateRows;

/// The tool one `update_rows` instance offers, derived from its table.
pub fn tool_name(table: &str) -> String {
    format!("update_{table}")
}

/// The fields of `table` a model may set here.
fn settable_fields(table: &Table, config: &Attrs) -> Result<Vec<String>> {
    let allowed = visible_fields(table, config)?;
    Ok(table
        .fields
        .iter()
        .filter(|f| !f.is_calc() && !f.primary_key && allowed.contains(&f.base.name))
        .map(|f| f.base.name.clone())
        .collect())
}

#[async_trait::async_trait]
impl AgentTrait for UpdateRows {
    fn name(&self) -> &str {
        "update_rows"
    }

    fn description(&self) -> &str {
        "Change the rows of one table that a filter selects"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_FIELDS, BasicType::Json)
                .label("Fields the agent may change")
                .default_value(Json::Array(Vec::new())),
            FormField::new(CFG_MAX_ROWS, BasicType::Int)
                .label("Maximum rows per call")
                .default_value(DEFAULT_MAX_WRITE_ROWS),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let table = check_table_config(check.catalog, check.config, DEFAULT_MAX_WRITE_ROWS as u64)?;
        for name in configured_fields(check.config)? {
            if table.field(&name).is_some_and(|f| f.is_calc()) {
                return Err(Error::invalid(format!(
                    "`{name}` is a calculated field of `{}` and cannot be written",
                    table.name
                )));
            }
        }
        // An allow-list of nothing but the primary key leaves a tool that can
        // change no field at all, which is a configuration mistake rather than a
        // very safe grant.
        if settable_fields(&table, check.config)?.is_empty() {
            return Err(Error::invalid(format!(
                "no field of `{}` would be changeable; the primary key and \
                 calculated fields cannot be updated",
                table.name
            )));
        }
        Ok(())
    }

    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let catalog = cx.catalog;
        let configured = config_str(config, CFG_TABLE);
        let name = tool_name(&configured);
        let Ok(table) = configured_table(catalog, config) else {
            return vec![ToolSpec::new(
                name,
                format!("Update rows of the `{configured}` table (which no longer exists)"),
                json!({ "type": "object", "properties": {} }),
            )];
        };
        let settable = settable_fields(&table, config).unwrap_or_default();
        let ceiling = max_rows(config, DEFAULT_MAX_WRITE_ROWS as u64)
            .unwrap_or(DEFAULT_MAX_WRITE_ROWS as u64);
        vec![ToolSpec::new(
            name,
            describe(&table, &settable, ceiling),
            parameters(&table, &settable),
        )]
    }

    async fn call(
        &self,
        config: &Attrs,
        _tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let table = configured_table(ctx.catalog, config)?;
        let settable = settable_fields(&table, config)?;
        let ceiling = max_rows(config, DEFAULT_MAX_WRITE_ROWS as u64)?;
        let args = arguments(args, &ARGUMENTS)?;

        // The filter may name any field of the table, not only the settable
        // ones: "the row whose title is Dune" is how a model addresses a row it
        // may not rename, and the rows it can reach are bounded by the caller's
        // own access rather than by this list.
        let filter = required_where(&table, &all_fields(&table), args.get(ARG_WHERE))?;
        let body = assignments(&table, &settable, args.get(ARG_SET))?;
        let matched = matching_ids(ctx, &table, Some(filter), ceiling, "update").await?;

        let mut ids = Vec::with_capacity(matched.len());
        for (id, id_json) in &matched {
            sc_api::update_row_as(
                ctx.catalog,
                &table,
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

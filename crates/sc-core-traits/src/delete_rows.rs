//! `delete_rows` — delete the rows of a configured table that a filter selects
//! (§11.3).
//!
//! The sharpest of the write traits, and the one whose bounds matter most. It
//! shares [`update_rows`](crate::UpdateRows)' two: the `where` is **required**
//! (§10.1's reason — an emptied table is not something an omitted argument
//! should be able to cause) and `max_rows` **refuses** rather than truncates,
//! because a delete that took the first 20 of 200 rows is the worst of both
//! outcomes.
//!
//! Rows go one at a time by primary key through [`sc_api::delete_row_as`], so
//! each is checked against §7.3 and each raises its own delete event carrying
//! the row as it was — the only copy of it anyone will get.

use sc_agent::{AgentTrait, ToolsContext, TraitCheck, TraitContext};
use sc_catalog::Table;
use sc_error::Result;
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::table::{
    ARG_WHERE, CFG_MAX_ROWS, CFG_TABLE, WHERE_HELP, all_fields, arguments, check_table_config,
    config_str, configured_table, field_list, max_rows, required_where, where_schema,
};
use crate::update_rows::DEFAULT_MAX_WRITE_ROWS;
use crate::write::matching_ids;

/// Every argument this tool takes. One, and it is not optional.
const ARGUMENTS: [&str; 1] = [ARG_WHERE];

/// Delete the rows of one configured table that a filter selects.
pub struct DeleteRows;

/// The tool one `delete_rows` instance offers, derived from its table.
pub fn tool_name(table: &str) -> String {
    format!("delete_from_{table}")
}

#[async_trait::async_trait]
impl AgentTrait for DeleteRows {
    fn name(&self) -> &str {
        "delete_rows"
    }

    fn description(&self) -> &str {
        "Delete the rows of one table that a filter selects"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            // No field allow-list: a delete takes the whole row, so there is
            // nothing for one to narrow. Offering the field anyway would suggest
            // a grant that does not exist.
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_MAX_ROWS, BasicType::Int)
                .label("Maximum rows per call")
                .default_value(DEFAULT_MAX_WRITE_ROWS),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        check_table_config(check.catalog, check.config, DEFAULT_MAX_WRITE_ROWS as u64)?;
        Ok(())
    }

    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let catalog = cx.catalog;
        let configured = config_str(config, CFG_TABLE);
        let name = tool_name(&configured);
        let Ok(table) = configured_table(catalog, config) else {
            return vec![ToolSpec::new(
                name,
                format!("Delete rows of the `{configured}` table (which no longer exists)"),
                json!({ "type": "object", "properties": {} }),
            )];
        };
        let ceiling = max_rows(config, DEFAULT_MAX_WRITE_ROWS as u64)
            .unwrap_or(DEFAULT_MAX_WRITE_ROWS as u64);
        vec![ToolSpec::new(
            name,
            describe(&table, ceiling),
            parameters(&table),
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
        let ceiling = max_rows(config, DEFAULT_MAX_WRITE_ROWS as u64)?;
        let args = arguments(args, &ARGUMENTS)?;

        let filter = required_where(&table, &all_fields(&table), args.get(ARG_WHERE))?;
        let matched = matching_ids(ctx, &table, Some(filter), ceiling, "delete").await?;

        let mut ids = Vec::with_capacity(matched.len());
        for (id, id_json) in &matched {
            sc_api::delete_row_as(
                ctx.catalog,
                &table,
                id,
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
        Ok(json!({ "table": table.name, "deleted": ids.len(), "ids": ids }))
    }
}

/// What the model is told this tool does, and how far it reaches.
fn describe(table: &Table, ceiling: u64) -> String {
    let mut out = format!("Delete rows from the `{}` table", table.name);
    if !table.description.trim().is_empty() {
        out.push_str(&format!(" ({})", table.description.trim()));
    }
    out.push_str(&format!(
        ". Every row matching `where` is deleted, and this cannot be undone. \
         `where` is required — there is no way to ask for every row — and a call \
         matching more than {ceiling} rows is refused without deleting anything.\
         \n\nFields: {}.",
        field_list(table, &all_fields(table))
    ));
    out.push_str("\n\n");
    out.push_str(WHERE_HELP);
    out
}

/// The tool's JSON Schema: a required `where`, and nothing else.
fn parameters(table: &Table) -> Json {
    json!({
        "type": "object",
        "properties": { ARG_WHERE: where_schema(table, &all_fields(table)) },
        "required": [ARG_WHERE],
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_table() {
        assert_eq!(tool_name("books"), "delete_from_books");
    }
}

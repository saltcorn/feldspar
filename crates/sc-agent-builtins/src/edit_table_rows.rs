//! `edit_table_rows` — insert, update and delete the rows of one configured
//! table, each under a checkbox of its own (§11.3).
//!
//! A read-only agent is still the default shape: a blank form grants nothing,
//! and is refused on save for that reason. Each way of changing data is a
//! separate grant an admin ticks — [`CFG_ALLOW_INSERT`], [`CFG_ALLOW_UPDATE`],
//! [`CFG_ALLOW_DELETE`] — so an agent's definition still says, without reading
//! any code, that it may insert into `orders` and may not delete from it. They
//! are checkboxes on one trait rather than three traits because they share the
//! table, the field allow-list and the row bound; three forms asking for the same
//! table three times was three chances to configure them differently.
//!
//! Each grant adds one tool, named from the table as before —
//! `insert_into_books`, `update_books`, `delete_from_books` — and a tool whose
//! grant is off is not declared at all. Its call is refused by name all the
//! same, because a tool name is a string a model can produce whether or not it
//! was offered.
//!
//! The three halves live in [`insert`], [`update`] and [`delete`]; what they
//! share about selecting rows is in [`crate::write`], and what every table trait
//! shares is in [`crate::table`].

mod delete;
mod insert;
mod update;

use sc_agent::{AgentTrait, ToolsContext, TraitCheck, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::coding::may;
use crate::table::{
    CFG_FIELDS, CFG_MAX_ROWS, CFG_TABLE, check_table_config, config_str, configured_fields,
    configured_table,
};

pub use delete::tool_name as delete_tool_name;
pub use insert::tool_name as insert_tool_name;
pub use update::tool_name as update_tool_name;

/// May the agent add rows.
pub const CFG_ALLOW_INSERT: &str = "allow_insert";
/// May the agent change the rows a filter selects.
pub const CFG_ALLOW_UPDATE: &str = "allow_update";
/// May the agent delete the rows a filter selects.
pub const CFG_ALLOW_DELETE: &str = "allow_delete";

/// How many rows one update or delete may change when the admin sets no
/// ceiling.
///
/// Smaller than a read's, and for a different reason: the cost of a read that
/// was too broad is context, and the cost of a write that was too broad is the
/// data. An admin who means "retag the whole catalogue" says so in a number.
pub const DEFAULT_MAX_WRITE_ROWS: i64 = 20;

/// The grants, each with the verb its refusal uses.
const GRANTS: [(&str, &str); 3] = [
    (CFG_ALLOW_INSERT, "insert into"),
    (CFG_ALLOW_UPDATE, "update"),
    (CFG_ALLOW_DELETE, "delete from"),
];

/// Insert, update and delete the rows of one configured table, under its
/// grants.
pub struct EditTableRows;

/// Every tool this trait can offer for a table, in the order it offers them —
/// regardless of the grants, because this is what the admin UI wants to show.
pub fn tool_names(table: &str) -> Vec<String> {
    vec![
        insert::tool_name(table),
        update::tool_name(table),
        delete::tool_name(table),
    ]
}

/// The tool one grant adds.
fn tool_name(grant: &str, table: &str) -> String {
    match grant {
        CFG_ALLOW_INSERT => insert::tool_name(table),
        CFG_ALLOW_UPDATE => update::tool_name(table),
        _ => delete::tool_name(table),
    }
}

#[async_trait::async_trait]
impl AgentTrait for EditTableRows {
    fn name(&self) -> &str {
        "edit_table_rows"
    }

    fn description(&self) -> &str {
        "Insert, update and delete the rows of one table — each only if permitted"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_ALLOW_INSERT, BasicType::Bool)
                .label("May insert rows")
                .default_value(false),
            FormField::new(CFG_ALLOW_UPDATE, BasicType::Bool)
                .label("May update rows")
                .default_value(false),
            FormField::new(CFG_ALLOW_DELETE, BasicType::Bool)
                .label("May delete rows")
                .default_value(false),
            // Bounds what an insert or an update may *set*; a delete takes the
            // whole row, so there is nothing for it to narrow there.
            FormField::new(CFG_FIELDS, BasicType::Json)
                .label("Fields the agent may set (insert and update)")
                .default_value(Json::Array(Vec::new())),
            // Bounds an update and a delete; an insert writes one row.
            FormField::new(CFG_MAX_ROWS, BasicType::Int)
                .label("Maximum rows per update or delete")
                .default_value(DEFAULT_MAX_WRITE_ROWS),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let table = check_table_config(check.catalog, check.config, DEFAULT_MAX_WRITE_ROWS as u64)?;
        for (key, _) in GRANTS {
            match check.config.get(key) {
                None | Some(Json::Null) | Some(Json::Bool(_)) => {}
                Some(other) => {
                    return Err(Error::invalid(format!(
                        "`{key}` should be true or false, got {other}"
                    )));
                }
            }
        }
        // A blank form is not a very safe grant but no grant at all: an agent
        // that should only read the table wants `query_table`.
        if !GRANTS.iter().any(|(key, _)| may(check.config, key)) {
            return Err(Error::invalid(format!(
                "`edit_table_rows` on `{}` allows nothing: tick at least one of \
                 {CFG_ALLOW_INSERT}, {CFG_ALLOW_UPDATE} and {CFG_ALLOW_DELETE}",
                table.name
            )));
        }
        // A calculated field has no column, so an allow-list naming one promises
        // something the write path would refuse — said here, in front of the
        // admin, rather than in a tool result nobody is reading.
        for name in configured_fields(check.config)? {
            if table.field(&name).is_some_and(|f| f.is_calc()) {
                return Err(Error::invalid(format!(
                    "`{name}` is a calculated field of `{}` and cannot be written",
                    table.name
                )));
            }
        }
        // An allow-list of nothing but the primary key leaves an update that can
        // change no field at all, which is a configuration mistake rather than a
        // very safe grant.
        if may(check.config, CFG_ALLOW_UPDATE)
            && update::settable_fields(&table, check.config)?.is_empty()
        {
            return Err(Error::invalid(format!(
                "no field of `{}` would be changeable; the primary key and \
                 calculated fields cannot be updated",
                table.name
            )));
        }
        Ok(())
    }

    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let configured = config_str(config, CFG_TABLE);
        let table = configured_table(cx.catalog, config);
        let mut tools = Vec::new();
        for (key, verb) in GRANTS {
            if !may(config, key) {
                continue;
            }
            tools.push(match (&table, key) {
                (Ok(table), CFG_ALLOW_INSERT) => insert::spec(table, config),
                (Ok(table), CFG_ALLOW_UPDATE) => update::spec(table, config),
                (Ok(table), _) => delete::spec(table, config),
                // A table dropped since the agent was saved keeps its tools'
                // names, described as unavailable: "this agent names a table
                // that is gone" is a repairable message, "this agent has no
                // tools" is not.
                (Err(_), _) => ToolSpec::new(
                    tool_name(key, &configured),
                    format!("Would {verb} the `{configured}` table (which no longer exists)"),
                    json!({ "type": "object", "properties": {} }),
                ),
            });
        }
        tools
    }

    async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let configured = config_str(config, CFG_TABLE);
        let Some((key, verb)) = GRANTS
            .into_iter()
            .find(|(key, _)| tool == tool_name(key, &configured))
        else {
            return Err(Error::invalid(format!(
                "`edit_table_rows` on `{configured}` has no tool `{tool}`"
            )));
        };
        if !may(config, key) {
            return Err(Error::invalid(format!(
                "agent `{}` may not {verb} `{configured}`: its `edit_table_rows` trait has \
                 `{key}` switched off. Tell the user an administrator must turn it on.",
                ctx.agent
            )));
        }
        let table = configured_table(ctx.catalog, config)?;
        match key {
            CFG_ALLOW_INSERT => insert::call(&table, config, args, ctx).await,
            CFG_ALLOW_UPDATE => update::call(&table, config, args, ctx).await,
            _ => delete::call(&table, config, args, ctx).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_names_are_derived_from_the_table() {
        assert_eq!(
            tool_names("books"),
            ["insert_into_books", "update_books", "delete_from_books"]
        );
    }
}

//! `insert_row` — add one row to a configured table (§11.3).
//!
//! The first of the three write traits, and the reason there are three: a
//! read-only agent is the default shape, and each grant beyond it is a separate
//! entry in the trait list with a form of its own. An admin reading an agent's
//! definition can see that it may insert into `orders` and may not delete from
//! it, without reading any code.
//!
//! The write goes through [`sc_api::insert_row_as`], which is §7.3's write rule
//! (`meets min_role_write` **or** the ownership formula grants the proposed row)
//! followed by `sc_api::rows::create_row_ctx` — so the values are coerced and
//! validated against the field types, a `File` field is checked against its
//! store, and the table's own insert trigger fires with the row the model wrote.
//! An agent is a caller of the row layer, not a second one.

use sc_agent::{AgentTrait, ToolsContext, TraitCheck, TraitContext};
use sc_catalog::Table;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Map, Value as Json, json};

use crate::table::{
    CFG_FIELDS, CFG_TABLE, check_table_config, config_str, configured_fields, configured_table,
    field_list, field_schema, visible_fields,
};

/// Insert one row into one configured table.
pub struct InsertRow;

/// The tool one `insert_row` instance offers, derived from its table.
pub fn tool_name(table: &str) -> String {
    format!("insert_into_{table}")
}

/// The fields of `table` a model may write: everything except the calculated
/// ones, which have no column, narrowed by the allow-list when there is one.
fn writable_fields(table: &Table, config: &Attrs) -> Result<Vec<String>> {
    let allowed = visible_fields(table, config)?;
    Ok(table
        .fields
        .iter()
        .filter(|f| !f.is_calc() && allowed.contains(&f.base.name))
        .map(|f| f.base.name.clone())
        .collect())
}

#[async_trait::async_trait]
impl AgentTrait for InsertRow {
    fn name(&self) -> &str {
        "insert_row"
    }

    fn description(&self) -> &str {
        "Insert one row into one table"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_FIELDS, BasicType::Json)
                .label("Fields the agent may set")
                .default_value(Json::Array(Vec::new())),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let table = check_table_config(check.catalog, check.config, 1)?;
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
        Ok(())
    }

    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let catalog = cx.catalog;
        let configured = config_str(config, CFG_TABLE);
        let name = tool_name(&configured);
        let Ok(table) = configured_table(catalog, config) else {
            return vec![ToolSpec::new(
                name,
                format!("Insert a row into the `{configured}` table (which no longer exists)"),
                json!({ "type": "object", "properties": {} }),
            )];
        };
        let writable = writable_fields(&table, config).unwrap_or_default();
        vec![ToolSpec::new(
            name,
            describe(&table, &writable),
            parameters(&table, &writable),
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
        let writable = writable_fields(&table, config)?;
        let body = row_body(&table, &writable, args)?;
        let row = sc_api::insert_row_as(
            ctx.catalog,
            &table,
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
        // The whole stored row, not the fields that were sent: the model needs
        // the primary key the database chose to be able to refer to what it just
        // created, and the defaults it did not set are the other half of the
        // answer to "what did that do?".
        Ok(json!({ "table": table.name, "inserted": row }))
    }
}

/// The arguments **are** the row: one flat object of field → value.
///
/// Flat rather than nested under a `values` key because it is the shape a model
/// produces most reliably, and because `additionalProperties: false` over the
/// table's own field names is then exactly the check that a hallucinated column
/// fails at the vendor rather than three steps later.
fn row_body(table: &Table, writable: &[String], args: &Json) -> Result<Json> {
    let obj = match args {
        Json::Null => Map::new(),
        Json::Object(map) => map.clone(),
        other => {
            return Err(Error::invalid(format!(
                "the arguments should be an object of field values, got {other}"
            )));
        }
    };
    for key in obj.keys() {
        if !writable.contains(key) {
            return Err(Error::invalid(format!(
                "`{}` has no field `{key}` you may set; the fields are {}",
                table.name,
                writable.join(", ")
            )));
        }
    }
    if obj.is_empty() {
        return Err(Error::invalid(format!(
            "an insert into `{}` needs at least one field; the fields are {}",
            table.name,
            writable.join(", ")
        )));
    }
    Ok(Json::Object(obj))
}

/// What the model is told this tool does, and what it may set.
fn describe(table: &Table, writable: &[String]) -> String {
    let mut out = format!("Insert one row into the `{}` table", table.name);
    if !table.description.trim().is_empty() {
        out.push_str(&format!(" ({})", table.description.trim()));
    }
    out.push_str(&format!(
        ". The arguments are the row itself: one key per field you are setting. \
         Returns the row as stored, including the fields the database filled in.\
         \n\nFields you may set: {}.",
        field_list(table, writable)
    ));
    if writable.len() < table.fields.len() {
        out.push_str(" Any other field of this table takes its default and cannot be set here.");
    }
    out
}

/// The tool's JSON Schema: one property per writable field.
///
/// **No `required` list**, deliberately. A `NOT NULL` column may well have a
/// database default, and this crate cannot see defaults — so a schema that
/// demanded every required column would make the model invent values for the
/// ones the database was going to fill in. A missing value the database really
/// does need comes back as its own error, which the model can read and retry;
/// an invented one is a wrong row nobody notices. The prose says which fields
/// are required, which is the reader that can weigh it.
fn parameters(table: &Table, writable: &[String]) -> Json {
    let mut properties = Map::new();
    for field in &table.fields {
        if writable.contains(&field.base.name) {
            properties.insert(field.base.name.clone(), field_schema(field));
        }
    }
    json!({
        "type": "object",
        "properties": Json::Object(properties),
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_name_is_derived_from_its_table() {
        assert_eq!(tool_name("books"), "insert_into_books");
    }
}

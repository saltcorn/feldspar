//! `describe_code_api` — what a JavaScript code body can call.
//!
//! A code body is written by the agent that saves it: a `run_js_code` trigger
//! or workflow step, or a custom API query in `javascript`. Nothing else in the
//! tool surface says what the body's `db` looks like, and a model that is not
//! told writes the API it has seen most — `db.invoices.update(id, values)`,
//! `findOne`, `getRows` — none of which exist here. The failure is silent until
//! the trigger fires, which is the wrong time for an agent to learn it.
//!
//! So the reference is **one page, [`JS_CODE_API`]**, reached two ways:
//! `describe_action` includes it for any action with a JavaScript code setting
//! (the step before `save_trigger`), and this tool answers it for the two paths
//! that never pass through `describe_action` — a custom query, which lives in
//! the applications area, and a workflow step. It is always offered, like the
//! schema tools: a reference grants nothing, and an agent whose areas let it
//! write code in either place must be able to read what the code can call.
//!
//! The page is a transcription of `sc-expr`'s preludes and of what
//! [`crate::code_host`] answers, on the terms `ui/admin/src/codeTypes.ts` is:
//! close to correct rather than provably so. The test below pins the parts an
//! agent most often gets wrong.

use sc_catalog::Catalog;
use sc_error::Result;
use serde_json::{Value as Json, json};

use crate::schema_edit::Grants;

use super::{AdminTool, ToolContext};

/// Reads the reference for a JavaScript code body.
pub const TOOL_DESCRIBE_CODE_API: &str = "describe_code_api";

/// The reference itself, as the model reads it.
pub const JS_CODE_API: &str = include_str!("code_api_js.md");

/// Answer the reference.
pub(super) struct DescribeCodeApi;

#[async_trait::async_trait]
impl AdminTool for DescribeCodeApi {
    fn name(&self) -> &'static str {
        TOOL_DESCRIBE_CODE_API
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        "Read the API a JavaScript code body can call — `db` for reading and \
         writing tables, `fetch`, `fs`, `trigger` — and what is in scope \
         (`row`, `user`, `payload`, …). Call it **before writing any** \
         `run_js_code` body, workflow code step or `javascript` API query: the \
         API is this server's own, and a method guessed from another library \
         (`db.t.update(id, values)`, `findOne`) does not exist and fails only \
         when the code runs."
            .to_owned()
    }

    fn parameters(&self) -> Json {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn call(&self, _ctx: &ToolContext<'_>, _grants: &Grants, args: &Json) -> Result<Json> {
        super::arguments(args, &[])?;
        Ok(json!({ "language": "javascript", "reference": JS_CODE_API }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_teaches_where_then_write_and_names_the_guesses() {
        // The shape an agent is to copy…
        assert!(
            JS_CODE_API.contains("db.invoices.where({ id: row.id }).update({"),
            "{JS_CODE_API}"
        );
        assert!(JS_CODE_API.contains(".where({ id: 7 }).delete()"));
        // …and the ones it guesses, named as absent so a model does not reach
        // for them.
        for guess in [
            "db.t.update(id, values)",
            "db.t.delete(id)",
            "db.t.findOne()",
        ] {
            assert!(JS_CODE_API.contains(guess), "missing `{guess}`");
        }
    }

    #[test]
    fn the_reference_teaches_the_model_handle_and_not_the_flat_functions() {
        for taught in [
            "const m = await models.get(\"House prices\");",
            "await m.predict(row)",
            "await r.draws(\"alpha\"",
            "await r.writePosterior({",
            "r.asUser().writePosterior(",
        ] {
            assert!(JS_CODE_API.contains(taught), "missing `{taught}`");
        }
        for gone in ["models.draws(", "models.summary(", "models.instance("] {
            assert!(!JS_CODE_API.contains(gone), "still teaches `{gone}`");
        }
    }
}

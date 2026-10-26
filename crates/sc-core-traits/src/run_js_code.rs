//! `run_js_code` — one JavaScript body as one tool (§11.3).
//!
//! The code lives **in the agent's configuration**, not in a trigger: an agent
//! with a dozen small tools (`word_of_the_day`, `convert_units`, …) keeps them
//! where they are used, rather than putting a dozen entries in the instance's
//! trigger list that nothing else runs and anything else could. It is the
//! counterpart of a v1 agent's "Run JavaScript code" skill, which the Saltcorn 1
//! importer converts to this.
//!
//! - **The body runs as a `run_js_code` action body does**, through
//!   [`TriggerDispatcher::run_code`](sc_action::TriggerDispatcher::run_code): the
//!   same `db`, `fetch`, `lib()` and `trigger()` host surfaces, the same bounds,
//!   the same isolate pool. There is one way to run JavaScript on the server,
//!   and an agent is one more thing that asks.
//! - **As the run's caller.** Its `db` reads and writes go through the row layer
//!   at the role and user of whoever the agent is talking to, exactly as
//!   `query_table` does — exposing an agent does not lend its tools more reach
//!   than the person using it has.
//! - **Each declared argument is a variable** in the body's scope, as in v1:
//!   `return text.toUpperCase()` for an argument `text`. All of them together
//!   are `args`. An argument the model left out is `null`, not a
//!   `ReferenceError`.
//! - **The tool is named by the configuration** (`tool_name`), verbatim, so one
//!   agent can carry many of these; two that share a name are refused on save
//!   like any other tool collision (§11.2).

use std::collections::BTreeSet;

use sc_agent::{AgentTrait, ToolsContext, TraitCheck, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Map, Value as Json, json};

use crate::table::config_str;

/// The name the model calls the tool by.
pub const CFG_TOOL_NAME: &str = "tool_name";
/// What the tool does, as the model is told it.
pub const CFG_TOOL_DESCRIPTION: &str = "description";
/// The JavaScript body.
pub const CFG_CODE: &str = "code";
/// The arguments the tool takes: `[{ name, type, description }]`.
pub const CFG_ARGUMENTS: &str = "arguments";

/// The binding that holds every argument at once.
const ALL_ARGS: &str = "args";
/// Names a body already has, which an argument must not take.
const RESERVED: &[&str] = &[
    ALL_ARGS, "user", "app", "session", "db", "fetch", "lib", "trigger", "console", "context",
    "payload", "row", "old", "request", "body", "query",
];
/// The argument types a tool schema may declare.
const ARG_TYPES: &[&str] = &["string", "number", "integer", "boolean", "object", "array"];

/// Run one configured JavaScript body as a tool.
pub struct RunJsCode;

/// One declared argument.
struct Argument {
    name: String,
    type_: String,
    description: String,
}

#[async_trait::async_trait]
impl AgentTrait for RunJsCode {
    fn name(&self) -> &str {
        "run_js_code"
    }

    fn description(&self) -> &str {
        "A tool whose body is JavaScript, kept in the agent"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TOOL_NAME, BasicType::Text)
                .label("Tool name")
                .required()
                .sublabel("What the model calls it: letters, digits, _ and -"),
            FormField::new(CFG_TOOL_DESCRIPTION, BasicType::Text)
                .label("Description")
                .multiline()
                .sublabel("When the model should use it and what it returns"),
            FormField::new(CFG_CODE, BasicType::Text)
                .label("Code")
                .required()
                .code("javascript")
                .sublabel("Each argument is a variable; return the tool's result"),
            FormField::new(CFG_ARGUMENTS, BasicType::Json)
                .label("Arguments")
                .sublabel(
                    "A list of { \"name\", \"type\" (string, number, integer, boolean, \
                     object, array), \"description\" }",
                ),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        tool_name(check.config)?;
        if config_str(check.config, CFG_CODE).trim().is_empty() {
            return Err(Error::invalid(format!("`{CFG_CODE}` is required")));
        }
        arguments(check.config)?;
        Ok(())
    }

    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        // Infallible by contract: a configuration that no longer validates is
        // still described under the name it gives, so the reason it is invalid
        // can be reported against that name.
        let name = config_str(config, CFG_TOOL_NAME);
        let arguments = arguments(config).unwrap_or_default();
        let mut properties = Map::new();
        for argument in &arguments {
            let mut schema = json!({ "type": argument.type_ });
            if !argument.description.is_empty() {
                schema["description"] = json!(argument.description);
            }
            properties.insert(argument.name.clone(), schema);
        }
        let description = match config_str(config, CFG_TOOL_DESCRIPTION).trim() {
            "" => format!("Run the `{name}` tool."),
            text => text.to_owned(),
        };
        vec![ToolSpec::new(
            name,
            description,
            json!({
                "type": "object",
                "properties": properties,
                "required": arguments.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                "additionalProperties": false,
            }),
        )]
    }

    async fn call(
        &self,
        config: &Attrs,
        _tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let name = tool_name(config)?;
        let code = config_str(config, CFG_CODE);
        let dispatcher = ctx.require_triggers()?;
        let given = args.as_object().cloned().unwrap_or_default();
        let mut scope = Attrs::new();
        for argument in arguments(config)? {
            let value = given.get(&argument.name).cloned().unwrap_or(Json::Null);
            scope.insert(argument.name, value);
        }
        scope.insert(ALL_ARGS.to_owned(), Json::Object(given));
        let caller = sc_api::caller_context_at(ctx.caller.role, ctx.caller.user.as_ref())
            .chained(ctx.caller.tool_chain());
        dispatcher
            .run_code(
                ctx.catalog,
                "run_js_code",
                &format!("{}/{name}", ctx.agent),
                &code,
                scope,
                Some(&caller),
                None,
                None,
            )
            .await
    }
}

/// The configured tool name, checked as a name every model vendor accepts.
fn tool_name(config: &Attrs) -> Result<String> {
    let name = config_str(config, CFG_TOOL_NAME);
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::invalid(format!("`{CFG_TOOL_NAME}` is required")));
    }
    if name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::invalid(format!(
            "`{CFG_TOOL_NAME}` `{name}` must be at most 64 letters, digits, `_` or `-`"
        )));
    }
    Ok(name.to_owned())
}

/// The declared arguments, each a usable JavaScript name of a known type.
fn arguments(config: &Attrs) -> Result<Vec<Argument>> {
    let entries = match config.get(CFG_ARGUMENTS) {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(entries)) => entries,
        Some(_) => {
            return Err(Error::invalid(format!(
                "`{CFG_ARGUMENTS}` must be a list of {{ name, type, description }}"
            )));
        }
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(Json::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let name = text("name");
        let starts_well = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$');
        if !starts_well
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        {
            return Err(Error::invalid(format!(
                "argument `{name}` must be a JavaScript name: a letter, `_` or `$`, then \
                 letters, digits, `_` or `$`"
            )));
        }
        if RESERVED.contains(&name.as_str()) {
            return Err(Error::invalid(format!(
                "argument `{name}` would hide the body's own `{name}`; name it something else"
            )));
        }
        if !seen.insert(name.clone()) {
            return Err(Error::invalid(format!(
                "argument `{name}` is declared twice"
            )));
        }
        let type_ = match text("type") {
            t if t.is_empty() => "string".to_owned(),
            t if ARG_TYPES.contains(&t.as_str()) => t,
            t => {
                return Err(Error::invalid(format!(
                    "argument `{name}`: `{t}` is not one of {}",
                    ARG_TYPES.join(", ")
                )));
            }
        };
        out.push(Argument {
            name,
            type_,
            description: text("description"),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(value: Json) -> Attrs {
        value.as_object().unwrap().clone().into_iter().collect()
    }

    #[test]
    fn the_tool_is_named_and_shaped_by_its_configuration() {
        let config = config(json!({
            "tool_name": "echo", "description": "Say it back", "code": "return text",
            "arguments": [ { "name": "text", "type": "string", "description": "What to say" },
                           { "name": "times" , "type": "integer" } ],
        }));
        assert_eq!(tool_name(&config).unwrap(), "echo");
        let args = arguments(&config).unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[1].type_, "integer");
    }

    #[test]
    fn names_the_body_already_has_or_vendors_refuse_are_refused() {
        assert!(tool_name(&config(json!({ "tool_name": "two words" }))).is_err());
        assert!(tool_name(&config(json!({}))).is_err());
        let refused = |arguments: Json| arguments_err(config(json!({ "arguments": arguments })));
        assert!(refused(json!([{ "name": "db" }])).contains("hide"));
        assert!(refused(json!([{ "name": "1x" }])).contains("JavaScript name"));
        assert!(refused(json!([{ "name": "a" }, { "name": "a" }])).contains("twice"));
        assert!(refused(json!([{ "name": "a", "type": "date" }])).contains("not one of"));
        // No type is a string, as v1's tools default.
        let fine = arguments(&config(json!({ "arguments": [{ "name": "a" }] }))).unwrap();
        assert_eq!(fine[0].type_, "string");
    }

    fn arguments_err(config: Attrs) -> String {
        match arguments(&config) {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        }
    }
}

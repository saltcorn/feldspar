//! `library_tools` — functions of a code library as tools (§11.3).
//!
//! The counterpart of [`run_js_code`](crate::RunJsCode) for tools that already
//! live in code: an application whose search, read and "propose a change" are
//! JavaScript functions in a code library names the library once and lists the
//! functions, each with the JSON schema of its argument, rather than keeping
//! one `run_js_code` body (or one trigger) per tool.
//!
//! - **Each tool calls one exported function** with the model's arguments as
//!   one object: `exports.search = async ({ query }) => …`. Its return value is
//!   the tool's result.
//! - **As the run's caller**, through
//!   [`TriggerDispatcher::run_code`](sc_action::TriggerDispatcher::run_code), the
//!   way a `run_js_code` body runs: the library's `db` reads and writes at the
//!   role of whoever the agent is talking to, and its `user` is them. In an
//!   application's chat, `lib()` resolves the application's libraries over the
//!   instance's.
//! - **A function may propose instead of act.** A result of the shape
//!   `{ "$proposal": { prompt, apply, payload, label } }` is put to the person
//!   as an approval (an `approve` request, AS6.1) and the turn ends. When they
//!   approve, the library's `apply` function is called with `payload` — as the
//!   person who approved, before the model hears of it — and the model is told
//!   what it returned; when they decline, it is told nothing changed. The
//!   payload is stored on the run, so what is applied is what the function
//!   proposed, not anything the model or the client says afterwards.

use std::collections::BTreeSet;

use sc_agent::{AgentTrait, RequestKind, ToolsContext, TraitCheck, TraitContext, UserRequest};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::table::config_str;

/// The code library the functions are exported from.
pub const CFG_LIBRARY: &str = "library";
/// The tools: `[{ name, description, parameters, function }]`.
pub const CFG_LIBRARY_TOOLS: &str = "tools";
/// The key a function's result proposes a change under.
pub const PROPOSAL_KEY: &str = "$proposal";
/// The approving button's label when a proposal gives none.
const DEFAULT_APPLY_LABEL: &str = "Apply";

/// Functions of one code library, each a tool.
pub struct LibraryTools;

/// One configured tool.
#[derive(Debug, Clone, PartialEq)]
struct LibraryTool {
    name: String,
    description: String,
    parameters: Json,
    function: String,
}

#[async_trait::async_trait]
impl AgentTrait for LibraryTools {
    fn name(&self) -> &str {
        "library_tools"
    }

    fn description(&self) -> &str {
        "Tools that call functions of a code library, as the person chatting"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_LIBRARY, BasicType::Text)
                .label("Code library")
                .required()
                .sublabel("The library the functions are exported from, as lib(\"name\")"),
            FormField::new(CFG_LIBRARY_TOOLS, BasicType::Json)
                .label("Tools")
                .required()
                .sublabel(
                    "A list of { \"name\", \"description\", \"parameters\" (a JSON schema of \
                     the one argument object), \"function\" (the export; the name when \
                     left out) }",
                ),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let library = library(check.config)?;
        // In any scope: an agent an application exposes reaches the
        // application's libraries, and one at the console the instance's.
        let libraries = sc_catalog::list_code_libraries(check.catalog).await?;
        if !libraries.iter().any(|l| l.name == library) {
            return Err(Error::invalid(format!("no code library named `{library}`")));
        }
        if tools(check.config)?.is_empty() {
            return Err(Error::invalid(format!(
                "`{CFG_LIBRARY_TOOLS}` lists no tools"
            )));
        }
        Ok(())
    }

    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        // Infallible by contract; a configuration that no longer validates
        // offers what still parses.
        tools(config)
            .unwrap_or_default()
            .into_iter()
            .map(|t| {
                let description = if t.description.is_empty() {
                    format!("Call `{}`.", t.name)
                } else {
                    t.description
                };
                ToolSpec::new(t.name, description, t.parameters)
            })
            .collect()
    }

    async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let library = library(config)?;
        let configured = tools(config)?
            .into_iter()
            .find(|t| t.name == tool)
            .ok_or_else(|| Error::invalid(format!("no tool named `{tool}` here")))?;
        let result = call_function(ctx, &library, &configured.function, args, tool).await?;
        let Some(proposal) = result.get(PROPOSAL_KEY) else {
            return Ok(result);
        };
        let proposal = Proposal::read(proposal, &configured.function)?;
        ctx.await_user(UserRequest::new(
            proposal.prompt.clone(),
            RequestKind::Approve {
                label: proposal.label.clone(),
                payload: json!({ "apply": proposal.apply, "payload": proposal.payload }),
            },
        ))?;
        // What else the function returned is still the model's to read.
        let mut told = result.clone();
        if let Some(obj) = told.as_object_mut() {
            obj.remove(PROPOSAL_KEY);
        }
        Ok(json!({
            "proposal": proposal.prompt,
            "status": "Put to the user for approval. Nothing has changed yet; the outcome \
                       arrives as the next user message.",
            "result": told,
        }))
    }

    async fn on_answer_in(
        &self,
        config: &Attrs,
        request: &UserRequest,
        value: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<String> {
        let RequestKind::Approve { payload, .. } = &request.kind else {
            return Ok(request.answer_text(value));
        };
        if value != &Json::Bool(true) {
            return Ok(format!(
                "The user declined the proposal \"{}\". Nothing was changed.",
                request.prompt
            ));
        }
        let apply = payload
            .get("apply")
            .and_then(Json::as_str)
            .unwrap_or_default();
        let library = library(config)?;
        let argument = payload.get("payload").cloned().unwrap_or(Json::Null);
        Ok(
            match call_function(ctx, &library, apply, &argument, apply).await {
                Ok(result) => format!(
                    "The user approved the proposal \"{}\", and it was applied. \
                     `{apply}` returned: {result}",
                    request.prompt
                ),
                Err(e) => format!(
                    "The user approved the proposal \"{}\", but applying it failed: {e}",
                    request.prompt
                ),
            },
        )
    }
}

/// What a function proposed.
struct Proposal {
    prompt: String,
    label: String,
    apply: String,
    payload: Json,
}

impl Proposal {
    /// The `$proposal` object `function` returned, checked.
    fn read(value: &Json, function: &str) -> Result<Proposal> {
        let refused = |what: &str| {
            Error::invalid(format!(
                "`{function}` returned a `{PROPOSAL_KEY}` without {what}"
            ))
        };
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let prompt = text("prompt").ok_or_else(|| refused("a `prompt` to show the user"))?;
        let apply = text("apply").ok_or_else(|| refused("an `apply` function"))?;
        if !is_identifier(&apply) {
            return Err(refused("an `apply` that names an exported function"));
        }
        Ok(Proposal {
            prompt,
            label: text("label").unwrap_or_else(|| DEFAULT_APPLY_LABEL.to_owned()),
            apply,
            payload: value.get("payload").cloned().unwrap_or(Json::Null),
        })
    }
}

/// Call `function` of `library` with `argument`, as the run's caller.
async fn call_function(
    ctx: &TraitContext<'_>,
    library: &str,
    function: &str,
    argument: &Json,
    tool: &str,
) -> Result<Json> {
    let dispatcher = ctx.require_triggers()?;
    // Names travel as JSON strings, so nothing configured is spliced in as code.
    let lib_name = Json::String(library.to_owned());
    let fn_name = Json::String(function.to_owned());
    let code = format!(
        "const exported = lib({lib_name});\n\
         const f = exported == null ? undefined : exported[{fn_name}];\n\
         if (typeof f !== \"function\") {{\n\
           throw new Error(\"code library \" + {lib_name} + \" exports no function \" + {fn_name});\n\
         }}\n\
         return await f(args);"
    );
    let mut scope = Attrs::new();
    scope.insert("args".to_owned(), argument.clone());
    let caller = sc_api::caller_context_at(ctx.caller.role, ctx.caller.user.as_ref())
        .chained(ctx.caller.tool_chain());
    // In an application's chat, its own libraries over the instance's.
    let application = caller.app.as_ref().map(|app| app.id);
    dispatcher
        .run_code(
            ctx.catalog,
            "run_js_code",
            &format!("{}/{tool}", ctx.agent),
            &code,
            scope,
            Some(&caller),
            application,
            None,
        )
        .await
}

/// The configured library's name.
fn library(config: &Attrs) -> Result<String> {
    let name = config_str(config, CFG_LIBRARY);
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::invalid(format!("`{CFG_LIBRARY}` is required")));
    }
    if !sc_catalog::valid_code_library_name(name) {
        return Err(Error::invalid(format!(
            "`{CFG_LIBRARY}` `{name}` is not a code library name"
        )));
    }
    Ok(name.to_owned())
}

/// Whether `name` is a plain JavaScript identifier: what an export is called.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// The configured tools, each checked.
fn tools(config: &Attrs) -> Result<Vec<LibraryTool>> {
    let entries = match config.get(CFG_LIBRARY_TOOLS) {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(entries)) => entries.clone(),
        // A form may send the JSON as text.
        Some(Json::String(text)) if text.trim().is_empty() => return Ok(Vec::new()),
        Some(Json::String(text)) => match serde_json::from_str::<Json>(text) {
            Ok(Json::Array(entries)) => entries,
            _ => return Err(not_a_list()),
        },
        Some(_) => return Err(not_a_list()),
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
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(Error::invalid(format!(
                "every tool needs a `name` of at most 64 letters, digits, `_` or `-`, \
                 not `{name}`"
            )));
        }
        if !seen.insert(name.clone()) {
            return Err(Error::invalid(format!("two tools are named `{name}`")));
        }
        let function = match text("function") {
            f if f.is_empty() => name.clone(),
            f => f,
        };
        if !is_identifier(&function) {
            return Err(Error::invalid(format!(
                "tool `{name}`: `{function}` is not a function name; set `function` to the \
                 export it calls"
            )));
        }
        let parameters = match entry.get("parameters") {
            None | Some(Json::Null) => {
                json!({ "type": "object", "properties": {}, "additionalProperties": false })
            }
            Some(schema @ Json::Object(obj))
                if obj.get("type").and_then(Json::as_str) == Some("object") =>
            {
                schema.clone()
            }
            Some(_) => {
                return Err(Error::invalid(format!(
                    "tool `{name}`: `parameters` must be a JSON schema of `\"type\": \
                     \"object\"` — the one argument the function is called with"
                )));
            }
        };
        out.push(LibraryTool {
            name,
            description: text("description"),
            parameters,
            function,
        });
    }
    Ok(out)
}

fn not_a_list() -> Error {
    Error::invalid(format!(
        "`{CFG_LIBRARY_TOOLS}` must be a list of {{ name, description, parameters, function }}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(tools_json: Json) -> Attrs {
        [
            (CFG_LIBRARY.to_owned(), json!("paddi")),
            (CFG_LIBRARY_TOOLS.to_owned(), tools_json),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn tools_default_their_function_and_schema_and_refuse_what_cannot_be_called() {
        let parsed = tools(&config(json!([
            { "name": "search", "description": "Find documents",
              "parameters": { "type": "object", "properties": { "query": { "type": "string" } } } },
            { "name": "read-doc", "function": "readDoc" },
        ])))
        .unwrap();
        assert_eq!(parsed[0].function, "search");
        assert_eq!(parsed[1].function, "readDoc");
        assert_eq!(parsed[1].parameters["type"], json!("object"));

        for bad in [
            json!([{ "name": "read-doc" }]),
            json!([{ "name": "a" }, { "name": "a" }]),
            json!([{ "name": "a", "parameters": { "type": "string" } }]),
            json!([{ "name": "" }]),
            json!({ "name": "a" }),
        ] {
            assert!(tools(&config(bad.clone())).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_proposal_needs_a_prompt_and_a_function_to_apply() {
        let ok = Proposal::read(
            &json!({ "prompt": "Rename the doc", "apply": "applyRename", "payload": { "id": 1 } }),
            "proposeRename",
        )
        .unwrap();
        assert_eq!(ok.label, DEFAULT_APPLY_LABEL);
        assert_eq!(ok.payload, json!({ "id": 1 }));
        assert!(Proposal::read(&json!({ "apply": "x" }), "f").is_err());
        assert!(Proposal::read(&json!({ "prompt": "p", "apply": "x()" }), "f").is_err());
    }
}

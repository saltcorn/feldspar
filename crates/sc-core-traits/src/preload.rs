//! `preload_rows` and `preload_code`: context an agent starts every
//! conversation with (TODO-agent-skills AS8.1, AS8.2).
//!
//! The counterpart of v1's "Preload Data" skill, which puts rows of a query —
//! or the output of a piece of code — into the system prompt or the first
//! interactions. Neither trait offers a tool: what they contribute is computed
//! **once per session** ([`AgentTrait::session_header`]) and kept with the run,
//! so a conversation that continues tomorrow reads what it started with rather
//! than querying again on every step.
//!
//! Where it goes is the admin's choice ([`CFG_PLACEMENT`]):
//!
//! - **`session_header`** (the default): the first user-side message, after
//!   the cached prefix. Data, presented as data.
//! - **`system_prompt`**: appended to the system prompt on every step
//!   ([`AgentTrait::on_turn`]). The text is computed once and stored in the
//!   trait's run state, so it is the same on every step and the prompt stays
//!   cacheable from one step to the next.
//!
//! Both run **as the run's caller**: `preload_rows` reads through the row
//! layer under the §7.3 rule, and `preload_code`'s body has the `db` a
//! `run_js_code` body has, at the caller's role.

use std::collections::BTreeMap;

use sc_agent::{AgentTrait, SessionContext, ToolsContext, TraitCheck, TraitContext, Turn};
use sc_api::rows::RowQuery;
use sc_error::{Error, Result};
use sc_expr::CodeCall;
use sc_llm::ToolSpec;
use sc_query::{Expr, OrderBy};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::table::{
    CFG_FIELDS, CFG_MAX_ROWS, CFG_TABLE, check_table_config, config_str, configured_table,
    max_rows, project, visible_fields, where_expr,
};

/// Where the preloaded text goes: [`PLACEMENT_HEADER`] or [`PLACEMENT_SYSTEM`].
pub const CFG_PLACEMENT: &str = "placement";
/// The first user-side message of the session.
pub const PLACEMENT_HEADER: &str = "session_header";
/// The end of the system prompt, on every step.
pub const PLACEMENT_SYSTEM: &str = "system_prompt";

/// `preload_rows`: the formula giving the `where` object.
pub const CFG_FILTER: &str = "filter";
/// `preload_rows`: a template each row is written through.
pub const CFG_CONTENTS: &str = "contents";
/// A heading put above the preloaded text.
pub const CFG_HEADING: &str = "heading";
/// `preload_code`: the JavaScript body.
pub const CFG_PRELOAD_CODE: &str = "code";

/// How many rows `preload_rows` reads when the admin set no bound.
pub const DEFAULT_PRELOAD_ROWS: u64 = 20;

/// The key in the trait's run state that holds text for the system prompt.
const STATE_SYSTEM: &str = "system_prompt";

fn placement_field() -> FormField {
    FormField::new(CFG_PLACEMENT, BasicType::Text)
        .label("Put it in")
        .options(vec![json!(PLACEMENT_HEADER), json!(PLACEMENT_SYSTEM)])
        .default_value(PLACEMENT_HEADER)
        .sublabel(
            "session_header: the conversation's first message; system_prompt: the end of \
             the system prompt",
        )
}

fn placement(config: &Attrs) -> Result<&'static str> {
    match config_str(config, CFG_PLACEMENT).as_str() {
        "" | PLACEMENT_HEADER => Ok(PLACEMENT_HEADER),
        PLACEMENT_SYSTEM => Ok(PLACEMENT_SYSTEM),
        other => Err(Error::invalid(format!(
            "`{CFG_PLACEMENT}` `{other}` is not one of {PLACEMENT_HEADER}, {PLACEMENT_SYSTEM}"
        ))),
    }
}

/// File the session's text: `placed` where the configuration says, `system`
/// in the system prompt whatever it says. What is for the header is returned;
/// what is for the system prompt is kept in the run state for
/// [`on_turn`](AgentTrait::on_turn).
fn place(
    config: &Attrs,
    cx: &mut SessionContext<'_>,
    placed: String,
    system: String,
) -> Result<Option<String>> {
    let (header, system) = match placement(config)? {
        PLACEMENT_SYSTEM => (String::new(), join(&[system, placed])),
        _ => (placed, system),
    };
    if !system.trim().is_empty() {
        *cx.trait_state = json!({ STATE_SYSTEM: system });
    }
    Ok((!header.trim().is_empty()).then_some(header))
}

fn join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What `session_header` kept for the system prompt, appended on every step.
fn append_kept(turn: &mut Turn<'_>) {
    if let Some(text) = turn
        .state
        .and_then(|s| s.get(STATE_SYSTEM))
        .and_then(Json::as_str)
        .map(str::to_owned)
    {
        turn.append_system(text);
    }
}

fn with_heading(config: &Attrs, body: String) -> String {
    match config_str(config, CFG_HEADING) {
        heading if heading.is_empty() => body,
        heading => format!("{heading}\n\n{body}"),
    }
}

// --- preload_rows ---------------------------------------------------------------

/// Rows of one table, chosen by a formula over the triggering row and the
/// user, preloaded into the session.
pub struct PreloadRows;

#[async_trait::async_trait]
impl AgentTrait for PreloadRows {
    fn name(&self) -> &str {
        "preload_rows"
    }

    fn description(&self) -> &str {
        "Start every conversation with rows of a table"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_TABLE, BasicType::Text)
                .label("Table")
                .required(),
            FormField::new(CFG_FILTER, BasicType::Text)
                .label("Filter")
                .code("javascript")
                .sublabel(
                    "A JavaScript expression giving the `where` object, over `user` and, \
                     in a run a trigger started, the triggering `row` (null otherwise) — \
                     e.g. { owner: user.id }. Empty reads every row.",
                ),
            FormField::new(CFG_FIELDS, BasicType::Json)
                .label("Fields")
                .default_value(Json::Array(Vec::new())),
            FormField::new(CFG_CONTENTS, BasicType::Text)
                .label("Contents")
                .multiline()
                .sublabel(
                    "Each row written through this template, {{field}} for a value; empty \
                     writes each row as JSON",
                ),
            FormField::new(CFG_HEADING, BasicType::Text)
                .label("Heading")
                .sublabel("Put above the rows, to tell the model what they are"),
            FormField::new(CFG_MAX_ROWS, BasicType::Int)
                .label("Maximum rows")
                .default_value(DEFAULT_PRELOAD_ROWS as i64),
            placement_field(),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let table = check_table_config(check.catalog, check.config, DEFAULT_PRELOAD_ROWS)?;
        placement(check.config)?;
        if let Some(source) = filter_source(check.config) {
            check_filter(&source)?;
        }
        // Every `{{field}}` the template names is one the rows will have.
        let visible = visible_fields(&table, check.config)?;
        for name in template_names(&config_str(check.config, CFG_CONTENTS)) {
            if !visible.contains(&name) {
                return Err(Error::invalid(format!(
                    "`{CFG_CONTENTS}` names `{{{{{name}}}}}`, which is not a field of `{}` this \
                     trait reads; it reads {}",
                    table.name,
                    visible.join(", ")
                )));
            }
        }
        Ok(())
    }

    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        _args: &Json,
        _ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        Err(Error::invalid(format!("`preload_rows` offers no tool `{tool}`")))
    }

    async fn session_header(
        &self,
        config: &Attrs,
        cx: &mut SessionContext<'_>,
    ) -> Result<Option<String>> {
        let text = preloaded_rows(config, cx).await?;
        place(config, cx, text, String::new())
    }

    async fn on_turn(&self, _config: &Attrs, turn: &mut Turn<'_>) -> Result<()> {
        append_kept(turn);
        Ok(())
    }
}

/// The filter formula as written, or `None` for every row.
fn filter_source(config: &Attrs) -> Option<String> {
    let source = config_str(config, CFG_FILTER);
    (!source.is_empty()).then_some(source)
}

/// The filter as the body of a pure JavaScript function: one expression,
/// parenthesised so a bare object literal (`{ a: 1 }`) is read as one rather
/// than as a block.
///
/// An expression rather than a formula because the formula language has no
/// object literals, and a `where` object is what this computes. It is run with
/// no host at all — no `db`, no `fetch` — so it computes a value and nothing
/// else.
fn filter_body(source: &str) -> String {
    format!("return (\n{}\n);", source.trim())
}

/// What can be checked of the filter without running it: that it is one
/// expression, not statements that would close the parentheses around it.
fn check_filter(source: &str) -> Result<()> {
    let mut depth: i64 = 0;
    for c in source.chars() {
        match c {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth -= 1,
            ';' if depth == 0 => depth = -1,
            _ => {}
        }
        if depth < 0 {
            break;
        }
    }
    if depth != 0 {
        return Err(Error::invalid(format!(
            "`{CFG_FILTER}` should be one JavaScript expression giving a `where` object, \
             with its brackets balanced, such as {{ owner: user.id }}"
        )));
    }
    Ok(())
}

/// The rows, read as the run's caller and written out.
async fn preloaded_rows(config: &Attrs, cx: &SessionContext<'_>) -> Result<String> {
    let table = configured_table(cx.catalog, config)?;
    let visible = visible_fields(&table, config)?;
    let limit = max_rows(config, DEFAULT_PRELOAD_ROWS)?.max(1);
    let filter = match filter_source(config) {
        None => None,
        Some(source) => {
            let where_ = filter_value(&source, cx).await?;
            match where_ {
                Json::Null => None,
                Json::Object(_) => where_expr(&table, &visible, Some(&where_))?,
                other => {
                    return Err(Error::invalid(format!(
                        "agent `{}`: `preload_rows` `{CFG_FILTER}` should give a `where` \
                         object, and gave {other}",
                        cx.agent
                    )));
                }
            }
        }
    };
    let order = table
        .primary_key
        .first()
        .map(|key| vec![OrderBy::asc(Expr::col(key.clone()))])
        .unwrap_or_default();
    let query = RowQuery::new().where_(filter).order_by(order).limit(limit);
    let found = sc_api::read_rows_as(
        cx.catalog,
        &table,
        &query,
        cx.caller.role,
        cx.caller.user.as_ref(),
        cx.evaluator,
    )
    .await?;
    let rows: Vec<Json> = found
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|row| project(row, &visible))
        .collect();
    let template = config_str(config, CFG_CONTENTS);
    let body = if rows.is_empty() {
        format!("(No rows of `{}` matched.)", table.name)
    } else if template.is_empty() {
        rows.iter()
            .map(Json::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        rows.iter()
            .map(|row| render_template(&template, row))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let heading = match config_str(config, CFG_HEADING) {
        h if h.is_empty() => format!("Rows of `{}`:", table.name),
        h => h,
    };
    Ok(format!("{heading}\n\n{body}"))
}

/// Evaluate the filter with `user` and the triggering `row` (null in a run no
/// row event started) in scope.
async fn filter_value(source: &str, cx: &SessionContext<'_>) -> Result<Json> {
    check_filter(source)?;
    let evaluator = cx.evaluator.ok_or_else(|| {
        Error::config(format!(
            "agent `{}`: `preload_rows`' filter needs the JavaScript evaluator, and none is \
             configured on this server",
            cx.agent
        ))
    })?;
    let caller = sc_api::caller_context_at(cx.caller.role, cx.caller.user.as_ref());
    // `row` is in scope in every run, and null where no row started it, so a
    // filter written for a triggered run still reads in a chat.
    let bindings = BTreeMap::from([
        ("user".to_owned(), caller.user.clone().unwrap_or(Json::Null)),
        ("row".to_owned(), cx.row.cloned().unwrap_or(Json::Null)),
    ]);
    let call = CodeCall {
        code: filter_body(source),
        bindings,
        ..CodeCall::default()
    };
    evaluator.run_code(call).await.map_err(|e| {
        Error::invalid(format!(
            "agent `{}`: `preload_rows` `{CFG_FILTER}`: {e}",
            cx.agent
        ))
    })
}

/// The `{{ name }}` placeholders of a template, in order.
fn template_names(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let name = after[..end].trim();
        if !name.is_empty() {
            out.push(name.to_owned());
        }
        rest = &after[end + 2..];
    }
    out
}

/// Write `row` through `template`: each `{{ name }}` is the field's value, text
/// as itself and anything else as JSON, null as nothing.
fn render_template(template: &str, row: &Json) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim();
        match row.get(name) {
            Some(Json::String(s)) => out.push_str(s),
            Some(Json::Null) | None => {}
            Some(other) => out.push_str(&other.to_string()),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

// --- preload_code -----------------------------------------------------------------

/// A JavaScript body run once per session, whose result is preloaded.
pub struct PreloadCode;

#[async_trait::async_trait]
impl AgentTrait for PreloadCode {
    fn name(&self) -> &str {
        "preload_code"
    }

    fn description(&self) -> &str {
        "Start every conversation with what a piece of JavaScript returns"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_PRELOAD_CODE, BasicType::Text)
                .label("Code")
                .required()
                .code("javascript")
                .sublabel(
                    "Runs once per conversation, with `db`, `user`, `prompt` (the first \
                     message) and `trigger_row`. Return a string, or { system_prompt, \
                     messages }.",
                ),
            FormField::new(CFG_HEADING, BasicType::Text)
                .label("Heading")
                .sublabel("Put above what the code returns"),
            placement_field(),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        if config_str(check.config, CFG_PRELOAD_CODE).is_empty() {
            return Err(Error::invalid(format!("`{CFG_PRELOAD_CODE}` is required")));
        }
        placement(check.config)?;
        Ok(())
    }

    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        _args: &Json,
        _ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        Err(Error::invalid(format!("`preload_code` offers no tool `{tool}`")))
    }

    async fn session_header(
        &self,
        config: &Attrs,
        cx: &mut SessionContext<'_>,
    ) -> Result<Option<String>> {
        let code = config_str(config, CFG_PRELOAD_CODE);
        let dispatcher = cx.triggers.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: `preload_code` runs JavaScript through the trigger dispatcher, \
                 and none is available in this context",
                cx.agent
            ))
        })?;
        let mut scope = Attrs::new();
        scope.insert("prompt".to_owned(), json!(cx.brief));
        scope.insert(
            "trigger_row".to_owned(),
            cx.row.cloned().unwrap_or(Json::Null),
        );
        let caller = sc_api::caller_context_at(cx.caller.role, cx.caller.user.as_ref())
            .chained(cx.caller.tool_chain());
        let result = dispatcher
            .run_code(
                cx.catalog,
                "run_js_code",
                &format!("{}/preload_code", cx.agent),
                &code,
                scope,
                Some(&caller),
                None,
                None,
            )
            .await
            .map_err(|e| Error::invalid(format!("agent `{}`: `preload_code`: {e}", cx.agent)))?;
        let (header, system) = code_result(&result)?;
        let header = if header.trim().is_empty() {
            header
        } else {
            with_heading(config, header)
        };
        let system = if system.trim().is_empty() || !header.trim().is_empty() {
            system
        } else {
            with_heading(config, system)
        };
        place(config, cx, header, system)
    }

    async fn on_turn(&self, _config: &Attrs, turn: &mut Turn<'_>) -> Result<()> {
        append_kept(turn);
        Ok(())
    }
}

/// What the body returned, as `(text for the configured placement, text for
/// the system prompt)`.
///
/// A string (or any other value, as JSON) goes where the configuration says.
/// An object with `system_prompt` and/or `messages` says it itself: the first
/// always goes to the system prompt, the second — strings, or `{ role,
/// content }` — is written into the configured placement in order.
fn code_result(result: &Json) -> Result<(String, String)> {
    match result {
        Json::Null => Ok((String::new(), String::new())),
        Json::String(s) => Ok((s.clone(), String::new())),
        Json::Object(obj) if obj.contains_key("system_prompt") || obj.contains_key("messages") => {
            let system = match obj.get("system_prompt") {
                None | Some(Json::Null) => String::new(),
                Some(Json::String(s)) => s.clone(),
                Some(other) => other.to_string(),
            };
            let messages = match obj.get("messages") {
                None | Some(Json::Null) => Vec::new(),
                Some(Json::Array(items)) => items.clone(),
                Some(other) => {
                    return Err(Error::invalid(format!(
                        "`preload_code` returned `messages` that are not a list: {other}"
                    )));
                }
            };
            let lines: Vec<String> = messages
                .iter()
                .map(|m| match m {
                    Json::String(s) => s.clone(),
                    Json::Object(o) => {
                        let content = match o.get("content") {
                            Some(Json::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };
                        match o.get("role").and_then(Json::as_str) {
                            Some(role) if !role.is_empty() => format!("{role}: {content}"),
                            _ => content,
                        }
                    }
                    other => other.to_string(),
                })
                .collect();
            Ok((lines.join("\n\n"), system))
        }
        other => Ok((other.to_string(), String::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_template_writes_each_row_by_its_fields() {
        let row = json!({ "title": "Dune", "pages": 412, "notes": null });
        assert_eq!(
            render_template("{{title}} ({{ pages }} pages){{notes}}", &row),
            "Dune (412 pages)"
        );
        assert_eq!(template_names("{{a}} and {{ b }} {{"), ["a", "b"]);
    }

    #[test]
    fn a_code_result_is_text_or_says_where_it_goes() {
        assert_eq!(
            code_result(&json!("hi")).unwrap(),
            ("hi".to_owned(), String::new())
        );
        let (header, system) = code_result(&json!({
            "system_prompt": "Be brief.",
            "messages": ["first", { "role": "user", "content": "second" }],
        }))
        .unwrap();
        assert_eq!(system, "Be brief.");
        assert_eq!(header, "first\n\nuser: second");
        assert!(code_result(&json!({ "messages": 3 })).is_err());
        assert_eq!(code_result(&json!(42)).unwrap().0, "42");
    }

    #[test]
    fn a_filter_is_one_expression() {
        assert!(check_filter("{ owner: user.email }").is_ok());
        assert!(check_filter("row ? { id: row.id } : null").is_ok());
        assert!(check_filter("{ owner: ").is_err());
        assert!(check_filter("1); db.x(").is_err());
        assert_eq!(filter_body(" { a: 1 } "), "return (\n{ a: 1 }\n);");
    }
}

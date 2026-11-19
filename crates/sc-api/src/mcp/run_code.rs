//! `run_code` — run a code body once, on the server, belonging to no trigger.
//!
//! What an admin wrote a throwaway trigger for before: a one-off data migration,
//! or a body tried out before it goes into a trigger, a workflow step or a
//! custom query. It runs on the same action a trigger would (`run_js_code`,
//! `run_python_code`), with the same scope and the same bounds, at the admin's
//! authority, with its console collected — through
//! [`TriggerDispatcher::run_snippet`](sc_action::TriggerDispatcher::run_snippet).
//!
//! ## One transaction, committed or not
//!
//! The body runs inside one transaction on the primary database. A body that
//! fails is rolled back, so a migration that stops half way changes nothing. One
//! that succeeds is committed, which is what a migration wants; with `rollback`
//! set it is rolled back instead, which is what a trial run wants: the writes
//! happen, their reads see them, and nothing is left behind. What a database
//! transaction cannot undo is not undone, and the description says so: a
//! `fetch`, a file written, an email sent by a trigger a write fired, rows of a
//! table on another database or a module's.
//!
//! A committing run needs `allow_edit`, the grant for changing what is already
//! there. A rolled-back run needs none: it is a question, not a change.
//!
//! ## Logged
//!
//! Every run is a line in the server log at the info level, whatever the
//! verbosity — the caller's name, whether it committed, and the code itself —
//! because code that changed production data is the thing someone will later
//! want to find. The MCP server's own audit line follows with the outcome.

use sc_catalog::{Authority, Catalog};
use sc_error::{Error, Result};
use sc_types::Attrs;
use serde_json::{Value as Json, json};

use crate::schema_edit::{GRANT_EDIT, Grants};

use super::{AdminTool, Area, TOOL_DESCRIBE_CODE_API, ToolContext, require_grant};

/// Runs a code body once.
pub const TOOL_RUN_CODE: &str = "run_code";

const ARG_CODE: &str = "code";
const ARG_LANGUAGE: &str = "language";
const ARG_ROLLBACK: &str = "rollback";
const ARG_PAYLOAD: &str = "payload";
const ARG_TIMEOUT: &str = "timeout_ms";

/// The name the run carries in a cascade's chain and in an error.
const RUN_NAME: &str = "run_code";

/// The longest a body may run, as the code actions bound it.
const MAX_TIMEOUT_MS: u64 = sc_expr::MAX_CODE_TIMEOUT.as_secs() * 1000;

/// Run a code body once.
pub(super) struct RunCode;

#[async_trait::async_trait]
impl AdminTool for RunCode {
    fn name(&self) -> &'static str {
        TOOL_RUN_CODE
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Triggers)
    }

    fn description(&self, _catalog: &Catalog, grants: &Grants) -> String {
        run_code_description(grants)
    }

    fn parameters(&self) -> Json {
        run_code_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        run_code(ctx, grants, args).await
    }
}

fn run_code_description(grants: &Grants) -> String {
    let commit = match grants.edit {
        true => "Without `rollback` a run that succeeds is **committed** — the way to \
                 run a one-off data migration."
            .to_owned(),
        false => format!(
            "This caller may only run with `rollback: true`: committing needs \
             `{GRANT_EDIT}`."
        ),
    };
    format!(
        "Run a code body once on the server, as the admin, without saving a \
         trigger: a one-off data migration, or a body tried out before it goes \
         into a trigger, a workflow step or a query. It is a `run_js_code` body \
         (or `run_python_code` with `language: \"python\"`): the same scope, \
         `db`, `fetch`, `fs`, `trigger`, `lib()` and bounds, at most \
         {MAX_TIMEOUT_MS} ms. Read `{TOOL_DESCRIBE_CODE_API}` first. `payload` \
         is what the body sees as `payload`; there is no `row`.\n\n\
         The body runs in one database transaction. A body that throws is rolled \
         back and changes nothing. With `rollback: true` a body that succeeds is \
         rolled back too: its writes are made and seen by its own reads, then \
         undone — the way to test code. {commit}\n\n\
         A transaction cannot take back what left the database: a `fetch`, a file \
         written through `fs`, an email or a request sent by a trigger that a write \
         fired, and rows of a table on another database or provided by a module. \
         Those happen even with `rollback`. Every run is written to the server log \
         with its code.\n\n\
         Answers `result` (what the body returned), `committed`, and `console` \
         (what it printed). A failure is an error carrying the printed lines."
    )
}

fn run_code_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            ARG_CODE: {
                "type": "string",
                "description": "The body: the inside of an async function, so `await` \
                                works at the top level and `return` is the result.",
            },
            ARG_ROLLBACK: {
                "type": "boolean",
                "description": "Roll back even when the body succeeds (a trial run). \
                                Default false: a run that succeeds is committed.",
            },
            ARG_LANGUAGE: {
                "type": "string",
                "enum": ["javascript", "python"],
                "description": "Default javascript.",
            },
            ARG_PAYLOAD: {
                "description": "What the body sees as `payload`. Default {}.",
            },
            ARG_TIMEOUT: {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_TIMEOUT_MS,
                "description": "Wall clock for the run, in milliseconds. Default: the \
                                server's default for a code body.",
            },
        },
        "required": [ARG_CODE],
        "additionalProperties": false,
    })
}

async fn run_code(ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
    let args = super::arguments(
        args,
        &[
            ARG_CODE,
            ARG_ROLLBACK,
            ARG_LANGUAGE,
            ARG_PAYLOAD,
            ARG_TIMEOUT,
        ],
    )?;
    let code = args
        .get(ARG_CODE)
        .and_then(Json::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::invalid(format!("`{ARG_CODE}` is required")))?;
    let rollback = match args.get(ARG_ROLLBACK) {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_ROLLBACK}` should be true or false, got {other}"
            )));
        }
    };
    let action = match args.get(ARG_LANGUAGE).and_then(Json::as_str) {
        None | Some("javascript") => "run_js_code",
        Some("python") => "run_python_code",
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_LANGUAGE}` should be javascript or python, got `{other}`"
            )));
        }
    };
    let payload = match args.get(ARG_PAYLOAD) {
        None | Some(Json::Null) => json!({}),
        Some(value) => value.clone(),
    };
    if !rollback {
        require_grant(
            grants.edit,
            "run code that commits (run it with `rollback: true` to try it out)",
            GRANT_EDIT,
        )?;
    }

    let mut config = Attrs::new();
    config.insert("code".to_owned(), Json::String(code.to_owned()));
    if let Some(timeout) = args.get(ARG_TIMEOUT).filter(|v| !v.is_null()) {
        // Checked by the action itself, which names the bound.
        config.insert(ARG_TIMEOUT.to_owned(), timeout.clone());
    }

    let admin = ctx.require_admin(TOOL_RUN_CODE)?;
    let dispatcher = ctx.require_triggers()?;
    let mode = match rollback {
        true => "rolled back",
        false => "committing",
    };
    sc_log::log_info!("run_code [{}] {action}, {mode}:\n{code}", ctx.actor);
    let run = dispatcher
        .run_snippet(
            ctx.catalog,
            action,
            RUN_NAME,
            &config,
            payload,
            Authority::Caller(admin.principal()),
            rollback,
        )
        .await;
    let console: Vec<Json> = run
        .console
        .iter()
        .map(|line| json!({ "level": line.level, "text": line.text }))
        .collect();
    match run.result {
        Ok(result) => Ok(json!({
            "result": result,
            "committed": !rollback,
            "console": console,
        })),
        Err(e) => Err(failure(&e, &run.console)),
    }
}

/// A failed run as the error the model reads: why, that nothing was committed,
/// and what the body printed before it failed — the lines that explain it.
fn failure(e: &Error, console: &[sc_expr::ConsoleLine]) -> Error {
    let mut message = format!(
        "the code failed, and its writes were rolled back: {}",
        sc_error::format_chain(e)
    );
    if !console.is_empty() {
        message.push_str("\n\nconsole:");
        for line in console {
            message.push_str(&format!("\n[{}] {}", line.level, line.text));
        }
    }
    Error::invalid(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_says_what_a_rollback_cannot_undo_and_what_committing_needs() {
        let text = run_code_description(&Grants::all());
        assert!(text.contains("**committed**"), "{text}");
        assert!(text.contains("Those happen even with `rollback`"), "{text}");
        let text = run_code_description(&Grants::none());
        assert!(text.contains(GRANT_EDIT), "{text}");
        assert_eq!(run_code_parameters()["required"], json!([ARG_CODE]));
    }

    #[test]
    fn a_failure_carries_what_the_body_printed() {
        let lines = vec![sc_expr::ConsoleLine {
            level: "log".to_owned(),
            text: "got here".to_owned(),
        }];
        let err = failure(&Error::invalid("boom"), &lines).to_string();
        assert!(err.contains("boom") && err.contains("rolled back"), "{err}");
        assert!(err.contains("[log] got here"), "{err}");
    }
}

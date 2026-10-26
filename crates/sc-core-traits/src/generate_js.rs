//! `generate_js` — the model writes JavaScript, and the server runs it in a
//! sandbox (TODO-agent-skills AS9.4).
//!
//! The counterpart of v1's "Generate and run JavaScript code" skill. v1 made a
//! second inference call to write the code; here the **tool's argument is the
//! code**, so the model that decided to compute something writes it in the
//! same turn, sees the error if it throws, and corrects it on the next, all in
//! its own transcript.
//!
//! The code runs under [`crate::sandbox`]: only what the configuration grants
//! is in scope — `fetch` (under the `http` trait's host policy, optionally
//! narrowed to a host list), `db` over the tables as the run's **caller**
//! (read-only or read-write, optionally narrowed to a table list) and `lib()`
//! for the code libraries — with limits stricter than a code body's, and every
//! evaluation, its source included, kept in the run's ledger.
//!
//! What the admin writes is the **tool's name and description** (what the
//! parent model reads when choosing a tool), a **code description** (what the
//! code should know: which tables matter, which API to call) and an optional
//! **follow-up prompt**, handed to the model with each result as what to do
//! next.

use std::collections::BTreeMap;
use std::time::Duration;

use sc_agent::{AgentTrait, ToolsContext, TraitCheck, TraitContext};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::coding::may;
use crate::http::guard::{HostPolicy, parse_allowed_hosts};
use crate::sandbox::{Grants, MAX_SANDBOX_TIMEOUT, SandboxLimits, TableAccess, run_model_code};
use crate::table::config_str;

/// The tool's name, verbatim.
pub const CFG_GEN_TOOL_NAME: &str = "tool_name";
/// What the tool is for, as the model choosing a tool reads it.
pub const CFG_GEN_TOOL_DESCRIPTION: &str = "tool_description";
/// What the code should know.
pub const CFG_CODE_DESCRIPTION: &str = "code_description";
/// Whether `fetch` is in scope.
pub const CFG_ALLOW_FETCH: &str = "allow_fetch";
/// The hosts `fetch` may reach; empty for any public host.
pub const CFG_FETCH_HOSTS: &str = "fetch_hosts";
/// `none`, `read` or `write`.
pub const CFG_TABLE_ACCESS: &str = "table_access";
/// The tables `db` may name; empty for every table the caller can reach.
pub const CFG_TABLES: &str = "tables";
/// Whether `lib()` imports the code libraries.
pub const CFG_ALLOW_LIBRARIES: &str = "allow_libraries";
/// Handed to the model with each result.
pub const CFG_FOLLOW_UP_PROMPT: &str = "follow_up_prompt";
/// One evaluation's wall clock, in seconds.
pub const CFG_GEN_TIMEOUT: &str = "timeout_seconds";

/// The argument the code is sent in.
pub const ARG_CODE: &str = "code";

/// Run model-written JavaScript in a sandbox.
pub struct GenerateJs;

/// One instance's configuration, read and checked.
struct Settings {
    tool: String,
    grants: Grants,
    limits: SandboxLimits,
    follow_up: String,
}

impl Settings {
    fn read(config: &Attrs) -> Result<Settings> {
        let tool = config_str(config, CFG_GEN_TOOL_NAME);
        if tool.is_empty() {
            return Err(Error::invalid(format!("`{CFG_GEN_TOOL_NAME}` is required")));
        }
        if tool.len() > 64
            || !tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(Error::invalid(format!(
                "`{CFG_GEN_TOOL_NAME}` `{tool}` must be at most 64 letters, digits, `_` or `-`"
            )));
        }
        let fetch = match may(config, CFG_ALLOW_FETCH) {
            true => Some(HostPolicy::new(
                parse_allowed_hosts(&config_str(config, CFG_FETCH_HOSTS))?,
                false,
            )),
            false => None,
        };
        let tables = TableAccess::parse(&config_str(config, CFG_TABLE_ACCESS))?;
        let only_tables: Vec<String> = config_str(config, CFG_TABLES)
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect();
        let mut limits = SandboxLimits::default();
        match config.get(CFG_GEN_TIMEOUT) {
            None | Some(Json::Null) => {}
            Some(v) => match v.as_u64() {
                Some(s) if s >= 1 && s <= MAX_SANDBOX_TIMEOUT.as_secs() => {
                    limits.timeout = Duration::from_secs(s);
                }
                _ => {
                    return Err(Error::invalid(format!(
                        "`{CFG_GEN_TIMEOUT}` should be a whole number of seconds from 1 to {}, \
                         got {v}",
                        MAX_SANDBOX_TIMEOUT.as_secs()
                    )));
                }
            },
        }
        Ok(Settings {
            tool,
            grants: Grants {
                fetch,
                tables,
                only_tables,
                libraries: may(config, CFG_ALLOW_LIBRARIES),
            },
            limits,
            follow_up: config_str(config, CFG_FOLLOW_UP_PROMPT),
        })
    }
}

/// What the code has in scope, in a sentence each, for the tool description.
fn scope_lines(grants: &Grants) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(policy) = &grants.fetch {
        lines.push(match policy.allowed() {
            [] => "`await fetch(url, options)` reaches public URLs; the answer has `status`, \
                   `headers` and `text`."
                .to_owned(),
            hosts => format!(
                "`await fetch(url, options)` reaches {} (and subdomains); the answer has \
                 `status`, `headers` and `text`.",
                hosts.join(", ")
            ),
        });
    }
    let tables = match grants.only_tables.as_slice() {
        [] => "the tables".to_owned(),
        list => format!("the tables {}", list.join(", ")),
    };
    match grants.tables {
        TableAccess::None => {}
        TableAccess::ReadOnly => lines.push(format!(
            "`db.<table>` reads {tables}, read-only, as the person you are talking to: \
             `await db.<table>.where({{...}}).orderBy('field').rows()`, `.first()`, `.count()`."
        )),
        TableAccess::ReadWrite => lines.push(format!(
            "`db.<table>` reads and writes {tables} as the person you are talking to: \
             `.where({{...}}).rows()`, `.count()`, `.insert(row)`, `.where({{...}}).update(changes)`, \
             `.where({{...}}).delete()`, each awaited."
        )),
    }
    if grants.libraries {
        lines.push("`lib('name')` imports a code library of this application.".to_owned());
    }
    lines
}

#[async_trait::async_trait]
impl AgentTrait for GenerateJs {
    fn name(&self) -> &str {
        "generate_js"
    }

    fn description(&self) -> &str {
        "JavaScript the model writes, run in a sandbox with only the grants given"
    }

    fn config_spec(&self) -> Vec<FormField> {
        let with_tables = || vec![json!("read"), json!("write")];
        vec![
            FormField::new(CFG_GEN_TOOL_NAME, BasicType::Text)
                .label("Tool name")
                .required()
                .sublabel("What the model calls it: letters, digits, _ and -"),
            FormField::new(CFG_GEN_TOOL_DESCRIPTION, BasicType::Text)
                .label("Tool description")
                .multiline()
                .sublabel("When the model should write code with this tool"),
            FormField::new(CFG_CODE_DESCRIPTION, BasicType::Text)
                .label("Code description")
                .multiline()
                .sublabel("What the code should know: the tables or the API it works with"),
            FormField::new(CFG_ALLOW_FETCH, BasicType::Bool)
                .label("Allow HTTP fetch")
                .default_value(false),
            FormField::new(CFG_FETCH_HOSTS, BasicType::Text)
                .label("Hosts fetch may reach, one per line (empty for any public host)")
                .multiline()
                .show_if(CFG_ALLOW_FETCH, vec![json!(true)]),
            FormField::new(CFG_TABLE_ACCESS, BasicType::Text)
                .label("Access to tables")
                .options(["none", "read", "write"])
                .default_value("none"),
            FormField::new(CFG_TABLES, BasicType::Text)
                .label("Only these tables, one per line (empty for every table)")
                .multiline()
                .show_if(CFG_TABLE_ACCESS, with_tables()),
            FormField::new(CFG_ALLOW_LIBRARIES, BasicType::Bool)
                .label("Allow the code libraries (lib())")
                .default_value(false),
            FormField::new(CFG_FOLLOW_UP_PROMPT, BasicType::Text)
                .label("Follow-up prompt")
                .multiline()
                .sublabel("Given to the model with each result: what to do with it"),
            FormField::new(CFG_GEN_TIMEOUT, BasicType::Int)
                .label("Time limit per run (seconds, at most 30)")
                .default_value(crate::sandbox::DEFAULT_SANDBOX_TIMEOUT.as_secs() as i64),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let settings = Settings::read(check.config)?;
        for table in &settings.grants.only_tables {
            if check.catalog.get(table)?.is_none() {
                return Err(Error::invalid(format!(
                    "`{CFG_TABLES}`: there is no table `{table}`"
                )));
            }
        }
        Ok(())
    }

    fn tools(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let name = config_str(config, CFG_GEN_TOOL_NAME);
        let settings = Settings::read(config).ok();
        let mut description = match config_str(config, CFG_GEN_TOOL_DESCRIPTION).as_str() {
            "" => "Run JavaScript you write.".to_owned(),
            text => text.to_owned(),
        };
        description.push_str(
            "\n\nSend the code in `code`: the body of an async function, so use `await` and \
             `return` the result (a string or JSON). If it throws, you get the error; fix the \
             code and call again.",
        );
        let code_description = config_str(config, CFG_CODE_DESCRIPTION);
        if !code_description.is_empty() {
            description.push_str(&format!("\n\n{code_description}"));
        }
        let lines = settings
            .as_ref()
            .map(|s| scope_lines(&s.grants))
            .unwrap_or_default();
        description.push_str("\n\nIn scope: ");
        description.push_str(&match lines.is_empty() {
            true => "plain JavaScript only — no network, tables or modules.".to_owned(),
            false => format!(
                "{} Nothing else (no `require`, files or modules).",
                lines.join(" ")
            ),
        });
        vec![ToolSpec::new(
            name,
            description,
            json!({
                "type": "object",
                "properties": {
                    ARG_CODE: {"type": "string", "minLength": 1, "description": "The JavaScript to run"},
                },
                "required": [ARG_CODE],
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
        let settings = Settings::read(config)?;
        let code = args
            .get(ARG_CODE)
            .and_then(Json::as_str)
            .unwrap_or_default()
            .trim();
        if code.is_empty() {
            return Err(Error::invalid(format!(
                "`{ARG_CODE}` is required: the JavaScript `{}` should run",
                settings.tool
            )));
        }
        // A fenced block is what a model writes out of habit.
        let code = unfence(code);
        let value = run_model_code(
            ctx,
            &settings.tool,
            code,
            BTreeMap::new(),
            &settings.grants,
            &settings.limits,
        )
        .await?;
        let mut out = json!({ "result": value });
        if !settings.follow_up.is_empty() {
            out["next"] = Json::String(settings.follow_up);
        }
        Ok(out)
    }
}

/// The code inside a Markdown fence, or the text as it is.
fn unfence(code: &str) -> &str {
    let Some(rest) = code.strip_prefix("```") else {
        return code;
    };
    let body = rest.split_once('\n').map_or("", |(_, body)| body);
    body.trim_end().strip_suffix("```").unwrap_or(body).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(value: Json) -> Attrs {
        value.as_object().unwrap().clone().into_iter().collect()
    }

    #[test]
    fn the_grants_are_read_from_the_configuration() {
        let s = Settings::read(&config(json!({
            "tool_name": "compute", "allow_fetch": true, "fetch_hosts": "api.example.com",
            "table_access": "read", "tables": "books\nauthors", "allow_libraries": true,
            "timeout_seconds": 5, "follow_up_prompt": "Summarise it",
        })))
        .unwrap();
        assert_eq!(
            s.grants.fetch.as_ref().unwrap().allowed(),
            ["api.example.com"]
        );
        assert_eq!(s.grants.tables, TableAccess::ReadOnly);
        assert_eq!(s.grants.only_tables, ["books", "authors"]);
        assert!(s.grants.libraries);
        assert_eq!(s.limits.timeout, Duration::from_secs(5));
        let none = Settings::read(&config(json!({ "tool_name": "compute" }))).unwrap();
        assert!(none.grants.fetch.is_none());
        assert_eq!(none.grants.tables, TableAccess::None);
        assert!(scope_lines(&none.grants).is_empty());
    }

    #[test]
    fn a_bad_configuration_is_refused_by_name() {
        assert!(Settings::read(&config(json!({}))).is_err());
        assert!(Settings::read(&config(json!({ "tool_name": "two words" }))).is_err());
        let err = Settings::read(&config(json!({ "tool_name": "t", "timeout_seconds": 60 })))
            .err()
            .expect("too long");
        assert!(err.to_string().contains(CFG_GEN_TIMEOUT), "{err}");
        assert!(
            Settings::read(&config(
                json!({ "tool_name": "t", "table_access": "admin" })
            ))
            .is_err()
        );
    }

    #[test]
    fn a_fenced_block_is_unwrapped() {
        assert_eq!(unfence("```javascript\nreturn 1\n```"), "return 1");
        assert_eq!(unfence("```\nreturn 2\n```\n"), "return 2");
        assert_eq!(unfence("return 3"), "return 3");
    }
}

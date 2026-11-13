//! `app_constructor` — the planning tools of the App Constructor
//! (TODO-app-constructor AC2; docs/TECHNICAL_DESIGN.md §11.6).
//!
//! Carried by an application's construction agent, `construct-<subdomain>`,
//! beside `admin_copilot`: the copilot builds the data model, and this trait
//! writes the plan it is built by. One application per instance, named by its
//! subdomain as `build_application` names it.
//!
//! - `save_phases` replaces the construction's phases
//!   ([`sc_construct::save_phases`], the validation the admin's edits go
//!   through too). Its result ends with the phase list, as `save_plan`'s ends
//!   with the plan, so the freshest copy is the last thing the model read.
//! - `save_tasks` plans the tasks of one phase ([`sc_construct::save_tasks`],
//!   AC3.1). The runner asks for it when it reaches the phase, so the tasks are
//!   written against what the earlier phases actually made.
//!
//! The session header carries the specification and the current phases, so a
//! conversation with the agent starts from the plan as it stands.
//!
//! Every tool is an admin's: the plan decides what the construction agent
//! later builds with the copilot's grants.

use sc_agent::{AgentTrait, SessionContext, ToolsContext, TraitCheck, TraitContext};
use sc_api::mcp::require_admin;
use sc_app::{Application, load_application_by_subdomain};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

use crate::coding::is_admin;
use crate::table::config_str;

/// The application whose construction this trait plans, by subdomain.
pub const CFG_CONSTRUCTED_APPLICATION: &str = "application";

/// Replaces the construction's phases.
pub const TOOL_SAVE_PHASES: &str = "save_phases";

/// Plans the tasks of one phase.
pub const TOOL_SAVE_TASKS: &str = "save_tasks";

const ARG_PHASES: &str = "phases";
const ARG_PHASE: &str = "phase";
const ARG_TASKS: &str = "tasks";

/// The planning tools of one application's construction.
pub struct AppConstructor;

/// What the agent is told about planning phases.
const PROMPT: &str = "You plan the construction of an application in phases. A phase \
     delivers a part of the application someone can use and check; its tasks are planned \
     later, when the phase is reached, so a phase says what, not how. The first phase is \
     always the data model: the tables, their fields and relations, and the queries the \
     application needs. Three to six phases suit most applications. Give each phase a \
     short title, a description, and requirements, each with a priority (`must`, `should` \
     or `could`). Save the phases with `save_phases`; to change them, call it again with \
     the whole list, naming each phase you keep by its `id`. A phase that already has \
     tasks stays even when you leave it out.\n\n\
     When a phase is reached you are asked to plan its tasks with `save_tasks`. A \
     `data_model` task makes tables, fields or queries, and you run it yourself; a \
     `feature` task makes part of the code, and the application's builder agent runs it. \
     Plan few tasks, each a sizeable piece of work: every task starts a fresh session that \
     reads the schema and the code again before it does anything, so two small tasks cost \
     far more than one that does both. One to three tasks suit most phases: the phase's \
     tables and fields in one `data_model` task, the queries over them in one more (or the \
     same one when there are few), and a whole screen or flow in one `feature` task. \
     Never plan a task that only builds, checks, tests, publishes or reviews: after every \
     feature task the application is built and checked anyway, and the task fails if it \
     does not build. A task's brief is all its agent reads, so say what to make and name \
     the tables and queries it uses. Never plan what exists already. `depends_on` lists \
     earlier tasks of the same list by number.";

fn configured(config: &Attrs) -> Result<String> {
    match config_str(config, CFG_CONSTRUCTED_APPLICATION) {
        subdomain if subdomain.is_empty() => Err(Error::invalid(format!(
            "`{CFG_CONSTRUCTED_APPLICATION}` must name an application by its subdomain"
        ))),
        subdomain => Ok(subdomain),
    }
}

async fn resolve(catalog: &Catalog, config: &Attrs) -> Result<Application> {
    let subdomain = configured(config)?;
    load_application_by_subdomain(catalog, &subdomain)
        .await?
        .ok_or_else(|| {
            Error::invalid(format!(
                "there is no application with the subdomain `{subdomain}`"
            ))
        })
}

#[async_trait::async_trait]
impl AgentTrait for AppConstructor {
    fn name(&self) -> &str {
        "app_constructor"
    }

    fn description(&self) -> &str {
        "Plans the construction of one application in phases (the App Constructor)"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_CONSTRUCTED_APPLICATION, BasicType::Text)
                .label("Application (subdomain)")
                .required(),
        ]
    }

    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        resolve(check.catalog, check.config).await.map(|_| ())
    }

    /// The planning instructions, and where the documentation is.
    fn prompt(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Option<String> {
        Some(format!("{PROMPT}\n\n{}", sc_docs::tools::PROMPT))
    }

    /// The documentation's two, which `coding`, `docs` and `admin_copilot`
    /// offer identically.
    fn shared_tools(&self) -> &[&'static str] {
        &sc_docs::tools::TOOL_NAMES
    }

    /// Withheld from a run whose caller is not an admin, and from a run a
    /// trigger started or an application's page sent: every one of them
    /// changes the application or the plan it is built from, and such a run's
    /// prompt was written by someone else.
    fn tools(&self, cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        if cx
            .caller
            .is_some_and(|caller| !is_admin(caller) || caller.untrusted_origin())
        {
            return Vec::new();
        }
        let data_model: Vec<ToolSpec> = data_model_tool_set()
            .specs(cx.catalog)
            .into_iter()
            .filter(|spec| DATA_MODEL_TOOLS.contains(&spec.name.as_str()))
            .collect();
        let requirement = json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" },
                "priority": { "type": "string", "enum": ["must", "should", "could"] },
            },
            "required": ["text", "priority"],
            "additionalProperties": false,
        });
        vec![
            ToolSpec::new(
                TOOL_SAVE_PHASES,
                "Save the construction's phases, in order, replacing the list there is. Name a \
             phase you keep by its `id` to update it in place; leave `id` out for a new one. \
             A phase left out is deleted, unless it already has tasks. Returns the phases as \
             they now stand.",
                json!({
                    "type": "object",
                    "properties": {
                        ARG_PHASES: {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string", "description": "An existing phase's id" },
                                    "title": { "type": "string" },
                                    "description": { "type": "string" },
                                    "requirements": { "type": "array", "items": requirement },
                                },
                                "required": ["title", "description", "requirements"],
                                "additionalProperties": false,
                            },
                        },
                    },
                    "required": [ARG_PHASES],
                    "additionalProperties": false,
                }),
            ),
            ToolSpec::new(
                TOOL_SAVE_TASKS,
                "Save the tasks of one phase, in order, replacing the tasks it has. Only for a \
             phase whose tasks have not started. Returns the tasks as saved.",
                json!({
                    "type": "object",
                    "properties": {
                        ARG_PHASE: { "type": "string", "description": "The phase's id" },
                        ARG_TASKS: {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "kind": { "type": "string", "enum": ["data_model", "feature"] },
                                    "title": { "type": "string" },
                                    "brief": {
                                        "type": "string",
                                        "description": "What to make; all the task's agent reads"
                                    },
                                    "depends_on": {
                                        "type": "array",
                                        "items": { "type": "integer", "minimum": 1 },
                                        "description": "Earlier tasks of this list, by number"
                                    },
                                },
                                "required": ["kind", "title", "brief"],
                                "additionalProperties": false,
                            },
                        },
                    },
                    "required": [ARG_PHASE, ARG_TASKS],
                    "additionalProperties": false,
                }),
            ),
        ]
        .into_iter()
        .chain(data_model)
        .collect()
    }

    async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        require_admin(ctx.caller.role(), tool)?;
        if ctx.caller.untrusted_origin() {
            return Err(Error::invalid(format!(
                "`{tool}` changes the application or its construction plan, so it is only for \
                 an admin's own runs, not one a trigger started or an application's page sent"
            )));
        }
        if DATA_MODEL_TOOLS.contains(&tool) {
            return data_model_call(tool, args, ctx).await;
        }
        let app = resolve(ctx.catalog, config).await?;
        if tool == TOOL_SAVE_TASKS {
            return save_tasks(ctx.catalog, &app, args).await;
        }
        let phases = sc_construct::parse_phases(args.get(ARG_PHASES).unwrap_or(&Json::Null))?;
        let saved = sc_construct::save_phases(ctx.catalog, app.id.0, &phases).await?;
        let mut text = format!("Saved {} phase(s).", phases.len());
        if !saved.kept.is_empty() {
            text.push_str(&format!(
                " Kept, because they have tasks: {}.",
                saved
                    .kept
                    .iter()
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        text.push_str("\n\nThe phases now:\n");
        text.push_str(&sc_construct::phase_list(&saved.phases));
        Ok(Json::String(text))
    }

    /// The specification and the phases as they stand. A construction not
    /// made yet is said so, rather than failing the run.
    async fn session_header(
        &self,
        config: &Attrs,
        cx: &mut SessionContext<'_>,
    ) -> Result<Option<String>> {
        let app = resolve(cx.catalog, config).await?;
        let Some(construction) = sc_construct::load_construction(cx.catalog, app.id.0).await?
        else {
            return Ok(Some(format!("`{}` has no construction yet.", app.name)));
        };
        let phases = sc_construct::phases_with_tasks(cx.catalog, construction.id).await?;
        let specification = match construction.specification.trim() {
            "" => "(none yet)",
            text => text,
        };
        Ok(Some(format!(
            "## The construction of `{}`\n\nSpecification:\n{specification}\n\nPhases:\n{}",
            app.name,
            sc_construct::phase_list(&phases)
        )))
    }
}

/// `save_tasks`: plan one phase's tasks and answer them as saved.
async fn save_tasks(catalog: &Catalog, app: &Application, args: &Json) -> Result<Json> {
    let phase = args
        .get(ARG_PHASE)
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid(format!("`{ARG_PHASE}` must be the phase's id")))?;
    let phase = uuid::Uuid::parse_str(phase)
        .map_err(|_| Error::invalid(format!("`{phase}` is not a phase id")))?;
    let tasks = sc_construct::parse_tasks(args.get(ARG_TASKS).unwrap_or(&Json::Null))?;
    let saved = sc_construct::save_tasks(catalog, app.id.0, phase, &tasks).await?;
    Ok(Json::String(format!(
        "Saved {} task(s).\n\n{}",
        saved.len(),
        sc_construct::task_list(&saved)
    )))
}

/// The administrative tools a construction builds the data model with: the
/// schema, the application's tables, queries and code libraries, the code
/// API's reference and the documentation. The same tools the admin copilot and
/// the MCP server offer (`sc_app::mcp::tool_set`), so each behaves and is
/// refused the same way; not the triggers, streams, translations, file stores,
/// or creating applications, which a construction never uses. Each tool's
/// description is sent with every request, so every tool here is paid for on
/// every step.
pub const DATA_MODEL_TOOLS: [&str; 12] = [
    sc_api::mcp::TOOL_DESCRIBE,
    sc_api::mcp::TOOL_EDIT,
    sc_api::mcp::TOOL_DESCRIBE_CODE_API,
    sc_app::mcp::TOOL_DESCRIBE_APPS,
    sc_app::mcp::TOOL_UPDATE_APP,
    sc_app::mcp::TOOL_SAVE_QUERY,
    sc_app::mcp::TOOL_DELETE_QUERY,
    sc_app::mcp::TOOL_DESCRIBE_CODE_LIBRARIES,
    sc_app::mcp::TOOL_SAVE_CODE_LIBRARY,
    sc_app::mcp::TOOL_DELETE_CODE_LIBRARY,
    sc_docs::tools::TOOL_SEARCH_DOCS,
    sc_docs::tools::TOOL_READ_DOCS,
];

/// What the data-model tools may do: create and change, but not drop, which
/// an admin does deliberately. A table with row-level security keeps its
/// access rules either way: only a person changes those.
fn data_model_tool_set() -> sc_api::mcp::ToolSet {
    sc_app::mcp::tool_set(
        sc_api::schema_edit::Grants {
            create: true,
            edit: true,
            drop: false,
        },
        sc_api::mcp::Areas::all(),
    )
}

/// Run one of the data-model tools, refused unless the run is an admin's
/// own: the schema and the queries are the application itself, and a
/// triggered run's prompt or an application page's message was written by
/// somebody else.
async fn data_model_call(tool: &str, args: &Json, ctx: &TraitContext<'_>) -> Result<Json> {
    let tool_context = sc_api::mcp::ToolContext {
        catalog: ctx.catalog,
        user: ctx.caller.user_ref(),
        admin: ctx.caller.console_admin().cloned(),
        triggers: ctx.triggers,
        actor: ctx.agent,
    };
    data_model_tool_set().call(tool, args, &tool_context).await
}

/// The trait's configuration for `subdomain`'s construction agent.
pub fn app_constructor_config(subdomain: &str) -> Attrs {
    [(
        CFG_CONSTRUCTED_APPLICATION.to_owned(),
        Json::String(subdomain.to_owned()),
    )]
    .into_iter()
    .collect()
}

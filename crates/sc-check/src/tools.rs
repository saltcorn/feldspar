//! The checklist tools an agent is offered (TODO C1.15): `describe_step_types`,
//! `describe_checklists`, `save_checklist`, `append_steps`,
//! `propose_step_change` and `run_checklists`.
//!
//! **One implementation, two callers**, as for the schema tools: the internal
//! `admin_copilot` agent and the administration MCP server both list
//! [`tool_specs`] and dispatch to [`call_tool`], each translating its own caller
//! into a [`ToolCall`] — an agent's run and checkbox, or a token's id and
//! grants. The rules of §4 are not here but in [`crate::edit`], which the
//! admin API goes through too; a refusal names the grant that is missing.

use sc_app::load_application_by_subdomain;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use crate::edit::{self, Change, ChecklistInput, Editor};
use crate::model::{ChecklistRun, Outcome, ProposalChange, StepDraft};
use crate::registry::StepRegistry;
use crate::runner::{CheckEnv, run_checklist, run_checklists};
use crate::store::{self, ChecklistScope};

/// List the step types.
pub const TOOL_DESCRIBE_STEP_TYPES: &str = "describe_step_types";
/// List the checklists, or show one.
pub const TOOL_DESCRIBE_CHECKLISTS: &str = "describe_checklists";
/// Create a checklist.
pub const TOOL_SAVE_CHECKLIST: &str = "save_checklist";
/// Add steps at the end of a checklist.
pub const TOOL_APPEND_STEPS: &str = "append_steps";
/// Change an existing step, or ask for the change.
pub const TOOL_PROPOSE_STEP_CHANGE: &str = "propose_step_change";
/// Run checklists.
pub const TOOL_RUN_CHECKLISTS: &str = "run_checklists";

/// Every tool's name.
pub const TOOL_NAMES: [&str; 6] = [
    TOOL_DESCRIBE_STEP_TYPES,
    TOOL_DESCRIBE_CHECKLISTS,
    TOOL_SAVE_CHECKLIST,
    TOOL_APPEND_STEPS,
    TOOL_PROPOSE_STEP_CHANGE,
    TOOL_RUN_CHECKLISTS,
];

/// Whether `name` is one of these tools.
pub fn is_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// What one call runs with, and as whom.
pub struct ToolCall<'a> {
    /// The catalog, the step types and the host's capabilities.
    pub env: CheckEnv<'a>,
    /// The agent's run, under its grant.
    pub editor: Editor,
    /// The role the call is authorized at. Every tool here wants an admin.
    pub role: u8,
}

const STEP_SCHEMA_NOTE: &str = "A step is {type, params, label, as}: `type` from \
describe_step_types, `params` its parameters, `label` what a person reads in the list, `as` an \
optional name its value is captured under. Every string parameter is a template over earlier \
captures and `run` — `{{ order.id }}`, `Test customer {{ run.id }}` — and one that is a single \
`{{ x }}` keeps the value's type.";

fn step_item_schema() -> Json {
    json!({
        "type": "object",
        "properties": {
            "type": { "type": "string" },
            "params": { "type": "object" },
            "label": { "type": "string" },
            "as": { "type": "string" },
        },
        "required": ["type"],
    })
}

/// The tools' specs, in order.
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            TOOL_DESCRIBE_STEP_TYPES,
            "List the step types a checklist can use: each one's name, what it does, its \
             parameters, what it needs from the host and whether it writes data. Give \
             `application` (a subdomain) for the ones an application's checklist may use.",
            json!({
                "type": "object",
                "properties": {
                    "application": { "type": "string", "description": "A subdomain." },
                },
            }),
        ),
        ToolSpec::new(
            TOOL_DESCRIBE_CHECKLISTS,
            "List the checklists — what \"working\" means for each application — with their \
             last result; or, with `checklist`, show one: its steps (ids, authors, whether a \
             person accepted each), its pending proposals and its last run's step results.",
            json!({
                "type": "object",
                "properties": {
                    "application": {
                        "type": "string",
                        "description": "Only this application's checklists (a subdomain).",
                    },
                    "checklist": { "type": "string", "description": "One checklist, by name." },
                },
            }),
        ),
        ToolSpec::new(
            TOOL_SAVE_CHECKLIST,
            format!(
                "Create a checklist: an ordered list of steps run as one user, each doing \
                 something and asserting something. Write one for what the user asked to work, \
                 so later changes can be checked against it. `run_as` is the email of a check \
                 user (leave it out to run as an anonymous visitor). {STEP_SCHEMA_NOTE}"
            ),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "description": { "type": "string" },
                    "application": { "type": "string", "description": "A subdomain; leave out for a data-only checklist." },
                    "run_as": { "type": "string", "description": "A check user's email." },
                    "steps": { "type": "array", "items": step_item_schema() },
                },
                "required": ["name"],
            }),
        ),
        ToolSpec::new(
            TOOL_APPEND_STEPS,
            format!(
                "Add steps at the end of a checklist. Always allowed: a step at the end cannot \
                 change what an earlier one does. {STEP_SCHEMA_NOTE}"
            ),
            json!({
                "type": "object",
                "properties": {
                    "checklist": { "type": "string" },
                    "steps": { "type": "array", "items": step_item_schema() },
                },
                "required": ["checklist", "steps"],
            }),
        ),
        ToolSpec::new(
            TOOL_PROPOSE_STEP_CHANGE,
            "Change an existing step: `edit` it (give `step` and `new_step`), `insert_before` \
             it, `remove` it, `reorder` the steps (`order`: every step id), or change the \
             checklist's `run_as`. A step your own run wrote and nobody accepted yet changes at \
             once; anything else becomes a proposal a person accepts or rejects, and until then \
             the old step keeps running — so never weaken a check to make it pass: fix the \
             application, or propose the change with an honest `reason` (a button renamed on \
             purpose). An accepted step cannot be removed.",
            json!({
                "type": "object",
                "properties": {
                    "checklist": { "type": "string" },
                    "change": {
                        "type": "string",
                        "enum": ["edit", "insert_before", "remove", "reorder", "run_as"],
                    },
                    "step": { "type": "string", "description": "The step's id." },
                    "new_step": step_item_schema(),
                    "order": { "type": "array", "items": { "type": "string" } },
                    "run_as": { "type": ["string", "null"], "description": "A check user's email, or null." },
                    "reason": { "type": "string" },
                },
                "required": ["checklist", "change", "reason"],
            }),
        ),
        ToolSpec::new(
            TOOL_RUN_CHECKLISTS,
            "Run checklists against the live application and report each one's result and the \
             steps that failed: one `checklist` by name, every checklist of an `application` \
             (a subdomain), or every checklist. Run them after a change, and fix what fails.",
            json!({
                "type": "object",
                "properties": {
                    "application": { "type": "string" },
                    "checklist": { "type": "string" },
                },
            }),
        ),
    ]
}

/// Run one tool.
pub async fn call_tool(name: &str, args: &Json, cx: &ToolCall<'_>) -> Result<Json> {
    sc_api::mcp::require_admin(cx.role, name)?;
    match name {
        TOOL_DESCRIBE_STEP_TYPES => describe_step_types(args, cx).await,
        TOOL_DESCRIBE_CHECKLISTS => describe_checklists(args, cx).await,
        TOOL_SAVE_CHECKLIST => save_checklist(args, cx).await,
        TOOL_APPEND_STEPS => append_steps(args, cx).await,
        TOOL_PROPOSE_STEP_CHANGE => propose_step_change(args, cx).await,
        TOOL_RUN_CHECKLISTS => run(args, cx).await,
        other => Err(Error::invalid(format!(
            "`{other}` is not a checklist tool; they are {}",
            TOOL_NAMES.join(", ")
        ))),
    }
}

fn args_of(args: &Json, allowed: &[&str]) -> Result<Map<String, Json>> {
    sc_api::mcp::arguments(args, allowed)
}

fn required(obj: &Map<String, Json>, key: &str) -> Result<String> {
    sc_api::mcp::optional_string(obj, key)?
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{key}` is required")))
}

async fn application_id(cx: &ToolCall<'_>, subdomain: Option<String>) -> Result<Option<Uuid>> {
    let Some(subdomain) = subdomain.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let app = load_application_by_subdomain(cx.env.catalog, subdomain.trim())
        .await?
        .ok_or_else(|| {
            Error::not_found(format!(
                "no application with subdomain `{subdomain}` (describe_applications lists them)"
            ))
        })?;
    Ok(Some(app.id.0))
}

async fn user_id(cx: &ToolCall<'_>, email: Option<&str>) -> Result<Option<Uuid>> {
    let Some(email) = email.map(str::trim).filter(|e| !e.is_empty()) else {
        return Ok(None);
    };
    let user = sc_auth::load_user_by_email(cx.env.catalog, email)
        .await?
        .ok_or_else(|| Error::not_found(format!("no user with email `{email}`")))?;
    Ok(Some(user.id))
}

fn drafts(value: Option<&Json>) -> Result<Vec<StepDraft>> {
    match value {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                StepDraft::from_json(item)
                    .map_err(|e| Error::invalid(format!("step {}: {e}", i + 1)))
            })
            .collect(),
        Some(other) => Err(Error::invalid(format!(
            "`steps` should be a list of steps, got {other}"
        ))),
    }
}

async fn describe_step_types(args: &Json, cx: &ToolCall<'_>) -> Result<Json> {
    let obj = args_of(args, &["application"])?;
    let with_app = application_id(cx, sc_api::mcp::optional_string(&obj, "application")?)
        .await?
        .is_some();
    let types: Vec<Json> = if with_app {
        cx.env
            .registry
            .offered(true)
            .into_iter()
            .map(|t| StepRegistry::describe(t.as_ref()))
            .collect()
    } else {
        cx.env
            .registry
            .all()
            .map(|t| {
                let mut described = StepRegistry::describe(t.as_ref());
                described["needs_application"] = json!(t.needs().application());
                described
            })
            .collect()
    };
    Ok(json!({ "step_types": types }))
}

async fn describe_checklists(args: &Json, cx: &ToolCall<'_>) -> Result<Json> {
    let obj = args_of(args, &["application", "checklist"])?;
    let catalog = cx.env.catalog;
    if let Some(name) = sc_api::mcp::optional_string(&obj, "checklist")? {
        let checklist = store::find_checklist(catalog, &name).await?;
        let steps = store::current_steps(catalog, checklist.id).await?;
        let proposals = store::proposals_of(catalog, checklist.id).await?;
        let last = store::last_run(catalog, checklist.id).await?;
        let mut out = checklist.to_json();
        out["steps"] = json!(steps.iter().map(|s| s.to_json()).collect::<Vec<_>>());
        out["proposals"] = json!(proposals.iter().map(|p| p.to_json()).collect::<Vec<_>>());
        out["last_run"] = last.as_ref().map_or(Json::Null, ChecklistRun::to_json);
        return Ok(out);
    }
    let scope = match application_id(cx, sc_api::mcp::optional_string(&obj, "application")?).await? {
        Some(app) => ChecklistScope::Application(app),
        None => ChecklistScope::All,
    };
    let mut listed = Vec::new();
    for checklist in store::list_checklists(catalog, scope).await? {
        let steps = store::current_steps(catalog, checklist.id).await?.len();
        let last = store::last_run(catalog, checklist.id).await?;
        let mut item = checklist.to_json();
        item["steps"] = json!(steps);
        item["last_status"] = last
            .as_ref()
            .map_or(Json::Null, |r| json!(r.status.as_str()));
        listed.push(item);
    }
    Ok(json!({ "checklists": listed }))
}

async fn save_checklist(args: &Json, cx: &ToolCall<'_>) -> Result<Json> {
    let obj = args_of(
        args,
        &["name", "description", "application", "run_as", "steps"],
    )?;
    let mut input = ChecklistInput::new(required(&obj, "name")?);
    input.description = sc_api::mcp::optional_string(&obj, "description")?.unwrap_or_default();
    input.application =
        application_id(cx, sc_api::mcp::optional_string(&obj, "application")?).await?;
    input.run_as = user_id(cx, sc_api::mcp::optional_string(&obj, "run_as")?.as_deref()).await?;
    let steps = drafts(obj.get("steps"))?;
    let checklist = edit::create_checklist(
        cx.env.catalog,
        cx.env.registry,
        &cx.editor,
        input,
        steps,
    )
    .await?;
    let steps = store::current_steps(cx.env.catalog, checklist.id).await?;
    let mut out = checklist.to_json();
    out["steps"] = json!(steps.iter().map(|s| s.to_json()).collect::<Vec<_>>());
    Ok(out)
}

async fn append_steps(args: &Json, cx: &ToolCall<'_>) -> Result<Json> {
    let obj = args_of(args, &["checklist", "steps"])?;
    let checklist = store::find_checklist(cx.env.catalog, &required(&obj, "checklist")?).await?;
    let steps = drafts(obj.get("steps"))?;
    if steps.is_empty() {
        return Err(Error::invalid("`steps` is empty: give at least one step"));
    }
    let written = edit::append_steps(
        cx.env.catalog,
        cx.env.registry,
        &cx.editor,
        checklist.id,
        steps,
    )
    .await?;
    Ok(json!({
        "checklist": checklist.name,
        "appended": written.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
    }))
}

async fn propose_step_change(args: &Json, cx: &ToolCall<'_>) -> Result<Json> {
    let obj = args_of(
        args,
        &["checklist", "change", "step", "new_step", "order", "run_as", "reason"],
    )?;
    let catalog = cx.env.catalog;
    let checklist = store::find_checklist(catalog, &required(&obj, "checklist")?).await?;
    let reason = sc_api::mcp::optional_string(&obj, "reason")?.unwrap_or_default();
    let step = || -> Result<Uuid> {
        let id = required(&obj, "step")?;
        id.parse()
            .map_err(|_| Error::invalid(format!("`step` should be a step id, got `{id}`")))
    };
    let new_step = || -> Result<StepDraft> {
        StepDraft::from_json(obj.get("new_step").unwrap_or(&Json::Null))
            .map_err(|e| Error::invalid(format!("`new_step`: {e}")))
    };
    let kind = required(&obj, "change")?;
    let change = match kind.as_str() {
        "remove" => {
            let id = step()?;
            edit::remove_step(catalog, cx.env.registry, &cx.editor, id).await?;
            return Ok(json!({ "applied": true, "removed": id.to_string() }));
        }
        "edit" => ProposalChange::Edit {
            step: step()?,
            draft: new_step()?,
        },
        "insert_before" => ProposalChange::InsertBefore {
            before: step()?,
            draft: new_step()?,
        },
        "reorder" => ProposalChange::Reorder {
            order: crate::model::parse_ids(obj.get("order").unwrap_or(&Json::Null), "order")?,
        },
        "run_as" => ProposalChange::RunAs {
            user: user_id(cx, obj.get("run_as").and_then(Json::as_str)).await?,
        },
        other => {
            return Err(Error::invalid(format!(
                "`change` should be edit, insert_before, remove, reorder or run_as, not `{other}`"
            )));
        }
    };
    match edit::request_change(
        catalog,
        cx.env.registry,
        &cx.editor,
        checklist.id,
        change,
        &reason,
    )
    .await?
    {
        Change::Applied => Ok(json!({
            "applied": true,
            "checklist": checklist.name,
        })),
        Change::Proposed(proposal) => Ok(json!({
            "applied": false,
            "proposal": proposal.to_json(),
            "note": "Stored as a proposal for a person to accept or reject; until then the \
                     current step keeps running.",
        })),
    }
}

async fn run(args: &Json, cx: &ToolCall<'_>) -> Result<Json> {
    let obj = args_of(args, &["application", "checklist"])?;
    let trigger = cx.editor.author().to_text();
    let runs = match sc_api::mcp::optional_string(&obj, "checklist")? {
        Some(name) => {
            let checklist = store::find_checklist(cx.env.catalog, &name).await?;
            vec![run_checklist(&cx.env, &checklist, &trigger).await?]
        }
        None => {
            let scope =
                match application_id(cx, sc_api::mcp::optional_string(&obj, "application")?)
                    .await?
                {
                    Some(app) => ChecklistScope::Application(app),
                    None => ChecklistScope::All,
                };
            run_checklists(&cx.env, scope, &trigger).await?
        }
    };
    Ok(json!({ "runs": runs.iter().map(summary).collect::<Vec<_>>() }))
}

/// A run as a model reads it: the status, and what did not pass.
pub fn summary(run: &ChecklistRun) -> Json {
    json!({
        "checklist": run.checklist_name,
        "status": run.status.as_str(),
        "message": run.message,
        "steps": run.steps.iter().map(|s| json!({
            "step": s.step.to_string(),
            "label": s.label,
            "type": s.type_,
            "outcome": s.outcome.as_str(),
            "message": if s.outcome == Outcome::Pass { Json::Null } else { json!(s.message) },
        })).collect::<Vec<_>>(),
        "cleanup_errors": run.cleanup_errors,
    })
}

//! `admin_copilot` — the agent that builds the application (§11.3, §13.6).
//!
//! The first **app-building** trait, and a deliberate revision of the boundary
//! the earlier traits drew. Everything before it reaches *rows*; this one reaches
//! the **catalog**, the **trigger set** and an application's **custom SQL
//! queries**: asked to "create the database schema for a law firm's ERP system"
//! it creates the connected tables and their fields in one act; asked to "email
//! the client when a matter closes" it writes the trigger that does it; asked for
//! "an endpoint that returns each fee earner's billed hours" it writes the SQL and
//! the database types the answer. It edits what is already there — including,
//! under its own grant, the access rules of §7.3 — deletes what it is granted to
//! delete, and answers questions about any of the three without ever seeing a
//! row.
//!
//! ## What is here, and what is not
//!
//! **The tools themselves are not here.** They are
//! [`sc_api::mcp`]'s — assembled into one [`ToolSet`] by
//! [`sc_app::mcp::tool_set`] — because this agent is no longer their only
//! caller: the administration MCP server (§13.6) offers the same ten tools to
//! an external coding agent, under a token's grants instead of an agent's
//! checkboxes. Two callers over one implementation, rather than two
//! implementations that check grants slightly differently and drift within a
//! release — the argument [`sc_api::schema_edit`]'s module comment makes for the
//! *operation*, applied to the *tool*.
//!
//! What is left here is what an `AgentTrait` is: the name and description the
//! admin picks it by, the configuration form, the validation of that form, and
//! the translation from a run's [`TraitContext`] to a
//! [`ToolContext`](sc_api::mcp::ToolContext). Everything else delegates.
//!
//! Three things still make it unlike every other built-in, each stated here
//! because each is a rule broken on purpose:
//!
//! - **It names no table in its configuration.** Every other trait does, because
//!   "which tables may this agent see?" must be answerable off the agent's
//!   definition. This one *cannot*: the tables it makes do not exist when it is
//!   configured. So it is scoped by **what it may do** rather than by what it may
//!   reach — four checkboxes ([`CFG_ALLOW_CREATE`] and its siblings) — and that
//!   difference is the phase's most load-bearing deviation.
//! - **Its grants are configuration rather than separate traits.** `insert_row`,
//!   `update_rows` and `delete_rows` are three traits over one table; these four
//!   are booleans on one trait, because the operations share a **batch**:
//!   creating `matters` with a key to an existing `clients` is a create *and* an
//!   edit, and a batch that half-applied for want of a grant is the state the
//!   transaction exists to avoid. A batch containing an ungranted operation is
//!   refused **whole**, naming the operation and the checkbox that would allow it.
//! - **The caller must be an admin.** Every other trait leans on §7.3 to decide
//!   what a caller may see; a schema has no ownership formula to fall back on,
//!   and the admin API guards every catalog endpoint with `admin()`. So every
//!   tool refuses a run whose [`RunCaller`](sc_agent::RunCaller) is not role 1 —
//!   otherwise an agent exposed to a role-80 user through a chat view would hand
//!   them the table editor. The check lives with the tools, because it must mean
//!   the same thing for the MCP caller.
//!
//! ## The four grants, over all three parts
//!
//! The same four checkboxes scope the schema tools, the trigger tools and the
//! application tools, and they are read the same way in each: creating a table, a
//! trigger and a custom SQL query are all [`CFG_ALLOW_CREATE`]; deleting a trigger
//! or a query is [`CFG_ALLOW_DROP`] alongside dropping a table; and a trigger's
//! `min_role` — which decides who may `POST /actions/{name}` — and a query's,
//! which decides who may call its endpoint, are **access rules**, so they need
//! [`CFG_ALLOW_ACCESS`] exactly as a table's role floors do. A second and a third
//! set of checkboxes would have been eight more decisions for the admin to make,
//! on the same question, with the same right answers.
//!
//! ## The two areas, which are the other question
//!
//! A grant says *what this agent may do*; [`CFG_ALLOW_TRIGGERS`] and
//! [`CFG_ALLOW_APPLICATIONS`] say *to which of the three it may do it*. Both
//! default on, and an area that is off takes its tools out of the model's list
//! rather than leaving them to be refused — the opposite of how a grant behaves,
//! deliberately. A model that may not drop a table still has to be able to say
//! that dropping one is what the admin asked for; an area that is off is not part
//! of this agent's job at all, and a tool the model can see is a tool it will try.
//!
//! The schema has no area of its own: it is what the trait *is*, and an
//! `admin_copilot` that may not describe a schema is an agent with no reason to
//! carry the trait.
//!
//! ## The two design decisions the tools embody
//!
//! Both are recorded where the tools now live, and named here because this is
//! where an admin reads about the agent:
//!
//! - [`edit_schema`](TOOL_EDIT) takes an **ordered list of operations** and
//!   [`save_trigger`](TOOL_SAVE_TRIGGER) takes **one trigger**, because a schema
//!   is a set of *connected* tables and two triggers are two independent rows.
//! - An action's settings arrive through
//!   [`describe_action`](TOOL_DESCRIBE_ACTION) — **progressive disclosure inside
//!   the one loop** — rather than through the nested inference call Saltcorn 1
//!   used, so the parameters are decided by the model that can see the
//!   conversation and a refusal reaches the model that can fix it.

//!
//! ## Building an application from a sentence
//!
//! "Build me a to-do list" is a request this agent can carry to a working first
//! draft without anybody opening a form, and [`BUILD_PLAYBOOK`] is the order it
//! is told to do it in. `create_application` and `update_application` are
//! shared tools ([`sc_app::mcp`]); what is this trait's own is what only an
//! *agent* can do:
//!
//! - [`delegate_to_coding_agent`](TOOL_DELEGATE) hands the code to a **coding
//!   agent** as a sub-agent — the application's builder, which
//!   `create_application` just created, or any other agent with a `coding`
//!   trait. Named per call rather than per configuration (the `subagent`
//!   trait's shape), because the builder this agent wants usually did not
//!   exist when this agent was configured. The session header lists the coding
//!   agents there are.
//! - [`publish_application`](TOOL_PUBLISH) builds the application and serves
//!   it on its subdomain — the admin's Build button, by subdomain.

use sc_agent::DEFAULT_MAX_DEPTH;
use sc_agent::{AgentTrait, SessionContext, ToolsContext, TraitCheck, TraitContext};
use sc_api::mcp::{
    Area, Areas, ToolContext, ToolSet, arguments, optional_string, require_admin, require_grant,
};
use sc_api::schema_edit::{self, Grants};
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Value as Json, json};

pub use sc_api::mcp::{
    TOOL_DELETE_TRIGGER, TOOL_DESCRIBE, TOOL_DESCRIBE_ACTION, TOOL_DESCRIBE_CODE_API,
    TOOL_DESCRIBE_TRIGGERS, TOOL_EDIT, TOOL_SAVE_TRIGGER,
};
pub use sc_app::mcp::{
    TOOL_CREATE_APP, TOOL_CREATE_STORE, TOOL_DELETE_QUERY, TOOL_DESCRIBE_APPS,
    TOOL_DESCRIBE_TRANSLATIONS, TOOL_SAVE_QUERY, TOOL_SAVE_TRANSLATIONS, TOOL_UPDATE_APP,
};

/// Hands a task to a coding agent as a sub-agent.
pub const TOOL_DELEGATE: &str = "delegate_to_coding_agent";
/// Builds an application and serves it on its subdomain.
pub const TOOL_PUBLISH: &str = "publish_application";

/// The trait a coding agent carries — what makes an agent one this agent can
/// delegate code to.
const CODING_TRAIT: &str = sc_app::TRAIT_CODING;

/// The step budget a delegation gets when the model names none, and the most
/// it may ask for. A first draft of an application is a long task; the
/// builder's own budget was chosen for a chat with a person, one feature at a
/// time.
const DEFAULT_DELEGATION_STEPS: u32 = 150;
const MAX_DELEGATION_STEPS: u32 = 400;

/// How this agent is told to build an application from a sentence.
///
/// In the prompt rather than in a tool description because it is an *order*
/// across six tools, and the model reads a tool's description only when it is
/// already thinking about that tool.
pub const BUILD_PLAYBOOK: &str = "## Building an application\n\n\
When the person asks you to build an application, a site or a tool (\"build me a to-do \
list\", \"make an app for booking rooms\"), build the whole first working draft yourself, \
without asking them anything you can decide:\n\n\
1. `describe_applications` and `describe_schema`, to see what already exists.\n\
2. `create_application`. Unless they named another technology, it is a React \
application in a new local file store — do not ask. Only if they asked for the code to \
live in a git repository, first `create_file_store` with `backend: \"git\"` and pass its \
name as `file_store`.\n\
3. Create every table the application needs with **one** `edit_schema` batch: fields, \
types, required flags, keys between tables. Tables come before code, because the \
application's typed client is generated from them.\n\
4. `update_application` with `tables: { add: [...] }` to connect those tables to the \
application.\n\
5. `delegate_to_coding_agent` with the application's builder agent (the \
`builder_agent` that `create_application` returned): give it the full specification — \
what the app is for, every page and what it shows, the tables and fields it reads and \
writes, who signs in — and ask it to implement it in the scaffolded project, using the \
generated client in `src/feldspar/`, and to check that it builds. Ask it to report what \
it built and anything it could not do.\n\
6. `publish_application`, so it is served on its subdomain. If the build fails, \
delegate the diagnostics back to the builder agent to fix, then publish again.\n\
7. Tell the person the address and what the draft does.\n\n\
To change an existing application's code later, delegate to its builder agent the same \
way; the session header lists the coding agents there are.";

/// May create tables, fields and triggers.
pub const CFG_ALLOW_CREATE: &str = schema_edit::GRANT_CREATE;
/// May change what is already there.
pub const CFG_ALLOW_EDIT: &str = schema_edit::GRANT_EDIT;
/// May drop tables and fields, and delete triggers. Off by default.
pub const CFG_ALLOW_DROP: &str = schema_edit::GRANT_DROP;
/// May write the access rules of §7.3, and a trigger's `min_role`. Off by
/// default, and above `allow_drop` — a drop announces itself and a widened role
/// floor does not.
pub const CFG_ALLOW_ACCESS: &str = schema_edit::GRANT_ACCESS_CHANGES;

/// Whether the trigger half is offered at all. On by default.
///
/// The first of the two **area** checkboxes, and a different kind of setting
/// from the four grants above it: a grant says what this agent may *do*, an area
/// says which of the three things it may do it *to*. They compose — an agent with
/// the triggers area on and `allow_drop` off can write a trigger and cannot
/// delete one — and an area that is off removes its tools from the model's list
/// entirely rather than leaving them there to be refused. A tool the model can
/// see is a tool it will try, and a conversation spent discovering what an agent
/// is not for is a conversation the admin pays for.
pub const CFG_ALLOW_TRIGGERS: &str = sc_api::mcp::Area::Triggers.key();
/// Whether the application half — an application's custom SQL queries — is
/// offered at all. On by default, and read exactly as [`CFG_ALLOW_TRIGGERS`] is.
pub const CFG_ALLOW_APPLICATIONS: &str = sc_api::mcp::Area::Applications.key();

/// Build and inspect the schema and the triggers over it.
pub struct AdminCopilot;

/// Every tool this trait can offer, under fixed names — the schema's two, the
/// triggers' four, the code API's reference, the applications' six, the
/// translations' two and its own two.
///
/// *Can*, not *does*: the two area checkboxes decide whether the trigger and
/// application halves are offered at all, so a configured instance offers a
/// subset of these. This is the whole set, which is what the admin UI's "what
/// will this be called?" and §11.2's collision check want — a name that any
/// configuration could produce is a name that could collide.
pub fn tool_names() -> [&'static str; 17] {
    [
        TOOL_DESCRIBE,
        TOOL_EDIT,
        TOOL_DESCRIBE_TRIGGERS,
        TOOL_DESCRIBE_ACTION,
        TOOL_SAVE_TRIGGER,
        TOOL_DELETE_TRIGGER,
        TOOL_DESCRIBE_CODE_API,
        TOOL_DESCRIBE_APPS,
        TOOL_CREATE_STORE,
        TOOL_CREATE_APP,
        TOOL_UPDATE_APP,
        TOOL_DESCRIBE_TRANSLATIONS,
        TOOL_SAVE_TRANSLATIONS,
        TOOL_SAVE_QUERY,
        TOOL_DELETE_QUERY,
        TOOL_DELEGATE,
        TOOL_PUBLISH,
    ]
}

#[async_trait::async_trait]
impl AgentTrait for AdminCopilot {
    fn name(&self) -> &str {
        "admin_copilot"
    }

    fn description(&self) -> &str {
        "Build the application: create applications and their file stores, \
         create, alter and drop tables and fields, configure the actions that \
         run when something happens, write the custom SQL queries an \
         application serves as API endpoints, and hand the code to coding \
         agents"
    }

    fn config_spec(&self) -> Vec<FormField> {
        vec![
            FormField::new(CFG_ALLOW_CREATE, BasicType::Bool)
                .label("May create tables, fields, triggers and custom SQL queries")
                .default_value(true),
            FormField::new(CFG_ALLOW_EDIT, BasicType::Bool)
                .label("May change existing tables, fields, triggers and custom SQL queries")
                .default_value(true),
            FormField::new(CFG_ALLOW_DROP, BasicType::Bool)
                .label("May drop tables and fields, and delete triggers and custom SQL queries")
                .default_value(false),
            FormField::new(CFG_ALLOW_ACCESS, BasicType::Bool)
                .label(
                    "May change access rules (roles, ownership formula, row-level \
                     security, a trigger's minimum role, who may call a custom \
                     SQL query)",
                )
                .default_value(false),
            FormField::new(CFG_ALLOW_TRIGGERS, BasicType::Bool)
                .label("May work on triggers")
                .default_value(true),
            FormField::new(CFG_ALLOW_APPLICATIONS, BasicType::Bool)
                .label(
                    "May work on applications: create them, connect their tables, \
                     write their custom SQL queries, build them and delegate their \
                     code to coding agents",
                )
                .default_value(true),
        ]
    }

    /// There is no table to resolve, so there is little here the spec cannot
    /// already say — which is itself the deviation §11.3 records. What is worth
    /// stating is that a configuration granting nothing is *not* an error: the
    /// grants bound the **writing** tools only, and an agent with none of them is
    /// a read-only describer of the schema and the triggers, which is a thing an
    /// admin may deliberately want.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        // The same six keys asked the same question a token's grants are asked
        // (§13.6): one validator, because there is one vocabulary.
        sc_api::mcp::validate_flags(check.config)
    }

    /// The schema's two tools always, and each other half's only where its area
    /// checkbox is on — which is [`ToolSet::specs`]'s rule, not one this trait
    /// applies on top of it.
    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let mut specs = tool_set(config).specs(cx.catalog);
        if areas(config).has(Area::Applications) {
            specs.push(delegate_spec());
            specs.push(publish_spec());
        }
        specs
    }

    /// The build playbook, where this agent may work on applications at all.
    fn prompt(&self, _cx: &ToolsContext<'_>, config: &Attrs) -> Option<String> {
        areas(config)
            .has(Area::Applications)
            .then(|| BUILD_PLAYBOOK.to_owned())
    }

    /// The coding agents there are, so the model can delegate to one it did not
    /// create in this conversation.
    async fn session_header(
        &self,
        config: &Attrs,
        cx: &mut SessionContext<'_>,
    ) -> Result<Option<String>> {
        if !areas(config).has(Area::Applications) {
            return Ok(None);
        }
        let agents = coding_agents(cx.catalog).await?;
        Ok(Some(match agents.is_empty() {
            true => "There are no coding agents yet. `create_application` creates \
                     one for each application it creates."
                .to_owned(),
            false => format!(
                "The coding agents you can delegate code to with \
                 `{TOOL_DELEGATE}`:\n{}",
                agents
                    .iter()
                    .map(|(name, app)| match app {
                        Some(app) => format!("- `{name}` (builds the application `{app}`)"),
                        None => format!("- `{name}`"),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        }))
    }

    async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        if tool == TOOL_DELEGATE || tool == TOOL_PUBLISH {
            require_admin(ctx.caller.role, tool)?;
            if !areas(config).has(Area::Applications) {
                return Err(Error::invalid(format!(
                    "`{tool}` is switched off for this agent; turn on `{}` in its \
                     `admin_copilot` settings to offer it.",
                    Area::Applications.key()
                )));
            }
            require_grant(
                grants(config).edit,
                match tool {
                    TOOL_DELEGATE => "hand an application's code to a coding agent",
                    _ => "build an application",
                },
                CFG_ALLOW_EDIT,
            )?;
            return match tool {
                TOOL_DELEGATE => delegate_to_coding_agent(args, ctx).await,
                _ => publish_application(args, ctx).await,
            };
        }
        let tools = tool_set(config);
        tools.call(tool, args, &tool_context(ctx)).await
    }
}

// --- delegate_to_coding_agent -------------------------------------------------

const ARG_AGENT: &str = "agent";
const ARG_APPLICATION: &str = "application";
const ARG_MAX_STEPS: &str = "max_steps";

fn delegate_spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_DELEGATE,
        format!(
            "Hand a coding task to a **coding agent** — an agent that reads, \
             writes and builds an application's source — and wait for its report. \
             This is how you write an application's code: name the application \
             (its builder agent is used) or the agent. It cannot see this \
             conversation, only what you send, so `{}` must be a complete \
             specification: the pages, what each shows and does, the tables and \
             fields it uses through the generated client in `src/feldspar/`, who \
             signs in. It works on its own, checks its work builds, and returns \
             one report. Create and connect the tables first: it cannot change \
             the schema.",
            crate::ARG_TASK
        ),
        json!({
            "type": "object",
            "properties": {
                ARG_APPLICATION: {
                    "type": "string",
                    "description":
                        "The application whose builder agent to use, by subdomain.",
                },
                ARG_AGENT: {
                    "type": "string",
                    "description":
                        "A coding agent by name, instead of an application's builder.",
                },
                crate::ARG_TASK: {
                    "type": "string",
                    "description":
                        "What to build or change, in full, as an instruction to \
                         someone who has read nothing above.",
                },
                crate::ARG_CONTEXT: {
                    "type": "string",
                    "description":
                        "What it cannot see for itself: the tables and fields you \
                         created, the application's subdomain, decisions already \
                         made.",
                },
                crate::ARG_OUTPUT: {
                    "type": "string",
                    "description": "What its final report must contain.",
                },
                ARG_MAX_STEPS: {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_DELEGATION_STEPS,
                    "description": format!(
                        "Its step budget for this task; {DEFAULT_DELEGATION_STEPS} \
                         when omitted, which suits a first draft."
                    ),
                },
            },
            "required": [crate::ARG_TASK],
            "additionalProperties": false,
        }),
    )
}

async fn delegate_to_coding_agent(args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let obj = arguments(
        args,
        &[
            ARG_AGENT,
            ARG_APPLICATION,
            crate::ARG_TASK,
            crate::ARG_CONTEXT,
            crate::ARG_OUTPUT,
            ARG_MAX_STEPS,
        ],
    )?;
    let named = |key: &str| -> Result<Option<String>> {
        Ok(optional_string(&obj, key)?
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()))
    };
    let agents = coding_agents(ctx.catalog).await?;
    let agent = match (named(ARG_AGENT)?, named(ARG_APPLICATION)?) {
        (Some(agent), _) => agent,
        (None, Some(app)) => agents
            .iter()
            .find(|(_, builds)| builds.as_deref() == Some(app.as_str()))
            .map(|(name, _)| name.clone())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "no coding agent builds the application `{app}`. {}",
                    the_coding_agents(&agents)
                ))
            })?,
        (None, None) => {
            return Err(Error::invalid(format!(
                "name the `{ARG_APPLICATION}` whose builder should do this, or the \
                 `{ARG_AGENT}`. {}",
                the_coding_agents(&agents)
            )));
        }
    };
    if !agents.iter().any(|(name, _)| *name == agent) {
        return Err(Error::invalid(format!(
            "`{agent}` is not a coding agent. {}",
            the_coding_agents(&agents)
        )));
    }
    let steps = match obj.get(ARG_MAX_STEPS) {
        None | Some(Json::Null) => DEFAULT_DELEGATION_STEPS,
        Some(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (1..=MAX_DELEGATION_STEPS).contains(n))
            .ok_or_else(|| {
                Error::invalid(format!(
                    "`{ARG_MAX_STEPS}` should be a number from 1 to {MAX_DELEGATION_STEPS}"
                ))
            })?,
    };
    let briefing = json!({
        crate::ARG_TASK: obj.get(crate::ARG_TASK),
        crate::ARG_CONTEXT: obj.get(crate::ARG_CONTEXT),
        crate::ARG_OUTPUT: obj.get(crate::ARG_OUTPUT),
    });
    crate::subagent::delegate(ctx, &agent, &briefing, Some(steps), DEFAULT_MAX_DEPTH).await
}

/// Every agent with a `coding` trait, by name, with the application it builds
/// where its trait names one.
async fn coding_agents(catalog: &sc_catalog::Catalog) -> Result<Vec<(String, Option<String>)>> {
    if catalog.get(sc_agent::AGENTS_TABLE)?.is_none() {
        return Ok(Vec::new());
    }
    Ok(sc_agent::list_agents(catalog)
        .await?
        .into_iter()
        .filter_map(|agent| {
            let coding = agent.traits.iter().find(|t| t.trait_ == CODING_TRAIT)?;
            let app = coding
                .config
                .get(sc_app::TRAIT_CFG_APPLICATION)
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            Some((agent.name, app))
        })
        .collect())
}

fn the_coding_agents(agents: &[(String, Option<String>)]) -> String {
    match agents.is_empty() {
        true => format!(
            "There are no coding agents; `{TOOL_CREATE_APP}` creates one with each \
             application."
        ),
        false => format!(
            "The coding agents are {}.",
            agents
                .iter()
                .map(|(name, app)| match app {
                    Some(app) => format!("`{name}` (application `{app}`)"),
                    None => format!("`{name}`"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

// --- publish_application ------------------------------------------------------

fn publish_spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_PUBLISH,
        "Build an application and serve it on its subdomain — the admin's Build \
         button. Do this when its code is written, and again after its tables \
         or queries change. A build that fails is a result, not a refusal: \
         `built` is false and `diagnostics` lists each error's file, line and \
         message — hand those to its coding agent to fix. The previous version \
         keeps serving until a build succeeds.",
        json!({
            "type": "object",
            "properties": {
                ARG_APPLICATION: {
                    "type": "string",
                    "description": "The application, by subdomain.",
                },
            },
            "required": [ARG_APPLICATION],
            "additionalProperties": false,
        }),
    )
}

async fn publish_application(args: &Json, ctx: &mut TraitContext<'_>) -> Result<Json> {
    let obj = arguments(args, &[ARG_APPLICATION])?;
    let subdomain = optional_string(&obj, ARG_APPLICATION)?
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{ARG_APPLICATION}` is required")))?;
    let app = sc_app::load_application_by_subdomain(ctx.catalog, &subdomain)
        .await?
        .ok_or_else(|| Error::not_found(format!("no application is served at `{subdomain}`")))?;
    let host = ctx.catalog.admin_host().ok_or_else(|| {
        Error::config(format!(
            "`{TOOL_PUBLISH}` needs the running server, and agent `{}` is not \
             connected to one",
            ctx.agent
        ))
    })?;
    let call = sc_catalog::AdminCall::new("buildApplication", Json::Null)
        .param("id", app.id.0.to_string())
        .user(ctx.caller.user.as_ref().map(|u| u.id));
    // A failed build is news about the application, so it is a result.
    Ok(match host.call_admin(call).await {
        Ok(mut body) => {
            let log = body
                .get("log")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned();
            body["application"] = json!(subdomain);
            body["diagnostics"] = json!(sc_app::build_diagnostics(&log));
            body
        }
        Err(e) => {
            let log = e.to_string();
            json!({
                "application": subdomain,
                "built": false,
                "log": log,
                "diagnostics": sc_app::build_diagnostics(&log),
            })
        }
    })
}

/// The ten tools under this agent's configuration.
///
/// The whole of what configuring this trait *means*: six checkboxes become a
/// [`Grants`] and an [`Areas`], and the set does the rest. A token minted for
/// the MCP server builds the same value from the same six flags (§13.6), which
/// is why there is nothing else in this function to keep in step.
fn tool_set(config: &Attrs) -> ToolSet {
    sc_app::mcp::tool_set(grants(config), areas(config))
}

/// A run's context, narrowed to what an administrative tool uses.
///
/// The run id, the delegator and the JavaScript evaluator are not carried,
/// because none of these tools touches a row: they work in the catalog, the
/// trigger set and the application store.
fn tool_context<'a>(ctx: &'a TraitContext<'a>) -> ToolContext<'a> {
    ToolContext {
        catalog: ctx.catalog,
        user: ctx.caller.user.as_ref(),
        role: ctx.caller.role,
        triggers: ctx.triggers,
        actor: ctx.agent,
    }
}

/// The four grants as configured; an absent checkbox reads as its default.
///
/// A rename of `sc_api::mcp`'s reader rather than a second one: an agent's six
/// checkboxes and a token's six flags are the same six flags, so the defaults
/// they fall back to have to be the same defaults (§13.6).
fn grants(config: &Attrs) -> Grants {
    sc_api::mcp::grants_from_attrs(config)
}

/// The two areas as configured; an absent checkbox reads as on.
fn areas(config: &Attrs) -> Areas {
    sc_api::mcp::areas_from_attrs(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(pairs: &[(&str, bool)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), json!(v)))
            .collect()
    }

    #[test]
    fn dropping_and_access_changes_are_off_unless_asked_for() {
        // An empty configuration is the safe one: it can build, it cannot
        // destroy, and it cannot widen anybody's access.
        let g = grants(&Attrs::new());
        assert!(g.create && g.edit);
        assert!(!g.drop && !g.access_changes);

        let g = grants(&config(&[(CFG_ALLOW_DROP, true), (CFG_ALLOW_ACCESS, true)]));
        assert!(g.drop && g.access_changes);

        let g = grants(&config(&[
            (CFG_ALLOW_CREATE, false),
            (CFG_ALLOW_EDIT, false),
        ]));
        assert!(!g.create && !g.edit);
    }

    #[test]
    fn both_areas_are_on_unless_switched_off() {
        assert_eq!(areas(&Attrs::new()), Areas::all());
        let a = areas(&config(&[
            (CFG_ALLOW_TRIGGERS, false),
            (CFG_ALLOW_APPLICATIONS, false),
        ]));
        assert_eq!(a, Areas::none());
    }

    /// The set this agent builds is the whole shared surface, in the order this
    /// crate has always published — the schema's two, the triggers' four, the
    /// code-body reference, the applications' six — and this trait's own two
    /// come after it.
    #[test]
    fn the_configured_set_and_the_own_tools_are_the_names_this_trait_publishes() {
        let set = tool_set(&Attrs::new());
        let mut names = set.all_names();
        names.extend([TOOL_DELEGATE, TOOL_PUBLISH]);
        assert_eq!(names, tool_names().to_vec());
        assert_eq!(*set.grants(), grants(&Attrs::new()));
    }

    #[test]
    fn the_playbook_builds_tables_before_code_and_defaults_to_react() {
        let tables = BUILD_PLAYBOOK.find("edit_schema").unwrap();
        let connect = BUILD_PLAYBOOK.find("update_application").unwrap();
        let code = BUILD_PLAYBOOK.find(TOOL_DELEGATE).unwrap();
        assert!(tables < connect && connect < code, "{BUILD_PLAYBOOK}");
        assert!(BUILD_PLAYBOOK.contains("React"), "{BUILD_PLAYBOOK}");
        assert!(BUILD_PLAYBOOK.contains("do not ask"), "{BUILD_PLAYBOOK}");
    }

    /// An area that is off removes its tools rather than leaving them to be
    /// refused — the rule stated in this module's docs, asserted through the
    /// value the trait actually builds.
    #[test]
    fn a_switched_off_area_takes_its_tools_out_of_the_offer() {
        let set = tool_set(&config(&[(CFG_ALLOW_TRIGGERS, false)]));
        assert!(!set.offers(TOOL_SAVE_TRIGGER));
        assert!(set.offers(TOOL_DESCRIBE) && set.offers(TOOL_SAVE_QUERY));

        let set = tool_set(&config(&[(CFG_ALLOW_APPLICATIONS, false)]));
        assert!(!set.offers(TOOL_SAVE_QUERY));
        assert!(set.offers(TOOL_SAVE_TRIGGER));
    }
}

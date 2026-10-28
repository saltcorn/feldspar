//! The administrative tool surface: one implementation, two callers (§13.6).
//!
//! These are the tools that build an application's **configuration half** — the
//! tables and their fields, the access rules of §7.3, the triggers over them and
//! the custom SQL queries an application serves. They were written for the
//! built-in `admin_copilot` agent and they live here because they have a second
//! caller: the **administration MCP server**, which projects the same tools to
//! an external coding agent over HTTP.
//!
//! Two callers, one implementation. The alternative — a copy of `edit_schema`
//! for the chat copilot and another for MCP — is the failure
//! [`crate::schema_edit`]'s module comment was written to prevent,
//! moved up one level: two bodies that check grants slightly differently and
//! drift within a release. So the bodies live at the lowest layer that can hold
//! them, and the copilot — `sc-core-traits`'s `admin_copilot` — and the MCP
//! server are both thin things over a [`ToolSet`].
//!
//! ## What a caller supplies, and what it does not
//!
//! A [`ToolSet`] is parameterised by **grants** and **areas** — never by *who*
//! the caller is. The copilot passes an agent's configuration checkboxes and the
//! MCP server passes a token's; neither knows the other exists, and the six
//! flags mean the same thing in both because they *are* the same six flags
//! ([`Grants`] plus [`Areas`]).
//!
//! Who the caller is arrives per call, on a [`ToolContext`]: the catalog to work
//! in, the role the call is authorized at, the user it runs as where there is
//! one, and the trigger dispatcher where the process has one. Every tool here
//! refuses a caller below role 1, because a schema has no ownership formula to
//! fall back on and the admin API guards every catalog endpoint with `admin()`.
//!
//! ## Grants refuse, areas disappear
//!
//! A **grant** — create, edit, drop, access changes — is checked when the tool
//! runs and refused in words the model can relay, naming the checkbox that would
//! allow it. The tool stays *visible*, because a model that may not drop a table
//! still has to be able to say that dropping one is what the admin asked for.
//!
//! An **area** — the triggers half, the applications half — is a statement that
//! this whole part of the job is not this caller's, so its tools are taken out
//! of the listing entirely. A tool the model can see is a tool it will try, and
//! a turn spent discovering what a caller is not for is a turn somebody pays
//! for. [`ToolSet::call`] still refuses a switched-off area by name, because
//! `call` is an entry point that takes a string and a switched-off area must mean
//! the same thing however the string arrived.
//!
//! ## Why the set is a list of [`AdminTool`]s
//!
//! Seven of the ten tools are implemented here; the three that reach an
//! `Application` cannot be, because `sc-app` is layer 8 *above* this crate and
//! the storage they read lives there. So a tool is a
//! trait object and a set is a list of them, which is also what phase 4's
//! generated tools want: a tool projected from a tagged [`Endpoint`](crate::Endpoint)
//! dispatches through the handler registry, which only the server holds.
//!
//! The full ten-tool set is therefore assembled by `sc_app::mcp::tool_set`, the
//! lowest layer that can name every tool in it.

mod code_api;
mod endpoint_tool;
mod json_schema;
mod schema;
mod triggers;

use std::sync::Arc;

use sc_action::TriggerDispatcher;
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_llm::ToolSpec;
use sc_types::Attrs;
use serde_json::{Map, Value as Json};

use crate::schema_edit::{self, Grants};

pub use code_api::{JS_CODE_API, TOOL_DESCRIBE_CODE_API};
pub use endpoint_tool::{BODY_KEY, ProjectedCall, Projection, check_caller};
pub use json_schema::{json_schema, object_schema, scalar_schema};
pub use schema::{TOOL_DESCRIBE, TOOL_EDIT};
pub use triggers::{
    TOOL_DELETE_TRIGGER, TOOL_DESCRIBE_ACTION, TOOL_DESCRIBE_TRIGGERS, TOOL_SAVE_TRIGGER,
    setting_json,
};

/// May create tables, fields, triggers and custom SQL queries.
pub const GRANT_CREATE: &str = schema_edit::GRANT_CREATE;
/// May change what is already there.
pub const GRANT_EDIT: &str = schema_edit::GRANT_EDIT;
/// May drop tables and fields, and delete triggers and queries.
pub const GRANT_DROP: &str = schema_edit::GRANT_DROP;
/// May write the access rules of §7.3, a trigger's `min_role` and a query's.
pub const GRANT_ACCESS_CHANGES: &str = schema_edit::GRANT_ACCESS_CHANGES;

/// The key the **checklists grant** is stored under (TODO-checklists C1.13):
/// `none`, `append` (the default) or `full`. Not one of the boolean
/// [`FLAG_KEYS`] — it has three values — but stored beside them, on an
/// `admin_copilot` agent's configuration and on a token's grants alike, so the
/// two callers of the checklist tools read the same answer. `sc-check` gives
/// the values their meaning.
pub const CHECKLISTS_GRANT: &str = "checklists";

/// The values [`CHECKLISTS_GRANT`] may take.
pub const CHECKLISTS_GRANT_VALUES: [&str; 3] = ["none", "append", "full"];

/// The half of the surface a tool belongs to, where it belongs to one.
///
/// An area is not a grant: a grant says what this caller may *do*, an area says
/// which of the three halves it may do it *to*. The schema half has no area,
/// because it is what this surface *is* — a caller that may not describe a schema
/// has no reason to be given these tools at all.
///
/// Serializable because an [`Endpoint`](crate::Endpoint) carries one in its
/// [`McpTag`](crate::McpTag), and an endpoint set is a value that travels — to
/// the client generator, to a stored application, over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    /// The four trigger tools.
    Triggers,
    /// The three tools over an application's custom SQL queries.
    Applications,
    /// Listing and installing **modules** — npm packages and Python
    /// distributions — on this server. Its own area, and the one area that is
    /// **off unless granted**: installing a package runs its install scripts and
    /// its code with the server's own privileges, and a Python distribution has
    /// no permission model at all. So no existing token or agent gains it by
    /// upgrading; somebody has to tick it.
    Modules,
    /// Reading **row history**: a row's versions, deleted rows, a row as it
    /// was, and changesets (§14.12). Off unless granted, like modules: history
    /// is row data, and an agent that may change the schema is not thereby one
    /// that may read customer rows.
    History,
}

impl Area {
    /// The configuration key an admin ticks — the same string in an agent's
    /// `admin_copilot` settings and in a token's grants, because it is the same
    /// question.
    pub const fn key(self) -> &'static str {
        match self {
            Area::Triggers => "allow_triggers",
            Area::Applications => "allow_applications",
            Area::Modules => "allow_modules",
            Area::History => "allow_history",
        }
    }
}

/// One of the four grants, named — so a tool can *declare* what it needs rather
/// than each one re-deriving it from a boolean.
///
/// [`Grants`] is four booleans, which is the right shape for the caller who has
/// them; this is the right shape for the tool that wants one of them. The
/// projection of a tagged [`Endpoint`](crate::Endpoint) is what needed it:
/// `deleteAgent` must require `allow_drop` for the same reason `delete_trigger`
/// does, and the alternative — inferring the grant from the HTTP method — reads
/// `POST` as *create* for `buildApplication`, which creates nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    /// May bring something new into existence.
    Create,
    /// May change something that is already there.
    Edit,
    /// May destroy something.
    Drop,
    /// May write the access rules of §7.3 — who may read, write or call a thing.
    AccessChanges,
}

impl Grant {
    /// The configuration key an admin ticks for it.
    pub const fn key(self) -> &'static str {
        match self {
            Grant::Create => GRANT_CREATE,
            Grant::Edit => GRANT_EDIT,
            Grant::Drop => GRANT_DROP,
            Grant::AccessChanges => GRANT_ACCESS_CHANGES,
        }
    }

    /// Whether this caller has it.
    pub fn allowed_by(self, grants: &Grants) -> bool {
        match self {
            Grant::Create => grants.create,
            Grant::Edit => grants.edit,
            Grant::Drop => grants.drop,
            Grant::AccessChanges => grants.access_changes,
        }
    }
}

/// Which parts of the surface this caller was given. Triggers and applications
/// default on; modules and history default **off** (see [`Area::Modules`],
/// [`Area::History`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Areas {
    /// Whether the trigger tools are offered at all.
    pub triggers: bool,
    /// Whether the application tools are offered at all.
    pub applications: bool,
    /// Whether the module tools are offered at all.
    pub modules: bool,
    /// Whether the row-history tools are offered at all.
    pub history: bool,
}

impl Default for Areas {
    fn default() -> Self {
        Areas {
            triggers: true,
            applications: true,
            modules: false,
            history: false,
        }
    }
}

impl Areas {
    /// What an unconfigured caller gets: triggers and applications. Not
    /// modules or history, which are only ever offered when ticked.
    pub fn all() -> Areas {
        Areas::default()
    }

    /// No area at all: the schema tools alone.
    pub fn none() -> Areas {
        Areas {
            triggers: false,
            applications: false,
            modules: false,
            history: false,
        }
    }

    /// Whether this area is switched on.
    pub fn has(&self, area: Area) -> bool {
        match area {
            Area::Triggers => self.triggers,
            Area::Applications => self.applications,
            Area::Modules => self.modules,
            Area::History => self.history,
        }
    }
}

/// Every key the eight flags are stored under, in the order an admin ticks them.
///
/// One list, because there are two things that write these — an
/// `admin_copilot` agent's configuration and an API token's `grants` column —
/// and a ninth key invented by one of them would be a flag the other silently
/// ignores.
pub const FLAG_KEYS: [&str; 8] = [
    GRANT_CREATE,
    GRANT_EDIT,
    GRANT_DROP,
    GRANT_ACCESS_CHANGES,
    Area::Triggers.key(),
    Area::Applications.key(),
    Area::Modules.key(),
    Area::History.key(),
];

/// The four grants as a stored configuration has them; an absent flag reads as
/// its default.
///
/// Dropping and access changes default **off** and the other two **on**, which
/// is the safe configuration rather than the empty one: a caller given nothing
/// can build and cannot destroy or widen anybody's access.
pub fn grants_from_attrs(config: &Attrs) -> Grants {
    let flag =
        |key: &str, default: bool| config.get(key).and_then(Json::as_bool).unwrap_or(default);
    Grants {
        create: flag(GRANT_CREATE, true),
        edit: flag(GRANT_EDIT, true),
        drop: flag(GRANT_DROP, false),
        access_changes: flag(GRANT_ACCESS_CHANGES, false),
    }
}

/// The four areas as a stored configuration has them; an absent flag reads as
/// its default — on for triggers and applications, off for modules and history.
pub fn areas_from_attrs(config: &Attrs) -> Areas {
    let defaults = Areas::default();
    let flag = |area: Area| {
        config
            .get(area.key())
            .and_then(Json::as_bool)
            .unwrap_or(defaults.has(area))
    };
    Areas {
        triggers: flag(Area::Triggers),
        applications: flag(Area::Applications),
        modules: flag(Area::Modules),
        history: flag(Area::History),
    }
}

/// The eight flags written back out, every key present and explicit.
///
/// What a token's `grants` column is made of. Explicit rather than sparse
/// because the column is the record of *what an admin agreed to* — a missing key
/// that reads as a default today is a key that reads as a different default the
/// day a default changes, and a credential is the wrong place to discover that.
pub fn flags_to_attrs(grants: &Grants, areas: &Areas) -> Attrs {
    let mut out = Attrs::new();
    for (key, value) in [
        (GRANT_CREATE, grants.create),
        (GRANT_EDIT, grants.edit),
        (GRANT_DROP, grants.drop),
        (GRANT_ACCESS_CHANGES, grants.access_changes),
        (Area::Triggers.key(), areas.triggers),
        (Area::Applications.key(), areas.applications),
        (Area::Modules.key(), areas.modules),
        (Area::History.key(), areas.history),
    ] {
        out.insert(key.to_owned(), Json::Bool(value));
    }
    out
}

/// The key a token's `grants` stores to hold **every** flag — the eight of
/// [`FLAG_KEYS`] and any added in a later version.
///
/// A token's own key, never an `admin_copilot` agent's: an agent's
/// configuration keeps the cautious defaults. Stored on the token rather than
/// expanded at the mint, so a long-lived token given "everything" keeps meaning
/// everything after an upgrade adds a flag — the token already acts as the full
/// administrator who minted it, and the flags it was not given are the ones it
/// was explicitly denied. Those stay denied: a flag stored `false` beside
/// `allow_all` is an exception, the `--allow-all --no-allow-drop` of the CLI.
pub const ALLOW_ALL: &str = "allow_all";

/// The flags a freshly minted API token stores, from what the mint submitted:
/// an absent flag is **granted**, and [`ALLOW_ALL`] is written explicitly too.
///
/// A token is minted by an admin, at a screen or a terminal, for an agent they
/// are about to hand their installation to; everything on — modules, drops and
/// access changes included — is what they almost always want, and unticking is
/// one click. An `admin_copilot` agent's configuration keeps the cautious
/// defaults of [`grants_from_attrs`] and [`areas_from_attrs`]; this applies only
/// at a mint, and the result is written explicitly, so a stored token never
/// re-reads itself against either set of defaults — except through
/// [`ALLOW_ALL`], which says on purpose that a flag it has never heard of is on
/// (read back by [`token_flags`]).
pub fn token_flags_from_submitted(submitted: &Attrs) -> Attrs {
    let mut out = Attrs::new();
    for key in FLAG_KEYS {
        let on = submitted.get(key).and_then(Json::as_bool).unwrap_or(true);
        out.insert(key.to_owned(), Json::Bool(on));
    }
    let all = submitted
        .get(ALLOW_ALL)
        .and_then(Json::as_bool)
        .unwrap_or(false);
    out.insert(ALLOW_ALL.to_owned(), Json::Bool(all));
    // The checklists grant is written explicitly too, `append` when the mint
    // said nothing: `full` is never anybody's default, `allow_all` included.
    let checklists = submitted
        .get(CHECKLISTS_GRANT)
        .and_then(Json::as_str)
        .filter(|v| CHECKLISTS_GRANT_VALUES.contains(v))
        .unwrap_or("append");
    out.insert(CHECKLISTS_GRANT.to_owned(), Json::from(checklists));
    out
}

/// A stored token's flags, as the readers ([`grants_from_attrs`],
/// [`areas_from_attrs`]) should see them: with [`ALLOW_ALL`] on, every key of
/// [`FLAG_KEYS`] the row does not mention reads as granted — which is what a
/// flag added after the mint is. A flag the row stores keeps its stored value.
pub fn token_flags(stored: &Attrs) -> Attrs {
    let mut out = stored.clone();
    if stored.get(ALLOW_ALL).and_then(Json::as_bool) == Some(true) {
        for key in FLAG_KEYS {
            out.entry(key.to_owned()).or_insert(Json::Bool(true));
        }
    }
    out
}

/// What a token mint may submit: the flags of [`validate_flags`], plus
/// [`ALLOW_ALL`], which only a token takes.
pub fn validate_token_flags(submitted: &Attrs) -> Result<()> {
    validate_flags(submitted)?;
    match submitted.get(ALLOW_ALL) {
        None | Some(Json::Null) | Some(Json::Bool(_)) => Ok(()),
        Some(other) => Err(Error::invalid(format!(
            "`{ALLOW_ALL}` should be true or false, got {other}"
        ))),
    }
}

/// Refuse a flag that is not `true`, `false` or absent, naming it.
///
/// The validation both writers share: an agent's `validate_config` and a token
/// mint ask the same question of the same six keys.
pub fn validate_flags(config: &Attrs) -> Result<()> {
    match config.get(CHECKLISTS_GRANT) {
        None | Some(Json::Null) => {}
        Some(Json::String(s)) if CHECKLISTS_GRANT_VALUES.contains(&s.as_str()) => {}
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{CHECKLISTS_GRANT}` should be one of none, append or full, got {other}"
            )));
        }
    }
    for key in FLAG_KEYS {
        match config.get(key) {
            None | Some(Json::Null) | Some(Json::Bool(_)) => {}
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`{key}` should be true or false, got {other}"
                )));
            }
        }
    }
    Ok(())
}

/// What one tool call runs against, and as whom.
///
/// The deliberate sibling of `sc_agent::TraitContext` rather than that type
/// itself: an agent's context carries a run id, a delegator and a JavaScript
/// evaluator, none of which an administrative tool touches, and naming it here
/// would put this crate below the agent loop for no gain. What is carried is
/// what these tools actually use.
pub struct ToolContext<'a> {
    /// The catalog every one of these tools works in.
    pub catalog: &'a Catalog,
    /// The user this call runs as, or `None` for a system run — a trigger-driven
    /// agent turn has authority but no user.
    pub user: Option<&'a User>,
    /// The role this call is authorized at: the user's own, or 1 for a system
    /// run. Every tool here refuses anything but 1.
    pub role: u8,
    /// The trigger dispatcher, where this process has one.
    ///
    /// Carried rather than reached for, exactly as `TraitContext` carries it: a
    /// context that has none must say so ([`require_triggers`](ToolContext::require_triggers))
    /// rather than become a second way to reach the trigger set.
    pub triggers: Option<&'a Arc<TriggerDispatcher>>,
    /// What to call this caller in a message it cannot see the stack behind —
    /// the agent's name, or the token's label.
    pub actor: &'a str,
}

impl ToolContext<'_> {
    /// The dispatcher, or the configuration error that says this context has
    /// none. Fail closed and say why.
    pub fn require_triggers(&self) -> Result<&Arc<TriggerDispatcher>> {
        self.triggers.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: this needs the trigger dispatcher, \
                 and none is available in this context",
                self.actor
            ))
        })
    }
}

/// One administrative tool: what it is called, what it declares, and what it
/// does.
///
/// Split into `description`/`parameters` rather than one `spec` so that a tool
/// living in a crate above this one need not name [`ToolSpec`] — and so the set
/// is the one place that decides what a spec is made of.
#[async_trait::async_trait]
pub trait AdminTool: Send + Sync {
    /// The fixed name the model calls it by.
    fn name(&self) -> &'static str;

    /// The half of the surface this belongs to, or `None` for the schema tools,
    /// which are always offered.
    fn area(&self) -> Option<Area> {
        None
    }

    /// The prose the model reads, which says what the grants do *not* allow —
    /// hence the grants, and the catalog for the tools that name the live set.
    fn description(&self, catalog: &Catalog, grants: &Grants) -> String;

    /// The JSON Schema of the arguments.
    fn parameters(&self) -> Json;

    /// Run it. An error here is the result the model reads, so it must read as
    /// an instruction to someone who cannot see the stack.
    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json>;
}

/// The administrative tools a caller was given, ready to list and to call.
///
/// Built once per caller — an agent's configuration or a token's grants — and
/// then asked for [`specs`](ToolSet::specs) and [`call`](ToolSet::call)ed. The
/// same grants and the same areas produce the same tools whichever caller built
/// it, which is the whole point of the type existing.
pub struct ToolSet {
    tools: Vec<Arc<dyn AdminTool>>,
    grants: Grants,
    areas: Areas,
}

impl ToolSet {
    /// The tools this crate can hold: the schema's two, the triggers' four and
    /// the code-body reference.
    ///
    /// Not the whole surface — `sc_app::mcp::tool_set` adds the three
    /// application tools and is what a caller should normally build.
    pub fn core(grants: Grants, areas: Areas) -> ToolSet {
        ToolSet {
            tools: vec![
                Arc::new(schema::DescribeSchema),
                Arc::new(schema::EditSchema),
                Arc::new(triggers::DescribeTriggers),
                Arc::new(triggers::DescribeAction),
                Arc::new(triggers::SaveTrigger),
                Arc::new(triggers::DeleteTrigger),
                Arc::new(code_api::DescribeCodeApi),
            ],
            grants,
            areas,
        }
    }

    /// Add tools that live above this crate, keeping the order they arrive in.
    pub fn with(mut self, tools: impl IntoIterator<Item = Arc<dyn AdminTool>>) -> ToolSet {
        self.tools.extend(tools);
        self
    }

    /// The grants this set was built with.
    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    /// The areas this set was built with.
    pub fn areas(&self) -> &Areas {
        &self.areas
    }

    /// Every tool in the set, area or no area.
    ///
    /// For a caller that wants to **document** the surface rather than offer it
    /// — the generated `SKILL.md` (§13.6) lists all of it, because a repository
    /// is worked on with more than one token and each is granted its own areas.
    pub fn tools(&self) -> &[Arc<dyn AdminTool>] {
        &self.tools
    }

    /// Every tool this set could offer, area or no area — the answer to "what
    /// will this be called?" that an agent's collision check (§11.2) wants
    /// before a configuration exists to filter by.
    pub fn all_names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// What this caller is offered: every tool whose area is on, in order.
    pub fn specs(&self, catalog: &Catalog) -> Vec<ToolSpec> {
        self.offered()
            .map(|tool| {
                ToolSpec::new(
                    tool.name(),
                    tool.description(catalog, &self.grants),
                    tool.parameters(),
                )
            })
            .collect()
    }

    /// Whether this set offers a tool by that name, areas applied.
    pub fn offers(&self, name: &str) -> bool {
        self.offered().any(|tool| tool.name() == name)
    }

    /// Run one tool by name, as the context's caller.
    ///
    /// Three refusals before the body: a caller who is not an admin, a name this
    /// set does not have, and a name whose area the admin switched off. Each
    /// names what would fix it, because each is read by a model rather than by a
    /// developer with a backtrace.
    pub async fn call(&self, tool: &str, args: &Json, ctx: &ToolContext<'_>) -> Result<Json> {
        require_admin(ctx.role, tool)?;
        self.resolve(tool)?.call(ctx, &self.grants, args).await
    }

    /// The tool this set offers under that name, or the refusal that says why
    /// not: a name nobody has, or a name whose area is switched off.
    fn resolve(&self, tool: &str) -> Result<&Arc<dyn AdminTool>> {
        let Some(found) = self.tools.iter().find(|t| t.name() == tool) else {
            return Err(Error::invalid(format!(
                "this trait offers {}, not `{tool}`",
                self.all_names()
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        if let Some(area) = found.area()
            && !self.areas.has(area)
        {
            return Err(Error::invalid(format!(
                "`{tool}` is switched off for this agent; turn on `{}` in its \
                 `admin_copilot` settings to offer it.",
                area.key()
            )));
        }
        Ok(found)
    }

    fn offered(&self) -> impl Iterator<Item = &Arc<dyn AdminTool>> {
        self.tools
            .iter()
            .filter(|tool| tool.area().is_none_or(|area| self.areas.has(area)))
    }
}

/// Refuse a caller who is not an administrator, in words the model can relay.
///
/// The check is on the **call's** caller, not on any agent's `min_role`: an
/// agent may be reachable at role 80 for everything else it does and still must
/// not hand that caller the schema.
pub fn require_admin(role: u8, tool: &str) -> Result<()> {
    if role == 1 {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "`{tool}` is only available to an administrator, and this conversation is \
         with a role-{role} user. Tell them the schema and the triggers over it can \
         only be seen or changed by an admin.",
    )))
}

/// Refuse an operation the admin did not tick the box for, naming the box.
///
/// The sibling of [`schema_edit`]'s own grant check, kept separate because that
/// one says "the whole batch was refused" — true of a list of schema operations
/// and untrue of one trigger.
///
/// The remedy is named without naming *where* it is ticked, because there are
/// two places and the caller cannot see which one it came from: an agent's
/// `admin_copilot` checkboxes and a token's `grants` are the same six flags
/// (§13.6), so a message that named only one of them would be wrong half the
/// time.
pub fn require_grant(granted: bool, what: &str, key: &str) -> Result<()> {
    if granted {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "not permitted to {what}; nothing was changed. Turn on `{key}` to allow \
         it — an agent's `admin_copilot` settings, or an API token's grants."
    )))
}

/// The arguments object, or an empty one, with every key checked against the
/// ones this tool declared.
///
/// A missing or null argument bag means "no arguments": both vendors send one
/// for a tool whose parameters are all optional, and refusing it would fail the
/// most ordinary call there is. Anything else that is not an object is the model
/// having produced something the schema did not describe, and saying so is what
/// lets it correct itself.
pub fn arguments(args: &Json, allowed: &[&str]) -> Result<Map<String, Json>> {
    let obj = match args {
        Json::Null => Map::new(),
        Json::Object(map) => map.clone(),
        other => {
            return Err(Error::invalid(format!(
                "the arguments should be an object, got {other}"
            )));
        }
    };
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(Error::invalid(format!(
                "unknown argument `{key}`; this tool takes {}",
                allowed.join(", ")
            )));
        }
    }
    Ok(obj)
}

/// An optional string argument, or the refusal that says what it should be.
pub fn optional_string(obj: &Map<String, Json>, key: &str) -> Result<Option<String>> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be a string, got {other}"
        ))),
    }
}

/// An optional boolean argument, or the refusal that says what it should be.
pub fn optional_bool(obj: &Map<String, Json>, key: &str) -> Result<Option<bool>> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be true or false, got {other}"
        ))),
    }
}

/// An optional role argument, checked against the 1–100 scale.
pub fn optional_role(obj: &Map<String, Json>, key: &str) -> Result<Option<u8>> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => n
            .as_i64()
            .and_then(|n| u8::try_from(n).ok())
            .filter(|r| (1..=100).contains(r))
            .map(Some)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "`{key}` should be a role between 1 and 100, got {n}"
                ))
            }),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` should be a number between 1 and 100, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_ungranted_operation_names_the_checkbox_that_would_allow_it() {
        let err = require_grant(false, "create a trigger", GRANT_CREATE)
            .unwrap_err()
            .to_string();
        assert!(err.contains("create a trigger"), "{err}");
        assert!(err.contains(GRANT_CREATE), "{err}");
        // The message an agent relays says nothing was changed, because for one
        // trigger that is the whole truth — there is no half-applied batch.
        assert!(err.contains("nothing was changed"), "{err}");
        assert!(require_grant(true, "create a trigger", GRANT_CREATE).is_ok());
    }

    #[test]
    fn only_an_admin_reaches_these_tools() {
        assert!(require_admin(1, TOOL_EDIT).is_ok());
        let err = require_admin(40, TOOL_EDIT).unwrap_err().to_string();
        assert!(err.contains("only available to an administrator"), "{err}");
        assert!(err.contains("role-40"), "{err}");
    }

    #[test]
    fn an_area_that_is_off_takes_its_tools_out_of_the_listing() {
        let set = ToolSet::core(Grants::all(), Areas::none());
        // The schema's two and the code-body reference are unconditional; the
        // triggers' four are gone.
        assert_eq!(set.all_names().len(), 7);
        assert!(set.offers(TOOL_DESCRIBE) && set.offers(TOOL_EDIT));
        assert!(set.offers(TOOL_DESCRIBE_CODE_API));
        assert!(!set.offers(TOOL_SAVE_TRIGGER));

        let set = ToolSet::core(Grants::all(), Areas::all());
        assert!(set.offers(TOOL_SAVE_TRIGGER));
    }

    #[test]
    fn a_switched_off_area_is_still_refused_by_name() {
        // `call` takes a string rather than a choice from `specs`, so a
        // withheld tool must be refused when its name arrives some other way —
        // and the refusal must say which checkbox would offer it.
        let set = ToolSet::core(Grants::all(), Areas::none());
        let err = set.resolve(TOOL_SAVE_TRIGGER).err().unwrap().to_string();
        assert!(err.contains(Area::Triggers.key()), "{err}");
        assert!(set.resolve(TOOL_DESCRIBE).is_ok());

        let err = set.resolve("no_such_tool").err().unwrap().to_string();
        assert!(err.contains(TOOL_DESCRIBE), "{err}");
        assert!(err.contains("not `no_such_tool`"), "{err}");
    }

    #[test]
    fn the_six_flags_round_trip_through_the_object_a_token_stores() {
        // An agent's checkboxes and a token's `grants` column are the same six
        // flags, so the readers and the writer have to agree: what
        // `flags_to_attrs` writes must read back as what it was given.
        for (grants, areas) in [
            (Grants::none(), Areas::none()),
            (Grants::all(), Areas::all()),
            (
                Grants {
                    create: true,
                    edit: false,
                    drop: false,
                    access_changes: true,
                },
                Areas {
                    triggers: false,
                    applications: true,
                    modules: true,
                    history: false,
                },
            ),
        ] {
            let stored = flags_to_attrs(&grants, &areas);
            // Explicit, never sparse: every key is written, so a default that
            // changes cannot retroactively rewrite what an admin agreed to.
            assert_eq!(stored.len(), FLAG_KEYS.len());
            for key in FLAG_KEYS {
                assert!(stored.contains_key(key), "`{key}` should be written");
            }
            assert_eq!(grants_from_attrs(&stored), grants);
            assert_eq!(areas_from_attrs(&stored), areas);
            assert!(validate_flags(&stored).is_ok());
        }
    }

    #[test]
    fn a_token_minted_with_nothing_said_is_granted_everything() {
        let stored = token_flags_from_submitted(&Attrs::new());
        // The eight, `allow_all` written down as off, and the checklists grant
        // written down as `append` — never `full` by default.
        assert_eq!(stored.len(), FLAG_KEYS.len() + 2);
        assert_eq!(stored[CHECKLISTS_GRANT], Json::from("append"));
        assert_eq!(grants_from_attrs(&stored), Grants::all());
        let areas = areas_from_attrs(&stored);
        assert!(areas.triggers && areas.applications && areas.modules && areas.history);

        // What was said is kept; only the silence defaults on.
        let mut said = Attrs::new();
        said.insert(GRANT_DROP.to_owned(), Json::Bool(false));
        let stored = token_flags_from_submitted(&said);
        assert_eq!(stored[GRANT_DROP], Json::Bool(false));
        assert_eq!(stored[GRANT_ACCESS_CHANGES], Json::Bool(true));
        assert_eq!(stored[Area::Modules.key()], Json::Bool(true));
        assert_eq!(stored[ALLOW_ALL], Json::Bool(false));

        // A checklists grant that was said is kept, and a bad one refused.
        said.insert(CHECKLISTS_GRANT.to_owned(), Json::from("full"));
        assert_eq!(token_flags_from_submitted(&said)[CHECKLISTS_GRANT], Json::from("full"));
        said.insert(CHECKLISTS_GRANT.to_owned(), Json::from("all"));
        assert!(validate_token_flags(&said).is_err());
    }

    #[test]
    fn allow_all_on_a_token_grants_flags_added_after_the_mint() {
        let mut said = Attrs::new();
        said.insert(ALLOW_ALL.to_owned(), json!(true));
        said.insert(GRANT_DROP.to_owned(), json!(false));
        assert!(validate_token_flags(&said).is_ok());
        let mut stored = token_flags_from_submitted(&said);
        assert_eq!(stored[ALLOW_ALL], json!(true));

        // A row minted by a version that knew fewer flags: modules (say) is
        // not in it. `allow_all` reads it as granted; the denied drop stays
        // denied.
        stored.remove(Area::Modules.key());
        let read = token_flags(&stored);
        assert!(areas_from_attrs(&read).has(Area::Modules));
        assert!(!grants_from_attrs(&read).drop);

        // Without `allow_all` the same row reads the missing flag at the
        // cautious default.
        stored.insert(ALLOW_ALL.to_owned(), json!(false));
        assert!(!areas_from_attrs(&token_flags(&stored)).has(Area::Modules));

        // Only a boolean is a valid `allow_all`.
        let mut bad = Attrs::new();
        bad.insert(ALLOW_ALL.to_owned(), json!("yes"));
        assert!(validate_token_flags(&bad).is_err());
    }

    #[test]
    fn an_empty_configuration_can_build_and_cannot_destroy() {
        let empty = Attrs::new();
        let grants = grants_from_attrs(&empty);
        assert!(grants.create && grants.edit);
        assert!(!grants.drop && !grants.access_changes);
        // Triggers and applications are offered until somebody says otherwise;
        // modules are not offered until somebody says so.
        assert_eq!(areas_from_attrs(&empty), Areas::all());
        assert!(!areas_from_attrs(&empty).has(Area::Modules));
        let mut ticked = Attrs::new();
        ticked.insert(Area::Modules.key().to_owned(), json!(true));
        assert!(areas_from_attrs(&ticked).has(Area::Modules));

        // A flag that is not a boolean is the caller's mistake, named.
        let mut wrong = Attrs::new();
        wrong.insert(GRANT_DROP.to_owned(), json!("yes"));
        let err = validate_flags(&wrong).unwrap_err().to_string();
        assert!(err.contains(GRANT_DROP), "{err}");
    }

    #[test]
    fn every_tool_declares_its_parameters_as_an_object_schema() {
        let set = ToolSet::core(Grants::all(), Areas::all());
        for tool in &set.tools {
            let params = tool.parameters();
            assert_eq!(
                params["type"],
                json!("object"),
                "{}'s parameters",
                tool.name()
            );
        }
    }
}

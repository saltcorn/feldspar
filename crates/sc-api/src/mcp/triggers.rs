//! The trigger half of the administrative surface (§13.6): four tools over
//! `_fd_triggers`.
//!
//! [`describe_triggers`](TOOL_DESCRIBE_TRIGGERS) reads the trigger set,
//! [`describe_action`](TOOL_DESCRIBE_ACTION) hands back what one action may be
//! configured with, [`save_trigger`](TOOL_SAVE_TRIGGER) writes one and
//! [`delete_trigger`](TOOL_DELETE_TRIGGER) removes one. Why the settings arrive
//! through a tool of their own rather than through a nested inference call is the
//! decision recorded in the parent module, and is the whole reason this file is
//! shaped the way it is.
//!
//! ## Everything goes through the same save path
//!
//! [`sc_action::save_trigger`] is the one authority, exactly as
//! [`crate::schema_edit`] is for the schema half: the same
//! [`validate_trigger`](sc_action::validate_trigger) that stands in front of the
//! admin's own form runs here, against the same
//! [`ActionRegistry`](sc_action::ActionRegistry) the dispatcher will fire with. So
//! an agent cannot save a trigger an admin could not have saved, and the message
//! it is refused with is the message the admin would have read.
//!
//! Afterwards the dispatcher is **reloaded**, so what the agent just wrote is
//! live before the tool result comes back — which is what lets the model say "it
//! is set up" truthfully, and what lets a following `run_trigger` find it.
//!
//! ## Two things the model is not asked to know
//!
//! - **Identity is the name.** A trigger's row has a UUID; nothing here shows it
//!   or accepts it. The name is what an API path, a Run button and an
//!   application's exposed subset already reference (§10.2), so it is what the
//!   model works in, and `save_trigger` resolves it to a row itself.
//! - **Omitted means unchanged.** An edit sends only what is changing, the same
//!   way `alter_table` does; `null` is how something is *cleared*, and the two
//!   are distinguished rather than collapsed. Sending the whole object back would
//!   mean a model that forgot one field silently reset it.

use std::collections::BTreeMap;

use sc_action::{
    ATTR_DAY_OF_WEEK, ATTR_HOUR, ATTR_MINUTE, ActionRegistry, EVENT_KINDS, EventKind, Trigger,
    TriggerDispatcher,
};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{Attrs, FormField, merge_secrets};
use serde_json::{Map, Value as Json, json};

use crate::schema_edit::{GRANT_ACCESS_CHANGES, GRANT_CREATE, GRANT_DROP, GRANT_EDIT, Grants};

use super::{
    AdminTool, Area, TOOL_DESCRIBE_CODE_API, ToolContext, optional_bool, optional_role,
    optional_string, require_grant,
};

/// Reads the trigger set.
pub const TOOL_DESCRIBE_TRIGGERS: &str = "describe_triggers";
/// Reads one action's settings — the progressive-disclosure step.
pub const TOOL_DESCRIBE_ACTION: &str = "describe_action";
/// Creates a trigger, or edits the one already holding the name.
pub const TOOL_SAVE_TRIGGER: &str = "save_trigger";
/// Deletes one.
pub const TOOL_DELETE_TRIGGER: &str = "delete_trigger";

/// Read the trigger set: what fires, on what, and whether it is usable.
pub(super) struct DescribeTriggers;

#[async_trait::async_trait]
impl AdminTool for DescribeTriggers {
    fn name(&self) -> &'static str {
        TOOL_DESCRIBE_TRIGGERS
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Triggers)
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        describe_triggers_description()
    }

    fn parameters(&self) -> Json {
        describe_triggers_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, _grants: &Grants, args: &Json) -> Result<Json> {
        describe_triggers(ctx, args).await
    }
}

/// The progressive-disclosure step: one action's settings, on request.
pub(super) struct DescribeAction;

#[async_trait::async_trait]
impl AdminTool for DescribeAction {
    fn name(&self) -> &'static str {
        TOOL_DESCRIBE_ACTION
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Triggers)
    }

    fn description(&self, _catalog: &Catalog, _grants: &Grants) -> String {
        describe_action_description()
    }

    fn parameters(&self) -> Json {
        describe_action_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, _grants: &Grants, args: &Json) -> Result<Json> {
        describe_action(ctx, args).await
    }
}

/// Create a trigger, or edit the one already holding the name.
pub(super) struct SaveTrigger;

#[async_trait::async_trait]
impl AdminTool for SaveTrigger {
    fn name(&self) -> &'static str {
        TOOL_SAVE_TRIGGER
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Triggers)
    }

    fn description(&self, _catalog: &Catalog, grants: &Grants) -> String {
        save_description(grants)
    }

    fn parameters(&self) -> Json {
        save_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        save(ctx, grants, args).await
    }
}

/// Delete one.
pub(super) struct DeleteTrigger;

#[async_trait::async_trait]
impl AdminTool for DeleteTrigger {
    fn name(&self) -> &'static str {
        TOOL_DELETE_TRIGGER
    }

    fn area(&self) -> Option<Area> {
        Some(Area::Triggers)
    }

    fn description(&self, _catalog: &Catalog, grants: &Grants) -> String {
        delete_description(grants)
    }

    fn parameters(&self) -> Json {
        delete_parameters()
    }

    async fn call(&self, ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
        delete(ctx, grants, args).await
    }
}

/// Name one trigger instead of all of them.
const ARG_TRIGGER: &str = "trigger";
/// The action whose settings are wanted, or that a trigger runs.
const ARG_ACTION: &str = "action";
/// The table — a trigger's channel, and the table an action's settings are
/// declared *for*.
const ARG_TABLE: &str = "table";
const ARG_NAME: &str = "name";
const ARG_DESCRIPTION: &str = "description";
const ARG_WHEN: &str = "when";
const ARG_ONLY_IF: &str = "only_if";
const ARG_CONFIGURATION: &str = "configuration";
const ARG_MIN_ROLE: &str = "min_role";
const ARG_ENABLED: &str = "enabled";

/// The arguments [`save`] accepts, which is also the order the description walks
/// them in.
const SAVE_ARGS: [&str; 11] = [
    ARG_NAME,
    ARG_DESCRIPTION,
    ARG_WHEN,
    ARG_TABLE,
    ARG_ONLY_IF,
    ARG_ACTION,
    ARG_CONFIGURATION,
    ARG_MIN_ROLE,
    ARG_ENABLED,
    ATTR_MINUTE,
    ATTR_HOUR,
];

// --- describe_triggers --------------------------------------------------------

fn describe_triggers_description() -> String {
    format!(
        "List the triggers: what each one listens for, which action it runs, how \
         that action is configured, who may run it, and whether it is currently \
         valid. Also lists every **action** that exists, with one line each — so \
         this is the call to make before `{TOOL_SAVE_TRIGGER}`, and the one that \
         tells you which name to pass to `{TOOL_DESCRIBE_ACTION}`.\n\n\
         A trigger is one event plus one configured action. The events are: \
         {}. `insert`, `update` and `delete` name a table and carry its row; \
         `none` is run on demand (a button, `POST /actions/<name>`, another \
         agent); `login`, `startup` and `error` carry a payload; `often`, \
         `hourly`, `daily` and `weekly` run on a schedule.",
        EVENT_KINDS
            .iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn describe_triggers_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            ARG_TRIGGER: {
                "type": "string",
                "description":
                    "Describe only the trigger with this name. Omit it to list \
                     them all, which is what you want before planning a change.",
            },
        },
        "additionalProperties": false,
    })
}

async fn describe_triggers(ctx: &ToolContext<'_>, args: &Json) -> Result<Json> {
    let args = super::arguments(args, &[ARG_TRIGGER])?;
    let only = args
        .get(ARG_TRIGGER)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let dispatcher = ctx.require_triggers()?;
    let registry = dispatcher.registry();
    let registry = registry.as_ref();

    // The **stored** rows, not the live set: a trigger that fails validation is
    // dropped from the live set and would otherwise be invisible to the agent
    // that has been asked to repair it. Its reason comes from the live set's
    // issues, beside it — the same arrangement the admin's own list makes.
    let stored = sc_action::list_triggers(ctx.catalog).await?;
    if let Some(name) = only
        && !stored.iter().any(|t| t.name == name)
    {
        return Err(unknown_trigger(name, &stored));
    }
    let issues: BTreeMap<String, String> = dispatcher
        .triggers()?
        .issues()
        .iter()
        .map(|i| (i.trigger.clone(), i.problem.clone()))
        .collect();

    let described: Vec<Json> = stored
        .iter()
        .filter(|t| only.is_none_or(|name| t.name == name))
        .map(|t| trigger_json(ctx.catalog, registry, t, issues.get(&t.name).cloned()))
        .collect();
    Ok(json!({
        "triggers": described,
        "actions": action_index(registry),
    }))
}

/// One stored trigger as the model reads it: names rather than ids, and the
/// action's configuration with its secrets masked.
fn trigger_json(
    catalog: &Catalog,
    registry: &ActionRegistry,
    trigger: &Trigger,
    problem: Option<String>,
) -> Json {
    let mut out = Map::new();
    out.insert("name".to_owned(), json!(trigger.name));
    out.insert("description".to_owned(), json!(trigger.description));
    out.insert("when".to_owned(), json!(trigger.when.as_str()));
    out.insert("table".to_owned(), json!(trigger.channel));
    out.insert("only_if".to_owned(), json!(trigger.only_if));
    // What the trigger *is*: an action with settings, or a workflow whose steps
    // are edited on the canvas rather than here (§10.3).
    out.insert("body".to_owned(), json!(trigger.body.as_str()));
    out.insert("action".to_owned(), json!(trigger.action()));
    out.insert(
        "configuration".to_owned(),
        Json::Object(
            visible_config(catalog, registry, trigger)
                .into_iter()
                .collect(),
        ),
    );
    // Absent means admin-only, which is a different fact from "set to 1" only in
    // how it got there — so it is reported as the number it behaves as, with the
    // word, because a model reading `null` would tell the admin "anyone".
    out.insert("min_role".to_owned(), json!(trigger.min_role.unwrap_or(1)));
    out.insert("enabled".to_owned(), json!(trigger.is_enabled()));
    for key in [ATTR_MINUTE, ATTR_HOUR, ATTR_DAY_OF_WEEK] {
        if let Some(value) = trigger.attributes.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(last) = trigger.last_run_at {
        out.insert("last_run_at".to_owned(), json!(last.to_rfc3339()));
    }
    // Why it is not live, when it is not. A trigger nobody can see the fault in
    // is a trigger nobody can repair (§10.2), and this tool exists to repair it.
    if let Some(problem) = problem {
        out.insert("error".to_owned(), json!(problem));
    }
    Json::Object(out)
}

/// A trigger's configuration with every secret setting masked.
///
/// The caller is an admin, so this is not an access control — it is the same rule
/// the admin API applies to itself (§11.1), applied here for the reason that is
/// specific to an agent: a tool result is written into `_fd_runs` and read back
/// into a provider's context on every later turn, so a key that reaches it has
/// been copied somewhere nobody thought about. [`save`] merges the stored value
/// back when the mask is sent in again, which is what makes the masking safe.
fn visible_config(catalog: &Catalog, registry: &ActionRegistry, trigger: &Trigger) -> Attrs {
    // A workflow body has no configuration at all: its steps carry their own.
    // An action nothing implements has no spec to mask against, and is shown
    // as stored, because the agent's job here is to fix a trigger naming an
    // action that is gone.
    registry
        .redact_trigger(catalog, trigger)
        .configuration()
        .cloned()
        .unwrap_or_default()
}

/// Every registered action's name and one line — the cheap index level, included
/// in both reading tools because it is what the next call needs and it costs a
/// dozen tokens per action.
fn action_index(registry: &ActionRegistry) -> Vec<Json> {
    registry
        .all()
        .map(|a| json!({ "name": a.name(), "description": a.description() }))
        .collect()
}

// --- describe_action ----------------------------------------------------------

fn describe_action_description() -> String {
    format!(
        "Ask what an action can be configured with, **before** configuring one \
         with `{TOOL_SAVE_TRIGGER}`. With no `{ARG_ACTION}` it lists every action \
         that exists with one line each; with one, it returns that action's \
         settings — each with its name, what it is for, its type, and whether it \
         is required. Those names are exactly the keys of `{ARG_CONFIGURATION}`.\n\n\
         Pass `{ARG_TABLE}` whenever the trigger will listen to a table: an \
         action's settings can **depend on the table** (`send_email` offers one \
         attachment checkbox per file field of it), so a spec fetched without the \
         table can be missing settings that exist and offering settings that will \
         be refused.\n\n\
         Many settings hold a formula or a template over the event rather than a \
         literal — `row`, `old`, `user` and `payload` are what is in scope, \
         `row` and `old` only for `insert`/`update`/`insert_or_update`/`delete`; a `none` trigger \
         run through `POST /actions/<name>` gets the request body as `payload` — and \
         which ones do is said in each setting's own description. If you get this \
         wrong, `{TOOL_SAVE_TRIGGER}` says so and names the setting; it does not \
         save a trigger that cannot run.\n\n\
         An action that takes JavaScript (`run_js_code`) also answers `code_api`: \
         the API the code can call. Write the code against that, not against an \
         API you know from elsewhere — its `db` is this server's own."
    )
}

fn describe_action_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            ARG_ACTION: {
                "type": "string",
                "description":
                    "The action whose settings you want. Omit it to list the \
                     actions that exist.",
            },
            ARG_TABLE: {
                "type": "string",
                "description":
                    "The table the trigger will fire on, for an `insert`, \
                     `update` or `delete` trigger. Omit it for every other event.",
            },
        },
        "additionalProperties": false,
    })
}

async fn describe_action(ctx: &ToolContext<'_>, args: &Json) -> Result<Json> {
    let args = super::arguments(args, &[ARG_ACTION, ARG_TABLE])?;
    let registry = ctx.require_triggers()?.registry();
    let registry = registry.as_ref();
    let Some(name) = args
        .get(ARG_ACTION)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(json!({
            "actions": action_index(registry),
            "note": format!(
                "Call `{TOOL_DESCRIBE_ACTION}` again with one of these names to \
                 see the settings it takes."
            ),
        }));
    };
    // The registry's own error lists the alternatives, which is the whole
    // recovery a model that guessed a name needs.
    let action = registry.require(name)?;
    let table = args
        .get(ARG_TABLE)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let spec = sc_catalog::resolve_options(ctx.catalog, action.config_spec_for(ctx.catalog, table))
        .await?;
    let mut out = json!({
        "action": action.name(),
        "description": action.description(),
        "table": table,
        "settings": spec.iter().map(setting_json).collect::<Vec<_>>(),
    });
    // What a direct run takes and answers, where the action says: the types of
    // `api.run<Name>()` once a `none` trigger over it is exposed on an app.
    if let Some(payload) = action.payload_type() {
        out["payload"] = payload;
    }
    if let Some(returns) = action.returns_type() {
        out["returns"] = returns;
    }
    // A JavaScript body is the one setting whose *contents* the model must be
    // taught: it is keyed on the declaration's own language hint rather than on
    // the action's name, so a plugin's code-taking action gets it too.
    if spec
        .iter()
        .any(|f| f.code_language.as_deref() == Some("javascript"))
    {
        out["code_api"] = json!(super::JS_CODE_API);
    }
    Ok(out)
}

/// One action setting, as the model reads it.
///
/// [`resolve_options`](sc_catalog::resolve_options) runs first, in
/// [`describe_action`], so a setting whose choices are a server-side query (the
/// file stores, say) arrives as a concrete list rather than as a query name the
/// model cannot answer — the same resolution the admin UI's form gets.
pub fn setting_json(field: &FormField) -> Json {
    let mut out = Map::new();
    out.insert("name".to_owned(), json!(field.base.name));
    // The label is the sentence the admin's own form shows above the input, and
    // it is the only place an action says what a setting *means* — so it is here
    // under a name that tells the model to read it as that.
    out.insert("what_it_is".to_owned(), json!(field.base.label));
    out.insert(
        "type".to_owned(),
        json!(
            field
                .base
                .type_
                .as_basic()
                .map(|b| b.name())
                .unwrap_or("text")
        ),
    );
    out.insert("required".to_owned(), json!(field.required));
    if let Some(default) = &field.default {
        out.insert("default".to_owned(), default.clone());
    }
    if !field.static_options().is_empty() {
        out.insert("one_of".to_owned(), json!(field.static_options()));
    }
    if field.secret {
        out.insert("secret".to_owned(), json!(true));
    }
    Json::Object(out)
}

// --- save_trigger -------------------------------------------------------------

fn save_description(grants: &Grants) -> String {
    let permitted = match (grants.create, grants.edit) {
        (true, true) => "You may create triggers and change existing ones.".to_owned(),
        (true, false) => "You may create triggers, but not change one that already \
                          exists."
            .to_owned(),
        (false, true) => "You may change existing triggers, but not create one.".to_owned(),
        (false, false) => "You may neither create nor change a trigger; this tool will \
                           refuse every call. Say so rather than retrying."
            .to_owned(),
    };
    let access = match grants.access_changes {
        true => format!(
            "You may set `{ARG_MIN_ROLE}`, which decides who can run the trigger \
             through the API. That changes what users of this deployment can \
             reach, so say plainly what you are about to do before you do it."
        ),
        false => format!(
            "You may **not** set `{ARG_MIN_ROLE}`; a call naming it is refused. \
             Without it a trigger is admin-only, which is the safe default."
        ),
    };
    format!(
        "Create a trigger, or change the one that already has this name — the \
         name is the identity, so saving under a name that exists **edits that \
         trigger**. Call `{TOOL_DESCRIBE_TRIGGERS}` first if you are not sure \
         which it will be.\n\n\
         Creating one needs `{ARG_NAME}`, `{ARG_WHEN}` and `{ARG_ACTION}`. \
         Editing one needs only `{ARG_NAME}` and what is changing: **anything you \
         omit is left as it is**, and `null` is how you clear something.\n\n\
         `{ARG_CONFIGURATION}` is the action's own settings, keyed by the names \
         `{TOOL_DESCRIBE_ACTION}` gives — get them from there rather than \
         guessing, especially for an action you have not configured in this \
         conversation. They are validated properly (a formula is resolved against \
         the fields the event will actually have), so a mistake comes back naming \
         the setting, with the settings that action takes, and **nothing is \
         saved**. Fix it and call again.\n\n\
         A `run_js_code` body is **not** checked on save: it is JavaScript \
         against this server's own API, which `{TOOL_DESCRIBE_ACTION}` returns \
         as `code_api` (and `{TOOL_DESCRIBE_CODE_API}` alone). Read it before \
         writing one — rows are changed with `db.t.where({{ id: row.id }}).update({{ … }})`, \
         and there is no `db.t.update(id, values)`.\n\n\
         A trigger belongs to **no application**, even one an application \
         exposes: its body's `lib(name)` imports **instance-wide** code \
         libraries only, never an application's own. Code a trigger shares with \
         an application's queries goes in an instance-wide library.\n\n\
         {permitted} {access}"
    )
}

fn save_parameters() -> Json {
    let events: Vec<&str> = EVENT_KINDS.iter().map(|k| k.as_str()).collect();
    json!({
        "type": "object",
        "properties": {
            ARG_NAME: {
                "type": "string",
                "description":
                    "The trigger's name, unique across the deployment: lower-case \
                     letters, digits and underscores. It is what a button, an API \
                     path (`POST /actions/<name>`) and another agent reference, \
                     so name it for what it does.",
            },
            ARG_DESCRIPTION: {
                "type": "string",
                "description": "What this trigger is for, in one line, for the admin.",
            },
            ARG_WHEN: {
                "type": "string",
                "description":
                    "The event that fires it. `insert`/`update`/`delete` need a \
                     `table`, and so does `insert_or_update`, which fires on \
                     either an insert or an update (guard its own write-back \
                     with `only_if`, e.g. `address !== old.address`); `none` is run on demand; `often`, `hourly`, `daily` \
                     and `weekly` are the scheduled ones and take `minute` and \
                     `hour`.",
                "enum": events,
            },
            ARG_TABLE: {
                "type": "string",
                "description":
                    "The table an `insert`, `update`, `insert_or_update` or \
                     `delete` trigger listens to. Every other event refuses one.",
            },
            ARG_ONLY_IF: {
                "type": "string",
                "description":
                    "A JavaScript condition the affected row must satisfy for the \
                     action to run, e.g. `status === 'closed' && old.status !== \
                     'closed'`. Bare names are the row's fields; `row`, `old` and \
                     `user` are also in scope. Table events only.",
            },
            ARG_ACTION: {
                "type": "string",
                "description":
                    "The action to run, by the name `describe_action` lists. \
                     Changing it on an existing trigger also needs a new \
                     `configuration`.",
            },
            ARG_CONFIGURATION: {
                "type": "object",
                "description":
                    "The action's settings, keyed by the names `describe_action` \
                     gives for this action **and this table**. Replaces the \
                     stored settings whole rather than merging into them, so send \
                     all of them.",
                // Open on purpose: the keys are the action's to declare, and a
                // closed schema here would be this file guessing at another
                // crate's — and at a plugin's — contract. What is sent is
                // checked against the live declaration instead.
                "additionalProperties": true,
            },
            ARG_MIN_ROLE: {
                "type": "integer",
                "description":
                    "Least-privileged role that may run this trigger through the \
                     API, 1 (admin) to 100 (anyone). Absent means admin-only.",
                "minimum": 1,
                "maximum": 100,
            },
            ARG_ENABLED: {
                "type": "boolean",
                "description":
                    "Whether the trigger fires. Set it false to switch one off \
                     without losing its configuration.",
            },
            ATTR_MINUTE: {
                "type": "integer",
                "description":
                    "For `hourly`, `daily` and `weekly`: the minute past the hour.",
                "minimum": 0,
                "maximum": 59,
            },
            ATTR_HOUR: {
                "type": "integer",
                "description": "For `daily` and `weekly`: the hour, 0 to 23.",
                "minimum": 0,
                "maximum": 23,
            },
        },
        "required": [ARG_NAME],
        "additionalProperties": false,
    })
}

async fn save(ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
    let mut allowed = SAVE_ARGS.to_vec();
    allowed.push(ATTR_DAY_OF_WEEK);
    let args = super::arguments(args, &allowed)?;
    let name = args
        .get(ARG_NAME)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{ARG_NAME}` is required")))?
        .to_owned();
    let dispatcher = ctx.require_triggers()?;

    let existing = sc_action::load_trigger_by_name(ctx.catalog, &name).await?;
    // Decided here, before anything is written: afterwards the answer is always
    // "it exists". It is also what picks the grant — creating and editing are
    // two different permissions over one tool, so an agent allowed to build new
    // triggers cannot quietly rewrite one an admin wrote by reusing its name.
    let creating = existing.is_none();
    match creating {
        false => require_grant(grants.edit, "change an existing trigger", GRANT_EDIT)?,
        true => require_grant(grants.create, "create a trigger", GRANT_CREATE)?,
    }
    if args.contains_key(ARG_MIN_ROLE) {
        require_grant(
            grants.access_changes,
            "set who may run a trigger",
            GRANT_ACCESS_CHANGES,
        )?;
    }

    let mut notes: Vec<String> = Vec::new();
    let trigger = build(ctx.catalog, dispatcher, &name, existing, &args, &mut notes)?;

    sc_action::save_trigger(ctx.catalog, &dispatcher.registry(), &trigger)
        .await
        .map_err(|e| explain(ctx.catalog, &dispatcher.registry(), &trigger, e))?;
    // Live before the answer is written: the model is about to tell somebody
    // this is set up, and a trigger that only fires after the next restart would
    // make that a lie. Any application exposing a trigger is re-projected by the
    // dispatcher's observer, so an endpoint appears with it.
    dispatcher.reload(ctx.catalog).await?;

    Ok(json!({
        "saved": trigger.name,
        "created": creating,
        // The whole trigger as it now stands, not an echo of what was sent: an
        // edit merged into what was stored, and the difference between the two
        // is exactly what the model has to be able to report back.
        "trigger": trigger_json(ctx.catalog, &dispatcher.registry(), &trigger, None),
        "notes": notes,
    }))
}

/// The trigger to save: the stored one with the given changes applied, or a new
/// one.
///
/// The rule is the one the tool's description states — omitted is unchanged,
/// `null` clears — plus the two places where a change to `when` makes another
/// setting meaningless. Those are dropped rather than carried into a validation
/// error, because "you changed this to `daily`, so its table no longer applies"
/// is a thing the code knows and the model would have to be told.
fn build(
    catalog: &Catalog,
    dispatcher: &TriggerDispatcher,
    name: &str,
    existing: Option<Trigger>,
    args: &Map<String, Json>,
    notes: &mut Vec<String>,
) -> Result<Trigger> {
    let given_when = match optional_string(args, ARG_WHEN)? {
        Some(raw) => Some(EventKind::parse(raw.trim())?),
        None => None,
    };
    let mut trigger = match existing {
        Some(stored) => stored,
        None => {
            let when = given_when.ok_or_else(|| {
                Error::invalid(format!(
                    "there is no trigger named `{name}`, so this creates one — which \
                     needs `{ARG_WHEN}` and `{ARG_ACTION}` as well as the name"
                ))
            })?;
            let action = optional_string(args, ARG_ACTION)?
                .map(|a| a.trim().to_owned())
                .filter(|a| !a.is_empty())
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "there is no trigger named `{name}`, so this creates one — which \
                         needs `{ARG_ACTION}`: the actions are {}",
                        dispatcher.registry().names().join(", ")
                    ))
                })?;
            Trigger::new(name, when, action)
        }
    };
    let when_before = trigger.when;
    if let Some(when) = given_when {
        trigger.when = when;
    }
    if args.contains_key(ARG_DESCRIPTION) {
        // `null` clears it, as it clears everything else here — the stored shape
        // is a `String` whose empty value *is* "none given" (§9), so the rule
        // holds without a second spelling of absence.
        trigger.description = optional_string(args, ARG_DESCRIPTION)?.unwrap_or_default();
    }
    if args.contains_key(ARG_TABLE) {
        trigger.channel = optional_string(args, ARG_TABLE)?
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
    }
    if args.contains_key(ARG_ONLY_IF) {
        trigger.only_if = optional_string(args, ARG_ONLY_IF)?
            .map(|f| f.trim().to_owned())
            .filter(|f| !f.is_empty());
    }

    // A workflow's body is its steps, and those are not this tool's to edit: a
    // model that "fixed" a workflow by naming an action would delete the program
    // without deleting it. Refused by name, before anything else is applied.
    if trigger.is_workflow()
        && (args.contains_key(ARG_ACTION) || args.contains_key(ARG_CONFIGURATION))
    {
        return Err(Error::invalid(format!(
            "trigger `{name}` is a workflow: its steps are edited as a workflow, \
             so `{ARG_ACTION}` and `{ARG_CONFIGURATION}` do not apply to it"
        )));
    }

    // The action, and with it the meaning of every stored setting. Changing one
    // without the other is refused rather than half-applied: `send_email`'s
    // `subject` is not `insert_row`'s anything, and validation's "unknown setting"
    // would report the symptom rather than what happened.
    let action_before = trigger.action().unwrap_or_default().to_owned();
    if let Some(action) = optional_string(args, ARG_ACTION)?
        .map(|a| a.trim().to_owned())
        .filter(|a| !a.is_empty())
    {
        trigger.set_action(action)?;
    }
    let action_now = trigger.action().unwrap_or_default().to_owned();
    let action_changed = action_now != action_before;
    let given_config = match args.get(ARG_CONFIGURATION) {
        None | Some(Json::Null) => None,
        Some(Json::Object(map)) => Some(map.clone().into_iter().collect::<Attrs>()),
        Some(other) => {
            return Err(Error::invalid(format!(
                "`{ARG_CONFIGURATION}` should be an object of the action's \
                 settings, got {other}"
            )));
        }
    };
    if action_changed && given_config.is_none() {
        return Err(Error::invalid(format!(
            "changing the action from `{action_before}` to `{action_now}` also needs a new \
             `{ARG_CONFIGURATION}`: the settings are that action's own, and \
             `{action_before}`'s do not carry over. Call `{TOOL_DESCRIBE_ACTION}` \
             for `{action_now}` and send its settings."
        )));
    }
    if let Some(config) = given_config {
        // The mask sent back unchanged means "leave the stored value", which is
        // what makes `visible_config`'s redaction safe rather than destructive
        // (§11.1's other half).
        let stored = trigger.configuration().cloned().unwrap_or_default();
        let registry = dispatcher.registry();
        let merged = match trigger.action().and_then(|a| registry.get(a)) {
            Some(action) => merge_secrets(
                &action.config_spec_for(catalog, trigger.channel.as_deref()),
                &stored,
                &config,
            ),
            None => config,
        };
        trigger.set_configuration(merged)?;
    }

    if let Some(role) = optional_role(args, ARG_MIN_ROLE)? {
        trigger.min_role = Some(role);
    } else if args.contains_key(ARG_MIN_ROLE) {
        trigger.min_role = None;
    }
    if let Some(enabled) = optional_bool(args, ARG_ENABLED)? {
        trigger.set_enabled(enabled);
    }
    for key in [ATTR_MINUTE, ATTR_HOUR, ATTR_DAY_OF_WEEK] {
        match args.get(key) {
            None => {}
            Some(Json::Null) => {
                trigger.attributes.remove(key);
            }
            Some(Json::Number(n)) if n.as_u64().is_some() => {
                trigger.attributes.insert(key.to_owned(), json!(n));
            }
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`{key}` should be a whole number, got {other}"
                )));
            }
        }
    }

    // The two consequences of a changed event that the model should not have to
    // remember, reported rather than done silently.
    if trigger.when != when_before {
        if !trigger.when.is_table_event() && trigger.channel.is_some() {
            trigger.channel = None;
            trigger.only_if = None;
            notes.push(format!(
                "`{}` has no row, so the table and any `{ARG_ONLY_IF}` were cleared",
                trigger.when
            ));
        }
        if !trigger.when.is_periodic() {
            let had_timing = [ATTR_MINUTE, ATTR_HOUR, ATTR_DAY_OF_WEEK]
                .iter()
                .any(|k| trigger.attributes.contains_key(*k));
            if had_timing {
                for key in [ATTR_MINUTE, ATTR_HOUR, ATTR_DAY_OF_WEEK] {
                    trigger.attributes.remove(key);
                }
                notes.push(format!(
                    "`{}` does not run on a schedule, so the timing was cleared",
                    trigger.when
                ));
            }
        }
    }
    Ok(trigger)
}

/// A refusal from [`validate_trigger`](sc_action::validate_trigger), with the
/// settings the action actually takes appended.
///
/// This is what makes `describe_action` optional rather than compulsory: a model
/// that guessed the settings and got them wrong is handed the real ones in the
/// same turn it was refused, instead of having to go and ask. Appended rather
/// than substituted — the validation's own sentence names the setting and says
/// what is wrong with it, and no summary of mine improves on that.
fn explain(catalog: &Catalog, registry: &ActionRegistry, trigger: &Trigger, error: Error) -> Error {
    let Some(action) = trigger.action().and_then(|a| registry.get(a)) else {
        return error;
    };
    let spec = action.config_spec_for(catalog, trigger.channel.as_deref());
    if spec.is_empty() {
        return error;
    }
    let settings: Vec<String> = spec
        .iter()
        .map(|f| {
            let required = match f.required {
                true => ", required",
                false => "",
            };
            format!(
                "`{}` ({}{required}) — {}",
                f.base.name,
                f.base.type_.as_basic().map(|b| b.name()).unwrap_or("text"),
                f.base.label
            )
        })
        .collect();
    Error::invalid(format!(
        "{error}\n\nNothing was saved. The settings `{}` takes{} are:\n{}",
        action.name(),
        match &trigger.channel {
            Some(table) => format!(" on `{table}`"),
            None => String::new(),
        },
        settings.join("\n")
    ))
}

// --- delete_trigger -----------------------------------------------------------

fn delete_description(grants: &Grants) -> String {
    match grants.drop {
        false => format!(
            "Delete a trigger. You are **not** permitted to, so this tool refuses \
             every call — say so rather than retrying. Switching a trigger off \
             with `{TOOL_SAVE_TRIGGER}`'s `{ARG_ENABLED}` is the thing you can do \
             instead."
        ),
        true => format!(
            "Delete a trigger by name. What it already did is not undone — the \
             rows it wrote, the mail it sent, stay — and its configuration is \
             gone with it. If the intent is to stop it running for now, set \
             `{ARG_ENABLED}` false with `{TOOL_SAVE_TRIGGER}` instead and it can \
             be switched back on."
        ),
    }
}

fn delete_parameters() -> Json {
    json!({
        "type": "object",
        "properties": {
            ARG_NAME: {
                "type": "string",
                "description": "The trigger to delete.",
            },
        },
        "required": [ARG_NAME],
        "additionalProperties": false,
    })
}

async fn delete(ctx: &ToolContext<'_>, grants: &Grants, args: &Json) -> Result<Json> {
    let args = super::arguments(args, &[ARG_NAME])?;
    let name = args
        .get(ARG_NAME)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("`{ARG_NAME}` is required")))?;
    require_grant(grants.drop, "delete a trigger", GRANT_DROP)?;

    let dispatcher = ctx.require_triggers()?;
    let Some(trigger) = sc_action::load_trigger_by_name(ctx.catalog, name).await? else {
        let stored = sc_action::list_triggers(ctx.catalog).await?;
        return Err(unknown_trigger(name, &stored));
    };
    sc_action::delete_trigger(ctx.catalog, trigger.id).await?;
    dispatcher.reload(ctx.catalog).await?;
    Ok(json!({
        "deleted": trigger.name,
        // What it was, because this is the last moment anything can say so and
        // an agent that has just deleted the wrong thing should be able to put
        // it back from its own transcript.
        "was": trigger_json(ctx.catalog, &dispatcher.registry(), &trigger, None),
    }))
}

/// "No trigger called that", with the ones there are.
fn unknown_trigger(name: &str, stored: &[Trigger]) -> Error {
    let names: Vec<&str> = stored.iter().map(|t| t.name.as_str()).collect();
    Error::not_found(format!(
        "no trigger named `{name}`; {}",
        match names.is_empty() {
            true => "there are no triggers yet".to_owned(),
            false => format!("the triggers are {}", names.join(", ")),
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_enum_in_the_schema_is_the_live_set() {
        let params = save_parameters();
        let events = params["properties"][ARG_WHEN]["enum"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let expected: Vec<Json> = EVENT_KINDS.iter().map(|k| json!(k.as_str())).collect();
        assert_eq!(events, expected);
        // A trigger is named, and nothing else is compulsory: an edit sends what
        // it is changing.
        assert_eq!(params["required"], json!([ARG_NAME]));
    }

    #[test]
    fn the_descriptions_say_what_a_body_can_reach() {
        // A trigger sees instance-wide libraries only, said where one is saved.
        let text = save_description(&Grants::all());
        assert!(text.contains("instance-wide** code"), "{text}");
        // A `none` trigger's input is `payload`, said where an action is read.
        let text = describe_action_description();
        assert!(
            text.contains("gets the request body as `payload`"),
            "{text}"
        );
    }

    #[test]
    fn the_descriptions_say_what_the_grants_do_not_allow() {
        let text = save_description(&Grants::none());
        assert!(text.contains("neither create nor change"), "{text}");
        assert!(text.contains("may **not** set"), "{text}");
        let text = save_description(&Grants::all());
        assert!(text.contains("create triggers and change"), "{text}");

        let text = delete_description(&Grants::none());
        assert!(text.contains("**not** permitted"), "{text}");
        // The thing it *can* do instead, so a refusal is not a dead end.
        assert!(text.contains(ARG_ENABLED), "{text}");
    }

    #[test]
    fn an_unknown_trigger_names_the_ones_that_exist() {
        let err = unknown_trigger("nofify", &[]).to_string();
        assert!(err.contains("no triggers yet"), "{err}");
        let stored = vec![Trigger::new("notify", EventKind::Insert, "send_email")];
        let err = unknown_trigger("nofify", &stored).to_string();
        assert!(err.contains("the triggers are notify"), "{err}");
    }
}

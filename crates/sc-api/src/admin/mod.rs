//! The admin UI's API, expressed as a fixed [`EndpointSet`] (design §13.1).
//!
//! The admin API is compile-time-known, but rather than a bespoke statically
//! typed router it is built as a set of constant [`Endpoint`] values fed through
//! the **same** [`EndpointSet`] machinery an application uses. This maximises
//! reuse: `ui/admin` consumes a generated typed client (see
//! [`crate::typescript`]) exactly as an application would, and `sc-server`
//! mounts these endpoints the same way it mounts a runtime application's.
//!
//! The [`HandlerRef::Named`] handlers here are resolved by `sc-server` when it
//! mounts the set (the "Server" subphase of Phase 6). This module defines the
//! *contract* — methods, paths, schemas, and auth — that both the server and the
//! generated client are held to.
//!
//! ## The `.mcp()` tags, and why there are so few of them
//!
//! A handful of endpoints below carry an [`Endpoint::mcp`] tag, which offers
//! them to the administration MCP server as tools (§13.6). It is **opt-in and
//! deliberately sparse**: a coding agent pays for every tool in its context on
//! every turn, so projecting all of these would be mechanical and wrong. The
//! composite tools of `sc_api::mcp` and `sc_app::mcp` plus these tags are every
//! tool an agent is offered, and the number is a design constraint rather than an outcome —
//! `the_tier_two_tags_are_the_ones_that_were_argued_for` is the test that makes
//! adding one a decision somebody has to write down.
//!
//! Three groups are tagged, and each comment above a tag says why that one:
//! what an agent needs to **read** before it can write (`listFieldTypes`,
//! `listAgentTraits`, `listTableProviders`, `listActions`, `listTriggers`), the
//! objects it may be asked to **build** that have no composite tool (the agents,
//! the workflows), and the one thing it must do **after** a schema change
//! (`buildApplication`, since re-projection runs no bundler). Row history's
//! reads are tagged too, in an area of their own that is off unless granted
//! (§14.12): they are row data. Row CRUD, the file
//! store, backup and restore and user management are tier 3 — absent on purpose,
//! not overlooked.
//!
//! ## One module per domain
//!
//! The endpoints are declared in one module per domain (`tables`, `rows`,
//! `applications`, …), each with a `register(set)`, and `sc-server`'s
//! `handlers` has a module of the same name for their handlers
//! (TODO-code-quality Q5.4). [`admin_endpoints`] calls each in turn, so the set
//! lists the endpoints grouped by domain.

use crate::auth::{credentials_schema, user_row_schema, user_summary_schema};
use crate::endpoint::{Endpoint, EndpointSet, McpTag, Method, PathSpec, QueryParam};
use crate::handler_ids as ids;
use crate::mcp::{Area, Grant};
use crate::schema::{StructField, TypeSchema, ValueType};

mod agent_memory;
mod agents;
mod applications;
mod auth;
mod backup;
mod checklists;
mod code_libraries;
mod connections;
mod constructions;
mod entities;
mod fields;
mod file_stores;
mod files;
mod llm;
mod mcp_servers;
mod models;
mod modules;
mod registry;
mod rows;
mod settings;
mod source_control;
mod streams;
mod tables;
mod translations;
mod triggers;
mod users;
mod views;

/// Path prefix every admin endpoint is mounted under.
pub const ADMIN_API_PREFIX: &str = "api";

/// The full set of admin API endpoints.
///
/// Grouped as: bootstrap/auth (first-user, login, logout, whoami), catalog
/// (tables + fields), row CRUD, and user management. Each maps to a
/// [`HandlerRef::Named`] the server resolves at mount time.
pub fn admin_endpoints() -> EndpointSet {
    let mut set = EndpointSet::new();
    auth::register(&mut set);
    tables::register(&mut set);
    entities::register(&mut set);
    users::register(&mut set);
    fields::register(&mut set);
    rows::register(&mut set);
    connections::register(&mut set);
    file_stores::register(&mut set);
    llm::register(&mut set);
    mcp_servers::register(&mut set);
    agent_memory::register(&mut set);
    checklists::register(&mut set);
    modules::register(&mut set);
    code_libraries::register(&mut set);
    agents::register(&mut set);
    files::register(&mut set);
    settings::register(&mut set);
    applications::register(&mut set);
    constructions::register(&mut set);
    views::register(&mut set);
    translations::register(&mut set);
    triggers::register(&mut set);
    models::register(&mut set);
    streams::register(&mut set);
    backup::register(&mut set);
    registry::register(&mut set);
    source_control::register(&mut set);

    // The Analytics UI's datasets and workspaces (analytics TODO A1.13).
    crate::analytics::register(&mut set);

    set
}

/// The most rows one `listRows` will answer with.
///
/// A ceiling rather than a page size: the grid picks its own page and the number
/// it sends is clamped to this, so a caller who asks for the whole of a million-
/// row table gets a page and not the table. It is generous because the admin
/// grid is the one surface that legitimately pages fast — a screen of rows plus
/// a screen of overscan either side.
pub const ROW_PAGE_CAP: u64 = 1_000;

/// A `PathSpec` rooted at the admin API prefix.
fn api() -> PathSpec {
    PathSpec::root().lit(ADMIN_API_PREFIX)
}

/// One run in a list: everything except the transcript.
///
/// The transcript is deliberately absent. A chat's history is a list of dozens
/// of runs and each `context` is a whole conversation, so a list carrying them
/// would send megabytes to render a sidebar; [`run_schema`] is what the panel
/// asks for when a run is opened.
fn run_summary_schema() -> TypeSchema {
    TypeSchema::Struct(vec![
        StructField::new("id", TypeSchema::uuid()),
        // `agent` or `workflow` (§10.3's engine shares this table).
        StructField::new("kind", TypeSchema::text()),
        // What the run is of: the agent's or the trigger's **name**.
        StructField::new("subject", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // `running` | `waiting` | `done` | `failed` | `aborted`.
        StructField::new("state", TypeSchema::text()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        StructField::new("user", TypeSchema::optional(TypeSchema::uuid())),
        StructField::new("created_at", TypeSchema::timestamp()),
        StructField::new("updated_at", TypeSchema::timestamp()),
        // The workflow half (§10.3), null on an agent run. The version this run
        // is **pinned** to is the whole of "a suspended run finishes on its own
        // version", and a list that did not show it would hide the one fact that
        // explains why two runs of one workflow behaved differently.
        StructField::new("subject_version", TypeSchema::optional(TypeSchema::int())),
        // The step it is on, or null once it is over.
        StructField::new("current_step", TypeSchema::optional(TypeSchema::text())),
        // When it next wants the engine. Null and `waiting` together mean "only a
        // person can wake this", which is what the run list has to be able to
        // show as an approval somebody is sitting on.
        StructField::new("wake_at", TypeSchema::optional(TypeSchema::timestamp())),
        // How an agent run's loop concluded, as the loop stores it:
        // `{"conclusion": "answered" | "max_steps" | "aborted" | "over_budget" |
        // "stuck", "budget"?, "reason"?}`. `done` covers an answer, a budget
        // and a stuck run alike, and this is what tells them apart. Null while
        // running and for a workflow run.
        StructField::new("conclusion", TypeSchema::optional(TypeSchema::json())),
        // The run that delegated this one — set on a subagent's run, null on a
        // run somebody started. The chat history lists only the latter: a
        // child's transcript is read nested inside its parent's.
        StructField::new("parent_run", TypeSchema::optional(TypeSchema::uuid())),
    ])
}

/// One whole run: the summary plus the state it can be read back from, and — for
/// a workflow run — its trace and the form it is waiting on.
///
/// `context` is passed through as JSON rather than described field by field: it
/// is `sc-agent`'s `AgentLoop` or `sc-workflow`'s `WorkflowRun`, whose shape
/// belongs to the engine and changes with it, and a second declaration of it here
/// would be a second thing to keep in step. What the chat panel reads out of it —
/// the messages — is stable, and so is what the run detail reads: the trace and
/// the pending form are lifted out into fields of their own below.
fn run_schema() -> TypeSchema {
    let TypeSchema::Struct(mut fields) = run_summary_schema() else {
        return run_summary_schema();
    };
    fields.push(StructField::new("context", TypeSchema::json()));
    fields.push(StructField::new("attributes", TypeSchema::json()));
    // The workflow half. Empty and null on an agent run rather than absent, so
    // one typed shape serves both and the client has no union to narrow.
    fields.push(StructField::new(
        "trace",
        TypeSchema::array(run_trace_schema()),
    ));
    fields.push(StructField::new(
        "pending_form",
        TypeSchema::optional(pending_form_schema()),
    ));
    // A planner run's plan, read out of its `coding` state (TODO §8), so the
    // chat can show the checklist without knowing where a trait keeps it. Null
    // on every other run.
    fields.push(StructField::new(
        "plan",
        TypeSchema::optional(run_plan_schema()),
    ));
    TypeSchema::Struct(fields)
}

/// A planner run's plan: its features, and one progress entry per session.
fn run_plan_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new(
            "features",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("id", TypeSchema::text()),
                StructField::new("title", TypeSchema::text()),
                StructField::new("description", TypeSchema::text()),
                // `feature` | `bug`.
                StructField::new("kind", TypeSchema::text()),
                StructField::new("acceptance", TypeSchema::array(TypeSchema::text())),
                StructField::new("files", TypeSchema::array(TypeSchema::text())),
                StructField::new("pages", TypeSchema::array(TypeSchema::text())),
                // `todo` | `in_progress` | `done` | `failed` | `blocked`.
                StructField::new("status", TypeSchema::text()),
                StructField::new("attempts", TypeSchema::int()),
                // Its sessions' run ids, latest last: what the checklist links to.
                StructField::new("runs", TypeSchema::array(TypeSchema::text())),
            ])),
        ),
        StructField::new(
            "progress",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("feature", TypeSchema::text()),
                StructField::new("run", TypeSchema::text()),
                StructField::new("status", TypeSchema::text()),
                StructField::new("summary", TypeSchema::text()),
                StructField::new("check", TypeSchema::text()),
                StructField::new("diffstat", TypeSchema::text()),
            ])),
        ),
    ])
}

/// One `_fd_run_traces` row: one attempt at one step, and the context after it
/// (§9, §10.3).
///
/// This is what the run detail draws its timeline from — and what the read-only
/// canvas highlights the path taken with, because the steps named here in `seq`
/// order *are* the path.
fn run_trace_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("seq", TypeSchema::int()),
        StructField::new("step", TypeSchema::text()),
        StructField::new("started_at", TypeSchema::timestamp()),
        StructField::new("finished_at", TypeSchema::timestamp()),
        StructField::new("attempt", TypeSchema::int()),
        // `ok` | `error` | `suspended`.
        StructField::new("outcome", TypeSchema::text()),
        StructField::new("error", TypeSchema::optional(TypeSchema::text())),
        // The context **after** the step, which is what makes a timeline a
        // diff: the change from the row before it is the step's contribution.
        StructField::new("context", TypeSchema::json()),
    ])
}

/// The form a suspended run is waiting for somebody to fill in (§10.3, phase
/// 4.2).
///
/// The fields are the ordinary [`form_field_schema`] vocabulary, so `resumeRun`'s
/// form is rendered by the **same** `SettingsFields` component that renders a
/// trigger's action settings and a file store's — and the step's declaration is
/// carried on the run rather than looked up, so the form somebody is looking at
/// does not change under them when the workflow is edited.
fn pending_form_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("fields", TypeSchema::array(form_field_schema())),
        StructField::new("assign_to", TypeSchema::text()),
        // The role floor for answering; null means admin-only, the same safe
        // reading a trigger's own `min_role` has.
        StructField::new("min_role", TypeSchema::optional(TypeSchema::int())),
    ])
}

/// A reference to a UI framework: its registered name and its settings bag. The
/// settings' shape is the framework's own `config_spec`, so `config` is opaque
/// JSON here (the form the SPA renders comes from [`framework_info_schema`]).
fn framework_ref_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("config", TypeSchema::json()),
    ])
}

/// An application's settings tabs (`sc_app::SettingsTab`): an id, a label, and
/// the groups of the form the tab holds, in order.
pub(crate) fn settings_tabs_schema() -> TypeSchema {
    TypeSchema::array(TypeSchema::struct_of([
        StructField::new("id", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("groups", TypeSchema::array(TypeSchema::text())),
    ]))
}

/// A registered framework and the settings it declares, so the admin UI can
/// render a form for a framework it knows nothing about (§13.3). Each setting is
/// a [`form_field_schema`] — the same `FormField` vocabulary a row editor uses.
fn framework_info_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        // The editorial half: a human name and a sentence saying who each
        // framework is for. The registry owns it, so the picker can present two
        // frameworks as the different propositions they are (§2.4) while staying
        // free of any knowledge of a particular one.
        StructField::new("label", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        // Whether an application on this framework has views and pages
        // (Saltcorn UI), like the application row's own `has_views`. Its
        // settings are then the running application's — the menu, the login
        // form, the languages — so the screen keeps them off the create form
        // and edits them on the application's App settings tab.
        StructField::new("has_views", TypeSchema::bool()),
        // How an application of this framework is edited: its settings tabs,
        // in order, each with the parts of the form it holds (TODO UX2.1). The
        // framework declares them; the server completes them, so every part of
        // the form is on one.
        StructField::new("settings_tabs", settings_tabs_schema()),
        // The settings whose value names a file store. `config_spec` cannot say
        // so — by the time it is sent, a store picker has been resolved to a
        // plain list of names — and a new application's form needs to know,
        // because on those pickers it offers to create a local store rather
        // than making the admin leave the form to define one first.
        StructField::new("file_store_settings", TypeSchema::array(TypeSchema::text())),
        // The settings whose value is a file in the application's store (an
        // app icon, a keystore), each with the extensions it accepts: the form
        // fills their choices from `listStoreFiles` for the store the
        // application names.
        StructField::new(
            "file_settings",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("extensions", TypeSchema::array(TypeSchema::text())),
            ])),
        ),
        // The builds the framework offers beside its web bundle, each with the
        // settings that configure it alone. Those settings are in `config_spec`
        // too; the form shows them under their target instead of among the
        // framework's own.
        StructField::new(
            "targets",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("label", TypeSchema::text()),
                StructField::new("options", TypeSchema::array(TypeSchema::text())),
                // What the module does for the target on request: a button
                // each, shown while its `show_if` holds.
                StructField::new(
                    "operations",
                    TypeSchema::array(TypeSchema::struct_of([
                        StructField::new("name", TypeSchema::text()),
                        StructField::new("label", TypeSchema::text()),
                        StructField::new("description", TypeSchema::text()),
                        StructField::new(
                            "show_if",
                            TypeSchema::array(TypeSchema::struct_of([
                                StructField::new("name", TypeSchema::text()),
                                StructField::new("values", TypeSchema::array(TypeSchema::json())),
                            ])),
                        ),
                    ])),
                ),
            ])),
        ),
    ])
}

/// One settings field of a framework's `config_spec`: enough for the admin UI to
/// render and label an input control for it.
/// One model provider the picker offers (TODO §10).
///
/// `config_spec` and `hyperparameters` are the ordinary
/// [`form_field_schema`] vocabulary — the same one a file-store backend, an
/// agent trait and an action declare their settings in — so the model form
/// renders a provider it has never heard of, whether it is a built-in or one a
/// module supplied.
///
/// `outcome` is what a fit of *this configuration* would produce, and it is
/// present only when the request carried a dataset and a configuration that
/// resolve to one; `outcome_error` is the sentence saying why it does not,
/// which is usually "no column is named as the label" and is what the form is
/// waiting to be told.
fn model_provider_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        // The module supplying it, or null for a built-in — what the picker
        // renders "built in" against.
        StructField::new("module", TypeSchema::optional(TypeSchema::text())),
        StructField::new("config_spec", TypeSchema::array(form_field_schema())),
        StructField::new("hyperparameters", TypeSchema::array(form_field_schema())),
        // The declaration: which configuration key holds the label, and what
        // happens to it. What the form switches on before a dataset exists.
        StructField::new("outcome_spec", TypeSchema::json()),
        StructField::new("outcome", TypeSchema::optional(TypeSchema::json())),
        StructField::new("outcome_error", TypeSchema::optional(TypeSchema::text())),
        // Whether the host standardises the numeric features before handing
        // them over — a k-means says yes, a regression says no because a
        // coefficient in the data's own units is what somebody reads it for.
        StructField::new("standardise", TypeSchema::bool()),
        // Whether it takes a program's data bound from the datasets — what the
        // form renders the binding editor for (Stan TODO §18).
        StructField::new("binds_data", TypeSchema::bool()),
        // Whether a running fit can be cancelled — what the instance screen
        // renders Cancel for.
        StructField::new("cancellable", TypeSchema::bool()),
        // Why it cannot fit anything on this server, when it cannot: "CmdStan
        // was not found: …". Listed rather than hidden (Stan TODO §4).
        StructField::new("unavailable", TypeSchema::optional(TypeSchema::text())),
    ])
}

fn form_field_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("name", TypeSchema::text()),
        StructField::new("label", TypeSchema::text()),
        StructField::new("type", TypeSchema::text()),
        StructField::new("required", TypeSchema::bool()),
        StructField::new("default", TypeSchema::optional(TypeSchema::json())),
        StructField::new("options", TypeSchema::array(TypeSchema::json())),
        // The label each option is shown with, in the same order; absent or
        // empty shows each value as itself.
        StructField::new(
            "option_labels",
            TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
        ),
        StructField::new("multiline", TypeSchema::bool()),
        // Whether the value is a secret (§11.1): the form renders a password
        // input, and what it is handed for this field is the redaction
        // sentinel, never the stored key.
        StructField::new("secret", TypeSchema::bool()),
        // Whether the value is fixed once the thing exists: the form renders the
        // control read-only on an edit, and a save that changes it anyway is
        // overwritten with what is stored.
        StructField::new("create_only", TypeSchema::bool()),
        // The language this value is source code in (`"javascript"`), or null for
        // a setting that is not code: the form renders a code editor for it.
        StructField::new("code_language", TypeSchema::optional(TypeSchema::text())),
        // The heading of a group of settings that starts with this one (v1's
        // `section_header`), or null for one that continues the group before
        // it: the form draws the heading above it.
        StructField::new("section", TypeSchema::optional(TypeSchema::text())),
        // The id of the settings tab the field is shown on (an application
        // form's, `sc_app::SettingsTab`), or null for the tab holding the
        // framework choice.
        StructField::new("tab", TypeSchema::optional(TypeSchema::text())),
        // A sentence or two shown under the control (v1's `sublabel`), or null.
        StructField::new("sublabel", TypeSchema::optional(TypeSchema::text())),
        // The help topic that explains the setting at length (`getHelp`), or
        // null: the form shows a question-mark that opens it.
        StructField::new("help_topic", TypeSchema::optional(TypeSchema::text())),
        // A regular expression a non-empty text value must match in full
        // (v1's `regexp`), and what to say when it does not, or null.
        StructField::new("pattern", TypeSchema::optional(TypeSchema::text())),
        StructField::new("pattern_error", TypeSchema::optional(TypeSchema::text())),
        // The environment variable a blank value leaves the job to, and its
        // value in the server's environment now (null when unset), or null.
        StructField::new(
            "env_fallback",
            TypeSchema::optional(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("value", TypeSchema::optional(TypeSchema::text())),
            ])),
        ),
        // When the setting applies: every named setting holds one of its
        // values. Empty means always. The form hides a setting that does not
        // apply, and the server does not require it.
        StructField::new(
            "show_if",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("values", TypeSchema::array(TypeSchema::json())),
            ])),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{Area, Projection};

    /// The census (§13.6). A tag is a decision about somebody's context window,
    /// so adding one has to mean editing this list — which is the moment to
    /// write down why the tool earns its place.
    ///
    /// In the order [`admin_endpoints`] declares them: by domain module.
    #[test]
    fn the_tier_two_tags_are_the_ones_that_were_argued_for() {
        let set = admin_endpoints();
        let tagged: Vec<&str> = set
            .iter()
            .filter(|e| e.mcp.is_some())
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            tagged,
            [
                "listTableProviders",
                // Roles and accounts (TODO F1.9): an agent building an app from
                // a terminal needs the roles its queries' `min_role` names, and
                // the field report's agent had no way to make one.
                "listRoles",
                "createRole",
                "deleteRole",
                // …and disabling or removing an account it made for testing.
                "listUsers",
                "deleteUser",
                "setUserDisabled",
                "listFieldTypes",
                // Row history (TODO-history H2.4), behind its own area that is
                // off unless granted: it is row data, which the schema tools
                // are deliberately not.
                "getRowHistory",
                "listDeletedRows",
                "getRowAsOf",
                "listChangesets",
                "getChangeset",
                // …and restore and undo (H3.3), in the same area, needing the
                // grants their writes need.
                "restoreRow",
                "revertRow",
                "undoChangeset",
                // The LLM providers and their models, read-only (the Paddi
                // field report): an agent creating an agent needs the names
                // `createAgent` takes. Setting one up stays an admin's, since
                // it is where the secrets are.
                "listLlmProviders",
                "listLlmModels",
                // Modules, behind their own area that is off unless granted: an
                // agent writing Python needs a library installed to import it,
                // and asking a human to click Install is the gap this closes.
                "listModules",
                "installModule",
                "updateModule",
                "listAgents",
                "createAgent",
                "updateAgent",
                "deleteAgent",
                "listAgentTraits",
                "listRuns",
                "getRun",
                "listApplications",
                "buildApplication",
                "listTriggers",
                "listActions",
                "getWorkflow",
                "saveWorkflow",
                "revertWorkflow",
                "listWorkflowRuns",
            ]
        );
    }

    /// A build is the one kind of failure that is *news about the thing* rather
    /// than a refusal of the call, so exactly one endpoint says so. A second one
    /// would be a second endpoint whose errors stop looking like errors, and that
    /// is a decision to argue for here rather than to acquire.
    #[test]
    fn only_the_build_reports_its_failure_as_a_result() {
        let set = admin_endpoints();
        let builds: Vec<&str> = set
            .iter()
            .filter(|e| e.mcp.as_ref().is_some_and(|tag| tag.build_result))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(builds, ["buildApplication"]);
    }

    /// Tier 3 is a list of things that are *absent*, which no compiler checks.
    /// These four are the ones §13.6 names, and each would be a different
    /// proposition from administering an application.
    #[test]
    fn the_surfaces_that_were_left_out_stayed_out() {
        let set = admin_endpoints();
        for name in [
            // Row data: an agent that can add a column and one that can read
            // customer rows are not the same offer.
            "listRows",
            // The file-store IDE: the coding agent has the repository already.
            "readFile",
            "writeFile",
            // Backup, restore and user management.
            "unlockBackup",
            "restoreBackup",
            "createBackupSchedule",
            "updateBackupSchedule",
            "deleteBackupSchedule",
            "clearAll",
            // Nor restarting the server under the agent's feet.
            "restartServer",
            // `listUsers`, `setUserDisabled` and `deleteUser` are projected
            // (TODO F1.9); creating one, becoming one and setting a password
            // still are not.
            "createUser",
            "becomeUser",
            "setRandomPassword",
            // Nor mailing a password, which the request carries in clear.
            "sendCredentialsEmail",
            // And above all: a token that could mint tokens could not be
            // revoked (phase 2.4 says so in its own comment).
            "createApiToken",
            "listApiTokens",
            "revokeApiToken",
        ] {
            let endpoint = set.find(name).expect("endpoint should exist");
            assert!(
                endpoint.mcp.is_none(),
                "`{name}` is tier 3 and must not be projected as an MCP tool"
            );
        }
    }

    /// An area that is off must mean the same thing whichever tier the tool came
    /// from, so a tagged trigger or application endpoint declares its half.
    #[test]
    fn a_tagged_endpoint_declares_the_half_of_the_surface_it_belongs_to() {
        let set = admin_endpoints();
        let area_of = |name: &str| {
            set.find(name)
                .and_then(|e| e.mcp.as_ref())
                .and_then(|tag| tag.area)
        };
        assert_eq!(area_of("listTriggers"), Some(Area::Triggers));
        assert_eq!(area_of("listActions"), Some(Area::Triggers));
        // A workflow is a trigger's body (§10.3), so it is the triggers half.
        assert_eq!(area_of("saveWorkflow"), Some(Area::Triggers));
        assert_eq!(area_of("listWorkflowRuns"), Some(Area::Triggers));
        assert_eq!(area_of("buildApplication"), Some(Area::Applications));
        assert_eq!(area_of("listApplications"), Some(Area::Applications));
        // Installing a package is its own area, off unless granted.
        assert_eq!(area_of("listModules"), Some(Area::Modules));
        assert_eq!(area_of("installModule"), Some(Area::Modules));
        assert_eq!(area_of("updateModule"), Some(Area::Modules));
        for name in [
            "getRowHistory",
            "listDeletedRows",
            "getRowAsOf",
            "listChangesets",
            "getChangeset",
            "restoreRow",
            "revertRow",
            "undoChangeset",
        ] {
            assert_eq!(area_of(name), Some(Area::History), "{name}");
        }
        // The schema half has no area: it is what this surface is.
        assert_eq!(area_of("listFieldTypes"), None);
        assert_eq!(area_of("listAgents"), None);
    }

    /// The merge of path, query and body into one arguments object is only
    /// unambiguous while the three name different things.
    #[test]
    fn no_tagged_endpoint_merges_two_arguments_under_one_name() {
        for projection in Projection::all(&admin_endpoints()) {
            let names = projection.argument_names();
            let mut seen = std::collections::HashSet::new();
            for name in &names {
                assert!(
                    seen.insert(name.clone()),
                    "`{}` declares `{name}` twice across its path, query and body",
                    projection.name()
                );
            }
            // And every one of them is an object schema, which is what an MCP
            // client requires of an `inputSchema`.
            assert_eq!(
                projection.parameters()["type"],
                serde_json::json!("object"),
                "{}'s parameters",
                projection.name()
            );
        }
    }

    /// A tool that changes something declares the grant that allows it, so the
    /// six flags mean one thing across both tiers. A read declares none — every
    /// one of these is `admin()` already, and the grants are about changes.
    #[test]
    fn a_tagged_endpoint_that_changes_something_declares_the_grant_for_it() {
        let set = admin_endpoints();
        let grant_of = |name: &str| {
            set.find(name)
                .and_then(|e| e.mcp.as_ref())
                .and_then(|tag| tag.grant)
        };
        assert_eq!(grant_of("createAgent"), Some(Grant::Create));
        assert_eq!(grant_of("updateAgent"), Some(Grant::Edit));
        assert_eq!(grant_of("deleteAgent"), Some(Grant::Drop));
        assert_eq!(grant_of("saveWorkflow"), Some(Grant::Edit));
        assert_eq!(grant_of("revertWorkflow"), Some(Grant::Edit));
        // Building writes the generated client and replaces what a subdomain
        // serves: a change to what is there, whatever the method reads as.
        assert_eq!(grant_of("buildApplication"), Some(Grant::Edit));
        // Roles and accounts (TODO F1.9) need no access grant since
        // TODO-type-hardening H8.9: creating is creating, disabling is an edit,
        // and deleting still needs `allow_drop`. The last enabled admin is
        // protected by `sc_auth::refuse_last_admin`, not by a grant.
        let also_of = |name: &str| {
            set.find(name)
                .and_then(|e| e.mcp.as_ref())
                .and_then(|t| t.also)
        };
        assert_eq!(grant_of("createRole"), Some(Grant::Create));
        assert_eq!(grant_of("setUserDisabled"), Some(Grant::Edit));
        assert_eq!(grant_of("deleteRole"), Some(Grant::Drop));
        assert_eq!(grant_of("deleteUser"), Some(Grant::Drop));
        for name in ["createRole", "setUserDisabled", "deleteRole", "deleteUser"] {
            assert_eq!(also_of(name), None, "{name}");
        }

        for read_only in [
            "listAgents",
            "listAgentTraits",
            "listRuns",
            "getRun",
            "listApplications",
            "listTriggers",
            "listActions",
            "listFieldTypes",
            "listTableProviders",
            "getWorkflow",
            "listWorkflowRuns",
            "listRoles",
            "listUsers",
        ] {
            assert_eq!(grant_of(read_only), None, "`{read_only}` only reads");
        }

        // And nothing tagged is a write with no grant behind it: a method that
        // is not GET has to have said which flag allows it.
        for endpoint in set.iter().filter(|e| e.mcp.is_some()) {
            if endpoint.method != Method::Get {
                assert!(
                    endpoint.mcp.as_ref().and_then(|t| t.grant).is_some(),
                    "`{}` changes something and declares no grant",
                    endpoint.name
                );
            }
        }
    }

    /// §13.6's load-bearing rule, asserted rather than trusted: every tool this
    /// server offers is behind the admin requirement it was already behind.
    #[test]
    fn every_projected_endpoint_is_still_admin_only() {
        for projection in Projection::all(&admin_endpoints()) {
            assert_eq!(
                projection.auth(),
                &crate::AuthRequirement::admin(),
                "`{}` is projected as an MCP tool and must be admin-only",
                projection.name()
            );
        }
    }
}

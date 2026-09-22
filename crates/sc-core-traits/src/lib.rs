//! The core built-in agent traits (layer 9; technical design §11.3, TODO Phase 3).
//!
//! Every trait Saltcorn ships an agent with, in one crate — the counterpart of
//! `sc-core-actions` and, deliberately, at the same layer for the same reason.
//! [`builtin_traits`] is the single constructor that assembles the set.
//!
//! ## Why a crate of its own, above the row layer
//!
//! A trait that touches rows must go through **`sc-api`**, not around it: a read
//! is the same read an API caller makes, under the same §7.3 access rule
//! (`sc_api::read_rows_as`), and a write is the same write, under the same rule's
//! other half (`sc_api::insert_row_as` and its siblings), with the same coercion,
//! the same rich-type and `File`-field validation and the same emitted events.
//! That fixes these *above* layer 8, while `sc-agent` (layer 7) stays what a
//! plugin needs to write a trait of its own: the
//! [`AgentTrait`](sc_agent::AgentTrait) seam, and nothing that knows which traits
//! exist.
//!
//! `sc-agent` therefore registers **nothing**, and this crate is the first
//! consumer of that seam rather than a privileged one.
//!
//! ## The set
//!
//! Ten traits over six things an agent can be given. **Tables**:
//! [`QueryTable`] reads one, and [`InsertRow`], [`UpdateRows`] and
//! [`DeleteRows`] are three separate opt-in grants over one — so a read-only
//! agent is the default shape and each way of changing data is a deliberate act
//! with a form field attached. **Actions**: [`RunTrigger`] exposes one configured
//! trigger, which is what connects an agent to the whole of §10 (and, once §10.3
//! lands, to workflows unchanged, because a workflow is a trigger). **Code**:
//! [`Coding`] is the whole loop over one configured file store (optionally
//! rooted at a sub-directory) — reading, listing and searching always, writing
//! and editing under one checkbox, and running a script the project's own
//! `package.json` declares under another, which is the bounded thing that ships
//! instead of a shell (decision 6) — and [`BuildApplication`] builds the
//! application whose source that store is and hands back its diagnostics. And
//! **the chat's own screen**: [`PreviewPane`] contributes no tool at all — it
//! says that this agent's work can be looked at, and at which URL, and the chat
//! puts that page beside the conversation. And
//! **the application itself**: [`AdminCopilot`] describes and edits the catalog
//! *and* the trigger set — the first *app-building* trait, and the first that
//! does not name a table in its configuration, because the tables it makes do
//! not exist when it is configured. It is also where the question "how does a
//! model configure one of an open-ended set of actions, each with its own
//! settings?" is answered, by handing it those settings when it asks rather than
//! by a second, hidden inference call. And **other agents**: [`Subagent`] hands one bounded task to one
//! configured agent, which does it in a context of its own and reports back —
//! the trait that makes an agent something an agent can be given, and the reason
//! `sc-agent` grew a [`Delegator`](sc_agent::Delegator) seam.
//!
//! ## And one thing that is not a trait
//!
//! [`RunAgent`] is an `Action`, not an [`AgentTrait`](sc_agent::AgentTrait): it
//! is how a **trigger runs an agent** (§11.5), the other direction from
//! [`RunTrigger`]. It is in this crate for the same reason everything else here
//! is — it drives a loop whose tools reach the row layer — and it is registered
//! through [`register_agent_actions`] rather than with `sc-core-actions`' set,
//! because it needs two things assembled first that no other action does: the
//! trait registry the agents were validated against, and how this deployment
//! connects a provider.
//!
//! ## What every trait here has in common
//!
//! - **It names its target in its configuration.** There is no trait that can
//!   reach *any* table, because "which tables may this agent see?" is the first
//!   question an admin needs to be able to answer off the agent's definition.
//!   **[`AdminCopilot`] is the one exception**, and a deliberate one: a
//!   trait that creates tables cannot name them in advance, so it is scoped by
//!   *what it may do* — four grants — rather than by what it may reach, and it
//!   refuses any caller who is not an admin (§11.3).
//! - **Its tool names are derived from that configuration** (`query_books`, not
//!   `query`), so one trait enabled twice offers two distinguishable tools —
//!   which is what makes a collision refusable on save (§11.2). See
//!   [`tool_names`]. `AdminCopilot`'s six names are fixed for the same reason it
//!   names no table; enabling it twice therefore *collides*, which is the
//!   intended outcome.
//! - **A trait may offer several tools, and may withhold some of them.**
//!   [`Coding`] offers seven over one scope and declares only the ones its grants
//!   allow, which is how "may this agent change the source?" became a checkbox
//!   rather than a second trait with the same form on it.
//! - **Its tool is described by what it is configured against**: the table's own
//!   fields, with their types, in the description *and* in the JSON schema. A
//!   model left to guess a column name will guess, and the guess costs a turn.
//! - **It runs as the run's caller** ([`RunCaller`](sc_agent::RunCaller)), never
//!   as the server. An agent is not a way around ownership or row-level
//!   security: the same table read by two callers gives two answers, and a write
//!   reaches only the rows that caller could have been shown.
//! - **Everything it refuses, it refuses by name**, listing the alternatives
//!   where there are any. These errors are read by a model that can only recover
//!   if it is told, so they are written for that reader.
//! - **Its `validate_config` checks what the spec cannot** — that the table
//!   exists, that it is addressable by primary key, that a named field is real
//!   and writable, that the trigger exists — on save *and* on load, so an agent
//!   whose world changed underneath it leaves the live set with a reason instead
//!   of failing mid-conversation.

mod admin_copilot;
mod build_application;
mod coding;
mod delete_rows;
mod files;
mod insert_row;
mod preview_pane;
mod query_table;
mod run_agent;
mod run_trigger;
mod subagent;
mod table;
mod update_rows;
mod write;

use std::sync::Arc;

use sc_action::ActionRegistry;
use sc_agent::{AgentRegistry, ProviderConnector};
use sc_error::Result;

pub use table::{CFG_FIELDS, CFG_MAX_ROWS, CFG_TABLE};

pub use files::{CFG_ROOT, CFG_STORE, FileScope, configured_scope, slugify};

pub use admin_copilot::{
    AdminCopilot, CFG_ALLOW_ACCESS, CFG_ALLOW_APPLICATIONS, CFG_ALLOW_CREATE, CFG_ALLOW_DROP,
    CFG_ALLOW_EDIT, CFG_ALLOW_TRIGGERS, TOOL_DELETE_QUERY, TOOL_DELETE_TRIGGER, TOOL_DESCRIBE,
    TOOL_DESCRIBE_ACTION, TOOL_DESCRIBE_APPS, TOOL_DESCRIBE_TRIGGERS, TOOL_EDIT, TOOL_SAVE_QUERY,
    TOOL_SAVE_TRIGGER,
};
pub use build_application::{BuildApplication, CFG_APPLICATION};
pub use coding::{
    Baseline, CFG_CHECKS, CFG_DIAGNOSE, CFG_EDIT_FORMAT, CFG_MAX_LINES, CFG_MAX_RESULTS,
    CFG_MAY_CHECK, CFG_MAY_EDIT, CFG_MAY_RUN_SCRIPTS, CFG_MAY_USE_SHELL, CFG_MAY_VIEW_APP,
    CFG_REPO_MAP_TOKENS, CFG_SHELL_IMAGE, CFG_SHELL_NETWORK, CFG_SHELL_RUNTIME, CFG_SHELL_SANDBOX,
    CFG_SHELL_TIMEOUT, CFG_SHELL_TIMEOUT_MAX, CFG_TIMEOUT, CFG_VIEW_APP_TIMEOUT, CFG_VIEW_APP_USER,
    ChangeStatus, Coding, CodingState, DEFAULT_DIAGNOSE, DEFAULT_MAX_LINES,
    DEFAULT_REPO_MAP_TOKENS, DEFAULT_SHELL_TIMEOUT, DEFAULT_SHELL_TIMEOUT_MAX,
    DEFAULT_TIMEOUT_SECONDS, DEFAULT_VIEW_APP_TIMEOUT, EDIT_FORMAT_AUTO, EditStats, FileChange,
    LONGEST_TOOL_PREFIX, Ledger, MAX_OUTPUT_CHARS, MAX_REPO_MAP_TOKENS, PreImage, RunDiff,
    SHELL_ENV, ScopeDiff, agent_run_diff, diff_ledger, edit_format, kill_all_processes, matching,
    run_diff, run_tree, running_count,
};
pub use coding::{
    CFG_COMMIT, CFG_MAX_SESSIONS, CFG_WORKFLOW, DEFAULT_MAX_SESSIONS, FAILURES_TO_FAIL, Feature,
    FeatureKind, FeatureStatus, Plan, Progress, WORKFLOW_DIRECT, WORKFLOW_PLANNED, checklist,
    run_plan,
};
pub use delete_rows::DeleteRows;
pub use insert_row::InsertRow;
pub use preview_pane::{
    CFG_RELOAD_ON_TURN, CFG_URL, DEFAULT_RELOAD_ON_TURN, PreviewPane, configured_url, is_framable,
    reloads_on_turn,
};
pub use query_table::{DEFAULT_MAX_ROWS, QueryTable};
pub use run_agent::{CFG_AGENT, CFG_PROMPT, RunAgent};
pub use run_trigger::{CFG_TRIGGER, RunTrigger};
// `subagent`'s own agent-name key is spelled `agent` too, so [`CFG_AGENT`] above
// is it: one string, exported once, rather than two constants a reader would have
// to check are equal.
pub use subagent::{
    ARG_CONTEXT, ARG_OUTPUT, ARG_TASK, CFG_MAX_DEPTH, CFG_MAX_STEPS, CFG_WHEN_TO_USE,
    MAX_CONFIGURABLE_DEPTH, Subagent,
};
pub use update_rows::{DEFAULT_MAX_WRITE_ROWS, UpdateRows};

/// What each built-in trait calls the tool it derives from its configuration —
/// the answer to "what will this be called?" the admin UI wants before an agent
/// is saved and the collision check (§11.2) wants at the moment of saving.
pub mod tool_names {
    pub use crate::admin_copilot::tool_names as admin_copilot;
    pub use crate::build_application::tool_name as build_application;
    pub use crate::coding::tool_names as coding;
    pub use crate::coding::{
        apply_patch_tool_name as apply_patch, check_checks_tool_name as check,
        edit_file_tool_name as edit_file, explore_tool_name as explore,
        find_files_tool_name as find_files, implement_feature_tool_name as implement_feature,
        list_assets_tool_name as list_assets, process_tool_name as process,
        read_file_tool_name as read_file, repo_map_tool_name as repo_map,
        run_script_tool_name as run_project_script, save_plan_tool_name as save_plan,
        search_files_tool_name as search_files, shell_tool_name as shell,
        view_app_tool_name as view_app, write_file_tool_name as write_file,
    };
    pub use crate::delete_rows::tool_name as delete_rows;
    pub use crate::insert_row::tool_name as insert_row;
    pub use crate::query_table::tool_name as query_table;
    pub use crate::run_trigger::tool_name as run_trigger;
    pub use crate::subagent::tool_name as subagent;
    pub use crate::update_rows::tool_name as update_rows;
}

/// The built-in trait set a server installs.
///
/// One constructor, so a deployment cannot end up with half the built-ins
/// depending on what it remembered to register.
pub fn builtin_traits() -> Result<AgentRegistry> {
    let mut registry = AgentRegistry::new();
    register_builtin_traits(&mut registry)?;
    Ok(registry)
}

/// Add the built-in traits to an existing registry — for a deployment (or a
/// test) that assembles its own set from these plus its plugins'.
///
/// Fails if one of the names is already taken, as any duplicate registration
/// does: which implementation answers to `query_table` must not depend on load
/// order.
/// Register the actions that **run** an agent, rather than the traits an agent
/// runs (§11.5).
///
/// There is exactly one — [`RunAgent`] — and it lives in this crate for the
/// reason the traits do: it drives a loop whose tools reach the row layer. It is
/// registered separately from `sc-core-actions`' built-in set because it needs
/// two things assembled first: the trait registry the agents were validated
/// against, and how this deployment connects a provider. A server calls
/// [`builtin_traits`], puts the result in its agent services, and passes both
/// here while building the action registry the trigger dispatcher will hold.
pub fn register_agent_actions(
    registry: &mut ActionRegistry,
    traits: Arc<AgentRegistry>,
    providers: Arc<dyn ProviderConnector>,
) -> Result<()> {
    registry.register(Arc::new(RunAgent::new(traits, providers)))
}

pub fn register_builtin_traits(registry: &mut AgentRegistry) -> Result<()> {
    registry.register(Arc::new(AdminCopilot))?;
    registry.register(Arc::new(QueryTable))?;
    registry.register(Arc::new(InsertRow))?;
    registry.register(Arc::new(PreviewPane))?;
    registry.register(Arc::new(UpdateRows))?;
    registry.register(Arc::new(DeleteRows))?;
    registry.register(Arc::new(RunTrigger))?;
    registry.register(Arc::new(Coding))?;
    registry.register(Arc::new(BuildApplication))?;
    registry.register(Arc::new(Subagent))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtins_are_registered_under_their_stored_names() {
        let registry = builtin_traits().unwrap();
        assert_eq!(
            registry.names(),
            vec![
                "admin_copilot",
                "build_application",
                "coding",
                "delete_rows",
                "insert_row",
                "preview_pane",
                "query_table",
                "run_trigger",
                "subagent",
                "update_rows",
            ]
        );
        // Every one of them describes itself and its configuration as data,
        // which is what lets the admin UI render a form for a trait it has never
        // heard of — and every one that names a *target* names it as required, so
        // a blank form cannot be saved. `coding` names one too (its store); what
        // its blank checkboxes then decide is what it may *do* there.
        //
        // `admin_copilot` is the exception, and the reason is the phase's point:
        // it names no table, because the tables it makes do not exist when it is
        // configured. Its form is four grants and two areas, each with a default,
        // and a blank one is a meaningful (read-only over all three) configuration
        // rather than an incomplete one.
        for trait_ in registry.all() {
            assert!(!trait_.description().is_empty(), "{}", trait_.name());
            let spec = trait_.config_spec();
            assert!(!spec.is_empty(), "{}", trait_.name());
            if trait_.name() != "admin_copilot" {
                assert!(spec.iter().any(|f| f.required), "{}", trait_.name());
            }
        }
    }

    #[test]
    fn registering_the_builtins_twice_is_refused() {
        let mut registry = builtin_traits().unwrap();
        let err = register_builtin_traits(&mut registry).unwrap_err();
        assert!(err.to_string().contains("admin_copilot"), "{err}");
    }

    #[test]
    fn each_trait_declares_the_settings_its_semantics_need() {
        let registry = builtin_traits().unwrap();
        let spec = |name: &str| -> Vec<String> {
            registry
                .require(name)
                .unwrap()
                .config_spec()
                .iter()
                .map(|f| f.name().to_owned())
                .collect()
        };
        assert_eq!(
            spec("query_table"),
            vec![CFG_TABLE, CFG_FIELDS, CFG_MAX_ROWS]
        );
        // An insert has no row bound to declare — it writes one row — and a
        // delete has no field allow-list, because it takes the whole row and a
        // setting that narrowed nothing would suggest a grant that does not
        // exist.
        assert_eq!(spec("insert_row"), vec![CFG_TABLE, CFG_FIELDS]);
        assert_eq!(
            spec("update_rows"),
            vec![CFG_TABLE, CFG_FIELDS, CFG_MAX_ROWS]
        );
        assert_eq!(spec("delete_rows"), vec![CFG_TABLE, CFG_MAX_ROWS]);
        assert_eq!(spec("run_trigger"), vec![CFG_TRIGGER]);
        // The sub-agent it delegates to, then the sentence that tells the
        // *parent* model when to reach for it — the field that decides whether
        // delegation happens at the right moment — then the two bounds: what one
        // delegation may spend, and how deep a chain of them may go.
        assert_eq!(
            spec("subagent"),
            vec![CFG_AGENT, CFG_WHEN_TO_USE, CFG_MAX_STEPS, CFG_MAX_DEPTH]
        );

        // The whole coding loop is **one** form: the scope filled in once — one
        // store, optionally one directory in it — then what the agent may do
        // there, what `check` runs, then the bound on each thing that brings something
        // back. Eight tools, one place to say where they work, so the scope cannot disagree
        // with itself.
        assert_eq!(
            spec("coding"),
            vec![
                CFG_STORE,
                CFG_ROOT,
                CFG_MAY_EDIT,
                CFG_MAY_RUN_SCRIPTS,
                CFG_MAY_CHECK,
                CFG_CHECKS,
                CFG_APPLICATION,
                CFG_DIAGNOSE,
                // Planning (TODO §5, §8).
                CFG_WORKFLOW,
                CFG_MAX_SESSIONS,
                CFG_COMMIT,
                CFG_EDIT_FORMAT,
                CFG_MAX_LINES,
                CFG_MAX_RESULTS,
                CFG_REPO_MAP_TOKENS,
                CFG_TIMEOUT,
                // Looking at the application (TODO §7b).
                CFG_MAY_VIEW_APP,
                CFG_VIEW_APP_USER,
                CFG_VIEW_APP_TIMEOUT,
                // Last: the shell is every grant above at once (TODO §7a).
                CFG_MAY_USE_SHELL,
                CFG_SHELL_TIMEOUT,
                CFG_SHELL_TIMEOUT_MAX,
                CFG_SHELL_SANDBOX,
                CFG_SHELL_IMAGE,
                CFG_SHELL_RUNTIME,
                CFG_SHELL_NETWORK,
            ]
        );
        // The build names an application rather than a store: which store the
        // source is in is the application's own configuration (§13.3), and
        // asking the admin for it twice would be two places to get it wrong.
        assert_eq!(spec("build_application"), vec![CFG_APPLICATION]);
        // The trait that names no table: four grants, scoping it by what it may
        // do rather than by what it may reach (§11.3) — over the schema, the
        // triggers and an application's custom SQL queries alike, which is why
        // there are still four of them. Then the two **areas**, which are the
        // other question and therefore two more checkboxes rather than eight more
        // grants: not "what may it do?" but "to which of the three?".
        assert_eq!(
            spec("admin_copilot"),
            vec![
                CFG_ALLOW_CREATE,
                CFG_ALLOW_EDIT,
                CFG_ALLOW_DROP,
                CFG_ALLOW_ACCESS,
                CFG_ALLOW_TRIGGERS,
                CFG_ALLOW_APPLICATIONS,
            ]
        );
    }

    /// Every tool name a built-in derives carries **what it does** and **what it
    /// does it to**, and no two traits over one table collide.
    ///
    /// Worth pinning: these are the names the model chooses between, and the
    /// collision check (§11.2) refuses a save that produces two of the same. If
    /// `insert_row` and `update_rows` over `books` both derived `books_write`,
    /// an agent could not have both.
    #[test]
    fn the_derived_tool_names_are_distinct_and_say_what_they_do() {
        let scope = FileScope {
            store: "app-src".to_owned(),
            root: "web".to_owned(),
        };
        let names = [
            tool_names::query_table("books"),
            tool_names::insert_row("books"),
            tool_names::update_rows("books"),
            tool_names::delete_rows("books"),
            tool_names::run_trigger("reindex"),
            tool_names::subagent("researcher"),
            tool_names::read_file(&scope),
            tool_names::write_file(&scope),
            tool_names::find_files(&scope),
            tool_names::edit_file(&scope),
            tool_names::apply_patch(&scope),
            tool_names::search_files(&scope),
            tool_names::repo_map(&scope),
            tool_names::list_assets(&scope),
            tool_names::run_project_script(&scope),
            tool_names::check(&scope),
            tool_names::view_app(&scope),
            tool_names::shell(&scope),
            tool_names::process(&scope),
            tool_names::save_plan(&scope),
            tool_names::implement_feature(&scope),
            tool_names::explore(&scope),
            tool_names::build_application("todo"),
        ];
        assert_eq!(
            names,
            [
                "query_books",
                "insert_into_books",
                "update_books",
                "delete_from_books",
                "run_reindex",
                "delegate_to_researcher",
                "read_file_app_src_web",
                "write_file_app_src_web",
                "find_files_app_src_web",
                "edit_file_app_src_web",
                "apply_patch_app_src_web",
                "search_files_app_src_web",
                "repo_map_app_src_web",
                "list_assets_app_src_web",
                "run_script_app_src_web",
                "check_app_src_web",
                "view_app_app_src_web",
                "shell_app_src_web",
                "process_app_src_web",
                "save_plan_app_src_web",
                "implement_feature_app_src_web",
                "explore_app_src_web",
                "build_todo",
            ]
        );
        let unique: std::collections::BTreeSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        // The sixteen file names above are `coding`'s whole set, which is what the
        // collision check compares when the trait is enabled twice: two
        // instances over one scope produce these same names and are refused.
        assert_eq!(
            tool_names::coding(&scope),
            [
                tool_names::read_file(&scope),
                tool_names::find_files(&scope),
                tool_names::search_files(&scope),
                tool_names::repo_map(&scope),
                tool_names::list_assets(&scope),
                tool_names::save_plan(&scope),
                tool_names::implement_feature(&scope),
                tool_names::explore(&scope),
                tool_names::write_file(&scope),
                tool_names::edit_file(&scope),
                tool_names::apply_patch(&scope),
                tool_names::run_project_script(&scope),
                tool_names::check(&scope),
                tool_names::view_app(&scope),
                tool_names::shell(&scope),
                tool_names::process(&scope),
            ]
        );
        // `admin_copilot`'s names are fixed rather than derived, and say the
        // same nine things every deployment's do — two over the schema, four over
        // the triggers, three over an application's custom SQL queries.
        assert_eq!(
            tool_names::admin_copilot(),
            [
                "describe_schema",
                "edit_schema",
                "describe_triggers",
                "describe_action",
                "save_trigger",
                "delete_trigger",
                "describe_applications",
                "save_api_query",
                "delete_api_query",
            ]
        );
        for name in tool_names::admin_copilot() {
            assert!(!names.iter().any(|n| n == name));
        }
    }
}

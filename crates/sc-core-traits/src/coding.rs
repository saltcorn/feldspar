//! `coding` — one trait for the whole coding loop over one file store (§11.3).
//!
//! Read, list, search, write, edit and run, in **one** grant with **one** form.
//! The six of them were six traits once, each with the same store-and-sub-directory
//! configuration, and that shape was wrong for the reason a form is wrong when it
//! asks the same question six times: an admin setting up a coding agent filled in
//! the same store and the same root six times over, and a change of mind about the
//! root was six edits, five of which could be forgotten. The capability an admin
//! actually grants is "this agent works on the code in `web/todo`", and that is
//! now one row in the traits list.
//!
//! ## What it offers, and what it takes to unlock
//!
//! Four tools are always there, and they are the read-only ones: `read_file`,
//! `find_files`, `search_files` and `repo_map` ([`repo_map`], a ranked map of the
//! project's definitions, which also opens every session). **Changing the source is a checkbox**
//! ([`CFG_MAY_EDIT`]), which adds `write_file` and one edit tool: `edit_file`
//! (the match cascade) or `apply_patch` (V4A), as [`CFG_EDIT_FORMAT`] resolves
//! for the model, or neither under `whole_file`. **Running a script is another
//! checkbox** ([`CFG_MAY_RUN_SCRIPTS`]), and **checking a third**
//! ([`CFG_MAY_CHECK`]): the `check` tool over the admin's [`CFG_CHECKS`] (and the
//! application build, when the `application` setting names one), and formatting
//! and type-checking after a turn's edits. All are off by default, so a
//! read-only coding agent stays the default shape. Their being configuration
//! rather than separate traits is `admin_copilot`'s move, made for the same
//! reason: the grants share a scope, and a scope filled in twice is a scope that
//! can disagree with itself.
//!
//! **Looking at the application is a fourth** ([`CFG_MAY_VIEW_APP`]): `view_app`
//! opens the run's preview of the application — mounted by a green `check` — in
//! the server's headless browser, as the person chatting ([`view_app`]).
//!
//! **The shell is the last checkbox** ([`CFG_MAY_USE_SHELL`]), because it is all
//! the others at once: `shell` and `process` ([`shell`], [`process`]), offered
//! only to a run whose caller is an admin, and implying none of the other grants.
//!
//! A tool call whose grant is off is refused **by name, naming the checkbox** —
//! the model never sees the tool, but a stale transcript can still carry one, and
//! "you may not do that" is not something a model can act on while "the agent's
//! `may_edit` setting is off" is something its user can.
//!
//! ## The edit engine
//!
//! Every change goes through the run's [`CodingState`]: a file must have been
//! read (and be unchanged since) before it is edited or overwritten, the
//! [`Ledger`] keeps each touched file's pre-image so [`run_diff`] can produce the
//! run's diff on any store backend, and the files a turn edited are formatted and
//! type-checked once, after the turn's last tool call (TODO §6).
//!
//! ## And what is still its own trait
//!
//! `build_application` ([`BuildApplication`](crate::BuildApplication)) is not part
//! of this one, deliberately: it is configured on a different axis — an
//! application's subdomain, not a store — and folding it in would mean an agent
//! that builds two applications out of one source tree needed two `coding`
//! instances, which would then collide on the file tools' names.
//!
//! ## The scope
//!
//! Everything about the store, the root, path resolution and §9's access rule is
//! [`crate::files`], unchanged: the tools' names are derived from the scope
//! (`edit_file_apps_web`), so one agent may have this trait twice over two
//! directories and the same directory twice is a collision refused on save
//! (§11.2).

mod assets;
mod change;
mod check;
mod commit;
mod edit;
mod explore;
mod feature;
mod feedback;
mod find;
mod header;
mod inspect;
mod ledger;
pub mod matching;
mod patch;
mod plan;
mod process;
mod prompt;
mod read;
mod repo_map;
mod script;
mod search;
mod shell;
mod snapshot;
mod state;
mod view_app;
mod write;

use sc_agent::{
    AfterToolsContext, AgentTrait, Elidable, RunCaller, RunId, RunMode, SessionContext,
    ToolsContext, TraitCheck, TraitContext,
};
use sc_error::{Error, Result};
use sc_files::DEFAULT_MAX_RESULTS;
use sc_llm::{EditFormat, ToolSpec};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::files::{
    FileScope, check_scope, check_tool_name, config_count, configured_scope, scope_as_written,
    scope_fields,
};
use crate::table::config_str;

pub use assets::tool_name as list_assets_tool_name;
pub use check::{Baseline, CFG_CHECKS, tool_name as check_checks_tool_name};
pub use commit::CFG_COMMIT;
pub use edit::tool_name as edit_file_tool_name;
pub use explore::tool_name as explore_tool_name;
pub use feature::{
    CFG_MAX_SESSIONS, DEFAULT_MAX_SESSIONS, FAILURES_TO_FAIL,
    tool_name as implement_feature_tool_name,
};
pub use find::tool_name as find_files_tool_name;
pub use inspect::{ScopeDiff, agent_run_diff, run_plan, run_tree};
pub use ledger::{ChangeStatus, FileChange, Ledger, PreImage, RunDiff, diff_ledger, run_diff};
pub use patch::tool_name as apply_patch_tool_name;
pub use plan::{
    Feature, Kind as FeatureKind, Plan, Progress, Status as FeatureStatus, checklist,
    tool_name as save_plan_tool_name,
};
pub use process::{kill_all_processes, running_count, tool_name as process_tool_name};
pub use read::{CFG_MAX_LINES, DEFAULT_MAX_LINES, tool_name as read_file_tool_name};
pub use repo_map::{
    CFG_REPO_MAP_TOKENS, DEFAULT_REPO_MAP_TOKENS, MAX_REPO_MAP_TOKENS,
    tool_name as repo_map_tool_name,
};
pub use script::{
    CFG_TIMEOUT, DEFAULT_TIMEOUT_SECONDS, MAX_OUTPUT_CHARS, tool_name as run_script_tool_name,
};
pub use search::{CFG_MAX_RESULTS, tool_name as search_files_tool_name};
pub use shell::{
    CFG_SHELL_IMAGE, CFG_SHELL_NETWORK, CFG_SHELL_RUNTIME, CFG_SHELL_SANDBOX, CFG_SHELL_TIMEOUT,
    CFG_SHELL_TIMEOUT_MAX, DEFAULT_SHELL_TIMEOUT, DEFAULT_SHELL_TIMEOUT_MAX, SHELL_ENV,
    tool_name as shell_tool_name,
};
pub use state::{CodingState, EditStats};
pub use view_app::{
    CFG_VIEW_APP_TIMEOUT, CFG_VIEW_APP_USER, DEFAULT_VIEW_APP_TIMEOUT,
    tool_name as view_app_tool_name,
};
pub use write::tool_name as write_file_tool_name;

/// May create and change files: adds `write_file` and the edit tool. Off by
/// default, because a read-only agent is the shape that cannot damage anything.
pub const CFG_MAY_EDIT: &str = "may_edit";

/// May run one of the project's own `package.json` scripts. Off by default, and
/// separate from [`CFG_MAY_EDIT`] because running a script executes code the
/// agent did not write.
pub const CFG_MAY_RUN_SCRIPTS: &str = "may_run_scripts";

/// May run the checks someone other than the model chose: the `check` tool over
/// [`CFG_CHECKS`] and the application build, and the post-turn formatting and
/// [`CFG_DIAGNOSE`] script (TODO §6, §7). Off by default. A smaller grant than
/// [`CFG_MAY_RUN_SCRIPTS`], because the model does not choose what runs.
pub const CFG_MAY_CHECK: &str = "may_check";

/// May run shell commands and managed processes (TODO §7a). Off by default,
/// offered only to an admin caller, and implying no other grant.
pub const CFG_MAY_USE_SHELL: &str = "may_use_shell";

/// May look at the application's preview in a headless browser (TODO §7b).
/// Off by default; needs [`crate::CFG_APPLICATION`] and a browser on the server.
pub const CFG_MAY_VIEW_APP: &str = "may_view_app";

/// The `package.json` script that type-checks the project after a turn's edits.
pub const CFG_DIAGNOSE: &str = "diagnose";

/// [`CFG_DIAGNOSE`] when the admin names none.
pub const DEFAULT_DIAGNOSE: &str = "typecheck";

/// What a chat run starts in: `direct` (`act`, doing the work) or `planned`
/// (`plan`, writing a plan and implementing it a feature per session; TODO §5).
pub const CFG_WORKFLOW: &str = "workflow";

/// The [`CFG_WORKFLOW`] that starts in `act`, and the default.
pub const WORKFLOW_DIRECT: &str = "direct";

/// The [`CFG_WORKFLOW`] that starts in `plan`.
pub const WORKFLOW_PLANNED: &str = "planned";

/// How the model edits: `auto`, `str_replace`, `apply_patch` or `whole_file`.
pub const CFG_EDIT_FORMAT: &str = "edit_format";

/// The [`CFG_EDIT_FORMAT`] that follows the model's capabilities.
pub const EDIT_FORMAT_AUTO: &str = "auto";

/// The longest tool-name prefix this trait will derive, including the tools of
/// later phases (`implement_feature_…`, TODO §8). Validation checks a scope
/// against it, so a scope that fits today does not stop fitting when that tool
/// arrives.
pub const LONGEST_TOOL_PREFIX: &str = "implement_feature_";

/// Work on the code in one file store: read it, search it, and — under its
/// grants — change it and run its scripts.
pub struct Coding;

/// Every tool this trait can offer for a scope, in the order it offers them.
///
/// The whole set regardless of the grants and the edit format, because this is
/// what the admin UI wants to *show*: a tool a grant currently withholds still
/// names the same thing.
pub fn tool_names(scope: &FileScope) -> Vec<String> {
    vec![
        read::tool_name(scope),
        find::tool_name(scope),
        search::tool_name(scope),
        repo_map::tool_name(scope),
        assets::tool_name(scope),
        plan::tool_name(scope),
        feature::tool_name(scope),
        explore::tool_name(scope),
        write::tool_name(scope),
        edit::tool_name(scope),
        patch::tool_name(scope),
        script::tool_name(scope),
        check::tool_name(scope),
        view_app::tool_name(scope),
        shell::tool_name(scope),
        process::tool_name(scope),
    ]
}

/// The edit format a configuration resolves to for a model: its own setting,
/// or under `auto` (and when unset) the model's preferred format.
pub fn edit_format(config: &Attrs, preferred: EditFormat) -> EditFormat {
    match config_str(config, CFG_EDIT_FORMAT).as_str() {
        "str_replace" => EditFormat::StrReplace,
        "apply_patch" => EditFormat::ApplyPatch,
        "whole_file" => EditFormat::WholeFile,
        _ => preferred,
    }
}

#[async_trait::async_trait]
impl AgentTrait for Coding {
    fn name(&self) -> &str {
        "coding"
    }

    fn description(&self) -> &str {
        "Work on the code in one file store: read, find and search it, and — if permitted — \
         edit it, check it and run its scripts"
    }

    fn config_spec(&self) -> Vec<FormField> {
        let mut spec = scope_fields();
        spec.push(
            FormField::new(CFG_MAY_EDIT, BasicType::Bool)
                .label("May create and change files")
                .default_value(false),
        );
        spec.push(
            FormField::new(CFG_MAY_RUN_SCRIPTS, BasicType::Bool)
                .label("May run the project's package.json scripts")
                .default_value(false),
        );
        spec.push(
            FormField::new(CFG_MAY_CHECK, BasicType::Bool)
                .label("May run the configured checks, and format and type-check after edits")
                .default_value(false),
        );
        spec.push(
            FormField::new(CFG_CHECKS, BasicType::Json)
                .label("Checks (package.json script names, in order)")
                .default_value(Json::Array(Vec::new())),
        );
        spec.push(
            FormField::new(crate::CFG_APPLICATION, BasicType::Text)
                .label("Application built by check (subdomain, optional)"),
        );
        spec.push(
            FormField::new(CFG_DIAGNOSE, BasicType::Text)
                .label("Type-check script")
                .default_value(DEFAULT_DIAGNOSE),
        );
        spec.push(
            FormField::new(CFG_WORKFLOW, BasicType::Text)
                .label(
                    "Workflow: direct does the work; planned plans it, then a session per feature",
                )
                .options([WORKFLOW_DIRECT, WORKFLOW_PLANNED].map(str::to_owned))
                .default_value(WORKFLOW_DIRECT),
        );
        spec.push(
            FormField::new(CFG_MAX_SESSIONS, BasicType::Int)
                .label("Sessions per planned feature, retries included")
                .default_value(DEFAULT_MAX_SESSIONS as i64),
        );
        spec.push(
            FormField::new(CFG_COMMIT, BasicType::Bool)
                .label("Commit each finished feature (git work trees only)")
                .default_value(true),
        );
        spec.push(
            FormField::new(CFG_EDIT_FORMAT, BasicType::Text)
                .label("Edit format")
                .options(
                    [EDIT_FORMAT_AUTO, "str_replace", "apply_patch", "whole_file"]
                        .map(str::to_owned),
                )
                .default_value(EDIT_FORMAT_AUTO),
        );
        spec.push(
            FormField::new(CFG_MAX_LINES, BasicType::Int)
                .label("Maximum lines per file read")
                .default_value(DEFAULT_MAX_LINES as i64),
        );
        spec.push(
            FormField::new(CFG_MAX_RESULTS, BasicType::Int)
                .label("Maximum search matches")
                .default_value(DEFAULT_MAX_RESULTS as i64),
        );
        spec.push(
            FormField::new(CFG_REPO_MAP_TOKENS, BasicType::Int)
                .label("Repo map size in the session header (tokens; 0 for none)")
                .default_value(DEFAULT_REPO_MAP_TOKENS as i64),
        );
        spec.push(
            FormField::new(CFG_TIMEOUT, BasicType::Int)
                .label("Script timeout (seconds)")
                .default_value(DEFAULT_TIMEOUT_SECONDS as i64),
        );
        spec.extend(view_app::config_fields());
        // Last, because it is every grant above at once.
        spec.extend(shell::config_fields());
        spec
    }

    /// The store exists, the root is inside it, the bounds are whole numbers, the
    /// edit format is one there is, and every name this scope derives is one a
    /// provider accepts.
    ///
    /// The last is checked against the **longest** name the trait will derive
    /// ([`LONGEST_TOOL_PREFIX`]), so a scope whose `search_files_…` fits but
    /// whose `implement_feature_…` does not is refused here, not by the vendor.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let scope = check_scope(check).await?;
        config_count(check.config, CFG_MAX_LINES, DEFAULT_MAX_LINES)?;
        config_count(check.config, CFG_MAX_RESULTS, DEFAULT_MAX_RESULTS as u64)?;
        config_count(check.config, CFG_TIMEOUT, DEFAULT_TIMEOUT_SECONDS)?;
        if config_count(check.config, CFG_MAX_SESSIONS, DEFAULT_MAX_SESSIONS)? == 0 {
            return Err(Error::invalid(format!(
                "`{CFG_MAX_SESSIONS}` must be at least 1"
            )));
        }
        repo_map::configured_tokens(check.config)?;
        match config_str(check.config, CFG_WORKFLOW).as_str() {
            "" | WORKFLOW_DIRECT | WORKFLOW_PLANNED => {}
            other => {
                return Err(Error::invalid(format!(
                    "`{CFG_WORKFLOW}` must be {WORKFLOW_DIRECT} or {WORKFLOW_PLANNED}, got \
                     `{other}`"
                )));
            }
        }
        for key in [
            CFG_COMMIT,
            CFG_MAY_EDIT,
            CFG_MAY_RUN_SCRIPTS,
            CFG_MAY_CHECK,
            CFG_MAY_VIEW_APP,
            CFG_MAY_USE_SHELL,
            shell::CFG_SHELL_NETWORK,
        ] {
            match check.config.get(key) {
                None | Some(Json::Null) | Some(Json::Bool(_)) => {}
                Some(other) => {
                    return Err(Error::invalid(format!(
                        "`{key}` should be true or false, got {other}"
                    )));
                }
            }
        }
        match config_str(check.config, CFG_EDIT_FORMAT).as_str() {
            "" | EDIT_FORMAT_AUTO | "str_replace" | "apply_patch" | "whole_file" => {}
            other => {
                return Err(Error::invalid(format!(
                    "`{CFG_EDIT_FORMAT}` must be auto, str_replace, apply_patch or \
                     whole_file, got `{other}`"
                )));
            }
        }
        check::validate(check.catalog, check.config).await?;
        view_app::validate(check).await?;
        shell::validate(check.catalog, &scope, check.config).await?;
        for name in tool_names(&scope) {
            check_tool_name(&name)?;
        }
        check_tool_name(&format!("{LONGEST_TOOL_PREFIX}{}", scope.slug()))?;
        Ok(())
    }

    /// The read-only tools in every mode (TODO §5). In `plan`, the plan tools
    /// and `explore`. In `act`, `explore` and, under the grants, the write
    /// tool, the one edit tool the edit format picks, the script runner,
    /// `check`, `view_app` and the shell.
    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec> {
        let scope = scope_as_written(config);
        let mut tools = vec![
            read::spec(&scope, config),
            find::spec(&scope, config),
            search::spec(&scope, config),
            repo_map::spec(&scope),
        ];
        // The application's assets, in every mode: it reads names and sizes,
        // which is not a grant, and a `plan` that does not know the images
        // exist plans around them.
        if assets::offered(config) {
            tools.push(assets::spec(&scope, config));
        }
        match cx.mode {
            RunMode::Explore => return tools,
            RunMode::Plan => {
                tools.push(plan::spec(&scope));
                tools.push(feature::spec(&scope));
                tools.push(explore::spec(&scope));
                return tools;
            }
            RunMode::Act => tools.push(explore::spec(&scope)),
        }
        if may(config, CFG_MAY_EDIT) {
            let format = edit_format(config, cx.capabilities.edit_format);
            tools.push(write::spec(&scope, format));
            match format {
                EditFormat::StrReplace => tools.push(edit::spec(&scope)),
                // Always the function tool: rig 0.41 cannot declare OpenAI's
                // native `apply_patch` tool type (TODO 5.8).
                EditFormat::ApplyPatch => tools.push(patch::spec(&scope)),
                EditFormat::WholeFile => {}
            }
        }
        if may(config, CFG_MAY_RUN_SCRIPTS) {
            tools.push(script::spec(&scope));
        }
        if may(config, CFG_MAY_CHECK) {
            tools.push(check::spec(&scope));
        }
        if may(config, CFG_MAY_VIEW_APP) {
            tools.push(view_app::spec(&scope, config, cx.capabilities.vision));
        }
        if may(config, CFG_MAY_USE_SHELL) && cx.caller.is_some_and(is_admin) {
            tools.push(shell::spec(&scope, config));
            tools.push(process::spec(&scope));
        }
        tools
    }

    async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        let scope = configured_scope(config)?;
        match tool {
            _ if tool == read::tool_name(&scope) => read::call(&scope, config, args, ctx).await,
            _ if tool == find::tool_name(&scope) => find::call(&scope, config, args, ctx).await,
            _ if tool == search::tool_name(&scope) => search::call(&scope, config, args, ctx).await,
            _ if tool == repo_map::tool_name(&scope) => {
                repo_map::call(&scope, config, args, ctx).await
            }
            _ if tool == assets::tool_name(&scope) => assets::call(config, args, ctx).await,
            _ if tool == plan::tool_name(&scope) => {
                permit_mode(&[RunMode::Plan], "write a plan", ctx)?;
                plan::call(args, ctx).await
            }
            _ if tool == feature::tool_name(&scope) => {
                permit_mode(&[RunMode::Plan], "implement a feature", ctx)?;
                feature::call(&scope, config, args, ctx).await
            }
            _ if tool == explore::tool_name(&scope) => {
                permit_mode(
                    &[RunMode::Plan, RunMode::Act],
                    "start an explore session",
                    ctx,
                )?;
                explore::call(args, ctx).await
            }
            _ if tool == write::tool_name(&scope)
                || tool == edit::tool_name(&scope)
                || tool == patch::tool_name(&scope) =>
            {
                permit(config, CFG_MAY_EDIT, "change files", ctx)?;
                check::record_baseline(&scope, config, ctx).await?;
                match tool {
                    _ if tool == write::tool_name(&scope) => write::call(&scope, args, ctx).await,
                    _ if tool == edit::tool_name(&scope) => edit::call(&scope, args, ctx).await,
                    _ => patch::call(&scope, args, ctx).await,
                }
            }
            _ if tool == script::tool_name(&scope) => {
                permit(config, CFG_MAY_RUN_SCRIPTS, "run scripts", ctx)?;
                script::call(&scope, config, args, ctx).await
            }
            _ if tool == check::tool_name(&scope) => {
                permit(config, CFG_MAY_CHECK, "run checks", ctx)?;
                check::call(&scope, config, args, ctx).await
            }
            _ if tool == view_app::tool_name(&scope) => {
                permit(config, CFG_MAY_VIEW_APP, "look at the application", ctx)?;
                view_app::call(config, args, ctx).await
            }
            _ if tool == shell::tool_name(&scope) => {
                permit_shell(config, ctx)?;
                shell::call(&scope, config, args, ctx).await
            }
            _ if tool == process::tool_name(&scope) => {
                permit_shell(config, ctx)?;
                process::call(&scope, config, args, ctx).await
            }
            other => Err(Error::invalid(format!(
                "`{other}` is not one of this trait's tools; it offers {}",
                tool_names(&scope).join(", ")
            ))),
        }
    }

    /// The workflow, the rules and the active edit format's rules, for the
    /// mode and the grants — and in `act`, with the shell offered, how to use it
    /// (TODO 8.1, 6a.7). The same text on every step, so the prefix caches.
    fn prompt(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Option<String> {
        Some(prompt::prompt(cx, config))
    }

    /// The session header: `AGENTS.md`, a repo map focused on what the brief
    /// mentions, and the recent git log (TODO 8.2).
    async fn session_header(
        &self,
        config: &Attrs,
        cx: &mut SessionContext<'_>,
    ) -> Result<Option<String>> {
        let scope = configured_scope(config)?;
        Ok(header::header(&scope, config, cx).await)
    }

    /// `plan` under the `planned` workflow (TODO §5).
    fn starting_mode(&self, config: &Attrs) -> Option<RunMode> {
        (config_str(config, CFG_WORKFLOW) == WORKFLOW_PLANNED).then_some(RunMode::Plan)
    }

    /// A shell command is the same call whatever its whitespace (TODO 6a.7).
    fn fingerprint(&self, config: &Attrs, tool: &str, args: &Json) -> Json {
        let scope = scope_as_written(config);
        match tool {
            _ if tool == shell::tool_name(&scope) => shell::fingerprint(args),
            _ if tool == view_app::tool_name(&scope) => view_app::fingerprint(args),
            _ => args.clone(),
        }
    }

    /// An old look at the application is one line (TODO §7b).
    fn elide(&self, config: &Attrs, old: &Elidable<'_>) -> Option<String> {
        let scope = scope_as_written(config);
        match old.call.name.as_str() {
            name if name == view_app::tool_name(&scope) => Some(view_app::elide(old)),
            // An old review keeps its verdict; the latest checklist is later.
            name if name == feature::tool_name(&scope) => Some(format!(
                "[elided {}]",
                old.content.lines().next().unwrap_or_default()
            )),
            _ => Some(old.default_stub()),
        }
    }

    /// The run's managed processes go with it (TODO 6a.4).
    fn run_ended(&self, config: &Attrs, run: RunId) {
        process::run_ended(run, &scope_as_written(config).slug());
    }

    /// After a turn's edits: format the edited files and type-check them, once
    /// (TODO 5.10).
    async fn after_tools(
        &self,
        config: &Attrs,
        cx: &mut AfterToolsContext<'_>,
    ) -> Result<Option<String>> {
        let scope = configured_scope(config)?;
        feedback::after_edits(&scope, config, cx.catalog, cx.caller.role, cx.trait_state).await
    }
}

/// Whether a grant is on. An absent checkbox reads as off, which is the reading
/// that cannot turn a forgotten field into a permission.
pub(crate) fn may(config: &Attrs, key: &str) -> bool {
    config.get(key).and_then(Json::as_bool).unwrap_or(false)
}

/// Whether a caller is an admin: the only caller the shell is offered to.
pub(crate) fn is_admin(caller: &RunCaller) -> bool {
    caller.role == 1
}

/// Refuse the shell to a run whose grant is off, whose mode is read-only, or
/// whose caller is not an admin — the last because a shell runs as the
/// server's OS user, which can read the server's own credentials (TODO §7a).
fn permit_shell(config: &Attrs, ctx: &TraitContext<'_>) -> Result<()> {
    permit(config, CFG_MAY_USE_SHELL, "use the shell", ctx)?;
    if is_admin(ctx.caller) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "agent `{}` may not use the shell for this user: the shell is only for runs whose \
         caller is an administrator, because it runs as the server's own operating-system user",
        ctx.agent
    )))
}

/// Refuse a tool the run's mode does not offer, naming the mode.
fn permit_mode(modes: &[RunMode], what: &str, ctx: &TraitContext<'_>) -> Result<()> {
    if modes.contains(&ctx.mode) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "agent `{}` may not {what} in a `{}` run",
        ctx.agent, ctx.mode
    )))
}

/// Refuse a tool whose grant is off, or that the run's mode does not offer,
/// naming what would allow it.
///
/// Unreachable through the tools the model is offered — a withheld tool is not
/// declared — and kept anyway, because a run resumed from a transcript that
/// carried the call is the case where "the model cannot see it" stops being the
/// enforcement.
fn permit(config: &Attrs, key: &str, what: &str, ctx: &TraitContext<'_>) -> Result<()> {
    if ctx.mode != RunMode::Act {
        return Err(Error::invalid(format!(
            "agent `{}` may not {what} in a `{}` run, which is read-only",
            ctx.agent, ctx.mode
        )));
    }
    if may(config, key) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "agent `{}` may not {what}: its `coding` trait has `{key}` switched off. \
         Tell the user an administrator must turn it on.",
        ctx.agent
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> FileScope {
        FileScope {
            store: "app-src".to_owned(),
            root: "web".to_owned(),
        }
    }

    fn config(edit: bool, run: bool) -> Attrs {
        [
            ("store".to_owned(), json!("app-src")),
            ("root".to_owned(), json!("web")),
            (CFG_MAY_EDIT.to_owned(), json!(edit)),
            (CFG_MAY_RUN_SCRIPTS.to_owned(), json!(run)),
        ]
        .into_iter()
        .collect()
    }

    /// Every tool this trait can offer is named after the scope, so one agent
    /// may have it twice over two directories (§11.2).
    #[test]
    fn every_tool_is_named_after_the_scope() {
        assert_eq!(
            tool_names(&scope()),
            [
                "read_file_app_src_web",
                "find_files_app_src_web",
                "search_files_app_src_web",
                "repo_map_app_src_web",
                "list_assets_app_src_web",
                "save_plan_app_src_web",
                "implement_feature_app_src_web",
                "explore_app_src_web",
                "write_file_app_src_web",
                "edit_file_app_src_web",
                "apply_patch_app_src_web",
                "run_script_app_src_web",
                "check_app_src_web",
                "view_app_app_src_web",
                "shell_app_src_web",
                "process_app_src_web",
            ]
        );
    }

    /// `auto`, and no setting at all, follow the model; anything else is the
    /// admin's choice whatever the model prefers.
    #[test]
    fn the_edit_format_follows_the_model_unless_the_admin_chose() {
        let with = |format: &str| -> Attrs {
            [(CFG_EDIT_FORMAT.to_owned(), json!(format))]
                .into_iter()
                .collect()
        };
        assert_eq!(
            edit_format(&Attrs::new(), EditFormat::ApplyPatch),
            EditFormat::ApplyPatch
        );
        assert_eq!(
            edit_format(&with("auto"), EditFormat::StrReplace),
            EditFormat::StrReplace
        );
        assert_eq!(
            edit_format(&with("whole_file"), EditFormat::ApplyPatch),
            EditFormat::WholeFile
        );
        assert_eq!(
            edit_format(&with("apply_patch"), EditFormat::StrReplace),
            EditFormat::ApplyPatch
        );
    }

    /// A grant is on only when the admin ticked it. **A missing field is not a
    /// permission**, which is the reading that keeps a half-filled form from
    /// handing out an edit — and the one the integration suite then pins against
    /// the tools actually offered.
    #[test]
    fn an_unticked_or_absent_checkbox_grants_nothing() {
        assert!(may(&config(true, false), CFG_MAY_EDIT));
        assert!(!may(&config(true, false), CFG_MAY_RUN_SCRIPTS));

        let bare: Attrs = [("store".to_owned(), json!("app-src"))]
            .into_iter()
            .collect();
        assert!(!may(&bare, CFG_MAY_EDIT));
        assert!(!may(&bare, CFG_MAY_RUN_SCRIPTS));
        // …and something that is not a boolean at all is not a grant either.
        let odd: Attrs = [(CFG_MAY_EDIT.to_owned(), json!("yes"))]
            .into_iter()
            .collect();
        assert!(!may(&odd, CFG_MAY_EDIT));
    }
}

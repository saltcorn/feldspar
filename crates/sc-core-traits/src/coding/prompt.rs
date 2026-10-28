//! What `coding` adds to the system prompt (TODO 8.1, R§4).
//!
//! R§4's layered skeleton: a `<workflow>`, the `<rules>`, and an `<edit_format>`
//! holding only the rules of the edit tool the run is offered. Role and platform
//! are the agent's own prompt, which comes first; the project (`AGENTS.md`) is
//! the session header's ([`super::header`]).
//!
//! **A function of the mode, the grants and the edit format, and nothing
//! else.** It is part of the cached prefix, so it names only tools the run is
//! offered — a workflow step that says "run check" to an agent without `check`
//! would send a cheap model looking for a tool that is not there — and it says
//! the same thing on every step.
//!
//! It says *when* and *in what order*; the tool descriptions say how each tool
//! is called and what it returns. The two together are budgeted by the
//! stable-prefix test (`coding_prompt.rs`), which is generous enough that
//! neither has to be cryptic.

use sc_agent::{RunMode, ToolsContext};
use sc_llm::EditFormat;
use sc_types::Attrs;

use super::{
    CFG_MAY_CHECK, CFG_MAY_EDIT, CFG_MAY_USE_SHELL, CFG_MAY_VIEW_APP, call_api, check, edit,
    edit_format, explore, feature, is_admin, may, patch, plan, repo_map, search, shell, view_app,
    write,
};
use crate::files::{FileScope, scope_as_written};

/// The static contribution for one configuration in one run.
pub fn prompt(cx: &ToolsContext<'_>, config: &Attrs) -> String {
    let scope = scope_as_written(config);
    let body = match cx.mode {
        RunMode::Plan => plan(config, &scope),
        RunMode::Explore => explore(config, &scope),
        RunMode::Act => act(cx, config, &scope),
    };
    // Said once here rather than in every tool's description: an agent with
    // two scopes tells them apart by the suffix.
    format!(
        "Tools ending `_{}` work on {}; their paths are relative to it.\n\n{body}",
        scope.slug(),
        scope.label()
    )
}

/// The step that says to ask the API before relying on it, where the run has
/// `call_api`.
fn ask_api(config: &Attrs, scope: &FileScope, when: &str) -> Option<String> {
    call_api::offered(config).then(|| {
        format!(
            "{when}, call it with `{}` to see what it really returns, as the user who will \
             see it.",
            call_api::tool_name(scope)
        )
    })
}

/// Where to look first, in every mode.
fn locate(scope: &FileScope) -> String {
    format!(
        "Locate with `{}` and `{}`, then read only what you need.",
        repo_map::tool_name(scope),
        search::tool_name(scope)
    )
}

fn act(cx: &ToolsContext<'_>, config: &Attrs, scope: &FileScope) -> String {
    let may_edit = may(config, CFG_MAY_EDIT);
    let may_check = may(config, CFG_MAY_CHECK);
    let check = check::tool_name(scope);

    let mut steps = vec![locate(scope)];
    if !may_edit {
        steps.push(
            "You cannot change files here: answer from what you read, naming files and lines."
                .to_owned(),
        );
        steps.push("Finish with a 3–5 line answer.".to_owned());
        return format!(
            "{}\n\n<rules>\n- Stay within what you were asked.\n</rules>",
            workflow(&steps)
        );
    }
    steps.push(match may_check {
        true => format!("For a bug, reproduce it first with a failing test or `{check}`."),
        false => "For a bug, find its cause before changing anything.".to_owned(),
    });
    steps.extend(ask_api(
        config,
        scope,
        "Before writing code against an API endpoint",
    ));
    steps.push("Make the smallest change. Edit only files you have read.".to_owned());
    if may_check {
        steps.push(format!(
            "Run `{check}` and fix until it has no new failures."
        ));
        if check::configured_application(config).is_some() {
            steps.push(format!(
                "`{check}` also runs the application's checklists against your preview: fix \
                 what you broke, never weaken a checklist, and name every checklist still \
                 failing in your summary."
            ));
        }
    }
    if may(config, CFG_MAY_VIEW_APP) {
        steps.push(format!(
            "Look at a visible change with `{}`.",
            view_app::tool_name(scope)
        ));
    }
    steps.push("End with a 3–5 line summary: what changed, and how you verified it.".to_owned());

    let mut out = workflow(&steps);
    out.push_str("\n\n");
    out.push_str(RULES);
    let format = edit_format(config, cx.capabilities.edit_format);
    out.push_str("\n\n");
    out.push_str(&edit_rules(scope, format));
    if may(config, CFG_MAY_USE_SHELL) && cx.caller.is_some_and(is_admin) {
        out.push_str("\n\n");
        out.push_str(&shell::prompt_note(scope, config));
    }
    out
}

/// The rules of a run that changes code.
const RULES: &str = "<rules>\n\
- Never delete, skip or weaken a test to make it pass.\n\
- Stay within the task: mention other problems, do not fix them.\n\
- Record lasting project facts in AGENTS.md, and say so.\n\
</rules>";

/// Only the active edit tool's rules.
fn edit_rules(scope: &FileScope, format: EditFormat) -> String {
    let write = write::tool_name(scope);
    let body = match format {
        EditFormat::StrReplace => format!(
            "`{}`: include enough lines in `old_text` to be unique. The result shows the \
             edited lines; do not re-read. `{write}` is for new files.",
            edit::tool_name(scope)
        ),
        EditFormat::ApplyPatch => format!(
            "`{}`: one patch may change several files. Copy context lines exactly, under a \
             `@@` line naming the function. The result shows the edited lines; do not re-read.",
            patch::tool_name(scope)
        ),
        EditFormat::WholeFile => format!(
            "`{write}` replaces a whole file: read it, then write back all of it, changed. \
             Never leave out a part with a placeholder comment."
        ),
    };
    format!("<edit_format>\n{body}\n</edit_format>")
}

fn plan(config: &Attrs, scope: &FileScope) -> String {
    let save = plan::tool_name(scope);
    let look = match may(config, CFG_MAY_VIEW_APP) {
        true => format!(
            " For a visible change, look at the page first with `{}`.",
            view_app::tool_name(scope)
        ),
        false => String::new(),
    };
    let mut steps = vec![format!(
        "{} For a wide question, ask `{}`.{look}",
        locate(scope),
        explore::tool_name(scope)
    )];
    steps.extend(ask_api(
        config,
        scope,
        "Before planning around an API endpoint",
    ));
    steps.extend([
        format!(
            "Write the plan with `{save}`: features, each one session of work, with acceptance \
             criteria and the files it likely touches. A one-line fix is a one-feature plan."
        ),
        format!(
            "Run `{}` on each feature in order. Review each result's check report and diff \
             before the next.",
            feature::tool_name(scope)
        ),
        format!("When a result says re-plan, change the plan with `{save}` first."),
        "End with a 3–5 line summary of what was done and what was not.".to_owned(),
    ]);
    format!(
        "{}\n\n<rules>\n\
         - You do not edit files in this mode.\n\
         - Never accept a result that deletes, skips or weakens a test.\n\
         - Keep every feature within the request.\n\
         </rules>",
        workflow(&steps)
    )
}

fn explore(config: &Attrs, scope: &FileScope) -> String {
    let mut steps = vec![locate(scope)];
    steps.extend(ask_api(
        config,
        scope,
        "For a question about an API endpoint",
    ));
    steps.push(
        "Answer the question in at most 300 words, naming files and line numbers.".to_owned(),
    );
    format!(
        "{}\n\n<rules>\n- You cannot change anything. Do not guess: say what you did not \
         find.\n</rules>",
        workflow(&steps)
    )
}

fn workflow(steps: &[String]) -> String {
    let lines: Vec<String> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| format!("{}. {step}", i + 1))
        .collect();
    format!("<workflow>\n{}\n</workflow>", lines.join("\n"))
}

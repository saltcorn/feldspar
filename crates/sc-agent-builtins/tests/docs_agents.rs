//! The documentation of Phase 12 against the code it documents.
//!
//! `docs/tutorial-agents.md` tells an admin which boxes to tick and which tool
//! names to look for in a transcript, and `docs/TECHNICAL_DESIGN.md` carries
//! the milestone's "what was built, where it deviates" notes. Both go stale
//! silently: a renamed checkbox or a changed tool-name rule leaves a tutorial
//! that is confidently wrong, which is worse than no tutorial. So the facts the
//! documents assert about the code are asserted here against the code.
//!
//! What this does **not** do is check prose. It checks the names.

use std::fs;
use std::path::{Path, PathBuf};

use sc_agent::AgentTrait;
use sc_agent_builtins::{
    CFG_MAY_CHECK, CFG_MAY_EDIT, CFG_MAY_RUN_SCRIPTS, CFG_MAY_USE_SHELL, CFG_MAY_VIEW_APP,
    CFG_SHELL_SANDBOX, CFG_WORKFLOW, Coding, FileScope, WORKFLOW_PLANNED, tool_names,
};

/// Walk up to the workspace root (the ancestor whose `Cargo.toml` declares
/// `[workspace]`), as `repo_hygiene` does.
fn workspace_root() -> PathBuf {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file()
            && fs::read_to_string(&manifest)
                .unwrap_or_default()
                .contains("[workspace]")
        {
            return dir;
        }
        assert!(dir.pop(), "no [workspace] Cargo.toml above this crate");
    }
}

/// A document with its line breaks folded, so a phrase that the file wraps over
/// two lines is still one phrase to search for.
fn doc(rel: &str) -> String {
    let path = workspace_root().join(rel);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing {rel}: {e}"));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The same, with the markdown emphasis taken out: a label the document bolds
/// or puts a setting name of its own in backticks is still that label.
fn plain(rel: &str) -> String {
    doc(rel).replace(['`', '*', '_'], "")
}

fn tutorial() -> String {
    doc("docs/tutorial-agents.md")
}

fn plain_tutorial() -> String {
    plain("docs/tutorial-agents.md")
}

fn design() -> String {
    doc("docs/TECHNICAL_DESIGN.md")
}

/// The label of one of `coding`'s settings, as the form shows it.
fn label(key: &str) -> String {
    Coding
        .config_spec()
        .into_iter()
        .find(|field| field.name() == key)
        .unwrap_or_else(|| panic!("`coding` has no `{key}` setting any more"))
        .base
        .label
}

/// A label is the whole sentence an admin reads on the form, and several of
/// them explain themselves at length. What the tutorial's tables and prose quote
/// is the **leading clause** — everything before the first `:`, `(` or `,` — so
/// that is what is looked for, with the first five words as the fallback when a
/// label has no punctuation at all.
fn leading_clause(label: &str) -> String {
    let clause = label.split([':', '(', ',']).next().unwrap_or(label).trim();
    match clause.is_empty() {
        true => label
            .split_whitespace()
            .take(5)
            .collect::<Vec<_>>()
            .join(" "),
        false => clause.to_owned(),
    }
}

/// Every grant the tutorial has to explain, because ticking it is a decision.
///
/// 12.2 requires the shell's admin-only rule and its sandbox choice to be
/// stated plainly; that cannot be true of a checkbox the document does not name.
#[test]
fn the_tutorial_names_every_coding_grant() {
    let raw = tutorial();
    let tutorial = plain_tutorial();
    for key in [
        CFG_MAY_EDIT,
        CFG_MAY_RUN_SCRIPTS,
        CFG_MAY_CHECK,
        CFG_MAY_VIEW_APP,
        CFG_MAY_USE_SHELL,
    ] {
        let clause = leading_clause(&label(key));
        assert!(
            tutorial.contains(&clause),
            "docs/tutorial-agents.md does not name the `{key}` checkbox (`{clause}…`)"
        );
    }
    // The sandbox is a choice between two values, so the tutorial must name
    // both of them and the setting they belong to.
    assert!(
        tutorial.contains(&leading_clause(&label(CFG_SHELL_SANDBOX))),
        "the tutorial does not name the shell sandbox setting"
    );
    for value in ["`none`", "`container`"] {
        assert!(
            raw.contains(value),
            "the tutorial does not name the {value} shell sandbox"
        );
    }
}

/// The `planned` workflow, by the name the setting takes.
#[test]
fn the_tutorial_names_the_planned_workflow() {
    let tutorial = tutorial();
    assert!(
        plain_tutorial().contains(&leading_clause(&label(CFG_WORKFLOW))),
        "the tutorial does not name the workflow setting"
    );
    assert!(
        tutorial.contains(&format!("`{WORKFLOW_PLANNED}`")),
        "the tutorial does not name the `{WORKFLOW_PLANNED}` workflow"
    );
    for tool in ["save_plan", "implement_feature", "explore"] {
        assert!(
            tutorial.contains(tool),
            "the tutorial does not mention `{tool}`, which the planned workflow is made of"
        );
    }
}

/// The names a transcript actually shows.
///
/// The tutorial tells the reader to look for `check_apps_todo` and
/// `shell_apps_todo` in the chat, which is only true while the slug rule
/// derives them from the store and the sub-directory it also names.
#[test]
fn the_tutorial_shows_the_tool_names_this_scope_derives() {
    let scope = FileScope {
        store: "apps".to_owned(),
        root: "todo".to_owned(),
    };
    let tutorial = tutorial();
    for name in [
        tool_names::check(&scope),
        tool_names::shell(&scope),
        tool_names::process(&scope),
    ] {
        assert!(
            tutorial.contains(&name),
            "the tutorial does not show `{name}`, the name this scope derives"
        );
    }
    // Every name the trait derives must still fit a provider's limit; the
    // tutorial's promise that the names carry the scope depends on it.
    for name in tool_names::coding(&scope) {
        assert!(name.len() <= 64, "`{name}` is longer than 64 characters");
    }
}

/// The admin form's roles and budgets, by the labels the tutorial quotes.
///
/// These labels live in `ui/admin/src/agentForm.ts` (the boxes are hand-built,
/// not spec-rendered), so this is the only place the two can be compared.
#[test]
fn the_tutorial_names_the_agent_form_roles_and_budgets() {
    let form = fs::read_to_string(workspace_root().join("ui/admin/src/agentForm.ts"))
        .expect("the agent form's attribute list");
    let tutorial = tutorial();
    let labels: Vec<String> = form
        .lines()
        .filter_map(|line| line.trim().strip_prefix("label: \""))
        .filter_map(|rest| rest.split('"').next().map(str::to_owned))
        .collect();
    assert!(
        labels.len() >= 9,
        "expected the roles, the numeric attributes and the budgets, found {labels:?}"
    );
    for label in labels {
        assert!(
            tutorial.contains(&label),
            "docs/tutorial-agents.md does not document the agent form's `{label}` box"
        );
    }
}

/// What a preview is, whose session it uses, and that its data is live (12.2).
#[test]
fn the_tutorial_states_what_a_preview_shows() {
    let tutorial = tutorial();
    for phrase in [
        // The host shape, which is what a reader sees in a transcript.
        "--todo.localhost",
        // Whose session it is, for a chat and for a triggered run.
        "User a triggered run looks at the application as",
        // That the rows are real.
        "The data is live",
        // The host requirement.
        "headless browser",
    ] {
        assert!(
            tutorial.contains(phrase),
            "the tutorial does not say `{phrase}` about previews"
        );
    }
}

/// 12.1: each section named there carries a note for this milestone.
#[test]
fn the_design_records_what_this_milestone_built() {
    let design = design();
    for (section, note) in [
        ("§9", "the `strong` and `cheap` **model roles**"),
        ("§11.1", "**Counting a request before it is sent.**"),
        (
            "§11.2",
            "**Loop control: what a stuck cheap model looks like",
        ),
        (
            "§11.2",
            "**Context management: the layout is the cache plan**",
        ),
        (
            "§11.3",
            "(coding agent milestone, Phases 5–10: the `coding` rework)",
        ),
        ("§12.1", "(coding agent milestone, Phase 10.4, the relay"),
        ("§13.2", "**The second registry: a coding run's previews**"),
    ] {
        assert!(
            design.contains(note),
            "docs/TECHNICAL_DESIGN.md {section} has no `{note}` note"
        );
    }
}

/// The design's §11.3 note must name the settings it describes, since those are
/// what an admin and a later reader search for.
#[test]
fn the_design_note_names_the_settings_it_describes() {
    let design = design();
    for key in [
        CFG_MAY_EDIT,
        CFG_MAY_RUN_SCRIPTS,
        CFG_MAY_CHECK,
        CFG_MAY_VIEW_APP,
        CFG_MAY_USE_SHELL,
        CFG_SHELL_SANDBOX,
        CFG_WORKFLOW,
    ] {
        assert!(
            design.contains(key),
            "docs/TECHNICAL_DESIGN.md does not mention `{key}`"
        );
    }
}

/// The `http` trait's two grants are decisions an admin makes on the form, so
/// the tutorial names both as the form labels them, and the tool name it tells
/// the reader to look for is the one the default configuration derives.
#[test]
fn the_tutorial_names_the_http_grants_and_its_tool() {
    use sc_agent_builtins::http::{CFG_MAY_SEND, CFG_PRIVATE_NETWORK, DEFAULT_NAME};
    let http = sc_agent_builtins::Http::new().expect("the http trait builds");
    let plain = plain_tutorial();
    for key in [CFG_MAY_SEND, CFG_PRIVATE_NETWORK] {
        let label = http
            .config_spec()
            .into_iter()
            .find(|field| field.name() == key)
            .unwrap_or_else(|| panic!("`http` has no `{key}` setting any more"))
            .base
            .label;
        let clause = leading_clause(&label);
        assert!(
            plain.contains(&clause),
            "docs/tutorial-agents.md does not name the `{key}` checkbox (`{clause}…`)"
        );
    }
    let tool = tool_names::http(DEFAULT_NAME);
    assert!(
        tutorial().contains(&format!("`{tool}`")),
        "the tutorial does not name `{tool}`"
    );
}

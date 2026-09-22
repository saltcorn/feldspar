//! The prompt `coding` gives a run (TODO Phase 8): its static contribution per
//! mode, grant and edit format; the session header's `AGENTS.md`, map and git
//! log; and the size of the React builder agent's stable prefix.

use crate::common;

use std::path::Path;
use std::sync::Arc;

use common::{Env, as_user, config};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{
    Agent, EnabledTrait, RunCaller, RunMode, Runner, ToolsContext, save_agent, stable_prefix,
};
use sc_app::{Application, CFG_PROJECT, FrameworkRef, REACT_FRAMEWORK, framework_builder_agent};
use sc_catalog::{connect_file_store_def, save_file_store};
use sc_core_traits::{
    CFG_EDIT_FORMAT, CFG_MAY_CHECK, CFG_MAY_EDIT, CFG_MAY_USE_SHELL, CFG_MAY_VIEW_APP, CFG_ROOT,
    CFG_STORE,
};
use sc_error::{Error, Result};
use sc_files::FileStoreDef;
use sc_llm::{LlmRequest, ModelCapabilities};
use sc_types::Attrs;
use serde_json::json;

fn coding_config(extra: &[(&str, serde_json::Value)]) -> Attrs {
    let mut entries = vec![(CFG_STORE, json!("apps")), (CFG_ROOT, json!("web"))];
    entries.extend(extra.iter().cloned());
    config(&entries)
}

#[tokio::test]
async fn the_prompt_follows_the_mode_the_grants_and_the_edit_format() -> Result<()> {
    let env = Env::new().await?;
    let coding = env.registry.require("coding")?.clone();
    let caps = ModelCapabilities::built_in("", "");
    let admin = RunCaller::system();
    let prompt = |mode: RunMode, config: &Attrs, caller: &RunCaller| {
        let cx = ToolsContext::new(&env.catalog, mode, &caps).for_caller(caller);
        coding.prompt(&cx, config).unwrap_or_default()
    };

    let full = coding_config(&[
        (CFG_MAY_EDIT, json!(true)),
        (CFG_MAY_CHECK, json!(true)),
        (CFG_MAY_VIEW_APP, json!(true)),
        (CFG_EDIT_FORMAT, json!("str_replace")),
    ]);
    let act = prompt(RunMode::Act, &full, &admin);
    for part in [
        "<workflow>",
        "</workflow>",
        "<rules>",
        "<edit_format>",
        "repo_map_apps_web",
        "reproduce it first",
        "Edit only files you have read",
        "Run `check_apps_web`",
        "view_app_apps_web",
        "Never delete, skip or weaken a test",
        "3–5 line summary",
        "AGENTS.md",
        "edit_file_apps_web",
    ] {
        assert!(act.contains(part), "missing {part:?} in\n{act}");
    }
    assert!(!act.contains("apply_patch_apps_web"), "{act}");
    // The same inputs, the same bytes: it is part of the cached prefix.
    assert_eq!(act, prompt(RunMode::Act, &full, &admin));

    // Only the active edit format's rules.
    let mut patch = full.clone();
    patch.insert(CFG_EDIT_FORMAT.to_owned(), json!("apply_patch"));
    let text = prompt(RunMode::Act, &patch, &admin);
    assert!(text.contains("apply_patch_apps_web"), "{text}");
    assert!(!text.contains("edit_file_apps_web"), "{text}");
    let mut whole = full.clone();
    whole.insert(CFG_EDIT_FORMAT.to_owned(), json!("whole_file"));
    let text = prompt(RunMode::Act, &whole, &admin);
    assert!(text.contains("write back all of it"), "{text}");
    assert!(!text.contains("edit_file_apps_web"), "{text}");

    // No tool is named that the run is not offered.
    let edit_only = coding_config(&[(CFG_MAY_EDIT, json!(true))]);
    let text = prompt(RunMode::Act, &edit_only, &admin);
    assert!(!text.contains("check_apps_web"), "{text}");
    assert!(!text.contains("view_app_apps_web"), "{text}");
    let read_only = prompt(RunMode::Act, &coding_config(&[]), &admin);
    assert!(read_only.contains("cannot change files"), "{read_only}");
    assert!(!read_only.contains("<edit_format>"), "{read_only}");

    // The shell's paragraph, for an admin only.
    let mut shell = full.clone();
    shell.insert(CFG_MAY_USE_SHELL.to_owned(), json!(true));
    assert!(prompt(RunMode::Act, &shell, &admin).contains("shell_apps_web"));
    let user = as_user("ada@example.com");
    assert!(!prompt(RunMode::Act, &shell, &user).contains("shell_apps_web"));

    // The read-only modes: no edit rules, no shell, whatever the grants.
    let plan = prompt(RunMode::Plan, &shell, &admin);
    assert!(plan.contains("features"), "{plan}");
    assert!(plan.contains("do not edit files"), "{plan}");
    assert!(!plan.contains("<edit_format>"), "{plan}");
    assert!(!plan.contains("shell_apps_web"), "{plan}");
    let explore = prompt(RunMode::Explore, &shell, &admin);
    assert!(explore.contains("300 words"), "{explore}");
    assert!(!explore.contains("<edit_format>"), "{explore}");
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Run `agent` for one answer on `brief`, returning the provider.
async fn one_run(env: &Env, agent: &Agent, brief: &str) -> Result<Arc<FakeProvider>> {
    save_agent(&env.catalog, &env.registry, agent).await?;
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("find_files_apps_web", json!({})),
        Reply::says("ok"),
    ]));
    Runner::new(
        &env.catalog,
        &env.registry,
        agent,
        sc_llm::ConnectedModel::unconfigured(provider.clone()),
        RunCaller::system(),
    )
    .start(brief)
    .await?;
    Ok(provider)
}

#[tokio::test]
async fn a_session_opens_with_the_projects_notes_its_map_and_its_git_log() -> Result<()> {
    if !git(Path::new("."), &["--version"]) {
        eprintln!("skipped: git is not installed");
        return Ok(());
    }
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/AGENTS.md", "# Notes\nRun `npm run typecheck`.\n")?;
    env.put(&dir, "web/src/billing/AGENTS.md", "Prices are in cents.\n")?;
    env.put(
        &dir,
        "web/src/billing/Invoice.tsx",
        "export function Invoice() {\n  return null;\n}\n",
    )?;
    env.put(&dir, "web/src/App.tsx", "export function App() {}\n")?;
    env.put(&dir, "other/notes.txt", "outside the scope\n")?;
    assert!(git(&dir, &["init", "-q", "."]));
    assert!(git(&dir, &["add", "-A"]));
    assert!(git(&dir, &["commit", "-qm", "Add invoices"]));
    env.put(&dir, "other/more.txt", "still outside\n")?;
    assert!(git(&dir, &["add", "-A"]));
    assert!(git(
        &dir,
        &["commit", "-qm", "Touch only the other directory"]
    ));

    let agent = Agent::new("coder", "main")
        .system_prompt("You build the billing app.")
        .with_trait(
            EnabledTrait::new("coding")
                .config(CFG_STORE, "apps")
                .config(CFG_ROOT, "web")
                .config(CFG_MAY_EDIT, true),
        );
    let provider = one_run(&env, &agent, "Show the total in src/billing/Invoice.tsx").await?;

    let header = provider.session_header(0).expect("a header");
    let root = header.find("<project path=\"AGENTS.md\">\n# Notes");
    let nested = header.find("<project path=\"src/billing/AGENTS.md\">\nPrices are in cents.");
    let map = header.find("Repo map of `web` in the `apps` file store");
    let log = header.find("<git_log>");
    assert!(
        root < nested && nested < map && map < log && root.is_some(),
        "{header}"
    );
    assert!(
        header.contains("focused on src/billing/Invoice.tsx"),
        "{header}"
    );
    // Commits touching the scope, and only those.
    assert!(header.contains(" Add invoices\n</git_log>"), "{header}");
    assert!(
        !header.contains("Touch only the other directory"),
        "{header}"
    );

    // The static contribution follows the agent's prompt, and the prefix is the
    // same on the second request as on the first.
    let first = &provider.requests()[0];
    let system = first.system.as_deref().unwrap_or_default();
    assert!(
        system.starts_with(
            "You build the billing app.\n\nTools ending `_apps_web` work on `web` in the `apps` \
             file store; their paths are relative to it.\n\n<workflow>"
        ),
        "{system}"
    );
    provider.assert_stable_prefix();
    assert_eq!(provider.requests().len(), 2);

    // A brief naming no billing file gets the root's notes alone.
    let agent = agent.clone();
    let provider = one_run(&env, &agent, "Add a footer to src/App.tsx").await?;
    let header = provider.session_header(0).expect("a header");
    assert!(header.contains("<project path=\"AGENTS.md\">"), "{header}");
    assert!(!header.contains("Prices are in cents."), "{header}");
    Ok(())
}

#[tokio::test]
async fn a_store_inside_someone_elses_repository_gets_no_git_log() -> Result<()> {
    if !git(Path::new("."), &["--version"]) {
        eprintln!("skipped: git is not installed");
        return Ok(());
    }
    let env = Env::new().await?;
    // The repository is the directory *above* the store's.
    let outer = common::temp_dir("outer-repo");
    let dir = outer.join("store");
    std::fs::create_dir_all(dir.join("web")).map_err(|e| Error::config(e.to_string()))?;
    env.put(&dir, "web/App.tsx", "export function App() {}\n")?;
    assert!(git(&outer, &["init", "-q", "."]));
    assert!(git(&outer, &["add", "-A"]));
    assert!(git(&outer, &["commit", "-qm", "Server history"]));
    let def = FileStoreDef::local("apps", dir.to_string_lossy());
    save_file_store(&env.catalog, &def).await?;
    connect_file_store_def(&env.catalog, &def)?;

    let agent = Agent::new("coder", "main").with_trait(
        EnabledTrait::new("coding")
            .config(CFG_STORE, "apps")
            .config(CFG_ROOT, "web"),
    );
    let provider = one_run(&env, &agent, "Look at App.tsx").await?;
    let header = provider.session_header(0).expect("a header");
    assert!(!header.contains("<git_log>"), "{header}");
    assert!(!header.contains("<project"), "{header}");
    assert!(header.starts_with("Repo map of"), "{header}");
    let _ = std::fs::remove_dir_all(&outer);
    Ok(())
}

/// TODO 8.3: the React builder agent's system prompt plus its tools stay within
/// R§4's budget, in `act` and in `plan`, for either edit tool — as it is
/// declared (§12: `coding` alone, checking and looking at the application).
///
/// **1 600 since the static-directories milestone**, and the extra hundred is
/// `list_assets` (TODO "Static directories" §6). R§4's original 1 500 was
/// already spent to the last token — `act` measured 1 496 — so the tenth tool
/// could not be added without either this or taking a description off one of
/// the other nine. It is the cheapest tool in the set at 279 characters, and
/// what it buys is the one question the other nine cannot answer: a model that
/// is not told an image's URL invents one, and the page 404s.
#[tokio::test]
async fn the_react_builder_agents_stable_prefix_is_at_most_1600_tokens() -> Result<()> {
    const LIMIT: u64 = 1_600;
    let env = Env::new().await?;
    let app = Application::new(
        "Todo",
        "todo",
        FrameworkRef::new(REACT_FRAMEWORK)
            .with("store", "apps")
            .with(CFG_PROJECT, "todo"),
    );
    let spec = framework_builder_agent(&app.framework, &app).expect("react declares one");
    let declared = spec.traits.iter().fold(
        Agent::new(&spec.name, "main").system_prompt(&spec.system_prompt),
        |agent, t| agent.with_trait(EnabledTrait::new(&t.trait_).configuration(t.config.clone())),
    );

    let system = RunCaller::system();
    let mut over: Vec<String> = Vec::new();
    for (backend, model) in [
        (sc_llm::ANTHROPIC_BACKEND, "claude-haiku-4-5"),
        (sc_llm::OPENAI_RESPONSES_BACKEND, "gpt-5-mini"),
    ] {
        let caps = ModelCapabilities::built_in(backend, model);
        for (label, agent) in [("declared", &declared)] {
            for mode in [RunMode::Act, RunMode::Plan] {
                let cx = ToolsContext::new(&env.catalog, mode, &caps).for_caller(&system);
                let prefix = stable_prefix(&env.registry, agent, &cx)?;
                let request = LlmRequest {
                    system: prefix.system,
                    tools: prefix.tools,
                    ..LlmRequest::default()
                };
                let tokens = sc_llm::estimate_tokens(&request, backend);
                eprintln!("{label} {mode} on {model}: {tokens} estimated tokens");
                if tokens > LIMIT {
                    over.push(format!(
                        "{label} builder agent in {mode} on {model}: {tokens} tokens\n\
                         system: {}\ntools: {}",
                        request.system.unwrap_or_default(),
                        request
                            .tools
                            .iter()
                            .map(|t| format!(
                                "{} ({} chars)",
                                t.name,
                                t.description.len() + t.parameters.to_string().len()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
    }
    assert!(
        over.is_empty(),
        "over {LIMIT} tokens:\n{}",
        over.join("\n\n")
    );
    Ok(())
}

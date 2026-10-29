#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Phase 5's "done when", as one run: an agent pointed at a store greps for a
//! declaration, reads the file, edits it, builds, reads the type error it
//! caused, fixes it and builds clean.
//!
//! The provider is scripted ([`FakeProvider`]) — decision 7 says no test in this
//! tree may need an API key — but **everything else is the production path**: the
//! loop, the tool dispatch, the run row written after every step, a real local
//! file store, a real application row and a real (stand-in) bundler whose output
//! is the diagnostics the model reads. What is being pinned is that the tools
//! compose into a working edit-build-fix cycle and that the whole cycle is in the
//! run's transcript afterwards, which is what the chat panel renders.

use crate::common;

use std::sync::Arc;

use common::Env;
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{Agent, EnabledTrait, RunCaller, Runner, save_agent};
use sc_app::{Application, FrameworkRef, save_application};
use sc_core_traits::{CFG_APPLICATION, CFG_MAY_EDIT, CFG_ROOT, CFG_STORE};
use sc_error::Result;
use serde_json::{Value as Json, json};

/// The app's source, as the agent finds it: one file declaring the to-do type.
const TODO_TS: &str = "\
export interface Todo {
  id: number;
  title: string;
}

export const empty: Todo[] = [];
";

/// A stand-in bundler that is really a type checker: it fails when the source
/// mentions `done` without declaring it, which is the mistake the scripted model
/// makes and then fixes.
const BUILD_SH: &str = "#!/bin/sh\n\
     if grep -q 'done: boolean' src/todo.ts; then\n\
       mkdir -p dist\n\
       printf 'ok' > dist/index.html\n\
       echo 'built 1 module'\n\
       exit 0\n\
     fi\n\
     if grep -q 'done' src/todo.ts; then\n\
       n=$(grep -n 'done' src/todo.ts | head -1 | cut -d: -f1)\n\
       echo \"src/todo.ts($n,3): error TS2339: Property 'done' does not exist on type 'Todo'.\"\n\
       exit 2\n\
     fi\n\
     mkdir -p dist\n\
     printf 'ok' > dist/index.html\n\
     echo 'built 1 module'\n";

#[tokio::test]
async fn an_agent_greps_edits_builds_reads_the_error_it_caused_and_fixes_it() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("apps", None).await?;
    env.put(&dir, "web/src/todo.ts", TODO_TS)?;
    env.put(&dir, "web/src/list.tsx", "import { Todo } from './todo';\n")?;
    let script = dir.join("web/build.sh");
    std::fs::write(&script, BUILD_SH).map_err(|e| sc_error::Error::config(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| sc_error::Error::config(e.to_string()))?;
    }

    let app = Application::new(
        "Todo",
        "todo",
        FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "sh build.sh"),
    );
    save_application(&env.catalog, &app).await?;

    // The agent: **one** `coding` trait over the app's source directory, with the
    // edit grant ticked, plus the build. Two enabled traits, and the scope filled
    // in once — which is the shape this used to need five rows of.
    let agent = Agent::new("coder", "main")
        .system_prompt("You maintain the to-do app.")
        .with_trait(
            EnabledTrait::new("coding")
                .config(CFG_STORE, "apps")
                .config(CFG_ROOT, "web")
                .config(CFG_MAY_EDIT, true),
        )
        .with_trait(EnabledTrait::new("build_application").config(CFG_APPLICATION, "todo"));
    save_agent(&env.catalog, &env.registry, &agent).await?;

    // The transcript the model produces: find where the type is declared, add a
    // field to the *use* first (the mistake), build, read the diagnostic, then
    // declare the field and build clean.
    let provider = Arc::new(FakeProvider::new([
        Reply::calls(
            "search_files_apps_web",
            json!({"pattern": "interface Todo"}),
        )
        .with_preamble("Let me find where the type is declared."),
        // An edit refuses a file this run has not read.
        Reply::calls("read_file_apps_web", json!({"path": "src/todo.ts"})),
        Reply::calls(
            "edit_file_apps_web",
            json!({
                "path": "src/todo.ts",
                "old_text": "export const empty: Todo[] = [];",
                "new_text": "export const empty: Todo[] = [];\nexport const first = (t: Todo) => t.done;",
            }),
        ),
        Reply::calls("build_todo", json!({})),
        Reply::calls(
            "edit_file_apps_web",
            json!({
                "path": "src/todo.ts",
                // No second read: the run's own edit counts as having seen
                // the file.
                "old_text": "  title: string;",
                "new_text": "  title: string;\n  done: boolean;",
            }),
        )
        .with_preamble("I introduced a type error; the field needs declaring."),
        Reply::calls("build_todo", json!({})),
        Reply::says("Added a `done` field to Todo. The app builds."),
    ]));

    let runner = Runner::new(
        &env.catalog,
        &env.registry,
        &agent,
        sc_llm::ConnectedModel::unconfigured(provider.clone()),
        RunCaller::system(),
    );
    let (run, conclusion) = runner.start("add a done field to the to-do type").await?;
    assert_eq!(
        conclusion.answer(),
        Some("Added a `done` field to Todo. The app builds.")
    );

    // Every tool was offered under the name its configuration derives — the
    // seven the one `coding` trait contributes (and its two documentation
    // tools) and the build's, sorted by name
    // so the request's prefix is stable (TODO §9). `run_script_apps_web` is not
    // among them: that grant was left off.
    let requests = provider.requests();
    let offered: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        offered,
        vec![
            "build_todo",
            "edit_file_apps_web",
            "explore_apps_web",
            "find_files_apps_web",
            "read_docs",
            "read_file_apps_web",
            "repo_map_apps_web",
            "search_docs",
            "search_files_apps_web",
            "write_file_apps_web",
        ]
    );

    // The file on disk carries both edits.
    let source = env.slurp(&dir, "web/src/todo.ts")?;
    assert!(source.contains("done: boolean;"), "{source}");

    // And the whole cycle is on the run — which is what the chat panel replays
    // and what makes a build the model reacted to auditable afterwards.
    let state = sc_agent::load_run(&env.catalog, run.id)
        .await?
        .expect("the run row")
        .agent_loop()?;
    let results: Vec<(String, String)> = state
        .messages()
        .iter()
        .filter_map(|m| match m {
            sc_llm::LlmMessage::ToolResult { content, name, .. } => {
                Some((name.clone(), content.clone()))
            }
            _ => None,
        })
        .collect();
    // The build still answers in JSON.
    let json = |i: usize| -> Json { serde_json::from_str(&results[i].1).unwrap_or(Json::Null) };
    let called: Vec<&str> = results.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        called,
        vec![
            "search_files_apps_web",
            "read_file_apps_web",
            "edit_file_apps_web",
            "build_todo",
            "edit_file_apps_web",
            "build_todo",
        ]
    );

    // The search found the declaration, with its line.
    assert_eq!(results[0].1, "src/todo.ts:1: export interface Todo {");
    // The edit showed the lines it changed, numbered.
    assert!(
        results[2]
            .1
            .contains("7\texport const first = (t: Todo) => t.done;"),
        "{}",
        results[2].1
    );

    // The first build failed, and what the model was handed was the diagnostic —
    // file, line and message — not "build failed".
    let failed = &json(3);
    assert_eq!(failed["built"], json!(false), "{failed}");
    assert_eq!(failed["diagnostics"][0]["file"], json!("src/todo.ts"));
    assert!(
        failed["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("does not exist"),
        "{failed}"
    );

    // The second one succeeded.
    assert_eq!(json(5)["built"], json!(true), "{}", results[5].1);
    Ok(())
}

/// A run whose agent is configured against a store nobody connected fails the
/// way §11.2 says it should: the agent does not validate, and saving says why.
#[tokio::test]
async fn an_agent_whose_store_is_missing_is_refused_on_save_with_the_reason() -> Result<()> {
    let env = Env::new().await?;
    let agent = Agent::new("coder", "main").with_trait(
        EnabledTrait::new("coding")
            .config(CFG_STORE, "not-a-store")
            .config(CFG_ROOT, ""),
    );
    let err = save_agent(&env.catalog, &env.registry, &agent)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not-a-store"), "{err}");
    assert!(err.contains("coding"), "{err}");
    Ok(())
}

/// Two `coding` traits over the **same** scope derive the same tool names, and
/// that clash is refused where it can be fixed (§11.2).
#[tokio::test]
async fn two_instances_over_one_scope_collide_on_save() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("apps", None).await?;
    let coding = |root: &str| {
        EnabledTrait::new("coding")
            .config(CFG_STORE, "apps")
            .config(CFG_ROOT, root)
    };

    // Two directories: two distinguishable sets of tools, which is the point of
    // deriving the names from the configuration.
    let ok = Agent::new("coder", "main")
        .with_trait(coding("web"))
        .with_trait(coding("docs"));
    save_agent(&env.catalog, &env.registry, &ok).await?;

    // The same directory twice: one set of names, refused on save rather than
    // discovered when the model picks the tool that is not there.
    let clash = Agent::new("twice", "main")
        .with_trait(coding("web"))
        .with_trait(coding("web"));
    let err = save_agent(&env.catalog, &env.registry, &clash)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("read_file_apps_web"), "{err}");
    Ok(())
}

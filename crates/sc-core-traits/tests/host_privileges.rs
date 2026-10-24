//! The tools that act as the server's own operating-system user — the shell,
//! the project's scripts and checks, the application build, the admin copilot
//! — and who a run must be for to hold them:
//!
//! - a **non-admin** caller is offered none of `run_project_script`, `check`
//!   or `build_*`, whatever the grants say, and a call to one is refused: whoever
//!   may edit `package.json` decides what `npm run` runs;
//! - a **triggered** run holds role 1 for rows, but is not an admin here: its
//!   prompt was computed from an event somebody else caused;
//! - no agent writes inside `.git`, where `core.fsmonitor` is a command the
//!   server's own `git status` runs;
//! - below admin, no agent writes what a build runs (`package.json`, configs,
//!   `node_modules`) or what a build wrote (an application's output directory,
//!   whose server entry runs with admin authority).

use crate::common;

use common::{Env, config};
use sc_agent::RunCaller;
use sc_core_traits::{
    CFG_MAY_CHECK, CFG_MAY_EDIT, CFG_MAY_RUN_SCRIPTS, CFG_MAY_USE_SHELL, CFG_ROOT, CFG_STORE,
    configured_scope, tool_names,
};
use sc_error::Result;
use sc_types::Attrs;
use serde_json::json;

/// A coding agent with every grant on.
fn everything() -> Attrs {
    config(&[
        (CFG_STORE, json!("code")),
        (CFG_ROOT, json!("")),
        (CFG_MAY_EDIT, json!(true)),
        (CFG_MAY_RUN_SCRIPTS, json!(true)),
        (CFG_MAY_CHECK, json!(true)),
        (CFG_MAY_USE_SHELL, json!(true)),
    ])
}

fn staff() -> RunCaller {
    RunCaller::user(sc_auth::User::new(uuid::Uuid::new_v4(), 40).expect("a user"))
}

fn offered_to(env: &Env, trait_: &str, config: &Attrs, caller: &RunCaller) -> Vec<String> {
    env.registry
        .require(trait_)
        .expect("a built-in trait")
        .tools(&env.tools_context().for_caller(caller), config)
        .into_iter()
        .map(|t| t.name)
        .collect()
}

#[tokio::test]
async fn the_projects_own_code_runs_only_for_an_admin() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(
        &dir,
        "package.json",
        r#"{"scripts": {"greet": "echo hello"}}"#,
    )?;
    let cfg = everything();
    let scope = configured_scope(&cfg)?;
    let script = tool_names::run_project_script(&scope);
    let check = tool_names::check(&scope);
    let shell = tool_names::shell(&scope);

    let admin = offered_to(&env, "coding", &cfg, &RunCaller::system());
    for tool in [&script, &check, &shell] {
        assert!(admin.contains(tool), "{tool} for an admin: {admin:?}");
    }

    for (who, caller) in [("staff", staff()), ("a trigger", RunCaller::triggered())] {
        let names = offered_to(&env, "coding", &cfg, &caller);
        for tool in [&script, &check, &shell] {
            assert!(!names.contains(tool), "{tool} offered to {who}: {names:?}");
        }
        // Still allowed to edit: that is the grant, and it runs nothing.
        assert!(names.contains(&tool_names::write_file(&scope)), "{who}");

        let err = env
            .call_tool("coding", &cfg, &script, json!({"script": "greet"}), &caller)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("administrator"), "{who}: {err}");
        let err = env
            .call_tool("coding", &cfg, &shell, json!({"command": "id"}), &caller)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("administrator"), "{who}: {err}");
    }

    // The admin's run does run it.
    let ran = env
        .call_tool(
            "coding",
            &cfg,
            &script,
            json!({"script": "greet"}),
            &RunCaller::system(),
        )
        .await?;
    assert!(ran.to_string().contains("hello"), "{ran}");
    Ok(())
}

#[tokio::test]
async fn no_agent_writes_inside_dot_git() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, ".git/config", "[core]\n")?;
    let cfg = everything();
    let scope = configured_scope(&cfg)?;
    for path in [".git/config", ".GIT/config", "sub/.git/hooks/pre-commit"] {
        let err = env
            .call_tool(
                "coding",
                &cfg,
                &tool_names::write_file(&scope),
                json!({"path": path, "content": "[core]\n\tfsmonitor = touch /tmp/owned\n"}),
                &RunCaller::system(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(".git"), "{path}: {err}");
    }
    assert_eq!(env.slurp(&dir, ".git/config")?, "[core]\n");
    Ok(())
}

#[tokio::test]
async fn below_admin_no_agent_writes_build_inputs_or_build_output() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("code", None).await?;
    env.put(&dir, "site/package.json", r#"{"scripts": {}}"#)?;
    sc_app::save_application(
        &env.catalog,
        &sc_app::Application::new(
            "Site",
            "site",
            sc_app::FrameworkRef::new("code")
                .with("store", "code")
                .with("source", "site")
                .with("output", "site/dist")
                .with("command", "npm run build"),
        ),
    )
    .await?;
    let cfg = everything();
    let write = tool_names::write_file(&configured_scope(&cfg)?);
    let refused = [
        "site/package.json",
        "site/astro.config.mjs",
        "site/node_modules/.bin/typescript-language-server",
        "site/dist/_server/entry.js",
        "site/DIST/index.html",
    ];

    for (who, caller) in [("staff", staff()), ("a trigger", RunCaller::triggered())] {
        for path in refused {
            let err = env
                .call_tool(
                    "coding",
                    &cfg,
                    &write,
                    json!({"path": path, "content": "export default 1;\n"}),
                    &caller,
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("administrator"), "{who} {path}: {err}");
        }
        // The project's own source is still theirs to edit.
        env.call_tool(
            "coding",
            &cfg,
            &write,
            json!({"path": format!("site/src/pages/{}.astro", who.replace(' ', "-")), "content": "<h1>hi</h1>\n"}),
            &caller,
        )
        .await?;
    }
    assert_eq!(env.slurp(&dir, "site/package.json")?, r#"{"scripts": {}}"#);
    assert!(!dir.join("site/dist").exists());

    // An admin's own run may write all of it (all but the existing `package.json`,
    // which a write would first have to read).
    for path in &refused[1..] {
        env.call_tool(
            "coding",
            &cfg,
            &write,
            json!({"path": path, "content": "{}\n"}),
            &RunCaller::system(),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn a_triggered_run_has_no_admin_copilot_and_builds_nothing() -> Result<()> {
    let env = Env::new().await?;
    let copilot = config(&[]);
    assert!(offered_to(&env, "admin_copilot", &copilot, &RunCaller::triggered()).is_empty());
    let err = env
        .call_tool(
            "admin_copilot",
            &copilot,
            "list_tables",
            json!({}),
            &RunCaller::triggered(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("trusted_input"), "{err}");

    let build = config(&[("application", json!("todo"))]);
    for caller in [staff(), RunCaller::triggered()] {
        assert!(offered_to(&env, "build_application", &build, &caller).is_empty());
        let err = env
            .call_tool(
                "build_application",
                &build,
                &tool_names::build_application("todo"),
                json!({}),
                &caller,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("administrator"), "{err}");
    }
    Ok(())
}

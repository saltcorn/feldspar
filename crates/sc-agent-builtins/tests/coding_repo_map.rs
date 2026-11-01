//! The repo map in `coding` (TODO 7.5): the `repo_map` tool in every mode, its
//! focus from the arguments or from what the run has read, its budget, and the
//! map in the session header, focused on what the request mentions.

use crate::common;

use std::sync::Arc;

use common::{Env, config};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{Agent, EnabledTrait, RunCaller, RunMode, Runner, ToolsContext, save_agent};
use sc_agent_builtins::{CFG_REPO_MAP_TOKENS, CFG_ROOT, CFG_STORE};
use sc_error::Result;
use serde_json::{Value as Json, json};

/// A small project: a list component using an API module, a settings screen
/// nothing uses, and a README.
async fn project(env: &Env) -> Result<()> {
    let dir = env.with_file_store("code", None).await?;
    env.put(
        &dir,
        "web/src/TaskList.tsx",
        "import { listTasks } from './api';\n\
         export function TaskList() {\n  return <ul>{listTasks().length}</ul>;\n}\n",
    )?;
    env.put(
        &dir,
        "web/src/api.ts",
        "export function listTasks() {\n  return fetchTasks();\n}\n\
         function fetchTasks() {\n  return [];\n}\n",
    )?;
    env.put(
        &dir,
        "web/src/Settings.tsx",
        "export function SettingsScreen() {\n  return <form />;\n}\n",
    )?;
    env.put(&dir, "web/README.md", "# Tasks\n")?;
    // Dependencies are not the project.
    env.put(
        &dir,
        "web/node_modules/react/index.js",
        "function createElement() {}\n",
    )?;
    Ok(())
}

/// One `repo_map` call in the run whose state is `state`.
async fn map_of(
    env: &Env,
    cfg: &sc_types::Attrs,
    caller: &RunCaller,
    state: &mut Json,
    args: Json,
) -> Result<String> {
    env.call_in_run(state, "coding", cfg, "repo_map_code_web", args, caller)
        .await
        .0
        .map(|j| j.as_str().unwrap_or_default().to_owned())
}

fn scope() -> Vec<(&'static str, Json)> {
    vec![(CFG_STORE, json!("code")), (CFG_ROOT, json!("web"))]
}

#[tokio::test]
async fn the_map_is_offered_in_every_mode_and_follows_its_focus() -> Result<()> {
    let env = Env::new().await?;
    project(&env).await?;
    let cfg = config(&scope());
    let coding = env.registry.require("coding")?.clone();

    let capabilities = sc_llm::ModelCapabilities::built_in("", "");
    for mode in [RunMode::Act, RunMode::Plan, RunMode::Explore] {
        let tools = coding.tools(&ToolsContext::new(&env.catalog, mode, &capabilities), &cfg);
        assert!(
            tools.iter().any(|t| t.name == "repo_map_code_web"),
            "{mode}"
        );
    }

    let caller = RunCaller::system();
    let mut state = Json::Null;
    // The whole project, without its dependencies.
    let map = map_of(&env, &cfg, &caller, &mut state, json!({})).await?;
    assert!(
        map.starts_with("Repo map of `web` in the `code` file store: 4 of 4 files."),
        "{map}"
    );
    assert!(map.contains("    2│ export function TaskList() {"), "{map}");
    assert!(map.contains("README.md"), "{map}");
    assert!(!map.contains("node_modules"), "{map}");

    // Focused on a file: its definitions first, then what it uses.
    let map = map_of(
        &env,
        &cfg,
        &caller,
        &mut state,
        json!({"focus": ["src/TaskList.tsx"]}),
    )
    .await?;
    let files: Vec<&str> = map
        .lines()
        .skip(1)
        .filter(|l| !l.starts_with(' '))
        .collect();
    assert_eq!(files[..2], ["src/TaskList.tsx", "src/api.ts"], "{map}");
    assert!(map.contains("focused on src/TaskList.tsx"), "{map}");

    // Without a focus, the files this run has read are the focus.
    env.call_in_run(
        &mut state,
        "coding",
        &cfg,
        "read_file_code_web",
        json!({"path": "src/Settings.tsx"}),
        &caller,
    )
    .await
    .0?;
    let map = map_of(&env, &cfg, &caller, &mut state, json!({})).await?;
    assert!(map.contains("focused on src/Settings.tsx"), "{map}");
    assert!(map.lines().nth(1) == Some("src/Settings.tsx"), "{map}");

    // A small budget is kept to, and still starts with the focus.
    let map = map_of(
        &env,
        &cfg,
        &caller,
        &mut state,
        json!({"focus": ["src/api.ts"], "tokens": 64}),
    )
    .await?;
    let body = map.split_once('\n').map(|(_, b)| b).unwrap_or_default();
    assert!(sc_repomap::estimate_tokens(body) <= 64, "{map}");
    assert!(body.starts_with("src/api.ts\n"), "{map}");

    let err = map_of(
        &env,
        &cfg,
        &caller,
        &mut state,
        json!({"focus": "src/api.ts"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("list of strings"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_session_opens_with_a_map_focused_on_the_request() -> Result<()> {
    let env = Env::new().await?;
    project(&env).await?;
    let run = |tokens: Option<i64>| {
        let env = &env;
        async move {
            let mut coding = EnabledTrait::new("coding")
                .config(CFG_STORE, "code")
                .config(CFG_ROOT, "web");
            if let Some(tokens) = tokens {
                coding = coding.config(CFG_REPO_MAP_TOKENS, tokens);
            }
            let name = if tokens.is_some() {
                "mapper-off"
            } else {
                "mapper"
            };
            let agent = Agent::new(name, "main").with_trait(coding);
            save_agent(&env.catalog, &env.registry, &agent).await?;
            let provider = Arc::new(FakeProvider::new([Reply::says("ok")]));
            Runner::new(
                &env.catalog,
                &env.registry,
                &agent,
                sc_llm::ConnectedModel::unconfigured(provider.clone()),
                RunCaller::system(),
            )
            .start("Add a theme toggle to src/Settings.tsx")
            .await?;
            Ok::<_, sc_error::Error>(provider.session_header(0))
        }
    };

    // No `AGENTS.md` and no git here: the map is the whole header.
    let header = run(None).await?.expect("a header");
    assert!(
        header.starts_with(
            "Repo map of `web` in the `code` file store: 4 of 4 files, focused on src/Settings.tsx"
        ),
        "{header}"
    );
    assert_eq!(header.lines().nth(1), Some("src/Settings.tsx"), "{header}");

    // `repo_map_tokens = 0` leaves it out.
    assert_eq!(run(Some(0)).await?, None);
    Ok(())
}

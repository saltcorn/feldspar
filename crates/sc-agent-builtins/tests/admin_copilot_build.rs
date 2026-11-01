//! `admin_copilot` builds an application from a sentence (§13.6): the half
//! that is the agent's own — handing the code to a coding agent as a sub-agent,
//! and the build playbook it is told to follow.
//!
//! Creating the application itself goes through the server's handlers, which
//! this crate's tests have no router for; `sc-server`'s `build_from_a_sentence`
//! drives that half end to end. What is asserted here is what a context with no
//! server answers, so a CLI-driven run is told why rather than half-creating.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::{Env, config};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{
    ATTR_DELEGATED_BY, Agent, Conclusion, EnabledTrait, ProviderConnector, RunCaller, Runner,
    load_run, save_agent,
};
use sc_agent_builtins::{ARG_CONTEXT, ARG_TASK, CFG_ROOT, CFG_STORE};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_llm::{ConnectedModel, LlmMessage, LlmProvider};
use serde_json::{Value as Json, json};

const DELEGATE: &str = "delegate_to_coding_agent";

/// A connector handing each agent its own script.
#[derive(Default)]
struct Scripts {
    by_agent: Mutex<BTreeMap<String, Arc<FakeProvider>>>,
}

impl Scripts {
    fn new(script: impl IntoIterator<Item = (&'static str, Vec<Reply>)>) -> Arc<Scripts> {
        let scripts = Scripts::default();
        for (agent, replies) in script {
            scripts
                .by_agent
                .lock()
                .unwrap()
                .insert(agent.to_owned(), Arc::new(FakeProvider::new(replies)));
        }
        Arc::new(scripts)
    }

    fn for_agent(&self, agent: &str) -> Arc<FakeProvider> {
        self.by_agent.lock().unwrap().get(agent).cloned().unwrap()
    }
}

#[async_trait::async_trait]
impl ProviderConnector for Scripts {
    async fn connect(
        &self,
        _catalog: &Catalog,
        agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> Result<ConnectedModel> {
        match self.by_agent.lock().unwrap().get(&agent.name) {
            Some(provider) => Ok(ConnectedModel::unconfigured(
                Arc::clone(provider) as Arc<dyn LlmProvider>
            )),
            None => Err(Error::config(format!("no script for `{}`", agent.name))),
        }
    }
}

/// The copilot, with its defaults: both areas on, creating and editing granted.
fn copilot() -> Agent {
    Agent::new("copilot", "main")
        .system_prompt("You build applications.")
        .with_trait(EnabledTrait::new("admin_copilot").configuration(config(&[])))
}

/// A coding agent over the `apps` store — no application setting, so it is
/// found by name rather than by the application it builds.
fn coder() -> Agent {
    Agent::new("build-todo", "main")
        .system_prompt("You build the to-do app.")
        .with_trait(
            EnabledTrait::new("coding")
                .config(CFG_STORE, "apps")
                .config(CFG_ROOT, "todo"),
        )
}

async fn chat(
    env: &Env,
    scripts: &Arc<Scripts>,
    agent: &Agent,
    message: &str,
) -> Result<(sc_agent::Run, Conclusion)> {
    let connector = Arc::clone(scripts) as Arc<dyn ProviderConnector>;
    let executor = connector
        .connect(&env.catalog, agent, sc_agent::ModelRole::Executor)
        .await?;
    Runner::new(
        &env.catalog,
        &env.registry,
        agent,
        executor,
        RunCaller::system(),
    )
    .with_connector(&connector)
    .start(message)
    .await
}

/// The content of the copilot's result for `tool`, out of its transcript.
fn tool_result(run: &sc_agent::Run, tool: &str) -> String {
    run.agent_loop()
        .expect("the run's context")
        .messages()
        .iter()
        .find_map(|m| match m {
            LlmMessage::ToolResult { content, name, .. } if name == tool => Some(content.clone()),
            _ => None,
        })
        .expect("the tool result is in the transcript")
}

/// The copilot hands the code to a coding agent, which runs in its own
/// context and reports back; the copilot's model was told the playbook and
/// which coding agents there are.
#[tokio::test]
async fn the_copilot_delegates_an_applications_code_to_a_coding_agent() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("apps", None).await?;
    save_agent(&env.catalog, &env.registry, &coder()).await?;
    save_agent(&env.catalog, &env.registry, &copilot()).await?;

    let scripts = Scripts::new([
        (
            "copilot",
            vec![
                Reply::calls(
                    DELEGATE,
                    json!({
                        "agent": "build-todo",
                        ARG_TASK: "Build the to-do list pages",
                        ARG_CONTEXT: "The `todos` table has `title` and `done`",
                    }),
                ),
                Reply::says("Your to-do list is built."),
            ],
        ),
        (
            "build-todo",
            vec![Reply::says("Built the list page and the add form.")],
        ),
    ]);
    let (run, conclusion) = chat(&env, &scripts, &copilot(), "build me a to-do list").await?;
    assert_eq!(conclusion.answer(), Some("Your to-do list is built."));

    let handed_back: Json = serde_json::from_str(&tool_result(&run, DELEGATE)).unwrap();
    assert_eq!(handed_back["agent"], json!("build-todo"));
    assert_eq!(
        handed_back["answer"],
        json!("Built the list page and the add form.")
    );
    let child = load_run(
        &env.catalog,
        sc_agent::RunId(handed_back["run"].as_str().unwrap().parse().unwrap()),
    )
    .await?
    .expect("the coding agent's run");
    assert_eq!(child.subject, "build-todo");
    assert_eq!(child.attributes[ATTR_DELEGATED_BY], json!("copilot"));

    // The coding agent saw the briefing, with the context it cannot see.
    let briefing = format!("{:?}", scripts.for_agent("build-todo").requests());
    assert!(
        briefing.contains("Build the to-do list pages"),
        "{briefing}"
    );
    assert!(briefing.contains("`todos` table"), "{briefing}");

    // The copilot's model read the playbook in its prompt and the coding agents
    // in its session header.
    let seen = format!("{:?}", scripts.for_agent("copilot").requests());
    assert!(seen.contains("## Building an application"), "{seen}");
    assert!(seen.contains("`build-todo`"), "{seen}");
    Ok(())
}

/// Naming something that is not a coding agent is refused with the ones there
/// are, and so is naming an application no coding agent builds.
#[tokio::test]
async fn a_delegation_to_something_that_is_not_a_coding_agent_names_the_ones_there_are()
-> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("apps", None).await?;
    save_agent(&env.catalog, &env.registry, &coder()).await?;
    save_agent(&env.catalog, &env.registry, &copilot()).await?;

    for (args, expect) in [
        (
            json!({ "agent": "copilot", ARG_TASK: "x" }),
            "not a coding agent",
        ),
        (
            json!({ "application": "blog", ARG_TASK: "x" }),
            "no coding agent builds the application `blog`",
        ),
        (json!({ ARG_TASK: "x" }), "name the `application`"),
    ] {
        let err = env
            .call_tool(
                "admin_copilot",
                &config(&[]),
                DELEGATE,
                args.clone(),
                &RunCaller::system(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(expect), "{args}: {err}");
        assert!(err.contains("`build-todo`"), "{args}: {err}");
    }
    Ok(())
}

/// With no server behind the catalog, creating is refused saying so — never a
/// half-made application.
#[tokio::test]
async fn creating_an_application_needs_the_running_server() -> Result<()> {
    let env = Env::new().await?;
    let err = env
        .call_tool(
            "admin_copilot",
            &config(&[]),
            "create_application",
            json!({ "name": "Todo list" }),
            &RunCaller::system(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs the running server"), "{err}");
    assert!(
        sc_app::list_applications(&env.catalog).await?.is_empty(),
        "nothing was created"
    );
    Ok(())
}

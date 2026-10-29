//! `subagent`: one agent delegating to another, against a real database (§11.3).
//!
//! What is pinned here:
//!
//! - the shape itself — a parent agent calls `delegate_to_researcher`, the
//!   sub-agent runs a **whole loop of its own** with its own tools, and the
//!   parent answers with what came back;
//! - **isolation**, which is the reason the pattern exists: the sub-agent's
//!   request carries the briefing and nothing of the parent's conversation, and
//!   the parent's context gains one tool result rather than the sub-agent's
//!   working;
//! - **authority does not escalate**: the child runs as the person chatting, so
//!   §7.3's ownership formula answers the sub-agent's query with that person's
//!   rows — and the sub-agent's own `min_role` gates it on top of that;
//! - the two bounds — a **cycle** refused by name and a **depth** refused by
//!   number — and that both come back as something the parent model can read;
//! - the failure mode this pattern is known for: a sub-agent that **finishes
//!   without reporting**, which must not reach the parent as an empty answer;
//! - and the configuration checked **on save and on load**: an unknown agent,
//!   an agent delegating to itself, and a sub-agent deleted afterwards, which
//!   leaves the parent out of the live set with a reason.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::{Env, as_user, config};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{
    ATTR_DELEGATED_BY, ATTR_PARENT_RUN, Agent, AgentRegistry, Agents, Conclusion, EnabledTrait,
    ProviderConnector, RunCaller, Runner, delete_agent, load_run, save_agent,
};
use sc_catalog::Catalog;
use sc_core_traits::{
    ARG_CONTEXT, ARG_OUTPUT, ARG_TASK, CFG_AGENT, CFG_FIELDS, CFG_MAX_DEPTH, CFG_MAX_STEPS,
    CFG_TABLE, CFG_WHEN_TO_USE, tool_names,
};
use sc_error::{Error, Result};
use sc_llm::{ConnectedModel, LlmMessage, LlmProvider};
use serde_json::{Value as Json, json};

// --- a model per agent -------------------------------------------------------

/// A connector that hands each agent its own script, and keeps every provider it
/// made.
///
/// Per **agent**, because that is the whole subject here: a delegation connects a
/// second model, and the questions worth asking — what was the sub-agent sent?
/// did the parent's conversation reach it? — are questions about *which* provider
/// saw what.
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

    /// The provider a given agent was given, for the assertions about what it saw.
    fn for_agent(&self, agent: &str) -> Arc<FakeProvider> {
        self.by_agent
            .lock()
            .unwrap()
            .get(agent)
            .cloned()
            .unwrap_or_else(|| panic!("no script for `{agent}`"))
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

// --- the agents --------------------------------------------------------------

/// The parent: it can do nothing itself except delegate to `researcher`.
fn librarian(extra: &[(&str, Json)]) -> Agent {
    let mut cfg = config(&[
        (CFG_AGENT, json!("researcher")),
        (
            CFG_WHEN_TO_USE,
            json!("a question needs the library's own data"),
        ),
    ]);
    for (k, v) in extra {
        cfg.insert((*k).to_owned(), v.clone());
    }
    Agent::new("librarian", "main")
        .system_prompt("You answer questions about the library.")
        .min_role(80)
        .with_trait(EnabledTrait::new("subagent").configuration(cfg))
}

/// The specialist: it can read the books table, and knows nothing else.
fn researcher() -> Agent {
    Agent::new("researcher", "main")
        .system_prompt("You look things up in the library's data.")
        .min_role(80)
        .with_trait(
            EnabledTrait::new("query_table")
                .config(CFG_TABLE, "books")
                .config(CFG_FIELDS, json!(["id", "title", "pages"])),
        )
}

/// Save every agent, validating each as the admin's Save button does.
async fn save_all(catalog: &Catalog, registry: &AgentRegistry, agents: &[Agent]) -> Result<()> {
    for agent in agents {
        save_agent(catalog, registry, agent).await?;
    }
    Ok(())
}

/// Drive `agent` through one turn as `caller`, delegating through `scripts`.
async fn chat(
    env: &Env,
    scripts: &Arc<Scripts>,
    agent: &Agent,
    caller: RunCaller,
    message: &str,
) -> Result<(sc_agent::Run, Conclusion)> {
    let connector = Arc::clone(scripts) as Arc<dyn ProviderConnector>;
    let executor = connector
        .connect(&env.catalog, agent, sc_agent::ModelRole::Executor)
        .await?;
    let mut runner = Runner::new(&env.catalog, &env.registry, agent, executor, caller)
        .with_connector(&connector);
    if let Some(evaluator) = &env.evaluator {
        runner = runner.with_evaluator(evaluator);
    }
    runner.start(message).await
}

/// The content of the parent's `delegate_to_…` tool result, out of its
/// transcript — what the model was actually handed back.
fn delegation_result(run: &sc_agent::Run, tool: &str) -> String {
    run.agent_loop()
        .expect("the run's context")
        .messages()
        .iter()
        .find_map(|m| match m {
            LlmMessage::ToolResult { content, name, .. } if name == tool => Some(content.clone()),
            _ => None,
        })
        .expect("the delegation is in the parent's transcript")
}

// --- the shape ---------------------------------------------------------------

#[tokio::test]
async fn a_parent_delegates_a_task_and_answers_with_what_came_back() -> Result<()> {
    // The whole feature: the parent has no way to read the library and does not
    // need one — it hands the task to `researcher`, which runs its own loop with
    // its own tool, and the parent answers with what that concluded.
    let env = Env::new().await?.with_engine();
    env.own("books", "owner === user.email").await?;
    save_all(&env.catalog, &env.registry, &[researcher(), librarian(&[])]).await?;

    let scripts = Scripts::new([
        (
            "librarian",
            vec![
                Reply::calls(
                    tool_names::subagent("researcher"),
                    json!({
                        ARG_TASK: "Find my longest book",
                        ARG_CONTEXT: "The reader is asking about their own shelf",
                        ARG_OUTPUT: "The title and its page count",
                    }),
                ),
                Reply::says("Your longest book is Ilium, at 576 pages."),
            ],
        ),
        (
            "researcher",
            vec![
                Reply::calls(
                    "query_books",
                    json!({"order_by": "pages", "descending": true}),
                ),
                Reply::says("Ilium, 576 pages."),
            ],
        ),
    ]);

    let agent = librarian(&[]);
    let (run, conclusion) = chat(
        &env,
        &scripts,
        &agent,
        as_user("ada@example.com"),
        "what is my longest book?",
    )
    .await?;
    assert_eq!(
        conclusion.answer(),
        Some("Your longest book is Ilium, at 576 pages.")
    );

    // What the parent's model was handed: the sub-agent's final message
    // **verbatim**, plus the run to read the working in and what it cost.
    let handed_back: Json = serde_json::from_str(&delegation_result(
        &run,
        &tool_names::subagent("researcher"),
    ))
    .unwrap();
    assert_eq!(handed_back["agent"], json!("researcher"));
    assert_eq!(handed_back["answer"], json!("Ilium, 576 pages."));
    assert_eq!(handed_back["steps"], json!(2));

    // …and that run is a real row, of the sub-agent, linked to its parent.
    let child_id = handed_back["run"].as_str().unwrap();
    let child = load_run(
        &env.catalog,
        sc_agent::RunId(child_id.parse().expect("a uuid")),
    )
    .await?
    .expect("the sub-agent's run row");
    assert_eq!(child.subject, "researcher");
    assert_eq!(child.state, sc_agent::RunState::Done);
    assert_eq!(child.attributes[ATTR_DELEGATED_BY], json!("librarian"));
    assert_eq!(child.attributes[ATTR_PARENT_RUN], json!(run.id.to_string()));
    assert_eq!(child.description, "delegated by `librarian`");
    Ok(())
}

#[tokio::test]
async fn the_sub_agent_sees_the_briefing_and_none_of_the_parents_conversation() -> Result<()> {
    // The reason the pattern exists. The child's context is the briefing; the
    // parent's context gains one tool result rather than the child's working.
    let env = Env::new().await?.with_engine();
    env.own("books", "owner === user.email").await?;
    save_all(&env.catalog, &env.registry, &[researcher(), librarian(&[])]).await?;

    let scripts = Scripts::new([
        (
            "librarian",
            vec![
                Reply::calls(
                    tool_names::subagent("researcher"),
                    json!({ARG_TASK: "Count the books on the reader's shelf", ARG_OUTPUT: "A number"}),
                ),
                Reply::says("Two."),
            ],
        ),
        (
            "researcher",
            vec![
                Reply::calls("query_books", json!({})),
                Reply::says("2 books."),
            ],
        ),
    ]);

    let agent = librarian(&[]);
    let secret = "my accountant is called Bob and this must not travel";
    chat(&env, &scripts, &agent, as_user("ada@example.com"), secret).await?;

    // The sub-agent's first request: one user message, which is the briefing.
    let child_request = &scripts.for_agent("researcher").requests()[0];
    assert_eq!(child_request.messages.len(), 1);
    let LlmMessage::User {
        content: briefing, ..
    } = &child_request.messages[0]
    else {
        panic!("the sub-agent's first message is the briefing");
    };
    assert!(briefing.contains("Count the books"), "{briefing}");
    assert!(briefing.contains("## Expected output"), "{briefing}");
    // The framing that fixes the "did the work, said nothing" failure mode.
    assert!(
        briefing.contains("`librarian` agent has asked you"),
        "{briefing}"
    );
    assert!(!briefing.contains(secret), "the parent's message travelled");

    // It is the sub-agent's own system prompt and the sub-agent's own tools —
    // not the parent's, which has neither.
    assert!(
        child_request
            .system
            .as_deref()
            .unwrap()
            .contains("look things up")
    );
    assert_eq!(
        child_request
            .tools
            .iter()
            .map(|t| &t.name)
            .collect::<Vec<_>>(),
        ["query_books"]
    );

    // And in the other direction: the parent never sees the sub-agent's query,
    // only what it concluded.
    let parent_last = scripts.for_agent("librarian").last_request().unwrap();
    let transcript = serde_json::to_string(
        &parent_last
            .messages
            .iter()
            .map(|m| format!("{m:?}"))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(transcript.contains("2 books."), "the answer came back");
    assert!(!transcript.contains("order_by"), "the working did not");
    Ok(())
}

#[tokio::test]
async fn the_sub_agent_runs_as_the_person_chatting_and_not_as_the_server() -> Result<()> {
    // Delegation is not a way around §7.3. The sub-agent reads `books` through
    // the *caller's* ownership formula, so Ada's delegation returns Ada's rows —
    // and the child run records Ada as the user it was for.
    let env = Env::new().await?.with_engine();
    env.own("books", "owner === user.email").await?;
    save_all(&env.catalog, &env.registry, &[researcher(), librarian(&[])]).await?;

    let scripts = Scripts::new([
        (
            "librarian",
            vec![
                Reply::calls(
                    tool_names::subagent("researcher"),
                    json!({ARG_TASK: "List every book you can see"}),
                ),
                Reply::says("Done."),
            ],
        ),
        (
            "researcher",
            vec![
                Reply::calls("query_books", json!({})),
                Reply::says("Listed them."),
            ],
        ),
    ]);

    let agent = librarian(&[]);
    let caller = as_user("bob@example.com");
    let bob = caller.user.as_ref().unwrap().id;
    let (run, _) = chat(&env, &scripts, &agent, caller, "what have I got?").await?;

    let child_id = serde_json::from_str::<Json>(&delegation_result(
        &run,
        &tool_names::subagent("researcher"),
    ))
    .unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let child = load_run(
        &env.catalog,
        sc_agent::RunId(child_id.parse().expect("a uuid")),
    )
    .await?
    .expect("the sub-agent's run");
    assert_eq!(
        child.user,
        Some(bob),
        "the child run is Bob's, not nobody's"
    );

    // Bob's two books, and neither of Ada's — the formula ran for the caller the
    // parent was chatting with.
    let rows = child
        .agent_loop()?
        .messages()
        .iter()
        .find_map(|m| match m {
            LlmMessage::ToolResult { content, name, .. } if name == "query_books" => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("the sub-agent's read is in its own transcript");
    let rows: Json = serde_json::from_str(&rows).unwrap();
    assert_eq!(common::titles(&rows), ["Emma", "Ubik"]);
    Ok(())
}

#[tokio::test]
async fn a_sub_agent_the_caller_may_not_use_is_refused_by_name() -> Result<()> {
    // Being allowed to chat with one agent does not thereby allow everything it
    // can reach — the rule `run_trigger` applies to a trigger, applied to an
    // agent. `researcher` here is admin-only.
    let env = Env::new().await?;
    let mut admin_only = researcher();
    admin_only.min_role = None;
    save_all(&env.catalog, &env.registry, &[admin_only, librarian(&[])]).await?;

    let scripts = Scripts::new([
        (
            "librarian",
            vec![
                Reply::calls(
                    tool_names::subagent("researcher"),
                    json!({ARG_TASK: "Look something up"}),
                ),
                Reply::says("I am not allowed to ask the researcher."),
            ],
        ),
        ("researcher", vec![Reply::says("never reached")]),
    ]);

    let agent = librarian(&[]);
    let (run, _) = chat(
        &env,
        &scripts,
        &agent,
        as_user("ada@example.com"),
        "look it up",
    )
    .await?;

    // A refusal the model reads and can report, not a failed run.
    let result = delegation_result(&run, &tool_names::subagent("researcher"));
    assert!(result.contains("may not use `researcher`"), "{result}");
    assert!(result.contains("role 1"), "{result}");
    // And nothing was spent on the sub-agent.
    assert_eq!(scripts.for_agent("researcher").remaining(), 1);
    Ok(())
}

#[tokio::test]
async fn a_delegation_that_would_loop_is_refused_with_the_path() -> Result<()> {
    // `a → b → a`. Self-delegation is refused at save time; this is the cycle
    // that needs the chain a run carries, and it is named rather than reported
    // as a budget.
    let env = Env::new().await?;
    // Neither can be saved first — each names the other — so the specialist is
    // saved plain, the parent against it, and the specialist then edited to point
    // back. That is also how an admin would arrive here, and the reason the cycle
    // cannot be refused on save.
    let plain = Agent::new("researcher", "main").min_role(80);
    save_all(
        &env.catalog,
        &env.registry,
        &[plain.clone(), librarian(&[])],
    )
    .await?;
    let back_again = plain.with_trait(
        EnabledTrait::new("subagent").configuration(config(&[(CFG_AGENT, json!("librarian"))])),
    );
    save_agent(&env.catalog, &env.registry, &back_again).await?;

    let scripts = Scripts::new([
        (
            "librarian",
            vec![
                Reply::calls(
                    tool_names::subagent("researcher"),
                    json!({ARG_TASK: "Ask the librarian"}),
                ),
                Reply::says("We would have gone round in circles."),
            ],
        ),
        (
            "researcher",
            vec![
                Reply::calls(
                    tool_names::subagent("librarian"),
                    json!({ARG_TASK: "Ask the researcher"}),
                ),
                Reply::says("The librarian would not take it back."),
            ],
        ),
    ]);

    let agent = librarian(&[]);
    let (run, conclusion) = chat(&env, &scripts, &agent, as_user("ada@example.com"), "go").await?;
    assert_eq!(
        conclusion.answer(),
        Some("We would have gone round in circles.")
    );

    // The sub-agent ran, tried to hand the work back, and was told why it could
    // not — in its own transcript, in words naming the loop.
    let child: Json = serde_json::from_str(&delegation_result(
        &run,
        &tool_names::subagent("researcher"),
    ))
    .unwrap();
    assert_eq!(
        child["answer"],
        json!("The librarian would not take it back.")
    );

    let child_run = load_run(
        &env.catalog,
        sc_agent::RunId(child["run"].as_str().unwrap().parse().unwrap()),
    )
    .await?
    .expect("the sub-agent's run");
    let refusal = child_run
        .agent_loop()?
        .messages()
        .iter()
        .find_map(|m| match m {
            LlmMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("the refused delegation is in the transcript");
    assert!(refusal.contains("would loop"), "{refusal}");
    assert!(
        refusal.contains("`librarian` → `researcher` → `librarian`"),
        "{refusal}"
    );
    Ok(())
}

#[tokio::test]
async fn a_chain_deeper_than_the_configured_bound_is_refused() -> Result<()> {
    // Three distinct agents is not a cycle, so only a number stops it. With the
    // bound at 1, the first delegation is allowed and the second is not.
    let env = Env::new().await?;
    let middle = Agent::new("researcher", "main").min_role(80).with_trait(
        EnabledTrait::new("subagent").configuration(config(&[
            (CFG_AGENT, json!("assistant")),
            (CFG_MAX_DEPTH, json!(1)),
        ])),
    );
    let bottom = Agent::new("assistant", "main")
        .min_role(80)
        .system_prompt("You help.");
    let top = librarian(&[(CFG_MAX_DEPTH, json!(1))]);
    save_all(&env.catalog, &env.registry, &[bottom, middle, top.clone()]).await?;

    let scripts = Scripts::new([
        (
            "librarian",
            vec![
                Reply::calls(
                    tool_names::subagent("researcher"),
                    json!({ARG_TASK: "Delegate onwards"}),
                ),
                Reply::says("Only one level was allowed."),
            ],
        ),
        (
            "researcher",
            vec![
                Reply::calls(
                    tool_names::subagent("assistant"),
                    json!({ARG_TASK: "Do the actual work"}),
                ),
                Reply::says("I had to do it myself."),
            ],
        ),
        ("assistant", vec![Reply::says("never reached")]),
    ]);

    let (run, _) = chat(&env, &scripts, &top, as_user("ada@example.com"), "go").await?;
    let child: Json = serde_json::from_str(&delegation_result(
        &run,
        &tool_names::subagent("researcher"),
    ))
    .unwrap();
    let child_run = load_run(
        &env.catalog,
        sc_agent::RunId(child["run"].as_str().unwrap().parse().unwrap()),
    )
    .await?
    .expect("the sub-agent's run");
    let refusal = child_run
        .agent_loop()?
        .messages()
        .iter()
        .find_map(|m| match m {
            LlmMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("the refused delegation is in the transcript");
    assert!(refusal.contains("2 agents deep"), "{refusal}");
    assert!(refusal.contains("allows 1"), "{refusal}");
    // The bottom agent was never connected.
    assert_eq!(scripts.for_agent("assistant").remaining(), 1);
    Ok(())
}

#[tokio::test]
async fn a_sub_agent_that_reports_nothing_is_a_failure_the_parent_can_act_on() -> Result<()> {
    // The documented failure mode of delegation: the specialist works all turn
    // and ends with an empty message. Handing that back as `answer: ""` would
    // have the parent report "nothing found" to the person.
    let env = Env::new().await?.with_engine();
    env.own("books", "owner === user.email").await?;
    let capped = researcher().attribute(sc_agent::ATTR_MAX_STEPS, 2);
    save_all(&env.catalog, &env.registry, &[capped, librarian(&[])]).await?;

    let scripts = Scripts::new([
        (
            "librarian",
            vec![
                Reply::calls(
                    tool_names::subagent("researcher"),
                    json!({ARG_TASK: "Count the books"}),
                ),
                Reply::says("The researcher did not get there."),
            ],
        ),
        (
            "researcher",
            vec![
                Reply::calls("query_books", json!({})),
                Reply::calls("query_books", json!({})),
                Reply::says("never reached: the budget stops it first"),
            ],
        ),
    ]);

    let agent = librarian(&[]);
    let (run, _) = chat(
        &env,
        &scripts,
        &agent,
        as_user("ada@example.com"),
        "count them",
    )
    .await?;

    let result = delegation_result(&run, &tool_names::subagent("researcher"));
    assert!(result.contains("whole budget of 2 steps"), "{result}");
    assert!(result.contains("smaller piece"), "{result}");
    // The transcript is still there to read, and the message says where.
    assert!(result.contains("run "), "{result}");
    Ok(())
}

#[tokio::test]
async fn one_delegation_may_bound_what_the_sub_agent_spends() -> Result<()> {
    // The step budget on the *calling* side: the sub-agent's own number was
    // chosen for it working alone, and the agent paying for the task is entitled
    // to bound it.
    let env = Env::new().await?.with_engine();
    env.own("books", "owner === user.email").await?;
    save_all(
        &env.catalog,
        &env.registry,
        &[researcher(), librarian(&[(CFG_MAX_STEPS, json!(1))])],
    )
    .await?;

    let scripts = Scripts::new([
        (
            "librarian",
            vec![
                Reply::calls(
                    tool_names::subagent("researcher"),
                    json!({ARG_TASK: "Count the books"}),
                ),
                Reply::says("It ran out of room."),
            ],
        ),
        (
            "researcher",
            vec![
                Reply::calls("query_books", json!({})),
                Reply::says("never reached"),
            ],
        ),
    ]);

    let agent = librarian(&[(CFG_MAX_STEPS, json!(1))]);
    let (run, _) = chat(&env, &scripts, &agent, as_user("ada@example.com"), "count").await?;
    let result = delegation_result(&run, &tool_names::subagent("researcher"));
    assert!(result.contains("budget of 1 steps"), "{result}");
    Ok(())
}

// --- configuration -----------------------------------------------------------

#[tokio::test]
async fn the_configuration_is_checked_on_save_and_again_on_load() -> Result<()> {
    let env = Env::new().await?;
    let catalog = &env.catalog;
    let registry = &env.registry;

    // An agent that does not exist.
    let err = save_agent(catalog, registry, &librarian(&[]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no agent named `researcher`"), "{err}");

    // An agent delegating to itself — the one cycle visible without running
    // anything, refused where it is cheapest to fix.
    let itself = Agent::new("librarian", "main").with_trait(
        EnabledTrait::new("subagent").configuration(config(&[(CFG_AGENT, json!("librarian"))])),
    );
    let err = save_agent(catalog, registry, &itself)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot delegate to itself"), "{err}");

    // A depth nobody could reason about.
    let too_deep = librarian(&[(CFG_MAX_DEPTH, json!(9))]);
    save_agent(catalog, registry, &researcher()).await?;
    let err = save_agent(catalog, registry, &too_deep)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("between 1 and 5"), "{err}");

    // The good one saves…
    save_agent(catalog, registry, &librarian(&[])).await?;
    assert!(
        Agents::load(catalog, registry)
            .await?
            .by_name("librarian")
            .is_some()
    );

    // …and stops being usable the moment its sub-agent is deleted — dropped from
    // the live set with its reason kept, still stored and still editable.
    let researcher_id = sc_agent::load_agent_by_name(catalog, "researcher")
        .await?
        .unwrap()
        .id;
    delete_agent(catalog, researcher_id).await?;
    let agents = Agents::load(catalog, registry).await?;
    assert!(agents.by_name("librarian").is_none());
    let issue = &agents.issues()[0];
    assert!(
        issue.problem.contains("no agent named `researcher`"),
        "{}",
        issue.problem
    );
    Ok(())
}

#[tokio::test]
async fn a_context_that_cannot_delegate_says_so_rather_than_running_an_agent() -> Result<()> {
    // A tool called outside a run — or inside one on a server that never
    // assembled a way to connect a sub-agent's provider — gets the configuration
    // error, not a second, weaker way to run an agent.
    let env = Env::new().await?;
    save_all(&env.catalog, &env.registry, &[researcher()]).await?;
    let err = env
        .call(
            "subagent",
            &config(&[(CFG_AGENT, json!("researcher"))]),
            json!({ARG_TASK: "look it up"}),
            &as_user("ada@example.com"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot delegate"), "{err}");
    Ok(())
}

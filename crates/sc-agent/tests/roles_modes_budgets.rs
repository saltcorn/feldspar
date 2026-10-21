//! The loop's roles, modes, per-run state, self-delegation, ledger and budgets
//! (coding agent milestone, Phase 2), end to end against the scripted provider
//! and a real `_fd_runs`.

use crate::common;

use std::sync::Arc;

use common::{Counter, catalog, registry};
use sc_agent::testing::{FakeModels, FakeProvider, Reply};
use sc_agent::{
    ATTR_CONTEXT_BUDGET, ATTR_MAX_COST, ATTR_MAX_WALL_SECONDS, ATTR_PARALLEL_TOOL_CALLS,
    ATTR_PARENT_RUN, Agent, AgentLoop, Budget, Conclusion, EnabledTrait, ModelRef, ModelRole,
    ProviderConnector, Run, RunCaller, RunMode, RunState, Runner, abort_run, load_run, save_agent,
    save_run,
};
use sc_error::Result;
use sc_llm::{LlmProvider, Prices};
use sc_test_harness::TestDb;
use serde_json::json;

fn tally_agent() -> Agent {
    Agent::new("builder", "main")
        .system_prompt("You build things.")
        .with_trait(EnabledTrait::new("tally"))
}

fn tool_names(request: &sc_llm::LlmRequest) -> Vec<String> {
    request.tools.iter().map(|t| t.name.clone()).collect()
}

#[tokio::test]
async fn roles_are_validated_like_the_agents_own_model() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    let agent = tally_agent().role(
        ModelRole::Strong,
        ModelRef::new("main", Some("claude-opus-5")),
    );
    save_agent(&catalog, &registry, &agent).await?;

    let missing = tally_agent().role(
        ModelRole::Cheap,
        ModelRef::new("main", Some("claude-haiku-9")),
    );
    let err = save_agent(&catalog, &registry, &missing).await.unwrap_err();
    assert!(err.to_string().contains("`cheap` role"), "{err}");
    assert!(err.to_string().contains("claude-haiku-9"), "{err}");

    let no_provider = tally_agent().role(ModelRole::Strong, ModelRef::new("elsewhere", None));
    let err = save_agent(&catalog, &registry, &no_provider)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`strong` role"), "{err}");
    assert!(err.to_string().contains("elsewhere"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_cost_budget_is_refused_until_every_model_has_a_price() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;

    let agent = tally_agent()
        .role(
            ModelRole::Strong,
            ModelRef::new("main", Some("claude-opus-5")),
        )
        .attribute(ATTR_MAX_COST, 2.5);
    let err = save_agent(&catalog, &registry, &agent).await.unwrap_err();
    assert!(err.to_string().contains("`executor` and `strong`"), "{err}");

    // Price both rows, and the same agent saves.
    let provider = sc_llm::require_llm_provider(&catalog, "main").await?;
    for model in sc_llm::list_llm_models(&catalog, &provider).await? {
        let priced = model
            .with(sc_llm::CFG_PRICE_INPUT, 3.0)
            .with(sc_llm::CFG_PRICE_OUTPUT, 15.0);
        sc_llm::save_llm_model(&catalog, &priced).await?;
    }
    save_agent(&catalog, &registry, &agent).await?;
    Ok(())
}

#[tokio::test]
async fn trait_state_survives_a_save_a_load_and_the_next_step() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = tally_agent();
    save_agent(&catalog, &registry, &agent).await?;

    let first = Arc::new(FakeProvider::new([
        Reply::calls("tally", json!({})),
        Reply::says("one"),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(first),
        RunCaller::system(),
    );
    let (run, _) = runner.start("count").await?;

    // A different runner, over the row as it was stored.
    let mut stored = load_run(&catalog, run.id).await?.expect("the run");
    let key = sc_agent::trait_state_key(0, "tally");
    assert_eq!(
        stored.agent_loop()?.trait_state(&key),
        Some(&json!({"count": 1}))
    );
    let second = Arc::new(FakeProvider::new([
        Reply::calls("tally", json!({})),
        Reply::says("two"),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(second.clone()),
        RunCaller::system(),
    );
    runner.continue_run(&mut stored, "again").await?;
    let LlmMessageResult(content) = last_tool_result(&second.requests()[1]);
    assert_eq!(content, "2");
    Ok(())
}

struct LlmMessageResult(String);

fn last_tool_result(request: &sc_llm::LlmRequest) -> LlmMessageResult {
    request
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            sc_llm::LlmMessage::ToolResult { content, .. } => {
                Some(LlmMessageResult(content.clone()))
            }
            _ => None,
        })
        .unwrap_or(LlmMessageResult(String::new()))
}

#[tokio::test]
async fn tools_follow_the_mode_and_parallel_calls_are_off_unless_asked() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = tally_agent().with_trait(EnabledTrait::new("count").config("collection", "books"));
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([Reply::says("ok"), Reply::says("ok")]));
    Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    )
    .start("act")
    .await?;
    Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    )
    .starting_in(RunMode::Plan, ModelRole::Executor)
    .start("plan")
    .await?;

    let requests = provider.requests();
    assert!(!tool_names(&requests[0]).contains(&"note_plan".to_owned()));
    assert!(tool_names(&requests[1]).contains(&"note_plan".to_owned()));
    assert!(tool_names(&requests[1]).contains(&"count_books".to_owned()));
    assert_eq!(requests[0].parallel_tool_calls, Some(false));

    let parallel = agent.clone().attribute(ATTR_PARALLEL_TOOL_CALLS, true);
    let provider = Arc::new(FakeProvider::new([Reply::says("ok")]));
    Runner::new(
        &catalog,
        &registry,
        &parallel,
        common::model(provider.clone()),
        RunCaller::system(),
    )
    .start("go")
    .await?;
    assert_eq!(provider.requests()[0].parallel_tool_calls, Some(true));
    Ok(())
}

#[tokio::test]
async fn a_run_started_as_strong_is_answered_by_the_strong_model_and_ledgered() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = tally_agent().role(
        ModelRole::Strong,
        ModelRef::new("main", Some("claude-opus-5")),
    );
    save_agent(&catalog, &registry, &agent).await?;

    let executor = Arc::new(FakeProvider::new([]));
    let strong = Arc::new(FakeProvider::new([Reply::says("planned")]).as_model("big"));
    let models: Arc<dyn ProviderConnector> = Arc::new(
        FakeModels::new()
            .role(ModelRole::Executor, executor.clone())
            .priced(
                ModelRole::Strong,
                strong.clone(),
                Prices {
                    input: Some(1_000_000.0),
                    output: Some(2_000_000.0),
                    ..Prices::default()
                },
            ),
    );
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(executor.clone()),
        RunCaller::system(),
    )
    .with_connector(&models)
    .starting_in(RunMode::Plan, ModelRole::Strong);
    let (run, conclusion) = runner.start("plan it").await?;
    assert_eq!(conclusion.answer(), Some("planned"));
    assert_eq!(executor.requests().len(), 0);
    assert_eq!(strong.requests().len(), 1);

    let stored = load_run(&catalog, run.id).await?.expect("run");
    assert_eq!(stored.mode()?, RunMode::Plan);
    assert_eq!(stored.role()?, ModelRole::Strong);
    let state = stored.agent_loop()?;
    let step = &state.ledger().steps()[0];
    assert_eq!(step.role, ModelRole::Strong);
    assert_eq!(step.model, "big");
    // 10 input tokens at 1 per token, 5 output at 2.
    assert_eq!(step.cost, Some(20.0));
    assert_eq!(state.ledger().totals()[&ModelRole::Strong].cost, Some(20.0));

    // Without a connector, a configured role cannot be reached, and says so.
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(executor),
        RunCaller::system(),
    )
    .starting_in(RunMode::Plan, ModelRole::Strong);
    let err = runner.start("plan it").await.unwrap_err();
    assert!(err.to_string().contains("`strong` role"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_planner_starts_one_session_of_itself_in_another_mode() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = tally_agent();
    save_agent(&catalog, &registry, &agent).await?;

    // One script, shared by every run, in the order they speak: the planner
    // delegates; the child asks for a `plan` session (refused) and an
    // `explore` one (allowed: the read-only leaf), whose run asks for an `act`
    // session (refused, two deep) and answers; the child answers; the planner
    // tries a same-mode session (refused) and answers.
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("start_session", json!({"mode": "act"})),
        Reply::calls("start_session", json!({"mode": "plan"})),
        Reply::calls("start_session", json!({"mode": "explore"})),
        Reply::calls("start_session", json!({"mode": "act"})),
        Reply::says("found it"),
        Reply::says("feature done"),
        Reply::calls("start_session", json!({"mode": "plan"})),
        Reply::says("all done"),
    ]));
    let models: Arc<dyn ProviderConnector> =
        Arc::new(FakeModels::new().role(ModelRole::Executor, provider.clone()));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    )
    .with_connector(&models)
    .starting_in(RunMode::Plan, ModelRole::Strong);
    let (run, conclusion) = runner.start("build two features").await?;
    assert_eq!(conclusion.answer(), Some("all done"));

    let requests = provider.requests();
    // The child's refused `plan` session, as its tool result.
    let refusal = last_tool_result(&requests[2]).0;
    assert!(
        refusal.contains("cannot start a session of its own agent, except one `explore`"),
        "{refusal}"
    );
    // The explore run, two deep, may start nothing of its own agent.
    let deep = last_tool_result(&requests[4]).0;
    assert!(deep.contains("would loop"), "{deep}");
    // The child got the explore run's answer, and the parent the child's.
    let explored = last_tool_result(&requests[5]).0;
    assert!(explored.contains("found it"), "{explored}");
    let session = last_tool_result(&requests[6]).0;
    assert!(session.contains("feature done"), "{session}");
    // A session in the parent's own mode is refused.
    let same = last_tool_result(&requests[7]).0;
    assert!(same.contains("different mode"), "{same}");

    let parent = load_run(&catalog, run.id).await?.expect("run");
    let state = parent.agent_loop()?;
    let child_id: serde_json::Value = serde_json::from_str(&session).expect("a JSON result");
    let child_id = child_id["run"].as_str().expect("child run id").to_owned();
    let child = load_run(
        &catalog,
        sc_agent::RunId(uuid::Uuid::parse_str(&child_id).expect("uuid")),
    )
    .await?
    .expect("child run");
    assert_eq!(child.subject, "builder");
    assert_eq!(child.mode()?, RunMode::Act);
    assert_eq!(child.role()?, ModelRole::Executor);
    assert_eq!(
        child.attributes[ATTR_PARENT_RUN],
        json!(parent.id.to_string())
    );
    // The child's three steps and its explore run's two roll up into the
    // parent's three. The parent's are ledgered as the role it was started as
    // — `strong`, answered here by the executor's model because the agent
    // leaves that role unset — and the sessions' as the executor.
    assert_eq!(state.ledger().children().len(), 1);
    assert_eq!(state.ledger().total().steps, 8);
    assert_eq!(state.ledger().totals()[&ModelRole::Strong].steps, 3);
    assert_eq!(state.ledger().totals()[&ModelRole::Executor].steps, 5);
    Ok(())
}

/// Phase 9's seams: a trait chooses the mode a new run starts in, and a tool
/// saves its state before the step ends, asks the cheap role a question that
/// is ledgered as an aside, reads a role's capabilities, and reads the state a
/// session of its own agent left.
#[tokio::test]
async fn a_trait_starts_the_run_in_its_mode_and_uses_the_runs_own_machinery() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = Agent::new("builder", "main")
        .with_trait(EnabledTrait::new("tally").config("starts", "plan"))
        .role(
            ModelRole::Cheap,
            ModelRef::new("main", Some("claude-opus-5")),
        );
    save_agent(&catalog, &registry, &agent).await?;

    let executor = Arc::new(FakeProvider::new([
        Reply::calls("aside", json!({})),
        Reply::calls("start_session", json!({"mode": "act"})),
        Reply::calls("tally", json!({})),
        Reply::says("counted"),
        Reply::says("planned"),
    ]));
    let cheap = Arc::new(FakeProvider::new([Reply::says("hi there")]).as_model("small"));
    let models: Arc<dyn ProviderConnector> = Arc::new(
        FakeModels::new()
            .role(ModelRole::Executor, executor.clone())
            .role(ModelRole::Cheap, cheap.clone()),
    );
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(executor.clone()),
        RunCaller::system(),
    )
    .with_connector(&models);
    let fresh = runner.new_run("plan it")?;
    assert_eq!(fresh.mode()?, RunMode::Plan);
    assert_eq!(fresh.role()?, ModelRole::Strong);

    let (run, conclusion) = runner.start("plan it").await?;
    assert_eq!(conclusion.answer(), Some("planned"));
    let requests = executor.requests();
    let aside: serde_json::Value =
        serde_json::from_str(&last_tool_result(&requests[1]).0).expect("JSON");
    assert_eq!(aside["saved"], json!({"saved": true}), "{aside}");
    assert_eq!(aside["answer"], "hi there");
    assert_eq!(aside["vision"], false);
    let session: serde_json::Value =
        serde_json::from_str(&last_tool_result(&requests[4]).0).expect("JSON");
    assert_eq!(session["state"], json!({"count": 1}), "{session}");
    // The question had no tools and was not part of the transcript.
    assert_eq!(cheap.requests().len(), 1);
    assert!(cheap.requests()[0].tools.is_empty());

    let stored = load_run(&catalog, run.id).await?.expect("run");
    assert_eq!(stored.mode()?, RunMode::Plan);
    let state = stored.agent_loop()?;
    let asides = state.ledger().asides();
    assert_eq!(asides.len(), 1);
    assert_eq!(
        (asides[0].role, asides[0].model.as_str()),
        (ModelRole::Cheap, "small")
    );
    assert_eq!(state.ledger().totals()[&ModelRole::Cheap].steps, 1);
    assert_eq!(state.ledger().totals()[&ModelRole::Strong].steps, 3);
    assert_eq!(state.ledger().totals()[&ModelRole::Executor].steps, 2);
    Ok(())
}

#[tokio::test]
async fn a_session_is_resumed_rather_than_started_twice() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = tally_agent();
    save_agent(&catalog, &registry, &agent).await?;

    // A planner run, and a child session of it that stopped mid-way (its model
    // had answered with a tool call before the process died).
    let planner = Run::new(&agent.name, &RunCaller::system(), &{
        let mut s = AgentLoop::for_agent(&agent);
        s.push_user("plan")?;
        s
    })
    .with_mode(RunMode::Plan);
    save_run(&catalog, &planner).await?;
    let mut child_state = AgentLoop::for_agent(&agent);
    child_state.push_user("do the feature")?;
    child_state.next_step();
    child_state.model_answered(sc_llm::AssistantMessage {
        tool_calls: vec![sc_llm::ToolCall {
            id: "c1".to_owned(),
            name: "tally".to_owned(),
            arguments: json!({}),
        }],
        ..sc_llm::AssistantMessage::default()
    })?;
    let mut child = Run::new(&agent.name, &RunCaller::system(), &child_state);
    child
        .attributes
        .insert(ATTR_PARENT_RUN.to_owned(), planner.id.to_string().into());
    save_run(&catalog, &child).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::says("resumed and done"),
        Reply::says("planner done"),
    ]));
    let models: Arc<dyn ProviderConnector> =
        Arc::new(FakeModels::new().role(ModelRole::Executor, provider.clone()));
    // The planner's re-dispatched call, driven straight into its tool step.
    let mut planner_state = planner.agent_loop()?;
    planner_state.next_step();
    planner_state.model_answered(sc_llm::AssistantMessage {
        tool_calls: vec![sc_llm::ToolCall {
            id: "p1".to_owned(),
            name: "start_session".to_owned(),
            arguments: json!({"mode": "act", "resume": child.id.to_string()}),
        }],
        ..sc_llm::AssistantMessage::default()
    })?;
    let mut planner = planner;
    planner.record(&planner_state);
    save_run(&catalog, &planner).await?;

    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    )
    .with_connector(&models);
    let conclusion = runner.drive(&mut planner).await?;
    assert_eq!(conclusion.answer(), Some("planner done"));

    let child = load_run(&catalog, child.id).await?.expect("child");
    assert_eq!(child.state, RunState::Done);
    let child_state = child.agent_loop()?;
    // The tally the child had been asked for ran on resume, then it answered.
    assert_eq!(child_state.messages().len(), 4);
    assert_eq!(
        sc_agent::list_live_children(&catalog, planner.id)
            .await?
            .len(),
        0
    );
    Ok(())
}

#[tokio::test]
async fn aborting_a_parent_aborts_its_live_children() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let state = {
        let mut s = AgentLoop::new(5);
        s.push_user("go")?;
        s
    };
    let mut parent = Run::new("builder", &RunCaller::system(), &state);
    save_run(&catalog, &parent).await?;
    let mut child = Run::new("builder", &RunCaller::system(), &state);
    child
        .attributes
        .insert(ATTR_PARENT_RUN.to_owned(), parent.id.to_string().into());
    save_run(&catalog, &child).await?;
    let mut grandchild = Run::new("helper", &RunCaller::system(), &state);
    grandchild
        .attributes
        .insert(ATTR_PARENT_RUN.to_owned(), child.id.to_string().into());
    save_run(&catalog, &grandchild).await?;
    let unrelated = Run::new("builder", &RunCaller::system(), &state);
    save_run(&catalog, &unrelated).await?;

    abort_run(&catalog, &mut parent).await?;
    for (id, expected) in [
        (parent.id, RunState::Aborted),
        (child.id, RunState::Aborted),
        (grandchild.id, RunState::Aborted),
        (unrelated.id, RunState::Running),
    ] {
        assert_eq!(load_run(&catalog, id).await?.expect("run").state, expected);
    }
    Ok(())
}

/// Drive `agent` (unsaved: budgets are checked by the loop, not on save) on a
/// script that calls `tool` for ever, and return how it ended.
async fn run_until_over_budget(
    agent: &Agent,
    tool: Reply,
    prices: Prices,
) -> Result<(Conclusion, AgentLoop)> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let provider = Arc::new(FakeProvider::repeating(tool, 10));
    let mut model = common::model(provider.clone());
    model.prices = prices;
    let runner = Runner::new(&catalog, &registry, agent, model, RunCaller::system());
    let (run, conclusion) = runner.start("go").await?;
    let stored = load_run(&catalog, run.id).await?.expect("run");
    assert_eq!(stored.state, RunState::Done);
    assert!(provider.model().starts_with("fake"));
    Ok((conclusion, stored.agent_loop()?))
}

#[tokio::test]
async fn each_budget_ends_a_run_as_over_budget() -> Result<()> {
    // Cost: 10 input tokens at 0.1 per token and 5 output at 0.2 is 2 per step;
    // a budget of 3 lets two steps through.
    let agent = tally_agent().attribute(ATTR_MAX_COST, 3.0);
    let (conclusion, state) = run_until_over_budget(
        &agent,
        Reply::calls("tally", json!({})),
        Prices {
            input: Some(100_000.0),
            output: Some(200_000.0),
            ..Prices::default()
        },
    )
    .await?;
    assert_eq!(
        conclusion,
        Conclusion::OverBudget {
            budget: Budget::Cost
        }
    );
    assert_eq!(state.step(), 2);
    assert_eq!(state.ledger().total().cost, Some(4.0));

    // Wall time: every tool call naps past a one-second budget.
    let agent = tally_agent().attribute(ATTR_MAX_WALL_SECONDS, 1);
    let (conclusion, state) = run_until_over_budget(
        &agent,
        Reply::calls("nap", json!({"ms": 1100})),
        Prices::default(),
    )
    .await?;
    assert_eq!(
        conclusion,
        Conclusion::OverBudget {
            budget: Budget::WallTime
        }
    );
    assert_eq!(state.step(), 1);
    assert!(state.ledger().working_ms() >= 1000);

    // Context: a budget smaller than the first request, which compacting
    // cannot shrink, ends the run before it calls the model.
    let agent = tally_agent().attribute(ATTR_CONTEXT_BUDGET, 10);
    let (conclusion, state) =
        run_until_over_budget(&agent, Reply::calls("tally", json!({})), Prices::default()).await?;
    assert_eq!(
        conclusion,
        Conclusion::OverBudget {
            budget: Budget::Context
        }
    );
    assert_eq!(state.step(), 0);
    assert_eq!(state.context().compactions().len(), 1);
    Ok(())
}

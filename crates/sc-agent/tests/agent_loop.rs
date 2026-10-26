//! The loop end to end: the [`Runner`] driving an [`AgentLoop`] against the
//! scripted [`FakeProvider`], with `_fd_runs` written after every step.
//!
//! Against a real Postgres, because the claim under test is not "the machine
//! transitions correctly" — `machine.rs`'s unit tests cover that without a
//! database — but "what the run row holds after each step is what the loop
//! holds". That is a claim about the row, and only a row can settle it.
//!
//! No test here needs an API key or spends a token (decision 7).

use crate::common;

use std::sync::Arc;

use common::{Counter, catalog, registry};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{
    Agent, AgentLoop, Conclusion, EnabledTrait, Run, RunCaller, RunState, Runner, list_runs,
    load_run, save_agent,
};
use sc_auth::User;
use sc_error::Result;
use sc_llm::LlmMessage;
use sc_test_harness::TestDb;
use serde_json::json;
use uuid::Uuid;

/// An agent that can count books, with a small step budget so the runaway test
/// does not need twenty scripted turns.
fn counting_agent() -> Agent {
    Agent::new("librarian", "main")
        .system_prompt("You answer questions about books.")
        .with_trait(EnabledTrait::new("count").config("collection", "books"))
        .attribute(sc_agent::ATTR_MAX_STEPS, 3)
}

#[tokio::test]
async fn a_two_turn_conversation_runs_with_no_traits_at_all() -> Result<()> {
    // Phase 2's "done when": an agent with no traits holds a conversation
    // through the library, and its run row reloads into the same history.
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = Agent::new("talker", "main").system_prompt("You chat.");
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::says("Hello there."),
        Reply::says("Still here."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );

    let (mut run, conclusion) = runner.start("hello").await?;
    assert_eq!(conclusion.answer(), Some("Hello there."));

    let conclusion = runner.continue_run(&mut run, "are you there?").await?;
    assert_eq!(conclusion.answer(), Some("Still here."));

    // The row holds the whole conversation: two questions, two answers.
    let stored = load_run(&catalog, run.id).await?.expect("the run row");
    assert_eq!(stored.state, RunState::Done);
    let state = stored.agent_loop()?;
    assert_eq!(state.messages().len(), 4);
    assert_eq!(state.step(), 2);
    // Usage accumulates across the run rather than being the last call's.
    assert_eq!(state.usage().total_tokens(), 30);

    // The system prompt reached the provider, and the second request carried the
    // first exchange.
    let requests = provider.requests();
    assert_eq!(requests[0].system.as_deref(), Some("You chat."));
    assert_eq!(requests[1].messages.len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_tool_call_runs_and_the_conversation_continues() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let counter = Counter::new();
    let registry = registry(counter.clone())?;
    let agent = counting_agent();
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::calls("count_books", json!({})).with_preamble("Let me look."),
        Reply::says("There are three."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );

    let (run, conclusion) = runner.start("how many books?").await?;
    assert_eq!(conclusion.answer(), Some("There are three."));
    assert_eq!(counter.seen().len(), 1);

    // The tool was offered under the name its configuration derives.
    let offered: Vec<String> = provider.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(offered, vec!["count_books"]);

    // The transcript has the call and its result, in that order.
    let state = load_run(&catalog, run.id)
        .await?
        .expect("run")
        .agent_loop()?;
    assert!(matches!(
        state.messages()[1],
        LlmMessage::Assistant { ref tool_calls, .. } if tool_calls.len() == 1
    ));
    let LlmMessage::ToolResult { content, name, .. } = &state.messages()[2] else {
        panic!("the third message is the tool result");
    };
    assert_eq!(name, "count_books");
    assert_eq!(content, "3");
    Ok(())
}

#[tokio::test]
async fn two_tool_calls_in_one_turn_run_in_the_order_the_model_asked() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let counter = Counter::new();
    let registry = registry(counter.clone())?;
    let agent = Agent::new("both", "main")
        .with_trait(EnabledTrait::new("count").config("collection", "orders"))
        .with_trait(EnabledTrait::new("count").config("collection", "books"));
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::calls_many([
            ("count_books".to_owned(), json!({"which": "first"})),
            ("count_orders".to_owned(), json!({"which": "second"})),
        ]),
        Reply::says("Three and seven."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider),
        RunCaller::system(),
    );
    let (run, _) = runner.start("count both").await?;

    // Sequentially, in the model's order — which is the order the arguments
    // arrived at the trait.
    let seen = counter.seen();
    assert_eq!(seen[0]["which"], "first");
    assert_eq!(seen[1]["which"], "second");

    // And both results are in the transcript, correlated with their calls.
    let state = load_run(&catalog, run.id)
        .await?
        .expect("run")
        .agent_loop()?;
    let results: Vec<&str> = state
        .messages()
        .iter()
        .filter_map(|m| match m {
            LlmMessage::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["3", "7"]);
    Ok(())
}

#[tokio::test]
async fn a_tool_that_fails_leaves_its_error_in_the_transcript_and_the_run_goes_on() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = Agent::new("broken", "main").with_trait(
        EnabledTrait::new("count")
            .config("collection", "books")
            .config("always_fails", true),
    );
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::calls("count_books", json!({})),
        Reply::says("I could not count them."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider),
        RunCaller::system(),
    );
    let (run, conclusion) = runner.start("how many?").await?;

    // The run finished normally: a failing tool is a result, not an exception.
    assert_eq!(conclusion.answer(), Some("I could not count them."));
    let stored = load_run(&catalog, run.id).await?.expect("run");
    assert_eq!(stored.state, RunState::Done);
    assert_eq!(stored.error, None);

    let state = stored.agent_loop()?;
    let LlmMessage::ToolResult { content, .. } = &state.messages()[2] else {
        panic!("the third message is the tool result");
    };
    // Readable by the model, which is what lets it recover.
    assert!(content.starts_with("error: "), "{content}");
    assert!(content.contains("unavailable"), "{content}");
    Ok(())
}

#[tokio::test]
async fn a_tool_the_model_invented_comes_back_as_a_result_naming_the_real_ones() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = counting_agent();
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::calls("delete_everything", json!({})),
        Reply::says("Sorry, I cannot."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider),
        RunCaller::system(),
    );
    let (run, conclusion) = runner.start("delete it all").await?;
    assert_eq!(conclusion.answer(), Some("Sorry, I cannot."));

    let state = load_run(&catalog, run.id)
        .await?
        .expect("run")
        .agent_loop()?;
    let LlmMessage::ToolResult { content, .. } = &state.messages()[2] else {
        panic!("the third message is the tool result");
    };
    assert!(content.contains("no tool named"), "{content}");
    // Told what it *may* call, because that is the recovery it needs.
    assert!(content.contains("count_books"), "{content}");
    Ok(())
}

#[tokio::test]
async fn a_model_that_never_stops_asking_is_stopped_by_the_step_budget() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = counting_agent(); // max_steps = 3
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::repeating(
        Reply::calls("count_books", json!({})),
        10,
    ));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );
    let (run, conclusion) = runner.start("keep going").await?;

    assert_eq!(conclusion, Conclusion::MaxSteps);
    // Exactly the budget, and not one call more.
    assert_eq!(provider.requests().len(), 3);
    let stored = load_run(&catalog, run.id).await?.expect("run");
    assert_eq!(stored.state, RunState::Done);
    assert_eq!(stored.agent_loop()?.step(), 3);
    Ok(())
}

#[tokio::test]
async fn a_provider_failure_fails_the_run_and_keeps_what_had_happened() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = counting_agent();
    save_agent(&catalog, &registry, &agent).await?;

    // One good turn, then the provider refuses — the shape of a key that expires
    // mid-conversation.
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("count_books", json!({})),
        Reply::fails("401 invalid x-api-key"),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider),
        RunCaller::system(),
    );

    let mut state = AgentLoop::new(agent.max_steps());
    state.push_user("how many?")?;
    let mut run = Run::new(&agent.name, &RunCaller::system(), &state);
    sc_agent::save_run(&catalog, &run).await?;
    let err = runner.drive(&mut run).await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");

    // The row says what happened, and still holds the work that had been done —
    // a chat window that simply stopped would be unfixable by the person
    // watching it.
    let stored = load_run(&catalog, run.id).await?.expect("run");
    assert_eq!(stored.state, RunState::Failed);
    assert!(stored.error.as_deref().unwrap_or_default().contains("401"));
    assert_eq!(stored.agent_loop()?.messages().len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_run_is_written_after_every_step_and_resumes_from_the_row() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = counting_agent();
    save_agent(&catalog, &registry, &agent).await?;

    // First process: it gets as far as the tool result and then "dies" — its
    // provider script has no second turn.
    let dying = Arc::new(FakeProvider::new([Reply::calls("count_books", json!({}))]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(dying),
        RunCaller::system(),
    );
    let mut state = AgentLoop::new(agent.max_steps());
    state.push_user("how many books?")?;
    let mut run = Run::new(&agent.name, &RunCaller::system(), &state);
    sc_agent::save_run(&catalog, &run).await?;
    assert!(runner.drive(&mut run).await.is_err());

    // What is on the row is what the loop held: the question, the tool call and
    // its result — written before the step that failed was attempted.
    let stored = load_run(&catalog, run.id).await?.expect("run");
    let recovered = stored.agent_loop()?;
    assert_eq!(recovered.messages().len(), 3);
    assert_eq!(recovered.step(), 1);

    // Second process: nothing but the row, and the conversation carries on.
    let mut resumed = load_run(&catalog, run.id).await?.expect("run");
    resumed.state = RunState::Running;
    let provider = Arc::new(FakeProvider::new([Reply::says("There are three.")]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider),
        RunCaller::system(),
    );
    let conclusion = runner.drive(&mut resumed).await?;
    assert_eq!(conclusion.answer(), Some("There are three."));

    let finished = load_run(&catalog, run.id).await?.expect("run");
    assert_eq!(finished.state, RunState::Done);
    assert_eq!(finished.agent_loop()?.messages().len(), 4);
    Ok(())
}

#[tokio::test]
async fn a_trait_speaks_before_every_turn_and_the_run_records_its_caller() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = Agent::new("guided", "main")
        .system_prompt("You are helpful.")
        .with_trait(EnabledTrait::new("count").config("collection", "books"))
        .with_trait(EnabledTrait::new("preamble").config("text", "Today is Tuesday."));
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::calls("count_books", json!({})),
        Reply::says("Three, on a Tuesday."),
    ]));
    let user = User::new(Uuid::new_v4(), 40)?;
    let user_id = user.id;
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::user(user),
    );
    let (run, _) = runner.start("how many books?").await?;

    // `on_turn` ran before *both* calls, and knew which step each was.
    let systems: Vec<String> = provider
        .requests()
        .iter()
        .map(|r| r.system.clone().unwrap_or_default())
        .collect();
    assert_eq!(systems[0], "You are helpful.\n\nToday is Tuesday. (step 1)");
    assert_eq!(systems[1], "You are helpful.\n\nToday is Tuesday. (step 2)");

    // The run records who it was for — the authority every tool in it ran with.
    let stored = load_run(&catalog, run.id).await?.expect("run");
    assert_eq!(stored.user, Some(user_id));
    assert_eq!(stored.subject, "guided");

    // And it is in that agent's history, which is what the chat panel lists.
    let history = list_runs(&catalog, "guided").await?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, run.id);
    Ok(())
}

/// What changes per step is sent after the conversation, so each request's
/// system prompt, tools and history are the previous request's, byte for
/// byte, and a host's prompt cache can serve them (the field report's prompt
/// caching point). The context is not kept on the run.
#[tokio::test]
async fn per_step_context_follows_a_byte_identical_prefix() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry(Counter::new())?;
    let agent = Agent::new("guided", "main")
        .system_prompt("You are helpful.")
        .with_trait(EnabledTrait::new("count").config("collection", "books"))
        .with_trait(EnabledTrait::new("preamble").config("context", "The page is /books."));
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::calls("count_books", json!({})),
        Reply::says("Three."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );
    let (run, _) = runner.start("how many books?").await?;
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    // The same system prompt and tools on both steps.
    assert_eq!(requests[0].system, requests[1].system);
    assert_eq!(requests[0].system.as_deref(), Some("You are helpful."));
    assert_eq!(requests[0].tools, requests[1].tools);
    // The second request's history starts with the first's, without its
    // context, and each ends on its own step's context.
    let first = &requests[0].messages;
    let second = &requests[1].messages;
    assert_eq!(first[..first.len() - 1], second[..first.len() - 1]);
    let last_text = |m: &[sc_llm::LlmMessage]| match m.last() {
        Some(sc_llm::LlmMessage::User { content }) => content.clone(),
        other => panic!("the request should end on the context: {other:?}"),
    };
    assert!(last_text(first).contains("The page is /books. (step 1)"));
    assert!(last_text(second).contains("The page is /books. (step 2)"));
    // The run keeps the conversation, not the context.
    let stored = load_run(&catalog, run.id).await?.expect("run");
    let kept = format!("{:?}", stored.agent_loop()?.messages());
    assert!(!kept.contains("The page is"), "{kept}");
    Ok(())
}

/// What a run says about itself while it happens (§16).
///
/// The model call's own lines come from the provider decorator, which a
/// [`FakeProvider`] built by hand is deliberately not wrapped in — so what is
/// under test here is exactly the loop's half: the tools it ran, and how the
/// run ended.
#[tokio::test]
async fn a_run_at_trace_logs_every_tool_it_ran_and_how_it_ended() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let counter = Counter::new();
    let registry = registry(counter.clone())?;
    let agent = counting_agent();
    save_agent(&catalog, &registry, &agent).await?;

    let provider = Arc::new(FakeProvider::new([
        Reply::calls("count_books", json!({ "since": 1999 })),
        Reply::says("There are three."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );

    let _log = sc_log::capture::guard(sc_log::Verbosity::Trace);
    let (run, conclusion) = runner.start("how many books?").await?;
    let log = sc_log::capture::take().join("\n");

    assert_eq!(conclusion.answer(), Some("There are three."));

    // The tool: named on its own line with how long it took, and its arguments
    // and its result in full — the two halves of the conversation that the
    // model's own request dump only shows a turn later.
    assert!(log.contains("tool `count_books` ok in"), "{log}");
    assert!(log.contains("\"since\": 1999"), "{log}");
    assert!(log.contains("tool `count_books` result"), "{log}");
    // How the run ended, with the step count and the run's own id — the line
    // that ties a transcript to a row in `_fd_runs`.
    assert!(
        log.contains(&format!(
            "agent `librarian` run {}: answered after 2 steps",
            run.id
        )),
        "{log}"
    );
    Ok(())
}

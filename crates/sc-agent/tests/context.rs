//! The context (coding agent milestone, Phase 4): the request layout, the
//! session header, clearing and compaction, end to end against the scripted
//! provider and a real `_fd_runs`.

use crate::common;

use std::sync::Arc;

use common::{Notes, catalog, registry_with_notes};
use sc_agent::context::{SUMMARY_HEADING, SUMMARY_PROMPT};
use sc_agent::testing::{FakeModels, FakeProvider, Match, Reply};
use sc_agent::{
    ATTR_CONTEXT_BUDGET, ATTR_KEEP_TURNS, Agent, EnabledTrait, ModelRef, ModelRole,
    ProviderConnector, RunCaller, RunState, Runner, load_run,
};
use sc_error::Result;
use sc_llm::{LlmMessage, LlmRequest, estimate_tokens};
use sc_test_harness::TestDb;
use serde_json::json;

fn notes_agent() -> Agent {
    Agent::new("builder", "main")
        .system_prompt("You build things.")
        .with_trait(EnabledTrait::new("notes"))
}

/// Every tool result in `request` answers a call made earlier in it.
fn every_result_has_its_call(request: &LlmRequest) -> bool {
    let mut calls: Vec<&str> = Vec::new();
    request.messages.iter().all(|m| match m {
        LlmMessage::Assistant { tool_calls, .. } => {
            calls.extend(tool_calls.iter().map(|c| c.id.as_str()));
            true
        }
        LlmMessage::ToolResult { tool_call_id, .. } => calls.contains(&tool_call_id.as_str()),
        LlmMessage::User { .. } => true,
    })
}

#[tokio::test]
async fn the_header_is_built_once_and_the_prefix_stays_byte_identical() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let notes = Notes::new();
    let registry = registry_with_notes(notes.clone())?;
    let agent = notes_agent();
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("dump", json!({"chars": 20})),
        Reply::says("one"),
        Reply::fails("503 overloaded"),
        Reply::says("two"),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );

    let (mut run, conclusion) = runner.start("add a page").await?;
    assert_eq!(conclusion.answer(), Some("one"));
    assert_eq!(notes.headers_built(), 1, "not rebuilt on step 2");

    // The next turn fails at the provider; resuming the failed run carries on.
    assert!(runner.continue_run(&mut run, "and another").await.is_err());
    let mut run = load_run(&catalog, run.id).await?.expect("run");
    assert_eq!(run.state, RunState::Failed);
    let conclusion = runner.drive(&mut run).await?;
    assert_eq!(conclusion.answer(), Some("two"));
    assert_eq!(
        notes.headers_built(),
        1,
        "not rebuilt on a new turn or a resume"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    provider.assert_stable_prefix();
    // Two consecutive requests share everything up to the history, and the
    // history only grows.
    for pair in requests.windows(2) {
        let before = serde_json::to_string(&pair[0].messages).unwrap_or_default();
        let after =
            serde_json::to_string(&pair[1].messages[..pair[0].messages.len()]).unwrap_or_default();
        assert_eq!(before, after);
    }

    // The header is the first message, built from the first thing said.
    assert_eq!(
        provider.session_header(0).as_deref(),
        Some("# Notes for builder in act mode\nbrief: add a page")
    );
    assert_eq!(requests[0].messages[1], LlmMessage::user("add a page"));
    let first = &requests[0];
    assert!(first.cache.prefix && first.cache.tail);
    assert_eq!(first.cache.session_header, Some(0));
    assert_eq!(first.prompt_cache_key, Some(run.id.to_string()));
    // Tools in name order, whatever order the trait declared them in.
    let names: Vec<&str> = first.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["check", "dump"]);

    // Stored with the run, and the trait's state written in the hook with it;
    // the transcript itself carries no header.
    let state = run.agent_loop()?;
    assert_eq!(
        state.context().header(),
        Some("# Notes for builder in act mode\nbrief: add a page")
    );
    assert_eq!(state.messages()[0], LlmMessage::user("add a page"));
    assert_eq!(
        state.trait_state(&sc_agent::trait_state_key(0, "notes")),
        Some(&json!({"headers": 1}))
    );
    Ok(())
}

#[tokio::test]
async fn old_tool_results_are_cleared_in_one_batch_and_the_transcript_stays_whole() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let notes = Notes::new();
    let registry = registry_with_notes(notes.clone())?;
    // Three 4000-character results are about 3400 tokens: past 80% of 4000
    // before the fourth call, and well under half once two are stubs.
    let agent = notes_agent()
        .attribute(ATTR_CONTEXT_BUDGET, 4000)
        .attribute(ATTR_KEEP_TURNS, 1);
    let provider = Arc::new(
        FakeProvider::new([
            Reply::calls("check", json!({"chars": 4000})),
            Reply::calls("dump", json!({"chars": 4000})),
            Reply::calls("dump", json!({"chars": 4000})),
            Reply::says("done"),
        ])
        .counting_input(),
    );
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );
    let (run, conclusion) = runner.start("go").await?;
    assert_eq!(conclusion.answer(), Some("done"));

    // Compacting called no model: pass 1 was enough.
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(FakeProvider::elided_results(&requests[2]), 0);
    let last = &requests[3];
    assert_eq!(FakeProvider::elided_results(last), 2);
    assert!(every_result_has_its_call(last));
    assert!(
        estimate_tokens(last, "") < 2000,
        "{}",
        estimate_tokens(last, "")
    );
    let results: Vec<&str> = last
        .messages
        .iter()
        .filter_map(|m| match m {
            LlmMessage::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        results[0], "[elided check: 1 failing]",
        "the trait's own stub"
    );
    assert_eq!(results[1], "[elided: 4000 characters of dump output]");
    assert_eq!(results[2].len(), 4000, "the kept turn is whole");
    // Clearing results leaves the prefix alone.
    provider.assert_stable_prefix();

    let stored = load_run(&catalog, run.id).await?.expect("run");
    let state = stored.agent_loop()?;
    for message in state.messages() {
        if let LlmMessage::ToolResult { content, .. } = message {
            assert_eq!(content.len(), 4000, "the stored transcript is whole");
        }
    }
    let compactions = state.context().compactions();
    assert_eq!(compactions.len(), 1);
    assert_eq!(compactions[0].step, 4);
    assert_eq!(compactions[0].elided, 2);
    assert_eq!(compactions[0].at, 7);
    assert!(compactions[0].summary.is_none());
    assert!(compactions[0].before_tokens >= 3000);
    assert!(compactions[0].after_tokens < 2000);
    let flags: Vec<bool> = state.ledger().steps().iter().map(|s| s.compacted).collect();
    assert_eq!(flags, vec![false, false, false, true]);
    Ok(())
}

#[tokio::test]
async fn when_clearing_is_not_enough_the_cheap_role_summarises_the_older_turns() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let notes = Notes::new();
    let registry = registry_with_notes(notes.clone())?;
    let agent = notes_agent()
        .attribute(ATTR_CONTEXT_BUDGET, 8000)
        .attribute(ATTR_KEEP_TURNS, 1)
        .role(
            ModelRole::Cheap,
            ModelRef::new("main", Some("claude-opus-5")),
        );
    // What fills the context is the model's own long preambles, which clearing
    // does not touch.
    let talk = "I am thinking about this at length. ".repeat(170);
    let turn = |n: usize| {
        Reply::calls("dump", json!({"chars": 100})).with_preamble(format!("Turn {n}. {talk}"))
    };
    let executor = Arc::new(
        FakeProvider::new([turn(1), turn(2), turn(3), turn(4), Reply::says("done")])
            .counting_input(),
    );
    let cheap = Arc::new(
        FakeProvider::new([])
            .when(
                Match::Summary,
                [Reply::says(
                    "## Goal\nbuild it\n## Decisions\nnone\n## Files changed\nnone\n\
                     ## Failing checks\nnone\n## Next step\nturn 4",
                )],
            )
            .as_model("small"),
    );
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
    let (run, conclusion) = runner.start("build it").await?;
    assert_eq!(conclusion.answer(), Some("done"));

    // The cheap role was asked once, under the summary prompt, with the
    // conversation it replaces.
    let summaries = cheap.requests();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].system.as_deref(), Some(SUMMARY_PROMPT));
    let LlmMessage::User {
        content: source, ..
    } = &summaries[0].messages[0]
    else {
        panic!("the conversation is one user message");
    };
    assert!(source.contains("build it") && source.contains("Turn 3."));
    assert!(
        !source.contains("Turn 4."),
        "the kept turn is not summarised"
    );

    // The executor's last request: header, summary, then the kept turn.
    let requests = executor.requests();
    assert_eq!(requests.len(), 5);
    let last = &requests[4];
    assert_eq!(last.messages.len(), 4);
    let LlmMessage::User { content, .. } = &last.messages[1] else {
        panic!("the summary follows the header");
    };
    assert!(content.starts_with(SUMMARY_HEADING) && content.contains("## Next step"));
    assert!(matches!(
        &last.messages[2],
        LlmMessage::Assistant { content, .. } if content.starts_with("Turn 4.")
    ));
    assert!(every_result_has_its_call(last));
    assert!(estimate_tokens(last, "") < 8000);
    executor.assert_stable_prefix();

    let stored = load_run(&catalog, run.id).await?.expect("run");
    let state = stored.agent_loop()?;
    assert_eq!(state.messages().len(), 10, "the stored transcript is whole");
    let compactions = state.context().compactions();
    assert_eq!(compactions.len(), 1);
    assert_eq!(compactions[0].up_to_index, Some(7));
    assert!(
        compactions[0]
            .summary
            .as_deref()
            .is_some_and(|s| s.starts_with("## Goal"))
    );
    assert!(compactions[0].after_tokens < 4000);
    // The summary is on the ledger, for the cheap role, and is not a step.
    assert_eq!(state.ledger().steps().len(), 5);
    assert!(state.ledger().steps()[4].compacted);
    assert_eq!(state.ledger().summaries()[0].model, "small");
    assert_eq!(state.ledger().totals()[&ModelRole::Cheap].steps, 1);
    Ok(())
}

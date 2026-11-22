//! Loop control (coding agent milestone, Phase 3), end to end against the
//! scripted provider and a real `_fd_runs`: argument schemas, fingerprints,
//! the detectors, the malformed-call cap, signals, and each rung of the
//! escalation ladder with the role that answered each step.

use crate::common;

use std::sync::Arc;

use common::catalog;
use sc_agent::testing::{FakeModels, FakeProvider, Reply};
use sc_agent::{
    ATTR_MAX_IDENTICAL_CALLS, AfterToolsContext, Agent, AgentRegistry, AgentTrait, Conclusion,
    EnabledTrait, ModelRef, ModelRole, ProviderConnector, Run, RunCaller, RunState, Runner, Signal,
    ToolsContext, TraitContext, load_run, save_agent,
};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_llm::{LlmMessage, LlmRequest, ToolSpec};
use sc_test_harness::TestDb;
use sc_types::{Attrs, FormField};
use serde_json::{Value as Json, json};

/// A trait shaped like the coding tools the ladder is for: `edit` has a real
/// schema and raises `EditFailed` when asked to fail, and `read`'s fingerprint
/// ignores the offset, so reading one file at three offsets is the same call.
struct Editor;

#[async_trait::async_trait]
impl AgentTrait for Editor {
    fn name(&self) -> &str {
        "editor"
    }

    fn description(&self) -> &str {
        "Edit and read files"
    }

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "edit",
                "Edit a file",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "line": {"type": "integer", "minimum": 1},
                        "fail": {"type": "boolean"},
                        "check": {"type": "boolean"}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            ToolSpec::new(
                "read",
                "Read a file",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "offset": {"type": "integer"}
                    },
                    "required": ["path"]
                }),
            ),
        ]
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        match tool {
            "edit" if args["fail"] == json!(true) => {
                ctx.signal(Signal::EditFailed);
                Err(Error::invalid("the old text does not appear in the file"))
            }
            "edit" => {
                let state = ctx.state();
                let edits = state.get("edits").and_then(Json::as_u64).unwrap_or(0) + 1;
                *state = json!({"edits": edits});
                Ok(json!("edited"))
            }
            "read" => Ok(json!("contents")),
            other => Err(Error::invalid(format!("no tool `{other}`"))),
        }
    }

    /// Once per turn: how many of the turn's calls were edits asking to be
    /// checked, said once on the last of this trait's results.
    async fn after_tools(
        &self,
        _config: &Attrs,
        cx: &mut AfterToolsContext<'_>,
    ) -> Result<Option<String>> {
        let checked = cx
            .calls
            .iter()
            .filter(|c| c.name == "edit" && c.arguments["check"] == json!(true))
            .count();
        Ok((checked > 0).then(|| format!("checked {checked} edits of {} calls", cx.calls.len())))
    }

    fn fingerprint(&self, _config: &Attrs, tool: &str, args: &Json) -> Json {
        match tool {
            "read" => json!({"path": args["path"]}),
            _ => args.clone(),
        }
    }
}

fn registry() -> Result<AgentRegistry> {
    let mut registry = AgentRegistry::new();
    registry.register(Arc::new(Editor))?;
    Ok(registry)
}

fn editor_agent() -> Agent {
    Agent::new("editor", "main")
        .system_prompt("You edit files.")
        .with_trait(EnabledTrait::new("editor"))
        .role(
            ModelRole::Strong,
            ModelRef::new("main", Some("claude-opus-5")),
        )
}

/// Every tool result in `request`, oldest first.
fn tool_results(request: &LlmRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter_map(|m| match m {
            LlmMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

/// Every tool result stored in `run`, oldest first.
fn stored_results(run: &Run) -> Vec<String> {
    run.agent_loop()
        .unwrap()
        .messages()
        .iter()
        .filter_map(|m| match m {
            LlmMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

/// The role that answered each step of `run`.
fn roles(run: &Run) -> Vec<ModelRole> {
    run.agent_loop()
        .unwrap()
        .ledger()
        .steps()
        .iter()
        .map(|s| s.role)
        .collect()
}

struct World {
    catalog: Catalog,
    registry: AgentRegistry,
    _db: TestDb,
}

async fn world(agent: &Agent) -> Result<World> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry()?;
    save_agent(&catalog, &registry, agent).await?;
    Ok(World {
        catalog,
        registry,
        _db: db,
    })
}

/// Drive `agent` from `message` with scripted executor and strong models.
async fn run_scripted(
    world: &World,
    agent: &Agent,
    executor: &Arc<FakeProvider>,
    strong: &Arc<FakeProvider>,
    message: &str,
) -> Result<(Run, Conclusion)> {
    let connector: Arc<dyn ProviderConnector> = Arc::new(
        FakeModels::new()
            .role(ModelRole::Executor, Arc::clone(executor))
            .role(ModelRole::Strong, Arc::clone(strong)),
    );
    Runner::new(
        &world.catalog,
        &world.registry,
        agent,
        common::model(Arc::clone(executor)),
        RunCaller::system(),
    )
    .with_connector(&connector)
    .start(message)
    .await
}

#[tokio::test]
async fn arguments_that_do_not_match_the_schema_are_refused_naming_each_path() -> Result<()> {
    let agent = editor_agent();
    let world = world(&agent).await?;
    let executor = Arc::new(FakeProvider::new([
        Reply::calls("edit", json!({"line": 2})),
        Reply::calls("edit", json!({"path": 3, "line": 1.5})),
        Reply::calls("edit", json!({"path": "a.ts"})),
        Reply::calls("edit", json!({"path": "a.ts", "file": "b.ts"})),
        Reply::says("done"),
    ]));
    let strong = Arc::new(FakeProvider::new([]));
    let (run, conclusion) = run_scripted(&world, &agent, &executor, &strong, "edit").await?;
    assert_eq!(conclusion.answer(), Some("done"));

    let results = stored_results(&run);
    assert!(
        results[0].contains("`/path`: is required"),
        "{}",
        results[0]
    );
    assert!(
        results[1].contains("`/line`: expected an integer, got a number")
            && results[1].contains("`/path`: expected a string, got an integer"),
        "{}",
        results[1]
    );
    assert_eq!(results[2], "edited");
    assert!(
        results[3].contains("`/file`: is not a known argument"),
        "{}",
        results[3]
    );
    // Only the valid call reached the trait.
    let key = sc_agent::trait_state_key(0, "editor");
    assert_eq!(
        run.agent_loop()?.trait_state(&key),
        Some(&json!({"edits": 1}))
    );
    Ok(())
}

#[tokio::test]
async fn three_malformed_calls_in_a_row_end_the_run_stuck() -> Result<()> {
    let agent = editor_agent();
    let world = world(&agent).await?;
    let executor = Arc::new(FakeProvider::new([
        // An unknown tool.
        Reply::calls("edit_file", json!({"path": "a.ts"})),
        // Arguments that never parsed, as the adapters pass them on.
        Reply::calls("edit", json!("{\"path\": \"a.ts\"")),
        // A schema failure.
        Reply::calls("edit", json!({})),
        Reply::says("never reached"),
    ]));
    let strong = Arc::new(FakeProvider::new([]));
    let (run, conclusion) = run_scripted(&world, &agent, &executor, &strong, "edit").await?;

    let Conclusion::Stuck { reason } = &conclusion else {
        panic!("expected stuck, got {conclusion:?}");
    };
    assert!(
        reason.starts_with("3 malformed tool calls in a row"),
        "{reason}"
    );
    assert_eq!(executor.remaining(), 1);

    let results = stored_results(&run);
    assert!(
        results[0].contains("no tool named `edit_file`"),
        "{}",
        results[0]
    );
    assert!(results[1].contains("not valid JSON"), "{}", results[1]);

    // Stored as done, with the conclusion a run list reads.
    let stored = load_run(&world.catalog, run.id).await?.expect("the run");
    assert_eq!(stored.state, RunState::Done);
    assert_eq!(stored.conclusion(), Some(conclusion));
    Ok(())
}

#[tokio::test]
async fn a_call_cut_off_at_the_output_limit_is_called_so() -> Result<()> {
    // The fake model writes 5 tokens an answer, so a cap of 5 is reached by
    // every answer: each call it makes was cut off.
    let agent = editor_agent().attribute(sc_agent::ATTR_MAX_TOKENS, 5);
    let world = world(&agent).await?;
    let executor = Arc::new(FakeProvider::new([
        Reply::calls("edit", json!({})),
        Reply::calls("edit", json!({})),
        Reply::calls("edit", json!({})),
    ]));
    let strong = Arc::new(FakeProvider::new([]));
    let (run, conclusion) = run_scripted(&world, &agent, &executor, &strong, "edit").await?;

    // The model is told why, on each result.
    for result in stored_results(&run) {
        assert!(
            result.contains("cut off: it reached the 5-token output limit"),
            "{result}"
        );
    }
    // And so is whoever reads why the run stopped.
    let Conclusion::Stuck { reason } = &conclusion else {
        panic!("expected stuck, got {conclusion:?}");
    };
    assert!(reason.starts_with("3 malformed tool calls in a row"), "{reason}");
    assert!(reason.contains("cut off at the 5-token output limit"), "{reason}");
    Ok(())
}

#[tokio::test]
async fn a_malformed_call_under_the_output_limit_says_nothing_of_it() -> Result<()> {
    let agent = editor_agent().attribute(sc_agent::ATTR_MAX_TOKENS, 4096);
    let world = world(&agent).await?;
    let executor = Arc::new(FakeProvider::new([
        Reply::calls("edit", json!({})),
        Reply::says("done"),
    ]));
    let strong = Arc::new(FakeProvider::new([]));
    let (run, _) = run_scripted(&world, &agent, &executor, &strong, "edit").await?;
    let results = stored_results(&run);
    assert!(!results[0].contains("cut off"), "{}", results[0]);
    Ok(())
}

#[tokio::test]
async fn each_rung_of_the_ladder_is_answered_by_the_right_role() -> Result<()> {
    let agent = editor_agent();
    let world = world(&agent).await?;
    // The same call four times from the executor, and once more from the
    // strong model it is escalated to.
    let same = || Reply::calls("edit", json!({"path": "a.ts", "line": 1}));
    let executor = Arc::new(FakeProvider::repeating(same(), 4));
    let strong = Arc::new(FakeProvider::new([same()]));
    let (run, conclusion) = run_scripted(&world, &agent, &executor, &strong, "edit").await?;

    let Conclusion::Stuck { reason } = &conclusion else {
        panic!("expected stuck, got {conclusion:?}");
    };
    assert!(
        reason.contains("after a warning and an escalation"),
        "{reason}"
    );
    assert_eq!(
        roles(&run),
        vec![
            ModelRole::Executor,
            ModelRole::Executor,
            ModelRole::Executor,
            ModelRole::Executor,
            ModelRole::Strong,
        ]
    );
    assert_eq!(executor.remaining(), 0);
    assert_eq!(strong.remaining(), 0);

    // Rung 1: the fourth request carries the warning on the third result, and
    // only there.
    let requests = executor.requests();
    let fourth = tool_results(&requests[3]);
    assert!(!fourth[1].contains("[harness]"));
    assert!(
        fourth[2].ends_with("try a different approach."),
        "{}",
        fourth[2]
    );
    // The system prompt is untouched, so the cached prefix survives.
    assert_eq!(requests[3].system, requests[0].system);
    // Rung 2: the strong model is told why it was handed the step.
    let escalated = tool_results(&strong.requests()[0]);
    assert!(
        escalated[3].contains("taken by a stronger model"),
        "{}",
        escalated[3]
    );
    Ok(())
}

#[tokio::test]
async fn signals_and_trait_fingerprints_climb_the_ladder_and_a_good_step_calms_it() -> Result<()> {
    let agent = editor_agent();
    let world = world(&agent).await?;
    let executor = Arc::new(FakeProvider::new([
        // One file at three offsets is the same read three times: a warning.
        Reply::calls("read", json!({"path": "a.ts", "offset": 0})),
        Reply::calls("read", json!({"path": "a.ts", "offset": 40})),
        Reply::calls("read", json!({"path": "a.ts", "offset": 80})),
        // Three failed edits to different places: the signal climbs again.
        Reply::calls("edit", json!({"path": "a.ts", "line": 1, "fail": true})),
        Reply::calls("edit", json!({"path": "a.ts", "line": 2, "fail": true})),
        Reply::calls("edit", json!({"path": "a.ts", "line": 3, "fail": true})),
        // After the strong model's step, the executor finishes.
        Reply::says("fixed"),
    ]));
    let strong = Arc::new(FakeProvider::new([Reply::calls(
        "edit",
        json!({"path": "a.ts", "line": 4}),
    )]));
    let (run, conclusion) = run_scripted(&world, &agent, &executor, &strong, "fix").await?;
    assert_eq!(conclusion.answer(), Some("fixed"));

    let results = stored_results(&run);
    assert!(
        results[2].contains("`read` with the same arguments 3 times"),
        "{}",
        results[2]
    );
    assert!(results[5].contains("3 edits have failed"), "{}", results[5]);
    assert!(results[5].contains("stronger model"), "{}", results[5]);

    let state = run.agent_loop()?;
    let steps = state.ledger().steps();
    assert_eq!(steps[3].signals, vec!["edit_failed".to_owned()]);
    assert!(steps[0].signals.is_empty());
    assert_eq!(
        roles(&run),
        vec![
            ModelRole::Executor,
            ModelRole::Executor,
            ModelRole::Executor,
            ModelRole::Executor,
            ModelRole::Executor,
            ModelRole::Executor,
            ModelRole::Strong,
            ModelRole::Executor,
        ]
    );
    Ok(())
}

#[tokio::test]
async fn detector_state_survives_a_save_and_a_resume_and_thresholds_are_attributes() -> Result<()> {
    let agent = editor_agent().attribute(ATTR_MAX_IDENTICAL_CALLS, 2);
    let world = world(&agent).await?;
    let same = || Reply::calls("read", json!({"path": "a.ts"}));

    // One identical call, then the provider fails: the run is stored mid-way.
    let first = Arc::new(FakeProvider::new([
        same(),
        Reply::fails("connection reset"),
    ]));
    let strong = Arc::new(FakeProvider::new([]));
    let err = run_scripted(&world, &agent, &first, &strong, "read")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("connection reset"), "{err}");
    let runs = sc_agent::list_runs(&world.catalog, "editor").await?;
    let mut stored = load_run(&world.catalog, runs[0].id)
        .await?
        .expect("the run");
    assert_eq!(stored.state, RunState::Failed);

    // A new runner over the stored row: the second identical call is the one
    // that warns, because the first was counted before the save.
    let second = Arc::new(FakeProvider::new([same(), Reply::says("ok")]));
    Runner::new(
        &world.catalog,
        &world.registry,
        &agent,
        common::model(Arc::clone(&second)),
        RunCaller::system(),
    )
    .drive(&mut stored)
    .await?;
    let results = stored_results(&stored);
    assert!(results[1].contains("2 times in a row"), "{}", results[1]);
    Ok(())
}

/// `after_tools` runs once per turn, after every call of the turn, and what it
/// says lands on the last result of that trait's calls (TODO 5.10).
#[tokio::test]
async fn after_tools_speaks_once_per_turn_on_the_last_result() -> Result<()> {
    let agent = editor_agent();
    let world = world(&agent).await?;
    let executor = Arc::new(FakeProvider::new([
        Reply::calls_many([
            ("edit".to_owned(), json!({"path": "a.ts", "check": true})),
            ("edit".to_owned(), json!({"path": "b.ts", "check": true})),
            ("read".to_owned(), json!({"path": "c.ts"})),
        ]),
        // A turn with nothing to check says nothing extra.
        Reply::calls("read", json!({"path": "a.ts"})),
        Reply::says("done"),
    ]));
    let strong = Arc::new(FakeProvider::new([]));
    let (run, conclusion) = run_scripted(&world, &agent, &executor, &strong, "edit").await?;
    assert_eq!(conclusion.answer(), Some("done"));
    assert_eq!(
        stored_results(&run),
        vec![
            "edited".to_owned(),
            "edited".to_owned(),
            "contents\n\nchecked 2 edits of 3 calls".to_owned(),
            "contents".to_owned(),
        ]
    );
    Ok(())
}

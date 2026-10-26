//! Interactive chat (TODO-agent-skills AS6.7), on the scripted provider and a
//! real `_fd_runs`: a run that asks, waits, is resumed by the answer and
//! finishes; an answer to a question no longer waiting, refused; a display part
//! the person sees and the next request does not carry; and a composer control
//! that reaches `on_turn`.

use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{Counter, catalog};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{
    Agent, AgentRegistry, AgentTrait, Answer, ChoiceOption, ComposerControl, Conclusion,
    ControlKind, DisplayPart, EnabledTrait, RequestKind, RunCaller, RunState, Runner, ToolsContext,
    TraitContext, Turn, UserRequest, load_display_part, load_run,
};
use sc_error::Result;
use sc_llm::{LlmMessage, LlmRequest, ToolSpec};
use sc_test_harness::TestDb;
use sc_types::{Attrs, FormField};
use serde_json::{Value as Json, json};

/// A trait that asks (`ask`), shows (`show`) and has a `tone` control.
struct Interview;

#[async_trait::async_trait]
impl AgentTrait for Interview {
    fn name(&self) -> &str {
        "interview"
    }

    fn description(&self) -> &str {
        "Ask the person, and show them things"
    }

    fn config_spec(&self) -> Vec<FormField> {
        Vec::new()
    }

    fn tools(&self, _cx: &ToolsContext<'_>, _config: &Attrs) -> Vec<ToolSpec> {
        let object = json!({ "type": "object", "properties": {} });
        vec![
            ToolSpec::new("ask", "Ask which colour", object.clone()),
            ToolSpec::new("show", "Show the person the shelf", object),
        ]
    }

    async fn call(
        &self,
        _config: &Attrs,
        tool: &str,
        _args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json> {
        match tool {
            "ask" => {
                ctx.await_user(UserRequest::new(
                    "Which colour?",
                    RequestKind::Choice {
                        options: vec![
                            ChoiceOption::new("r").label("Red"),
                            ChoiceOption::new("b").label("Blue"),
                        ],
                        multiple: false,
                    },
                ))?;
                Ok(json!("asked the person; their answer will follow"))
            }
            _ => {
                ctx.show(DisplayPart::Rows {
                    title: Some("Shelf".into()),
                    view: None,
                    columns: vec!["title".into()],
                    rows: vec![json!({ "title": "secret-row-value" })],
                })?;
                ctx.show(DisplayPart::Image {
                    mime_type: "image/png".into(),
                    data: b"\x89PNGbytes".to_vec(),
                    name: Some("cover.png".into()),
                })?;
                Ok(json!("shown the shelf to the person"))
            }
        }
    }

    async fn on_answer(
        &self,
        _config: &Attrs,
        _request: &UserRequest,
        value: &Json,
    ) -> Result<String> {
        Ok(format!(
            "The person picked {}",
            value.as_str().unwrap_or("?")
        ))
    }

    fn controls(&self, _config: &Attrs) -> Vec<ComposerControl> {
        vec![ComposerControl {
            name: "tone".into(),
            label: "Tone".into(),
            kind: ControlKind::Select {
                options: vec![ChoiceOption::new("plain"), ChoiceOption::new("fancy")],
                default: Some("plain".into()),
            },
        }]
    }

    async fn on_turn(&self, _config: &Attrs, turn: &mut Turn<'_>) -> Result<()> {
        if let Some(tone) = turn.controls.get("tone").and_then(Json::as_str) {
            turn.append_system(format!("Tone: {tone}"));
        }
        Ok(())
    }
}

fn registry() -> Result<AgentRegistry> {
    let mut registry = common::registry(Counter::new())?;
    registry.register(Arc::new(Interview))?;
    Ok(registry)
}

fn agent() -> Agent {
    Agent::new("interviewer", "main").with_trait(EnabledTrait::new("interview"))
}

/// The text of the last user message a request carries.
fn last_user(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            LlmMessage::User { content } => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_run_asks_waits_is_resumed_by_the_answer_and_finishes() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry()?;
    let agent = agent();
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("ask", json!({})),
        Reply::says("Blue it is."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );
    let (mut run, conclusion) = runner.start("Paint the shelf").await?;
    let Conclusion::AwaitingUser { request } = conclusion else {
        panic!("expected a question, got {conclusion:?}");
    };
    assert_eq!(request.prompt, "Which colour?");
    assert_eq!(request.trait_key, "0:interview");
    assert!(!request.tool_call.is_empty());
    // Stored, waiting on a person: nothing holds the run open.
    assert_eq!(run.state, RunState::Waiting);
    assert_eq!(provider.requests().len(), 1);
    let stored = load_run(&catalog, run.id).await?.expect("the run");
    assert_eq!(stored.state, RunState::Waiting);
    assert_eq!(
        stored.agent_loop()?.pending_request().map(|r| r.id.clone()),
        Some(request.id.clone())
    );

    // A value the question did not offer is refused, and the run still waits.
    let wrong = Answer {
        answers_request: request.id.clone(),
        value: json!("green"),
    };
    assert!(
        runner
            .answer_run(&mut run, &wrong, BTreeMap::new())
            .await
            .is_err()
    );

    let answer = Answer {
        answers_request: request.id.clone(),
        value: json!("b"),
    };
    let conclusion = runner
        .answer_run(&mut run, &answer, BTreeMap::new())
        .await?;
    assert_eq!(conclusion.answer(), Some("Blue it is."));
    assert_eq!(run.state, RunState::Done);
    // The trait that asked said what the model reads.
    assert_eq!(last_user(&provider.requests()[1]), "The person picked b");

    // Answering again is answering a question no longer waiting.
    let err = runner
        .answer_run(&mut run, &answer, BTreeMap::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no longer waiting"), "{err}");
    Ok(())
}

#[tokio::test]
async fn an_ordinary_message_passes_a_question_over() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry()?;
    let agent = agent();
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("ask", json!({})),
        Reply::says("Never mind, then."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );
    let (mut run, conclusion) = runner.start("Paint the shelf").await?;
    let Conclusion::AwaitingUser { request } = conclusion else {
        panic!("expected a question");
    };
    runner
        .continue_run_with(&mut run, "Actually, leave it", BTreeMap::new())
        .await?;
    let stale = Answer {
        answers_request: request.id,
        value: json!("r"),
    };
    let err = runner
        .answer_run(&mut run, &stale, BTreeMap::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no longer waiting"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_display_part_is_stored_for_the_person_and_not_in_the_next_request() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry()?;
    let agent = agent();
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("show", json!({})),
        Reply::says("There it is."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );
    let (run, conclusion) = runner.start("Show me the shelf").await?;
    assert_eq!(conclusion.answer(), Some("There it is."));

    let state = run.agent_loop()?;
    let shown: Vec<_> = state.display().values().flatten().collect();
    assert_eq!(shown.len(), 2);
    assert_eq!(shown[0].kind, "rows");
    assert_eq!(shown[0].summary, "Shelf (1 rows)");
    assert_eq!(shown[1].mime_type, "image/png");

    let rows = load_display_part(&catalog, run.id, shown[0].id)
        .await?
        .expect("stored");
    assert_eq!(rows.data["rows"][0]["title"], json!("secret-row-value"));
    let image = load_display_part(&catalog, run.id, shown[1].id)
        .await?
        .expect("stored");
    assert_eq!(image.content.as_deref(), Some(&b"\x89PNGbytes"[..]));
    // Another run's id does not reach it.
    assert!(
        load_display_part(&catalog, sc_agent::RunId::new(), shown[0].id)
            .await?
            .is_none()
    );

    // The model read the tool's words, never the rows or the image.
    let next = serde_json::to_string(&provider.requests()[1].messages).expect("json");
    assert!(next.contains("shown the shelf to the person"));
    assert!(!next.contains("secret-row-value"), "{next}");
    assert!(!next.contains("cover.png"), "{next}");

    // Deleting the run deletes what it showed.
    sc_agent::delete_run(&catalog, run.id).await?;
    assert!(
        load_display_part(&catalog, run.id, shown[0].id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn a_composer_control_reaches_on_turn_and_an_unknown_one_is_refused() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let registry = registry()?;
    let agent = agent();
    let provider = Arc::new(FakeProvider::new([
        Reply::says("Hello."),
        Reply::says("Good day to you."),
    ]));
    let runner = Runner::new(
        &catalog,
        &registry,
        &agent,
        common::model(provider.clone()),
        RunCaller::system(),
    );
    assert_eq!(runner.controls().await?.len(), 1);
    let (mut run, _) = runner.start("Hi").await?;
    let fancy: BTreeMap<String, Json> = [("tone".to_owned(), json!("fancy"))].into();
    runner
        .continue_run_with(&mut run, "Hi again", fancy)
        .await?;
    let requests = provider.requests();
    assert!(
        !requests[0]
            .system
            .clone()
            .unwrap_or_default()
            .contains("Tone:")
    );
    assert!(
        requests[1]
            .system
            .clone()
            .unwrap_or_default()
            .contains("Tone: fancy")
    );

    for bad in [json!({ "tone": "loud" }), json!({ "volume": 11 })] {
        let controls: BTreeMap<String, Json> =
            bad.as_object().unwrap().clone().into_iter().collect();
        assert!(
            runner
                .continue_run_with(&mut run, "Again", controls)
                .await
                .is_err(),
            "{bad}"
        );
    }
    Ok(())
}

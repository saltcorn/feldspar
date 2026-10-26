//! `generate_image` and `speech` (TODO-agent-skills AS11), driven through
//! whole runs against a stub OpenAI-compatible server (AS3.6's approach): no
//! test needs an API key or spends a token.
//!
//! What is pinned here:
//!
//! - an image is generated with the configured options, saved to the
//!   configured store, shown to the person as an image part, and the model is
//!   told its file name and size **only**;
//! - the image call is ledgered and costed as an aside of the run;
//! - a host's refusal reaches the model as an error, and nothing is saved;
//! - every final answer is spoken with the configured voice, speed, format
//!   and instructions, attached to the answer as an audio part, and costed;
//!   a triggered run says nothing;
//! - the configuration is checked on save: a store that does not exist, an
//!   Anthropic provider, a model of the wrong kind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use common::{Env, as_user, config};
use sc_agent::testing::{FakeModels, FakeProvider, Reply};
use sc_agent::{
    Agent, Conclusion, EnabledTrait, ModelRole, ProviderConnector, RunCaller, Runner,
    answer_display_key, load_display_part,
};
use sc_core_traits::media::{
    ARG_NAME, ARG_PROMPT, CFG_FORMAT, CFG_INSTRUCTIONS, CFG_MODEL, CFG_PROVIDER, CFG_QUALITY,
    CFG_SIZE, CFG_SPEED, CFG_TRANSPARENT, CFG_VOICE,
};
use sc_core_traits::{CFG_ROOT, CFG_STORE, tool_names};
use sc_error::Result;
use sc_llm::{LlmMessage, ModelKind};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The bytes of a "PNG": the signature, which is all `sc-llm` reads.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-pretend-pixels";
/// The bytes of an "MP3".
const MP3: &[u8] = b"ID3\x04-pretend-audio";

// --- the stub host -----------------------------------------------------------

/// One request the stub saw: its path and its JSON body.
type Seen = (String, Json);

/// A stub OpenAI-compatible host answering every request with one reply,
/// recording what it was sent.
struct Host {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Host {
    async fn start(status: u16, content_type: &'static str, body: Vec<u8>) -> Host {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let log = Arc::clone(&log);
                let body = body.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut socket).await else {
                        return;
                    };
                    log.lock().unwrap().push(request);
                    let mut reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    reply.extend_from_slice(&body);
                    let _ = socket.write_all(&reply).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Host { base, seen }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Seen> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let path = head.split(' ').nth(1)?.to_owned();
    let length: usize = head
        .split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = serde_json::from_slice(&buf[head_end..]).unwrap_or(Json::Null);
    Some((path, body))
}

/// An OpenAI-compatible provider `media` at the stub, with an image model and
/// a speech model, each priced and each its kind's default.
async fn media_provider(env: &Env, host: &Host) -> Result<()> {
    sc_llm::save_llm_provider(
        &env.catalog,
        &sc_llm::LlmProviderDef::new("media", sc_llm::OPENAI_RESPONSES_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-test")
            .with(sc_llm::CFG_BASE_URL, host.base.as_str()),
    )
    .await?;
    let provider = sc_llm::require_llm_provider(&env.catalog, "media").await?;
    sc_llm::save_llm_model(
        &env.catalog,
        &sc_llm::LlmModelDef::new(provider.id, "gpt-image-1")
            .kind(ModelKind::Image)
            .with(sc_llm::CFG_PRICE_PER_IMAGE, 0.04)
            .default_model(),
    )
    .await?;
    sc_llm::save_llm_model(
        &env.catalog,
        &sc_llm::LlmModelDef::new(provider.id, "gpt-4o-mini-tts")
            .kind(ModelKind::Speech)
            .with(sc_llm::CFG_PRICE_PER_MILLION_CHARACTERS, 15.0)
            .default_model(),
    )
    .await?;
    env.catalog.reload().await
}

/// Drive `agent` through one turn as `caller`, answered by `script`, with a
/// connector so the run can charge what its tools spend.
async fn chat(
    env: &Env,
    agent: &Agent,
    caller: RunCaller,
    script: Vec<Reply>,
) -> Result<(sc_agent::Run, Conclusion)> {
    let models = FakeModels::new().role(ModelRole::Executor, Arc::new(FakeProvider::new(script)));
    let connector = Arc::new(models) as Arc<dyn ProviderConnector>;
    let executor = connector
        .connect(&env.catalog, agent, ModelRole::Executor)
        .await?;
    Runner::new(&env.catalog, &env.registry, agent, executor, caller)
        .with_connector(&connector)
        .start("go")
        .await
}

fn image_config() -> sc_types::Attrs {
    config(&[
        (CFG_STORE, json!("media")),
        (CFG_ROOT, json!("generated")),
        (CFG_PROVIDER, json!("media")),
        (CFG_SIZE, json!("1024x1024")),
        (CFG_QUALITY, json!("low")),
        (CFG_FORMAT, json!("png")),
        (CFG_TRANSPARENT, json!(true)),
    ])
}

fn artist() -> Agent {
    Agent::new("artist", "main")
        .with_trait(EnabledTrait::new("generate_image").configuration(image_config()))
}

/// The one tool result in a run's transcript, with its call id.
fn tool_result(run: &sc_agent::Run) -> (String, String) {
    run.agent_loop()
        .unwrap()
        .messages()
        .iter()
        .find_map(|m| match m {
            LlmMessage::ToolResult {
                tool_call_id,
                content,
                ..
            } => Some((tool_call_id.clone(), content.clone())),
            _ => None,
        })
        .expect("a tool result")
}

// --- generate_image ----------------------------------------------------------

#[tokio::test]
async fn an_image_is_saved_shown_and_only_named_to_the_model() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("media", None).await?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
    let host = Host::start(
        200,
        "application/json",
        format!(r#"{{"data":[{{"b64_json":"{b64}"}}]}}"#).into_bytes(),
    )
    .await;
    media_provider(&env, &host).await?;
    let agent = artist();
    env.check("generate_image", &image_config()).await?;

    let scope = sc_core_traits::configured_scope(&image_config())?;
    let tool = tool_names::generate_image(&scope);
    assert_eq!(tool, "generate_image_media_generated");
    let (run, conclusion) = chat(
        &env,
        &agent,
        as_user("ada@example.com"),
        vec![
            Reply::calls(
                &tool,
                json!({ARG_PROMPT: "a lighthouse at dusk", ARG_NAME: "Lighthouse!"}),
            ),
            Reply::says("Here is your lighthouse."),
        ],
    )
    .await?;
    assert!(matches!(conclusion, Conclusion::Answered { .. }));

    // Sent with the configured options.
    let requests = host.requests();
    assert_eq!(requests.len(), 1);
    let (path, body) = &requests[0];
    assert_eq!(path, "/images/generations");
    assert_eq!(body["model"], "gpt-image-1");
    assert_eq!(body["prompt"], "a lighthouse at dusk");
    assert_eq!(body["size"], "1024x1024");
    assert_eq!(body["quality"], "low");
    assert_eq!(body["output_format"], "png");
    assert_eq!(body["background"], "transparent");

    // Saved under the root, named after what the model asked for.
    let (call_id, content) = tool_result(&run);
    let result: Json = serde_json::from_str(&content).expect("a JSON result");
    let file = result["file"].as_str().unwrap();
    assert!(
        file.starts_with("lighthouse-") && file.ends_with(".png"),
        "{file}"
    );
    assert_eq!(result["bytes"], PNG.len());
    assert_eq!(result["shown_to_user"], true);
    assert_eq!(
        std::fs::read(dir.join("generated").join(file)).unwrap(),
        PNG
    );
    // The model is told the name and the size, and not the picture or a link.
    assert!(!content.contains(&b64), "{content}");
    assert!(!content.contains("http"), "{content}");

    // Shown to the person, beside that call.
    let state = run.agent_loop()?;
    let shown = &state.display()[&call_id];
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].kind, "image");
    assert_eq!(shown[0].mime_type, "image/png");
    let stored = load_display_part(&env.catalog, run.id, shown[0].id)
        .await?
        .expect("the stored part");
    assert_eq!(stored.content.as_deref(), Some(PNG));

    // Ledgered as an aside, at the image price.
    let asides = state.ledger().asides();
    assert_eq!(asides.len(), 1);
    assert_eq!(asides[0].kind, ModelKind::Image);
    assert_eq!(asides[0].model, "gpt-image-1");
    assert!((asides[0].cost.unwrap() - 0.04).abs() < 1e-9);
    Ok(())
}

#[tokio::test]
async fn a_refused_image_reaches_the_model_and_saves_nothing() -> Result<()> {
    let env = Env::new().await?;
    let dir = env.with_file_store("media", None).await?;
    let host = Host::start(
        400,
        "application/json",
        br#"{"error":{"message":"Your request was rejected by the safety system."}}"#.to_vec(),
    )
    .await;
    media_provider(&env, &host).await?;
    let scope = sc_core_traits::configured_scope(&image_config())?;
    let (run, _) = chat(
        &env,
        &artist(),
        as_user("ada@example.com"),
        vec![
            Reply::calls(
                tool_names::generate_image(&scope),
                json!({ARG_PROMPT: "something"}),
            ),
            Reply::says("I could not draw that."),
        ],
    )
    .await?;
    let (call_id, content) = tool_result(&run);
    assert!(content.starts_with("error: "), "{content}");
    assert!(content.contains("safety system"), "{content}");
    assert!(!run.agent_loop()?.display().contains_key(&call_id));
    let saved = std::fs::read_dir(dir.join("generated"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(saved, 0);
    Ok(())
}

#[tokio::test]
async fn media_configuration_is_checked_on_save() -> Result<()> {
    let env = Env::new().await?;
    let host = Host::start(200, "application/json", b"{}".to_vec()).await;
    media_provider(&env, &host).await?;

    // No such store.
    let err = env
        .check("generate_image", &image_config())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("media"), "{err}");
    env.with_file_store("media", None).await?;

    // The default provider is Anthropic, which has no image endpoint.
    let mut cfg = image_config();
    cfg.insert(CFG_PROVIDER.to_owned(), json!(""));
    let err = env
        .check("generate_image", &cfg)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Anthropic"), "{err}");

    // A speech model is not an image model, and the reverse.
    let mut cfg = image_config();
    cfg.insert(CFG_MODEL.to_owned(), json!("gpt-4o-mini-tts"));
    let err = env
        .check("generate_image", &cfg)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not an image model"), "{err}");
    let err = env
        .check(
            "speech",
            &config(&[
                (CFG_PROVIDER, json!("media")),
                (CFG_MODEL, json!("gpt-image-1")),
            ]),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a speech model"), "{err}");

    // A jpeg cannot be transparent; a speed out of range is refused.
    let mut cfg = image_config();
    cfg.insert(CFG_FORMAT.to_owned(), json!("jpeg"));
    assert!(env.check("generate_image", &cfg).await.is_err());
    let err = env
        .check(
            "speech",
            &config(&[(CFG_PROVIDER, json!("media")), (CFG_SPEED, json!(9.0))]),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("speed"), "{err}");

    env.check("generate_image", &image_config()).await?;
    env.check("speech", &config(&[(CFG_PROVIDER, json!("media"))]))
        .await?;
    Ok(())
}

// --- speech ------------------------------------------------------------------

fn speaker() -> Agent {
    Agent::new("speaker", "main").with_trait(EnabledTrait::new("speech").configuration(config(&[
        (CFG_PROVIDER, json!("media")),
        (CFG_VOICE, json!("coral")),
        (CFG_SPEED, json!(1.25)),
        (CFG_FORMAT, json!("mp3")),
        (CFG_INSTRUCTIONS, json!("Speak warmly.")),
    ])))
}

#[tokio::test]
async fn the_final_answer_is_spoken_and_attached_as_audio() -> Result<()> {
    let env = Env::new().await?;
    let host = Host::start(200, "audio/mpeg", MP3.to_vec()).await;
    media_provider(&env, &host).await?;
    let agent = speaker();
    // No tool: the trait only listens to the answer.
    assert!(env.tools("speech", &agent.traits[0].config).is_empty());

    let (run, conclusion) = chat(
        &env,
        &agent,
        as_user("ada@example.com"),
        vec![Reply::says("Hello there.")],
    )
    .await?;
    assert!(matches!(conclusion, Conclusion::Answered { .. }));

    let requests = host.requests();
    assert_eq!(requests.len(), 1);
    let (path, body) = &requests[0];
    assert_eq!(path, "/audio/speech");
    assert_eq!(body["model"], "gpt-4o-mini-tts");
    assert_eq!(body["input"], "Hello there.");
    assert_eq!(body["voice"], "coral");
    assert_eq!(body["speed"], 1.25);
    assert_eq!(body["response_format"], "mp3");
    assert_eq!(body["instructions"], "Speak warmly.");

    // Attached to the answer, the last message, and not to any tool call.
    let state = run.agent_loop()?;
    let answer = state.messages().len() - 1;
    let shown = &state.display()[&answer_display_key(answer)];
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].kind, "audio");
    assert_eq!(shown[0].mime_type, "audio/mpeg");
    let stored = load_display_part(&env.catalog, run.id, shown[0].id)
        .await?
        .expect("the stored part");
    assert_eq!(stored.content.as_deref(), Some(MP3));

    // Costed per character, as an aside.
    let asides = state.ledger().asides();
    assert_eq!(asides.len(), 1);
    assert_eq!(asides[0].kind, ModelKind::Speech);
    assert!(asides[0].cost.unwrap() > 0.0);
    Ok(())
}

#[tokio::test]
async fn a_triggered_run_is_not_spoken() -> Result<()> {
    let env = Env::new().await?;
    env.with_file_store("media", None).await?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
    let host = Host::start(
        200,
        "application/json",
        format!(r#"{{"data":[{{"b64_json":"{b64}"}}]}}"#).into_bytes(),
    )
    .await;
    media_provider(&env, &host).await?;

    // A run a trigger started has nobody listening: its image is drawn, and
    // its answer is not spoken.
    let scope = sc_core_traits::configured_scope(&image_config())?;
    let agent = artist().with_trait(
        EnabledTrait::new("speech").configuration(config(&[(CFG_PROVIDER, json!("media"))])),
    );
    chat(
        &env,
        &agent,
        RunCaller::triggered(),
        vec![
            Reply::calls(
                tool_names::generate_image(&scope),
                json!({ARG_PROMPT: "a cat"}),
            )
            .with_preamble("Let me draw that."),
            Reply::says("Done."),
        ],
    )
    .await?;
    let paths: Vec<String> = host.requests().into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths, vec!["/images/generations"]);
    Ok(())
}

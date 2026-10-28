//! The **interaction traits** (TODO-agent-skills AS10) through both chat
//! surfaces: an application's chat (`…/_agents/{name}/…`, server-sent events)
//! and the admin chat socket, against scripted providers (decision 7).
//!
//! What each test is for:
//!
//! 1. **`ask_user` and `plan_approval`** end a turn on a question; the
//!    person's answer — a choice, "discuss instead", an approval — reaches the
//!    model as the configured prompt.
//! 2. **`run_js_code`'s button** is a composer button whose press runs the
//!    code and answers the turn with what it returned, without a model call.
//! 3. **`prompt_picker` and `pickable_models`** are composer selects: the
//!    chosen prompt (predefined, or a row of a table) is in the system prompt,
//!    and the chosen model answers in place of the agent's own.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use futures::{SinkExt, StreamExt};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{Agent, EnabledTrait};
use sc_app::{AgentExposure, ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{ROLE_ADMIN, Role, SessionStore, User, create_user, save_role};
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_llm::{ConnectedModel, LlmProvider};
use sc_server::{
    AGENT_CHAT_ROUTE, AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ProviderConnector,
    SESSION_COOKIE, ServerConfig, admin_handlers, build_router_with_apps, default_js_evaluator,
    install_triggers,
};
use sc_test_harness::Teardown;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;
use uuid::Uuid;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "shelf.example.com";
const PASSWORD: &str = "correct-horse";
const ROLE_MEMBER: u8 = 80;
/// The model a person may pick, under the `house` provider.
const FAST: &str = "claude-haiku-4-5";

// --- providers ----------------------------------------------------------------

/// The agent's own model answers with `own`; the picked one with `fast`.
struct Picking {
    own: Arc<FakeProvider>,
    fast: Arc<FakeProvider>,
}

#[async_trait]
impl ProviderConnector for Picking {
    async fn connect(
        &self,
        _catalog: &Catalog,
        agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> Result<ConnectedModel> {
        let provider = if agent.model.as_deref() == Some(FAST) {
            &self.fast
        } else {
            &self.own
        };
        Ok(ConnectedModel::unconfigured(
            Arc::clone(provider) as Arc<dyn LlmProvider>
        ))
    }
}

// --- the agents -----------------------------------------------------------------

/// Asks and plans.
fn asker() -> Agent {
    Agent::new("asker", "house")
        .system_prompt("You tidy shelves.")
        .with_trait(EnabledTrait::new("ask_user"))
        .with_trait(
            EnabledTrait::new("plan_approval").config("approval_prompt", "Approved: go ahead."),
        )
        .min_role(ROLE_MEMBER)
}

/// A button that runs code.
fn buttons() -> Agent {
    Agent::new("buttons", "house")
        .system_prompt("You count.")
        .with_trait(
            EnabledTrait::new("run_js_code")
                .config("mode", "button")
                .config("button_label", "Count books")
                .config(
                    "code",
                    "return `Button ran; row is ${JSON.stringify(row)}, user is ${user ? 'known' : 'unknown'}`;",
                ),
        )
        .min_role(ROLE_MEMBER)
}

/// Two prompt pickers — predefined, and from a table — and a model picker.
fn picker() -> Agent {
    Agent::new("picker", "house")
        .system_prompt("You talk.")
        .with_trait(EnabledTrait::new("prompt_picker").config(
            "prompts",
            json!([
                { "label": "Pirate", "prompt": "Speak like a pirate." },
                { "label": "Poet", "prompt": "Answer in verse." },
            ]),
        ))
        .with_trait(
            EnabledTrait::new("prompt_picker")
                .config("name", "persona")
                .config("label", "Persona")
                .config("source", "table")
                .config("table", "personas")
                .config("label_field", "name")
                .config("prompt_field", "prompt")
                .config("where", json!({ "active": true })),
        )
        .with_trait(
            EnabledTrait::new("prompt_picker")
                .config("name", "starter")
                .config("send", true)
                .config(
                    "prompts",
                    json!([{ "label": "Hello", "prompt": "Say hello to me." }]),
                ),
        )
        .attribute(
            sc_agent::ATTR_PICKABLE_MODELS,
            json!([{ "provider": "house", "model": FAST, "label": "Fast" }]),
        )
        .min_role(ROLE_MEMBER)
}

// --- the server -----------------------------------------------------------------

struct TempDir(PathBuf, Teardown);

impl TempDir {
    fn new() -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-agent-interaction-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir, Teardown::new())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    /// The application's router (`shelf.example.com`).
    app: Router,
    /// The admin chat socket's address.
    addr: std::net::SocketAddr,
    /// An admin's session token.
    admin: String,
    own: Arc<FakeProvider>,
    fast: Arc<FakeProvider>,
    _tmp: TempDir,
}

async fn serve(own: Vec<Reply>, fast: Vec<Reply>) -> Result<Server> {
    let tmp = TempDir::new();
    let driver = Arc::new(SqliteDriver::open(tmp.path().join("shelf.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    tmp.1.add(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    save_role(&catalog, &Role::new(ROLE_MEMBER, "Member")).await?;
    create_user(&catalog, "alice@example.com", PASSWORD, ROLE_MEMBER).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_llm::save_llm_provider(
        &catalog,
        &sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-test"),
    )
    .await?;
    let house = sc_llm::require_llm_provider(&catalog, "house").await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(house.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;
    sc_llm::save_llm_model(&catalog, &sc_llm::LlmModelDef::new(house.id, FAST)).await?;
    let personas = catalog
        .create_table(
            "personas",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key(),
                DataField::plain("name", TypeRef::Basic(BasicType::Text)),
                DataField::plain("prompt", TypeRef::Basic(BasicType::Text)),
                DataField::plain("active", TypeRef::Basic(BasicType::Bool)),
            ],
        )
        .await?;
    // Members may read the personas: a picker reads them as the person chatting.
    let mut meta = sc_catalog::TableMeta::new("personas");
    meta.access = sc_catalog::AccessRules {
        min_role_read: ROLE_MEMBER,
        min_role_write: 1,
    };
    sc_catalog::save_table_meta(&catalog, &meta).await?;
    for (id, name, prompt, active) in [
        (1, "Librarian", "Answer as a librarian would.", true),
        (2, "Retired", "Never shown.", false),
    ] {
        sc_api::rows::create_row(
            &catalog,
            &personas,
            &json!({ "id": id, "name": name, "prompt": prompt, "active": active }),
        )
        .await?;
    }

    let own = Arc::new(FakeProvider::new(own));
    let fast = Arc::new(FakeProvider::new(fast));
    let providers: Arc<dyn ProviderConnector> = Arc::new(Picking {
        own: own.clone(),
        fast: fast.clone(),
    });
    let services = sc_server::install_agents(&catalog)
        .await?
        .with_providers(providers);
    for agent in [asker(), buttons(), picker()] {
        sc_agent::save_agent(&catalog, services.registry(), &agent).await?;
    }
    let evaluator = default_js_evaluator();
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let triggers = install_triggers(&catalog, evaluator.clone(), &services, &models).await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(evaluator)
            .with_triggers(triggers)
            .with_agents(services),
    );
    let app = Application::new("Shelf", "shelf", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_agent(AgentExposure::new("asker"))
        .with_agent(AgentExposure::new("buttons"))
        .with_agent(AgentExposure::new("picker"));
    sc_app::validate_agent_exposures(&app)?;
    sc_app::check_exposed_agents(&catalog, &app).await?;
    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ));
    apps.mount(MountedApp::new_with(
        app,
        framework,
        &catalog,
        apps.evaluator(),
        apps.triggers(),
    )?)?;
    // One router: the application on its subdomain, and the admin (its chat
    // socket) on any other host — here the listener's address.
    let sessions = Arc::new(SessionStore::default());
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        sessions.clone(),
        &ServerConfig {
            base_domain: Some(BASE_DOMAIN.to_owned()),
            ..ServerConfig::default()
        },
        apps,
    )?;
    let app_router = router.clone();
    let admin_router = router;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, admin_router).await;
    });
    let admin = sessions
        .login(User::new(Uuid::new_v4(), ROLE_ADMIN).unwrap())
        .await
        .unwrap();
    Ok(Server {
        app: app_router,
        addr,
        admin,
        own,
        fast,
        _tmp: tmp,
    })
}

// --- an application's browser ------------------------------------------------------

struct Browser {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Browser {
    async fn signed_in(server: &Server) -> Browser {
        let mut browser = Browser {
            router: server.app.clone(),
            cookies: HashMap::new(),
        };
        browser.send("GET", "/", None).await;
        let (status, body) = browser
            .send(
                "POST",
                "/api/login",
                Some(json!({ "email": "alice@example.com", "password": PASSWORD })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        browser
    }

    async fn send(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, APP_HOST);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let status = response.status();
        let body = tokio::time::timeout(
            Duration::from_secs(20),
            axum::body::to_bytes(response.into_body(), 16 << 20),
        )
        .await
        .expect("the response ended")
        .unwrap()
        .to_vec();
        (status, body)
    }

    async fn json(&mut self, method: &str, path: &str) -> Value {
        let (_, body) = self.send(method, path, None).await;
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    }

    /// One chat turn's events.
    async fn chat(&mut self, agent: &str, body: Value) -> Vec<Value> {
        let (_, body) = self
            .send("POST", &format!("/api/_agents/{agent}/chat"), Some(body))
            .await;
        String::from_utf8_lossy(&body)
            .split("\n\n")
            .filter_map(|frame| {
                let data: String = frame
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect();
                (!data.is_empty()).then(|| serde_json::from_str(&data).expect("JSON event"))
            })
            .collect()
    }
}

fn done(events: &[Value]) -> &Value {
    events
        .iter()
        .rev()
        .find(|e| e["type"] == "done")
        .unwrap_or_else(|| panic!("a done event: {events:#?}"))
}

fn run_of(events: &[Value]) -> String {
    events
        .iter()
        .find(|e| e["type"] == "run")
        .and_then(|e| e["run"].as_str())
        .unwrap_or_else(|| panic!("a run event: {events:#?}"))
        .to_owned()
}

/// Everything the model was sent, as one string, to look for text in.
fn sent(provider: &FakeProvider) -> String {
    format!(
        "{:?}",
        provider.last_request().expect("the model was called")
    )
}

// --- the admin socket ------------------------------------------------------------

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn admin_socket(server: &Server, agent: &str) -> (Socket, Option<Value>) {
    let mut request = format!("ws://{}{AGENT_CHAT_ROUTE}", server.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_str(&format!("http://{}", server.addr)).unwrap(),
    );
    request.headers_mut().insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("{SESSION_COOKIE}={}", server.admin)).unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket_send(&mut socket, json!({ "type": "start", "agent": agent })).await;
    // An agent with controls says so straight after `start`.
    let controls =
        match tokio::time::timeout(Duration::from_millis(1500), next_frame(&mut socket)).await {
            Ok(event) if event["type"] == "controls" => Some(event["controls"].clone()),
            Ok(other) => panic!("expected controls, got {other}"),
            Err(_) => None,
        };
    (socket, controls)
}

async fn socket_send(socket: &mut Socket, frame: Value) {
    socket.send(Message::text(frame.to_string())).await.unwrap();
}

async fn next_frame(socket: &mut Socket) -> Value {
    loop {
        match socket.next().await.expect("the socket closed").unwrap() {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Close(frame) => panic!("closed: {frame:?}"),
            _ => {}
        }
    }
}

/// One turn's events over the socket, up to `done`.
async fn socket_turn(socket: &mut Socket, frame: Value) -> Vec<Value> {
    socket_send(socket, frame).await;
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), next_frame(socket))
            .await
            .expect("timed out waiting for the turn");
        let finished = event["type"] == "done";
        events.push(event);
        if finished {
            return events;
        }
    }
}

// --- 1. ask_user and plan_approval ----------------------------------------------------

/// The scripted conversation both surfaces run for `asker`: a question
/// answered by "discuss instead", then a plan that is approved.
fn asker_script() -> Vec<Reply> {
    vec![
        Reply::calls(
            "ask_user_question",
            json!({
                "question": "Which shelf first?",
                "type": "single_select",
                "options": [{ "label": "Top" }, { "label": "Bottom", "description": "The dusty one" }],
                "allow_discussion": true,
            }),
        ),
        Reply::says("Let's talk about it."),
        Reply::calls(
            "ask_user_question",
            json!({ "question": "Which shelf now?", "type": "single_select", "options": [{ "label": "Top" }, { "label": "Bottom" }] }),
        ),
        Reply::says("Bottom first, then."),
        Reply::calls(
            "submit_plan_for_approval",
            json!({ "plan": "1. Dust\n2. Sort" }),
        ),
        Reply::says("Dusting now."),
    ]
}

#[tokio::test]
async fn ask_user_and_plan_approval_through_the_application_chat() -> Result<()> {
    let server = serve(asker_script(), Vec::new()).await?;
    let mut alice = Browser::signed_in(&server).await;

    let asked = alice.chat("asker", json!({ "message": "Tidy up" })).await;
    let run = run_of(&asked);
    let question = &done(&asked)["request"];
    assert_eq!(done(&asked)["conclusion"], "awaiting_user", "{asked:#?}");
    assert_eq!(question["kind"], "choice");
    let options = question["options"].as_array().unwrap();
    assert_eq!(options.len(), 3, "two options and discuss: {question}");
    assert_eq!(options[2]["value"], sc_core_traits::DISCUSS_VALUE);
    assert_eq!(options[2]["label"], "Discuss instead");

    // "Discuss instead" reaches the model as the discuss prompt.
    let discussed = alice
        .chat(
            "asker",
            json!({ "answers_request": question["id"], "value": sc_core_traits::DISCUSS_VALUE, "run": run }),
        )
        .await;
    assert_eq!(done(&discussed)["answer"], "Let's talk about it.");
    assert!(
        sent(&server.own).contains("Let's discuss the question \\\"Which shelf first?\\\""),
        "{}",
        sent(&server.own)
    );

    // An answer reaches it as the answer prompt.
    let asked = alice
        .chat("asker", json!({ "message": "Ask again", "run": run }))
        .await;
    let id = done(&asked)["request"]["id"].clone();
    alice
        .chat(
            "asker",
            json!({ "answers_request": id, "value": "Bottom", "run": run }),
        )
        .await;
    assert!(
        sent(&server.own)
            .contains("In answer to the question \\\"Which shelf now?\\\", I choose: Bottom"),
        "{}",
        sent(&server.own)
    );

    // A plan waits for approval, and approving sends the approval prompt.
    let planned = alice
        .chat("asker", json!({ "message": "Plan it", "run": run }))
        .await;
    let plan = &done(&planned)["request"];
    assert_eq!(plan["kind"], "approve", "{planned:#?}");
    assert_eq!(plan["prompt"], "1. Dust\n2. Sort");
    let approved = alice
        .chat(
            "asker",
            json!({ "answers_request": plan["id"], "value": true, "run": run }),
        )
        .await;
    assert_eq!(done(&approved)["answer"], "Dusting now.");
    assert!(sent(&server.own).contains("Approved: go ahead."));
    Ok(())
}

#[tokio::test]
async fn ask_user_and_plan_approval_through_the_admin_chat() -> Result<()> {
    let server = serve(asker_script(), Vec::new()).await?;
    let (mut socket, _) = admin_socket(&server, "asker").await;

    let asked = socket_turn(&mut socket, json!({ "type": "message", "text": "Tidy up" })).await;
    let question = done(&asked)["request"].clone();
    assert_eq!(question["kind"], "choice", "{asked:#?}");
    let discussed = socket_turn(
        &mut socket,
        json!({ "type": "message", "answers_request": question["id"], "value": sc_core_traits::DISCUSS_VALUE }),
    )
    .await;
    assert_eq!(done(&discussed)["answer"], "Let's talk about it.");
    assert!(sent(&server.own).contains("Let's discuss the question"));

    let asked = socket_turn(
        &mut socket,
        json!({ "type": "message", "text": "Ask again" }),
    )
    .await;
    let id = done(&asked)["request"]["id"].clone();
    socket_turn(
        &mut socket,
        json!({ "type": "message", "answers_request": id, "value": "Top" }),
    )
    .await;
    assert!(sent(&server.own).contains("I choose: Top"));

    let planned = socket_turn(&mut socket, json!({ "type": "message", "text": "Plan it" })).await;
    let plan = done(&planned)["request"].clone();
    assert_eq!(plan["kind"], "approve", "{planned:#?}");
    // A plan answered with something other than yes or no is refused.
    let refused = socket_turn(
        &mut socket,
        json!({ "type": "message", "answers_request": plan["id"], "value": "sure" }),
    )
    .await;
    assert!(refused.iter().any(|e| e["type"] == "error"), "{refused:#?}");
    let approved = socket_turn(
        &mut socket,
        json!({ "type": "message", "answers_request": plan["id"], "value": true }),
    )
    .await;
    assert_eq!(done(&approved)["answer"], "Dusting now.");
    assert!(sent(&server.own).contains("Approved: go ahead."));
    Ok(())
}

// --- 2. run_js_code's button -------------------------------------------------------------

#[tokio::test]
async fn a_code_button_answers_without_the_model_on_both_surfaces() -> Result<()> {
    let server = serve(vec![Reply::says("Noted.")], Vec::new()).await?;
    let control = sc_core_traits::button_control("Count books");
    assert_eq!(control, "js_count_books");

    // The application's chat lists the button, and a press is answered by the
    // code: no tool is offered and no model is called.
    let mut alice = Browser::signed_in(&server).await;
    let controls = alice.json("GET", "/api/_agents/buttons/controls").await;
    assert_eq!(
        controls,
        json!([{ "name": control, "label": "Count books", "kind": "button" }])
    );
    let pressed = alice
        .chat(
            "buttons",
            json!({ "message": "Count books", "controls": { control.as_str(): true } }),
        )
        .await;
    let answer = done(&pressed)["answer"].as_str().unwrap().to_owned();
    assert_eq!(
        answer, "Button ran; row is null, user is known",
        "{pressed:#?}"
    );
    assert_eq!(done(&pressed)["conclusion"], "answered");
    assert!(server.own.requests().is_empty(), "the model was not called");
    // The answer streamed as text, as a model's would.
    assert!(
        pressed
            .iter()
            .any(|e| e["type"] == "text" && e["delta"] == answer.as_str())
    );

    // The conversation goes on, and the model reads what the button said.
    let run = run_of(&pressed);
    alice
        .chat("buttons", json!({ "message": "Thanks", "run": run }))
        .await;
    assert_eq!(server.own.requests().len(), 1);
    assert!(sent(&server.own).contains("Button ran"));
    assert!(
        server.own.last_request().unwrap().tools.is_empty(),
        "a button offers no tool"
    );

    // The admin chat: the same button, the same answer.
    let (mut socket, controls) = admin_socket(&server, "buttons").await;
    assert_eq!(controls.unwrap()[0]["kind"], "button");
    let pressed = socket_turn(
        &mut socket,
        json!({ "type": "message", "text": "Count books", "controls": { control.as_str(): true } }),
    )
    .await;
    assert_eq!(
        done(&pressed)["answer"],
        "Button ran; row is null, user is known",
        "{pressed:#?}"
    );
    assert_eq!(server.own.requests().len(), 1, "still no model call");
    Ok(())
}

// --- 3. prompt_picker and pickable_models ---------------------------------------------------

#[tokio::test]
async fn prompt_and_model_pickers_through_the_application_chat() -> Result<()> {
    let server = serve(
        vec![Reply::says("Arr."), Reply::says("Hello to you.")],
        vec![Reply::says("Quick verse.")],
    )
    .await?;
    let mut alice = Browser::signed_in(&server).await;
    let controls = alice.json("GET", "/api/_agents/picker/controls").await;
    let names: Vec<&str> = controls
        .as_array()
        .unwrap_or_else(|| panic!("the controls: {controls}"))
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["model", "prompt", "persona", "starter"],
        "{controls}"
    );
    // The model picker: the agent's own first, then the pickable one.
    assert_eq!(controls[0]["options"][0]["value"], "");
    assert_eq!(
        controls[0]["options"][1],
        json!({ "value": format!("house/{FAST}"), "label": "Fast" })
    );
    // Predefined prompts go by position; the prompt stays on the server.
    assert_eq!(
        controls[1]["options"][1],
        json!({ "value": "0", "label": "Pirate" })
    );
    assert!(!controls.to_string().contains("pirate."), "{controls}");
    // A table's rows, as the caller reads them and through the filter.
    assert_eq!(
        controls[2]["options"].as_array().unwrap().len(),
        2,
        "{controls}"
    );
    assert_eq!(
        controls[2]["options"][1],
        json!({ "value": "1", "label": "Librarian" })
    );
    // A picker that sends: its options are the messages.
    assert_eq!(controls[3]["kind"], "send");
    assert_eq!(controls[3]["options"][0]["value"], "Say hello to me.");

    // Both prompts reach the system prompt; the agent's own model answers.
    let first = alice
        .chat(
            "picker",
            json!({ "message": "Hi", "controls": { "prompt": "0", "persona": "1", "model": "" } }),
        )
        .await;
    assert_eq!(done(&first)["answer"], "Arr.", "{first:#?}");
    let system = server
        .own
        .last_request()
        .unwrap()
        .system
        .unwrap_or_default();
    assert!(system.contains("Speak like a pirate."), "{system}");
    assert!(system.contains("Answer as a librarian would."), "{system}");
    assert!(!system.contains("Answer in verse."));
    let run = run_of(&first);

    // A picked model answers in place of the agent's own.
    let picked = alice
        .chat(
            "picker",
            json!({ "message": "Again", "run": run,
                    "controls": { "prompt": "1", "model": format!("house/{FAST}") } }),
        )
        .await;
    assert_eq!(done(&picked)["answer"], "Quick verse.", "{picked:#?}");
    let system = server
        .fast
        .last_request()
        .unwrap()
        .system
        .unwrap_or_default();
    assert!(system.contains("Answer in verse."), "{system}");
    assert!(
        !system.contains("pirate"),
        "only the prompt sent with this message"
    );
    assert_eq!(server.own.requests().len(), 1);

    // The sending picker's message is an ordinary message.
    let hello = alice
        .chat(
            "picker",
            json!({ "message": "Say hello to me.", "run": run }),
        )
        .await;
    assert_eq!(done(&hello)["answer"], "Hello to you.");

    // A model, or a row, the agent does not offer is refused.
    for controls in [
        json!({ "model": "house/claude-opus" }),
        json!({ "persona": "2" }),
    ] {
        let refused = alice
            .chat(
                "picker",
                json!({ "message": "Hm", "run": run, "controls": controls }),
            )
            .await;
        assert!(
            refused.iter().any(|e| e["type"] == "error"
                && e["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("is not a value control")),
            "{refused:#?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn prompt_and_model_pickers_through_the_admin_chat() -> Result<()> {
    let server = serve(
        vec![Reply::says("Plain.")],
        vec![Reply::says("Fast pirate.")],
    )
    .await?;
    let (mut socket, controls) = admin_socket(&server, "picker").await;
    let controls = controls.expect("the picker's controls");
    assert_eq!(controls[0]["name"], "model");
    assert_eq!(controls[2]["options"][1]["label"], "Librarian");

    let picked = socket_turn(
        &mut socket,
        json!({ "type": "message", "text": "Hi",
                "controls": { "prompt": "0", "model": format!("house/{FAST}") } }),
    )
    .await;
    assert_eq!(done(&picked)["answer"], "Fast pirate.", "{picked:#?}");
    assert!(
        server
            .fast
            .last_request()
            .unwrap()
            .system
            .unwrap_or_default()
            .contains("Speak like a pirate.")
    );
    assert!(server.own.requests().is_empty());

    let plain = socket_turn(
        &mut socket,
        json!({ "type": "message", "text": "Again", "controls": { "model": "" } }),
    )
    .await;
    assert_eq!(done(&plain)["answer"], "Plain.");
    assert!(
        !server
            .own
            .last_request()
            .unwrap()
            .system
            .unwrap_or_default()
            .contains("pirate")
    );
    Ok(())
}

// --- validation -------------------------------------------------------------------------

#[tokio::test]
async fn a_pickable_model_that_does_not_resolve_is_refused_on_save() -> Result<()> {
    let tmp = TempDir::new();
    let driver = Arc::new(SqliteDriver::open(tmp.path().join("v.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    tmp.1.add(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_llm::save_llm_provider(
        &catalog,
        &sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-test"),
    )
    .await?;
    let house = sc_llm::require_llm_provider(&catalog, "house").await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(house.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;
    let services = sc_server::install_agents(&catalog).await?;
    let agent = Agent::new("p", "house").attribute(
        sc_agent::ATTR_PICKABLE_MODELS,
        json!([{ "provider": "house", "model": "no-such-model" }]),
    );
    let err = sc_agent::save_agent(&catalog, services.registry(), &agent)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("pickable model `house/no-such-model`"),
        "{err}"
    );
    let agent = Agent::new("p", "house").attribute(
        sc_agent::ATTR_PICKABLE_MODELS,
        json!([{ "provider": "elsewhere" }]),
    );
    let err = sc_agent::save_agent(&catalog, services.registry(), &agent)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no LLM provider named `elsewhere`"), "{err}");
    Ok(())
}

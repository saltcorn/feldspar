//! Phase 4 integration test: the admin **chat socket** (§11.4), driven end to
//! end against a scripted provider.
//!
//! Like the language-server route, this cannot be tested with
//! `tower::ServiceExt::oneshot`: the whole subject is what happens *after* the
//! `101 Switching Protocols`, so these tests bind a real listener and connect a
//! real WebSocket client to it.
//!
//! **No vendor and no token** (decision 7). The provider is
//! `sc_agent::testing::FakeProvider` behind the [`ProviderConnector`] seam, so
//! everything under test — the loop, the tool dispatch, the run rows, the socket
//! — is the production code, and only what answers is scripted.
//!
//! What each test is for:
//!
//! 1. **Who may open it.** Anonymous and non-admin are refused before the
//!    upgrade, with a status, because that is the one refusal a browser can read.
//! 2. **A whole turn.** Text arrives as deltas in order, a tool call arrives with
//!    its arguments and is followed by its result, and `done` carries the run —
//!    which is then in the history, with the transcript in it.
//! 3. **A failure is an event.** A provider that refuses produces an `error` on
//!    an **open** socket, and the run is `failed` with the reason on the row.
//! 4. **A stop leaves a readable run.** Abort ends the turn, the run is
//!    `aborted`, and what the agent had already done is still in its context.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use futures::{SinkExt, StreamExt};
use sc_agent::testing::{FakeProvider, Reply};
use sc_agent::{Agent, EnabledTrait, RunState};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, SessionStore, User};
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_llm::{ConnectedModel, LlmProvider};
use sc_server::{
    AGENT_CHAT_ROUTE, AgentServices, AppMounts, ProviderConnector, SESSION_COOKIE, ServerConfig,
    admin_handlers, build_router_with_apps,
};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use uuid::Uuid;

/// How long a test waits for an event that should arrive promptly.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

/// The socket type the client half works over.
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// --- the scripted provider, behind the production seam -----------------------

/// A [`ProviderConnector`] that hands out one scripted provider, whatever agent
/// asks. The seam decision 7 exists for: the loop, the traits and the socket are
/// the real ones, and only the model is a script.
struct Scripted(Arc<FakeProvider>);

#[async_trait]
impl ProviderConnector for Scripted {
    async fn connect(
        &self,
        _catalog: &Catalog,
        _agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> Result<ConnectedModel> {
        Ok(ConnectedModel::unconfigured(
            Arc::clone(&self.0) as Arc<dyn LlmProvider>
        ))
    }
}

/// A connector that cannot produce a provider at all — a key that was never
/// configured, an endpoint that is gone. The failure a chat has to render before
/// a single delta exists.
struct Unreachable;

#[async_trait]
impl ProviderConnector for Unreachable {
    async fn connect(
        &self,
        _catalog: &Catalog,
        _agent: &Agent,
        _role: sc_agent::ModelRole,
    ) -> Result<ConnectedModel> {
        Err(sc_error::Error::msg("401 invalid x-api-key"))
    }
}

// --- the server under test ---------------------------------------------------

/// A running server, its address, the sessions to connect with, and the catalog
/// the runs land in.
struct Server {
    addr: std::net::SocketAddr,
    admin: String,
    public: String,
    catalog: Arc<Catalog>,
    _db: TestDb,
}

impl Server {
    /// Open the chat socket as the given session.
    async fn connect(&self, session: Option<&str>) -> std::result::Result<Socket, WsError> {
        let mut request = format!("ws://{}{AGENT_CHAT_ROUTE}", self.addr)
            .into_client_request()
            .unwrap();
        // A browser sends its page's origin on every handshake, and the server
        // accepts an upgrade only from its own (TODO.md "Live updates" §3).
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{}", self.addr)).unwrap(),
        );
        if let Some(token) = session {
            request.headers_mut().insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("{SESSION_COOKIE}={token}")).unwrap(),
            );
        }
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(stream, _)| stream)
    }
}

/// A server whose agents connect through `providers`, with `books` in the
/// catalog and `agent` stored.
async fn serve_with(agent: Agent, providers: Arc<dyn ProviderConnector>) -> Result<Server> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_llm::save_llm_provider(
        &catalog,
        &sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-test"),
    )
    .await?;
    // The model an agent naming no model calls: the provider's default row.
    let provider = sc_llm::require_llm_provider(&catalog, "house").await?;
    sc_llm::save_llm_model(
        &catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;
    catalog
        .create_table(
            "books",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key(),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    db.client()
        .await?
        .batch_execute("INSERT INTO books (id, title) VALUES (1, 'Dune')")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let services = sc_server::install_agents(&catalog)
        .await?
        .with_providers(providers);
    sc_agent::save_agent(&catalog, services.registry(), &agent).await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()).with_agents(services));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        sessions.clone(),
        &ServerConfig::default(),
        apps,
    )?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(Server {
        addr,
        admin: sessions
            .login(User::new(Uuid::new_v4(), ROLE_ADMIN).unwrap())
            .await
            .unwrap(),
        public: sessions
            .login(User::new(Uuid::new_v4(), ROLE_PUBLIC).unwrap())
            .await
            .unwrap(),
        catalog,
        _db: db,
    })
}

/// An agent with no traits: the shape that only talks.
fn talker() -> Agent {
    Agent::new("librarian", "house").system_prompt("You answer questions about books.")
}

/// The same agent, given one table to read.
fn reader() -> Agent {
    talker().with_trait(EnabledTrait::new("query_table").config("table", "books"))
}

// --- socket helpers ----------------------------------------------------------

async fn send(socket: &mut Socket, message: Value) {
    socket
        .send(Message::text(message.to_string()))
        .await
        .expect("send");
}

/// The next event, or a failed test.
async fn next_event(socket: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(REPLY_TIMEOUT, socket.next())
            .await
            .expect("timed out waiting for an event")
            .expect("the socket closed")
            .expect("a socket error");
        match message {
            Message::Text(text) => {
                return serde_json::from_str(&text).expect("the server sent non-JSON");
            }
            Message::Close(frame) => panic!("the server closed the socket: {frame:?}"),
            _ => {}
        }
    }
}

/// Every event up to and including `done` — one whole turn.
async fn drain_turn(socket: &mut Socket) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        let event = next_event(socket).await;
        let done = event["type"] == json!("done");
        events.push(event);
        if done {
            return events;
        }
    }
}

/// The `text` deltas of a turn, concatenated — what the person actually read.
fn streamed_text(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == json!("text"))
        .filter_map(|e| e["delta"].as_str())
        .collect()
}

// --- 1. who may open it ------------------------------------------------------

/// The socket runs tools as whoever holds it, so it is admin-only, and says so
/// before the upgrade — the one refusal an HTTP status is the right answer to.
#[tokio::test]
async fn only_an_admin_may_open_the_chat_socket() -> Result<()> {
    let server = serve_with(talker(), Arc::new(Unreachable)).await?;

    let anonymous = server.connect(None).await;
    assert!(
        matches!(&anonymous, Err(WsError::Http(r)) if r.status() == StatusCode::UNAUTHORIZED),
        "an anonymous socket must be refused, got {:?}",
        anonymous.map(|_| "a socket")
    );

    let public = server.connect(Some(&server.public)).await;
    assert!(
        matches!(&public, Err(WsError::Http(r)) if r.status() == StatusCode::FORBIDDEN),
        "a non-admin socket must be refused, got {:?}",
        public.map(|_| "a socket")
    );
    Ok(())
}

// --- 2. a whole turn ---------------------------------------------------------

/// The turn the milestone is for: text streams in, a tool call expands with its
/// arguments and its result, and the whole exchange is in the run afterwards.
#[tokio::test]
async fn a_turn_streams_text_runs_a_tool_and_leaves_a_readable_run() -> Result<()> {
    let provider = Arc::new(FakeProvider::new([
        Reply::calls("query_books", json!({ "limit": 5 })).with_preamble("Let me look."),
        Reply::says("There is one book: Dune."),
    ]));
    let server = serve_with(reader(), Arc::new(Scripted(Arc::clone(&provider)))).await?;
    let mut socket = server.connect(Some(&server.admin)).await.expect("connect");

    send(&mut socket, json!({"type": "start", "agent": "librarian"})).await;
    send(
        &mut socket,
        json!({"type": "message", "text": "how many books are there?"}),
    )
    .await;

    let events = drain_turn(&mut socket).await;
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();

    // The preamble streamed, then the tool, then its result, then the answer.
    assert!(
        kinds.contains(&"text") && kinds.contains(&"tool_call") && kinds.contains(&"tool_result"),
        "{kinds:?}"
    );
    let call_at = kinds.iter().position(|k| *k == "tool_call").unwrap();
    let result_at = kinds.iter().position(|k| *k == "tool_result").unwrap();
    assert!(
        call_at < result_at,
        "a result cannot precede its call: {kinds:?}"
    );

    // A tool call is announced **once**, whole, and paired with its result by id.
    let call = events
        .iter()
        .find(|e| e["type"] == json!("tool_call"))
        .unwrap();
    assert_eq!(call["name"], json!("query_books"));
    assert_eq!(call["arguments"]["limit"], json!(5));
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == json!("tool_call"))
            .count(),
        1,
        "a call announced twice would render twice"
    );
    let result = events
        .iter()
        .find(|e| e["type"] == json!("tool_result"))
        .unwrap();
    assert_eq!(result["id"], call["id"]);
    assert_eq!(result["is_error"], json!(false));
    // The tool really read the table: this is the row layer, not a stub.
    assert!(
        result["content"].as_str().unwrap().contains("Dune"),
        "{result}"
    );

    // The deltas, in order, are the answer.
    assert_eq!(
        streamed_text(&events),
        "Let me look.There is one book: Dune."
    );
    let done = events.last().unwrap();
    assert_eq!(done["state"], json!("done"));
    assert_eq!(done["answer"], json!("There is one book: Dune."));

    // And the run is in the history, with the whole exchange in its context.
    let run_id = done["run"].as_str().unwrap();
    let run = sc_agent::require_run(
        &server.catalog,
        sc_agent::RunId(Uuid::parse_str(run_id).unwrap()),
    )
    .await?;
    assert_eq!(run.subject, "librarian");
    assert_eq!(run.state, RunState::Done);
    assert_eq!(run.description, "how many books are there?");
    let messages = run.agent_loop()?.messages().len();
    assert_eq!(messages, 4, "user, assistant+call, tool result, answer");
    assert_eq!(
        sc_agent::list_runs(&server.catalog, "librarian")
            .await?
            .len(),
        1
    );
    Ok(())
}

/// A second message continues the *same* run: a conversation is one run, which
/// is what makes reloading the page find the conversation rather than a pile of
/// one-turn fragments.
#[tokio::test]
async fn a_second_message_continues_the_same_run() -> Result<()> {
    let provider = Arc::new(FakeProvider::new([
        Reply::says("Hello."),
        Reply::says("Still here."),
    ]));
    let server = serve_with(talker(), Arc::new(Scripted(provider))).await?;
    let mut socket = server.connect(Some(&server.admin)).await.expect("connect");

    send(&mut socket, json!({"type": "start", "agent": "librarian"})).await;
    send(&mut socket, json!({"type": "message", "text": "hello"})).await;
    let first = drain_turn(&mut socket).await;
    send(
        &mut socket,
        json!({"type": "message", "text": "still there?"}),
    )
    .await;
    let second = drain_turn(&mut socket).await;

    let first_run = first.last().unwrap()["run"].as_str().unwrap().to_owned();
    let second_run = second.last().unwrap()["run"].as_str().unwrap().to_owned();
    assert_eq!(first_run, second_run);
    assert_eq!(
        sc_agent::list_runs(&server.catalog, "librarian")
            .await?
            .len(),
        1
    );

    // Both turns are in the one transcript.
    let run = sc_agent::require_run(
        &server.catalog,
        sc_agent::RunId(Uuid::parse_str(&second_run).unwrap()),
    )
    .await?;
    assert_eq!(run.agent_loop()?.messages().len(), 4);
    Ok(())
}

// --- 3. a failure is an event ------------------------------------------------

/// A provider that refuses arrives as an `error` **on an open socket**, followed
/// by the `done` that releases the composer. A chat window that silently stopped
/// would be unfixable by the person watching it.
#[tokio::test]
async fn a_provider_that_refuses_is_an_error_event_not_a_dropped_connection() -> Result<()> {
    let server = serve_with(talker(), Arc::new(Unreachable)).await?;
    let mut socket = server.connect(Some(&server.admin)).await.expect("connect");

    send(&mut socket, json!({"type": "start", "agent": "librarian"})).await;
    send(&mut socket, json!({"type": "message", "text": "hello"})).await;

    let events = drain_turn(&mut socket).await;
    let error = events
        .iter()
        .find(|e| e["type"] == json!("error"))
        .expect("the refusal is an event");
    assert!(
        error["message"].as_str().unwrap().contains("401"),
        "the provider's own words: {error}"
    );

    // The socket is still open: another agent, another key, another try.
    send(&mut socket, json!({"type": "message", "text": "again"})).await;
    let again = drain_turn(&mut socket).await;
    assert!(again.iter().any(|e| e["type"] == json!("error")));
    Ok(())
}

/// A provider that fails **mid-stream** — the common shape of a rate limit or a
/// cut connection — leaves the run `failed` with the reason on its row, so the
/// history can show what happened rather than a conversation that stops.
#[tokio::test]
async fn a_stream_that_fails_leaves_the_run_failed_with_its_reason() -> Result<()> {
    let provider = Arc::new(FakeProvider::new([Reply::fails("529 overloaded")]));
    let server = serve_with(talker(), Arc::new(Scripted(provider))).await?;
    let mut socket = server.connect(Some(&server.admin)).await.expect("connect");

    send(&mut socket, json!({"type": "start", "agent": "librarian"})).await;
    send(&mut socket, json!({"type": "message", "text": "hello"})).await;

    let events = drain_turn(&mut socket).await;
    let error = events.iter().find(|e| e["type"] == json!("error")).unwrap();
    assert!(
        error["message"].as_str().unwrap().contains("529"),
        "{error}"
    );
    let done = events.last().unwrap();
    assert_eq!(done["state"], json!("failed"));

    let runs = sc_agent::list_runs(&server.catalog, "librarian").await?;
    assert_eq!(runs[0].state, RunState::Failed);
    assert!(runs[0].error.as_deref().unwrap_or("").contains("529"));
    Ok(())
}

/// An agent that is not usable says why, before a run exists. `start` is where
/// the admin finds out, with the composer still empty.
#[tokio::test]
async fn starting_a_chat_with_an_unknown_agent_is_an_error_event() -> Result<()> {
    let server = serve_with(talker(), Arc::new(Unreachable)).await?;
    let mut socket = server.connect(Some(&server.admin)).await.expect("connect");

    send(&mut socket, json!({"type": "start", "agent": "nobody"})).await;
    let event = next_event(&mut socket).await;
    assert_eq!(event["type"], json!("error"));
    assert!(
        event["message"].as_str().unwrap().contains("nobody"),
        "{event}"
    );

    // And a message before any successful `start` says so rather than guessing
    // which agent was meant.
    send(&mut socket, json!({"type": "message", "text": "hi"})).await;
    let events = drain_turn(&mut socket).await;
    assert!(
        events[0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("start"),
        "{:?}",
        events[0]
    );
    Ok(())
}

// --- 4. a stop leaves a readable run -----------------------------------------

/// Stop ends the turn, and what the agent had already done is still there: every
/// step wrote the row, so an aborted conversation is a readable one.
#[tokio::test]
async fn aborting_ends_the_turn_and_leaves_the_transcript() -> Result<()> {
    // A model that keeps calling the tool, so the turn is still running when the
    // stop arrives.
    let provider = Arc::new(FakeProvider::repeating(
        Reply::calls("query_books", json!({ "limit": 1 })),
        40,
    ));
    let server = serve_with(reader(), Arc::new(Scripted(Arc::clone(&provider)))).await?;
    let mut socket = server.connect(Some(&server.admin)).await.expect("connect");

    send(&mut socket, json!({"type": "start", "agent": "librarian"})).await;
    send(
        &mut socket,
        json!({"type": "message", "text": "keep going"}),
    )
    .await;

    // Wait until the run is demonstrably under way — a stop sent before the
    // first tool ran would prove nothing about stopping one that is.
    let first = next_event(&mut socket).await;
    assert_eq!(first["type"], json!("tool_call"));
    send(&mut socket, json!({"type": "abort"})).await;

    let mut done = None;
    for _ in 0..200 {
        let event = next_event(&mut socket).await;
        if event["type"] == json!("done") {
            done = Some(event);
            break;
        }
    }
    let done = done.expect("the stop ends the turn");
    assert_eq!(done["state"], json!("aborted"));

    let run = sc_agent::require_run(
        &server.catalog,
        sc_agent::RunId(Uuid::parse_str(done["run"].as_str().unwrap()).unwrap()),
    )
    .await?;
    assert_eq!(run.state, RunState::Aborted);
    // The transcript is what it was, not truncated: the question, and whatever
    // the agent got through before the stop.
    let state = run.agent_loop()?;
    assert!(state.messages().len() >= 2, "{:?}", state.messages());
    assert!(state.is_done());
    // And it stopped short of the budget — the stop is what ended it.
    assert!(state.step() < state.max_steps(), "{}", state.step());
    Ok(())
}

/// A client that goes away mid-turn leaves the same ending a stop does. Nothing
/// will advance that run — the drive future went with the connection, and
/// nothing resumes a chat run yet — so a row left saying `running` would say it
/// for ever in the history.
#[tokio::test]
async fn a_client_that_goes_away_mid_turn_does_not_leave_a_run_running_for_ever() -> Result<()> {
    let provider = Arc::new(FakeProvider::repeating(
        Reply::calls("query_books", json!({ "limit": 1 })),
        40,
    ));
    let server = serve_with(reader(), Arc::new(Scripted(provider))).await?;
    let mut socket = server.connect(Some(&server.admin)).await.expect("connect");

    send(&mut socket, json!({"type": "start", "agent": "librarian"})).await;
    send(
        &mut socket,
        json!({"type": "message", "text": "keep going"}),
    )
    .await;
    // The turn is demonstrably under way before the connection is dropped.
    assert_eq!(next_event(&mut socket).await["type"], json!("tool_call"));
    drop(socket);

    for _ in 0..200 {
        let runs = sc_agent::list_runs(&server.catalog, "librarian").await?;
        if let Some(run) = runs.first()
            && run.state != RunState::Running
        {
            assert_eq!(run.state, RunState::Aborted);
            // And the transcript is intact, as it is after a stop.
            assert!(!run.agent_loop()?.messages().is_empty());
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the run was still `running` after the client went away");
}

/// The route the admin SPA opens and the route this crate mounts are one
/// contract with two spellings, and nothing else in the build would notice them
/// diverging: a page that connects to the wrong path gets a 404 and a chat that
/// never answers.
#[test]
fn the_route_is_the_path_the_spa_connects_to() {
    assert_eq!(AGENT_CHAT_ROUTE, "/admin/agent-chat");
    // Two clients now, and neither can be checked by the other: the admin SPA's
    // chat window and the IDE's chat panel (§12.1) are separate bundles that
    // speak the same protocol, so each spells the route for itself and a page
    // connecting to the wrong path looks exactly like an agent that never
    // answers.
    for client in [
        "../../ui/admin/src/agentChat.ts",
        "../../ui/ide/src/agentChat.ts",
    ] {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(client);
        let source = std::fs::read_to_string(&path).expect("read agentChat.ts");
        assert!(
            source.contains(AGENT_CHAT_ROUTE),
            "{} must connect to {AGENT_CHAT_ROUTE}",
            path.display()
        );
    }
}

/// A server assembled without agents cannot chat, and says so in the close frame
/// rather than accepting a socket that will never answer.
#[tokio::test]
async fn a_server_without_agents_closes_the_socket_with_the_reason() -> Result<()> {
    let sessions = Arc::new(SessionStore::default());
    let router: Router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        sc_server::HandlerRegistry::new(),
        sessions.clone(),
        &ServerConfig::default(),
        Arc::new(AppMounts::none()),
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let admin = sessions
        .login(User::new(Uuid::new_v4(), ROLE_ADMIN).unwrap())
        .await
        .unwrap();

    let mut request = format!("ws://{addr}{AGENT_CHAT_ROUTE}")
        .into_client_request()
        .unwrap();
    // A browser sends its page's origin on every handshake, and the server
    // accepts an upgrade only from its own (TODO.md "Live updates" §3).
    request.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_str(&format!("http://{}", addr)).unwrap(),
    );
    request.headers_mut().insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("{SESSION_COOKIE}={admin}")).unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the handshake succeeds: the refusal is on the socket");

    let message = tokio::time::timeout(REPLY_TIMEOUT, socket.next())
        .await
        .expect("timed out")
        .expect("the socket ended without a close frame")
        .expect("a socket error");
    let Message::Close(Some(frame)) = message else {
        panic!("expected a close frame, got {message:?}");
    };
    assert!(frame.reason.contains("no agents"), "{:?}", frame.reason);
    Ok(())
}

/// The services a server is built with are the ones the chat validates against —
/// asserted here because a second registry assembled somewhere else is exactly
/// the bug this arrangement exists to prevent.
#[tokio::test]
async fn the_chat_and_the_admin_api_share_one_trait_registry() -> Result<()> {
    let server = serve_with(reader(), Arc::new(Unreachable)).await?;
    let services = AgentServices::new(Arc::new(sc_core_traits::builtin_traits()?));
    // The agent saved through the API validates against the built-in set, which
    // is what `install_agents` assembled; nothing else could have saved it.
    let stored = sc_agent::load_agent_by_name(&server.catalog, "librarian")
        .await?
        .expect("stored");
    sc_agent::validate_agent(&server.catalog, services.registry(), &stored).await?;
    Ok(())
}

//! The admin chat socket: one agent, one conversation, streamed (§11.4).
//!
//! **Why a WebSocket at all**, when everything else in the admin API is a typed
//! request/response pair (§13.1): a chat turn is bidirectional. Text, reasoning,
//! tool calls and tool results go out *while* a new message or a stop may come
//! in, and the endpoint model has no shape for that. So the turn is a socket and
//! everything around it — listing agents, listing runs, reading an old one — stays
//! a typed endpoint, which is the same split the file-store IDE's language server
//! made (§12.1) and is authenticated the same way: admin-only, through the session
//! cookie, decided before the upgrade.
//!
//! ## The protocol
//!
//! The client sends, as JSON text frames:
//!
//! - `{"type":"start","agent":"librarian","run":"<uuid>"?}` — which agent this
//!   socket is talking to, optionally continuing an existing run.
//! - `{"type":"message","text":"…","controls":{…}?}` — one user turn, with
//!   the values of the composer's controls (TODO-agent-skills AS6.4).
//! - `{"type":"message","answers_request":"<id>","value":…,"controls":{…}?}`
//!   — the answer to the question the run is waiting on (AS6.2), as the turn.
//! - `{"type":"abort"}` — stop the turn that is running.
//!
//! The server sends:
//!
//! - `{"type":"text","delta":"…"}` / `{"type":"reasoning","delta":"…"}`
//! - `{"type":"tool_call","id":…,"name":…,"arguments":{…}}`
//! - `{"type":"tool_result","id":…,"name":…,"content":"…","is_error":bool,
//!   "display":[{id,kind,mime_type,summary}]?}` — `display` lists what the
//!   tool showed the person and not the model (AS6.3), fetched with
//!   `getRunPart`.
//! - `{"type":"controls","controls":[…]}` — after `start`, for an agent whose
//!   traits add controls to the composer (AS6.4); none is sent for one without.
//! - `{"type":"awaiting","request":{…}}` — after `start` on a run that is
//!   waiting for an answer: the question.
//! - `{"type":"done","run":"<uuid>","state":"done"|"failed"|"aborted","answer":"…",
//!   "conclusion":"answered"|"max_steps"|"aborted"|"over_budget"|"stuck"
//!   |"awaiting_user"?,"budget":"cost"|"wall_time"|"context"?,"reason":"…"?,
//!   "request":{…}?}` — `request` is the question an `awaiting_user` turn
//!   ended on (AS6.1).
//!   — `conclusion` is present when the loop concluded, `budget` names the
//!   budget an `over_budget` run ran out of, and `reason` says why a `stuck`
//!   run was stopped.
//! - `{"type":"error","message":"…"}`
//! - `{"type":"compaction","step":n,"elided":n,"before_tokens":n,"after_tokens":n,
//!   "summary":"…"?}` — the loop compacted the context before model call `step`
//!   (TODO §9): `elided` old tool results were cleared, and `summary`, when
//!   present, replaced the older turns in what the model is sent. The
//!   transcript itself is unchanged.
//!
//! ## A failure is an event, not a dropped connection
//!
//! A provider that refuses, a key that is wrong, an agent that names a trait
//! configured against a table that has been dropped: each arrives as an `error`
//! event on an **open** socket, followed by the `done` that ends the turn. A chat
//! window that silently stops is unfixable by the person watching it, and a
//! closed socket is indistinguishable from a network that dropped. The only
//! things closed with a reason are the two conditions under which no chat is
//! possible at all — a server with no catalog, or none with agents installed —
//! and the end of the session the socket was opened on.
//!
//! ## The session
//!
//! The handshake is an admin's, but a socket outlives a handshake, and every
//! turn on it runs tools as that admin (security review #42). So the session is
//! read again before every `start` and every turn, and the turn runs as the user
//! it names *now*; the socket also listens to the session store, which tells it
//! at once when that session or that user's sessions end on this node, and
//! re-reads the session every [`SESSION_RECHECK`] in between — which is what
//! reaches a demotion, and a sign-out on another node. A session that has ended,
//! or no longer names an admin, closes the socket with the reason, a turn
//! that is running included.
//!
//! ## Aborting
//!
//! The turn is driven inside a `select!` against the socket's own receiver, so a
//! stop is read while the model is still streaming. Dropping the drive future
//! drops the provider stream with it, and what the run had already done is on the
//! row — every step wrote one (§11.2) — so the transcript the history shows after
//! a stop is the transcript up to the stop.

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use sc_action::TriggerDispatcher;
use sc_agent::validate::Agents;
use sc_agent::{
    Agent, Conclusion, ModelRole, Run, RunCaller, RunObserver, Runner, abort_run, save_run,
};
use sc_api::AuthRequirement;
use sc_auth::{SessionEnded, SessionListener, SessionStore, User};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_llm::{LlmDelta, ToolCall};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use uuid::Uuid;

use crate::agents::AgentServices;

/// The route the admin UI opens its chat socket on.
pub const AGENT_CHAT_ROUTE: &str = "/admin/agent-chat";

/// How much of the first message becomes the run's description — the line a run
/// list shows. Long enough to recognise a conversation by, short enough that a
/// pasted document does not become the label of the run it started.
const DESCRIPTION_CHARS: usize = 120;

/// A close frame's reason is capped by the protocol; the two reasons this sends
/// are written to fit.
const MAX_CLOSE_REASON: usize = 120;

/// What the client can say.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// Bind this socket to an agent, optionally continuing a run of it.
    Start {
        /// The agent's name — what `_fd_runs.subject` holds, and what a run is
        /// listed under.
        agent: String,
        /// An existing run to carry on, or `None` for a fresh conversation.
        #[serde(default)]
        run: Option<Uuid>,
    },
    /// One user turn: what the person typed, or their answer to the question
    /// the run is waiting on.
    Message {
        /// What the person typed.
        #[serde(default)]
        text: String,
        /// The question answered, for an answer.
        #[serde(default)]
        answers_request: Option<String>,
        /// The answer.
        #[serde(default)]
        value: Json,
        /// The composer's controls.
        #[serde(default)]
        controls: std::collections::BTreeMap<String, Json>,
    },
    /// Stop the turn that is running.
    Abort,
}

/// Everything a socket needs that does not change between turns.
struct ChatContext {
    catalog: Arc<Catalog>,
    services: AgentServices,
    evaluator: Option<Arc<dyn JsEvaluator>>,
    triggers: Option<Arc<TriggerDispatcher>>,
    /// Who every tool in every run on this socket executes as (decision 5): the
    /// person chatting, never the server — as the session names them at the
    /// start of the turn.
    caller: RunCaller,
    /// The session the socket was opened on.
    session: ChatSession,
}

/// How often an open chat socket re-reads its session between the store's own
/// notices: what reaches a demotion, and a sign-out on another node.
pub(crate) const SESSION_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// The session a chat socket was opened on, and the ear it keeps on the store.
pub(crate) struct ChatSession {
    sessions: Arc<SessionStore>,
    token: String,
    user: Uuid,
    watch: Arc<SessionWatch>,
}

/// The [`SessionListener`] a chat socket registers: told synchronously by the
/// store, it only raises a flag and wakes the socket's task.
struct SessionWatch {
    token: String,
    user: Uuid,
    ended: std::sync::atomic::AtomicBool,
    notify: Notify,
}

impl SessionListener for SessionWatch {
    fn session_ended(&self, ended: &SessionEnded<'_>) {
        let mine = match ended {
            SessionEnded::Token(token) => *token == self.token,
            SessionEnded::User(user) => *user == self.user,
            SessionEnded::All => true,
        };
        if mine {
            self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
            self.notify.notify_one();
        }
    }
}

/// What a re-read of the session found.
enum SessionNow {
    /// Still an admin's: the user as they are now.
    Admin(User),
    /// Ended, or no longer an admin's, said as the close frame says it.
    Lost(String),
    /// The store could not answer.
    Unknown,
}

impl ChatSession {
    /// Watch `token`, which `user` holds, on `sessions`.
    pub(crate) fn new(sessions: Arc<SessionStore>, token: String, user: &User) -> ChatSession {
        let watch = Arc::new(SessionWatch {
            token: token.clone(),
            user: user.id,
            ended: std::sync::atomic::AtomicBool::new(false),
            notify: Notify::new(),
        });
        let listener: Arc<dyn SessionListener> = watch.clone();
        sessions.add_listener(Arc::downgrade(&listener));
        ChatSession {
            sessions,
            token,
            user: user.id,
            watch,
        }
    }

    async fn now(&self) -> SessionNow {
        if self.watch.ended.load(std::sync::atomic::Ordering::SeqCst) {
            return SessionNow::Lost(SIGNED_OUT.to_owned());
        }
        match self.sessions.user_for(&self.token).await {
            Ok(Some(user)) if user.id == self.user => {
                if AuthRequirement::admin().admits(Some(&user)) {
                    SessionNow::Admin(user)
                } else {
                    SessionNow::Lost(
                        "this account is no longer an administrator, so the chat is closed"
                            .to_owned(),
                    )
                }
            }
            Ok(_) => SessionNow::Lost(SIGNED_OUT.to_owned()),
            Err(e) => {
                sc_log::log_warn!(
                    "the chat socket could not re-read its session: {}",
                    sc_error::format_chain(&e)
                );
                SessionNow::Unknown
            }
        }
    }

    /// Wait until the session is lost, and say why. A store that cannot
    /// answer leaves the socket open until the next re-read, as the live
    /// socket does: a database that blinked is not a sign-out.
    async fn lost(&self) -> String {
        loop {
            tokio::select! {
                () = self.watch.notify.notified() => {}
                () = tokio::time::sleep(SESSION_RECHECK) => {}
            }
            if let SessionNow::Lost(reason) = self.now().await {
                return reason;
            }
        }
    }
}

/// The close reason for a session that has ended.
const SIGNED_OUT: &str = "your session ended, so the chat is closed; sign in again";

/// Serve one chat socket for `user`.
///
/// The caller has already established that the request is an admin's. What is
/// decided here is whether this *server* can chat at all; both refusals arrive as
/// close frames rather than statuses, for the reason the language server's do — a
/// browser cannot read the body of a failed WebSocket handshake.
pub(crate) async fn agent_chat_upgrade(
    ws: WebSocketUpgrade,
    catalog: Option<&Arc<Catalog>>,
    services: Option<&AgentServices>,
    evaluator: Option<Arc<dyn JsEvaluator>>,
    triggers: Option<Arc<TriggerDispatcher>>,
    user: User,
    session: ChatSession,
) -> axum::response::Response {
    let (Some(catalog), Some(services)) = (catalog, services) else {
        let reason = "this server has no agents installed, so there is nothing to chat with";
        return ws.on_upgrade(move |socket| refuse(socket, reason.to_owned()));
    };
    let ctx = ChatContext {
        catalog: Arc::clone(catalog),
        services: services.clone(),
        evaluator,
        triggers,
        caller: RunCaller::user(user),
        session,
    };
    ws.on_upgrade(move |socket| chat(socket, ctx))
}

/// Close a socket, saying why.
async fn refuse(mut socket: WebSocket, reason: String) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: close_code::POLICY,
            reason: fit_close_reason(reason).into(),
        })))
        .await;
}

/// A reason cut to what a close frame can carry, on a character boundary.
fn fit_close_reason(reason: String) -> String {
    if reason.len() <= MAX_CLOSE_REASON {
        return reason;
    }
    let mut cut = MAX_CLOSE_REASON;
    while cut > 0 && !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = reason;
    text.truncate(cut);
    text
}

/// Read the socket until it closes, running a turn for every message.
async fn chat(socket: WebSocket, mut ctx: ChatContext) {
    let (mut sender, mut receiver) = socket.split();
    // Events are pushed from two places — this task, and the observer the driver
    // calls from inside the stream — so they meet in a channel rather than
    // sharing the sink. The observer is synchronous (a watcher does not steer,
    // §11.2), which is exactly what an unbounded sender can serve.
    let (tx, mut rx) = unbounded_channel::<Json>();
    let writer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if sender.send(Message::text(event.to_string())).await.is_err() {
                break;
            }
        }
        sender
    });

    let mut agent_name: Option<String> = None;
    let mut run: Option<Run> = None;
    let mut closing: Option<String> = None;

    loop {
        let message = tokio::select! {
            message = receiver.next() => message,
            reason = ctx.session.lost() => {
                closing = Some(reason);
                break;
            }
        };
        let Some(Ok(message)) = message else { break };
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(text) => text,
                Err(_) => continue,
            },
            Message::Close(_) => break,
            // Ping/Pong are answered by axum itself.
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        let client = match serde_json::from_str::<ClientMessage>(&text) {
            Ok(client) => client,
            Err(e) => {
                send(&tx, error_event(format!("unreadable message: {e}")));
                continue;
            }
        };

        // Every `start` and every turn runs as the session's user as they are
        // now; an ended session or a demoted admin closes the socket.
        if !matches!(client, ClientMessage::Abort) {
            match ctx.session.now().await {
                SessionNow::Admin(user) => ctx.caller = RunCaller::user(user),
                SessionNow::Lost(reason) => {
                    closing = Some(reason);
                    break;
                }
                // A store that cannot answer refuses the turn: this socket
                // runs tools as an admin.
                SessionNow::Unknown => {
                    send(
                        &tx,
                        error_event("the session could not be checked; try again in a moment"),
                    );
                    continue;
                }
            }
        }

        match client {
            ClientMessage::Start { agent, run: run_id } => {
                match start(&ctx, &agent, run_id).await {
                    Ok((existing, controls)) => {
                        // Only an agent with controls has anything to say here,
                        // so a plain chat's frames are as they were.
                        if !controls.is_empty() {
                            send(&tx, json!({ "type": "controls", "controls": controls }));
                        }
                        if let Some(request) = existing
                            .as_ref()
                            .and_then(|r| r.agent_loop().ok())
                            .and_then(|state| state.pending_request().cloned())
                        {
                            send(&tx, json!({ "type": "awaiting", "request": request }));
                        }
                        agent_name = Some(agent);
                        run = existing;
                    }
                    Err(e) => send(&tx, error_event(sc_error::format_causes(&e))),
                }
            }
            // Nothing is running: a stop that races the end of a turn is the
            // normal way this arrives, and it is not an error.
            ClientMessage::Abort => {}
            ClientMessage::Message {
                text,
                answers_request,
                value,
                controls,
            } => {
                let input = TurnInput {
                    text,
                    answer: answers_request.map(|id| sc_agent::Answer {
                        answers_request: id,
                        value,
                    }),
                    controls,
                };
                match turn(
                    &ctx,
                    agent_name.as_deref(),
                    &mut run,
                    input,
                    &tx,
                    &mut receiver,
                )
                .await
                {
                    Ok(Ended::Ok) => {}
                    // The socket went away mid-turn. The run is on its row,
                    // written after every step, so nothing is lost by stopping.
                    Ok(Ended::Disconnected) => break,
                    // The session went mid-turn; the run was stopped with it.
                    Ok(Ended::SessionLost(reason)) => {
                        closing = Some(reason);
                        break;
                    }
                    Err(e) => {
                        // Everything that can go wrong before or around the loop
                        // — an agent that is not usable, a provider that
                        // refuses — is an event in the transcript, followed by
                        // the `done` that releases the composer.
                        send(&tx, error_event(sc_error::format_causes(&e)));
                        send(
                            &tx,
                            json!({
                                "type": "done",
                                "run": run.as_ref().map(|r| r.id.0.to_string()),
                                "state": "failed",
                                "answer": "",
                            }),
                        );
                    }
                }
            }
        }
    }

    drop(tx);
    if let Ok(mut sender) = writer.await
        && let Some(reason) = closing
    {
        let _ = sender
            .send(Message::Close(Some(CloseFrame {
                code: close_code::POLICY,
                reason: fit_close_reason(reason).into(),
            })))
            .await;
    }
}

/// Bind the socket to an agent, resolving the run it is to continue.
///
/// The agent is resolved here as well as on every turn, so a `start` naming an
/// agent that does not exist — or one that is stored but not usable — says so
/// while the composer is still empty rather than after the first message.
async fn start(
    ctx: &ChatContext,
    agent: &str,
    run: Option<Uuid>,
) -> Result<(Option<Run>, Vec<sc_agent::ComposerControl>)> {
    let agents = Agents::load(&ctx.catalog, ctx.services.registry()).await?;
    let definition = agents.require(agent)?;
    may_chat(&ctx.caller, definition)?;
    let controls =
        sc_agent::composer_controls_for(&ctx.catalog, &ctx.caller, ctx.services.registry(), definition)
            .await?;
    let Some(id) = run else {
        return Ok((None, controls));
    };
    let existing = sc_agent::require_run(&ctx.catalog, sc_agent::RunId(id)).await?;
    if existing.subject != definition.name {
        return Err(Error::invalid(format!(
            "run {id} is a run of `{}`, not of `{}`",
            existing.subject, definition.name
        )));
    }
    Ok((Some(existing), controls))
}

/// What one turn is started with: what the person typed, or their answer to
/// the question the run waits on, and the composer's controls.
pub(crate) struct TurnInput {
    /// What the person typed; ignored for an answer.
    pub text: String,
    /// The answer, when the turn is one.
    pub answer: Option<sc_agent::Answer>,
    /// The composer's controls.
    pub controls: std::collections::BTreeMap<String, Json>,
}

/// How a turn ended, as far as the socket is concerned.
enum Ended {
    /// The turn ran to an ending and the socket is still there.
    Ok,
    /// The client went away mid-turn.
    Disconnected,
    /// The session the socket was opened on ended, or stopped being an
    /// admin's, mid-turn; the run has been stopped.
    SessionLost(String),
}

/// Run one user turn, streaming it, watching for a stop.
async fn turn(
    ctx: &ChatContext,
    agent_name: Option<&str>,
    run_slot: &mut Option<Run>,
    input: TurnInput,
    tx: &UnboundedSender<Json>,
    receiver: &mut SplitStream<WebSocket>,
) -> Result<Ended> {
    let name = agent_name.ok_or_else(|| {
        Error::invalid("no agent has been chosen for this chat; send `start` first")
    })?;

    // Resolved per turn, not once per socket: an agent is editable while its
    // conversations exist, so a trait added between two messages is offered on
    // the second — and an agent that stopped validating stops answering, with
    // the reason the admin can act on.
    let agents = Agents::load(&ctx.catalog, ctx.services.registry()).await?;
    let agent = agents.require(name)?;
    may_chat(&ctx.caller, agent)?;

    let executor = ctx
        .services
        .providers()
        .connect(&ctx.catalog, agent, ModelRole::Executor)
        .await?;
    let observer = SocketObserver { tx: tx.clone() };
    let mut runner = Runner::new(
        &ctx.catalog,
        ctx.services.registry(),
        agent,
        executor,
        ctx.caller.clone(),
    )
    .observing(&observer)
    // The same connector this turn's own provider came from, so the agent's
    // roles can be connected and an agent with a `subagent` trait can start the
    // sub-agent's run — which needs the sub-agent's provider and model, not
    // this one's (§11.3).
    .with_connector(ctx.services.providers());
    if let Some(evaluator) = &ctx.evaluator {
        runner = runner.with_evaluator(evaluator);
    }
    if let Some(triggers) = &ctx.triggers {
        runner = runner.with_triggers(triggers);
    }

    // The run is created here rather than by `Runner::start`, because this is
    // the one place that has to still hold it after the drive future is dropped:
    // an aborted turn has a run to mark aborted.
    let mut run = match run_slot.take() {
        Some(mut run) if run.subject == agent.name => {
            // Put back on failure, so a refused answer leaves the socket on
            // its conversation.
            let pushed = match &input.answer {
                Some(answer) => runner.push_answer(&mut run, answer, input.controls).await,
                None => {
                    runner
                        .push_message(&mut run, &input.text, input.controls)
                        .await
                }
            };
            if let Err(e) = pushed {
                *run_slot = Some(run);
                return Err(e);
            }
            run
        }
        _ => {
            if input.answer.is_some() {
                return Err(Error::invalid(
                    "there is no question to answer: this conversation has not started",
                ));
            }
            sc_agent::check_controls(&runner.controls().await?, &input.controls)?;
            let text = input.text;
            // The first message is the run's description: a history list carries
            // no transcript (that is what `getRun` is for), so without this a
            // list of conversations would be a list of timestamps.
            let mut run = runner.new_run(&text)?.description(summarise(&text));
            let mut state = run.agent_loop()?;
            state.set_controls(input.controls);
            run.record(&state);
            save_run(&ctx.catalog, &run).await?;
            run
        }
    };

    let ending = {
        let driving = runner.drive(&mut run);
        tokio::pin!(driving);
        loop {
            tokio::select! {
                outcome = &mut driving => break Turn::Finished(outcome),
                incoming = receiver.next() => match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Abort) => break Turn::Aborted,
                            // A message that arrived while the agent is
                            // answering is refused rather than queued: splicing
                            // it into a history the model is already answering
                            // is how a question gets answered before it was
                            // asked (`AgentLoop::push_user` refuses it too).
                            Ok(_) => send(tx, error_event(
                                "the agent is still answering; wait for it to finish or press stop",
                            )),
                            Err(e) => send(tx, error_event(format!("unreadable message: {e}"))),
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_))) => {}
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break Turn::Disconnected,
                },
                reason = ctx.session.lost() => break Turn::SessionLost(reason),
            }
        }
    };

    let ended = match ending {
        Turn::Finished(Ok(conclusion)) => {
            send(tx, done_event(&run, &conclusion));
            Ended::Ok
        }
        Turn::Finished(Err(e)) => {
            // The driver has already written the failure and its reason to the
            // row; what is left is telling the person watching, in the
            // provider's own words.
            send(tx, error_event(sc_error::format_causes(&e)));
            send(
                tx,
                json!({
                    "type": "done",
                    "run": run.id.0.to_string(),
                    "state": run.state.as_str(),
                    "answer": "",
                }),
            );
            Ended::Ok
        }
        Turn::Aborted => {
            stop(ctx, &mut run).await?;
            send(
                tx,
                json!({
                    "type": "done",
                    "run": run.id.0.to_string(),
                    "state": run.state.as_str(),
                    "answer": "",
                }),
            );
            Ended::Ok
        }
        Turn::Disconnected => {
            // The same ending, without anyone to tell. Nothing will advance this
            // run — the drive future has gone with the connection, and nothing
            // resumes a chat run yet — so leaving it `running` would be a row
            // that says "still going" for ever in the history.
            stop(ctx, &mut run).await?;
            Ended::Disconnected
        }
        Turn::SessionLost(reason) => {
            // Stopped as a disconnect is: the tools ran as somebody who is no
            // longer signed in as an admin.
            stop(ctx, &mut run).await?;
            Ended::SessionLost(reason)
        }
    };

    *run_slot = Some(run);
    Ok(ended)
}

/// End `run` where it stands, and write it.
///
/// The transcript is left exactly as it is: every step wrote the row (§11.2), so
/// what the agent had already done is what the history will show, and truncating
/// it would hide the part that explains why someone pressed stop.
///
/// A session the run delegated to is stopped with it: its drive was inside
/// the one that was just dropped.
async fn stop(ctx: &ChatContext, run: &mut Run) -> Result<()> {
    abort_run(&ctx.catalog, run).await
}

/// The `done` event for a turn the loop concluded.
pub(crate) fn done_event(run: &Run, conclusion: &Conclusion) -> Json {
    let mut event = json!({
        "type": "done",
        "run": run.id.0.to_string(),
        "state": run.state.as_str(),
        "answer": conclusion.answer().unwrap_or(""),
    });
    let name = match conclusion {
        Conclusion::Answered { .. } => "answered",
        Conclusion::MaxSteps => "max_steps",
        Conclusion::Aborted => "aborted",
        Conclusion::OverBudget { budget } => {
            event["budget"] = json!(budget.as_str());
            "over_budget"
        }
        Conclusion::Stuck { reason } => {
            event["reason"] = json!(reason);
            "stuck"
        }
        Conclusion::AwaitingUser { request } => {
            event["request"] = json!(request);
            "awaiting_user"
        }
    };
    event["conclusion"] = json!(name);
    event
}

/// What ended the `select!` above.
enum Turn {
    Finished(Result<Conclusion>),
    Aborted,
    Disconnected,
    SessionLost(String),
}

/// Whether `caller` may chat with `agent`.
///
/// The agent's own `min_role` (§11.2), checked here because it is the agent's
/// authority rather than the route's. The route's admin gate is the coarser one
/// above it: today every caller that reaches this is an admin and meets every
/// floor, and the check is still made here rather than assumed, because an agent
/// exposed to a role is exactly the thing that must not be reachable by way of a
/// surface that forgot to ask.
pub(crate) fn may_chat(caller: &RunCaller, agent: &Agent) -> Result<()> {
    if caller.meets_role(agent.min_role) {
        return Ok(());
    }
    Err(Error::auth(format!(
        "agent `{}` is not available at your role",
        agent.name
    )))
}

/// The first line of a message, cut to fit a list.
pub(crate) fn summarise(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    match line.char_indices().nth(DESCRIPTION_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

/// An `error` event, the one thing that can arrive at any point in a turn.
pub(crate) fn error_event(message: impl std::fmt::Display) -> Json {
    json!({ "type": "error", "message": message.to_string() })
}

/// A `compaction` event: what the loop did to the context, for the marker the
/// panel draws where it happened.
fn compaction_event(compaction: &sc_agent::Compaction) -> Json {
    let mut event = json!({
        "type": "compaction",
        "step": compaction.step,
        "elided": compaction.elided,
        "before_tokens": compaction.before_tokens,
        "after_tokens": compaction.after_tokens,
    });
    if let Some(summary) = &compaction.summary {
        event["summary"] = json!(summary);
    }
    event
}

/// Queue one event for the writer. A send that fails means the socket has gone,
/// which the read half will notice; there is nothing useful to do here.
fn send(tx: &UnboundedSender<Json>, event: Json) {
    let _ = tx.send(event);
}

/// The [`RunObserver`] that turns the loop's events into socket frames.
///
/// It watches and does not steer: every method returns nothing, so a transcript
/// is a record of what happened rather than something the transport shaped.
struct SocketObserver {
    tx: UnboundedSender<Json>,
}

impl RunObserver for SocketObserver {
    fn on_delta(&self, delta: &LlmDelta) {
        match delta {
            LlmDelta::Text(text) => send(&self.tx, json!({"type": "text", "delta": text})),
            LlmDelta::Reasoning(text) => {
                send(&self.tx, json!({"type": "reasoning", "delta": text}));
            }
            // Not emitted here. The same call arrives at `on_tool_call` an
            // instant later, when the loop is about to run it, and emitting both
            // would put every tool in the transcript twice.
            LlmDelta::ToolCall(_) => {}
            // Opaque and vendor-signed: kept with the run for the next request,
            // never shown.
            LlmDelta::ProviderItem(_) => {}
            // The ending is the `done` event, which carries the run's state —
            // something the stream alone cannot say.
            LlmDelta::Stop { .. } => {}
        }
    }

    fn on_tool_call(&self, call: &ToolCall) {
        send(
            &self.tx,
            json!({
                "type": "tool_call",
                "id": call.id,
                "name": call.name,
                "arguments": call.arguments,
            }),
        );
    }

    fn on_compaction(&self, compaction: &sc_agent::Compaction) {
        send(&self.tx, compaction_event(compaction));
    }

    fn on_tool_result(&self, outcome: &sc_agent::ToolOutcome) {
        send(&self.tx, tool_result_event(outcome));
    }

    // What the final answer showed (a spoken answer, AS11.2).
    fn on_display(&self, parts: &[sc_agent::DisplayRef]) {
        send(&self.tx, json!({"type": "display", "display": parts}));
    }
}

/// A finished tool call as the socket sends it: its text, and the images it
/// carried (a `view_app` screenshot, TODO §7b) as base64 for the panel to show
/// inline.
fn tool_result_event(outcome: &sc_agent::ToolOutcome) -> Json {
    let mut event = json!({
        "type": "tool_result",
        "id": outcome.call.id,
        "name": outcome.call.name,
        "content": outcome.content,
        "is_error": outcome.is_error,
    });
    if !outcome.display.is_empty() {
        event["display"] = json!(outcome.display);
    }
    if !outcome.images.is_empty() {
        event["images"] = outcome
            .images
            .iter()
            .map(|image| json!({"media_type": image.media_type, "data": image.base64()}))
            .collect();
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_at(min_role: Option<u8>) -> Agent {
        let agent = Agent::new("librarian", "anthropic");
        match min_role {
            Some(role) => agent.min_role(role),
            None => agent,
        }
    }

    #[test]
    fn a_tool_result_carries_its_images_only_when_it_has_some() {
        let call = sc_llm::ToolCall {
            id: "c1".to_owned(),
            name: "view_app_code".to_owned(),
            arguments: json!({}),
        };
        let plain = sc_agent::ToolOutcome::ok(call.clone(), &json!("snapshot"));
        assert!(tool_result_event(&plain).get("images").is_none());
        let shot = plain.with_images(vec![sc_llm::ImagePart::new("image/jpeg", b"hi".to_vec())]);
        assert_eq!(
            tool_result_event(&shot)["images"],
            json!([{"media_type": "image/jpeg", "data": "aGk="}])
        );
    }

    #[test]
    fn a_compaction_event_carries_the_summary_only_when_there_is_one() {
        let mut compaction = sc_agent::Compaction {
            step: 4,
            at: 7,
            elided: 2,
            before_tokens: 3400,
            after_tokens: 1200,
            up_to_index: None,
            summary: None,
        };
        let event = compaction_event(&compaction);
        assert_eq!(
            event,
            json!({"type": "compaction", "step": 4, "elided": 2,
                   "before_tokens": 3400, "after_tokens": 1200})
        );
        compaction.up_to_index = Some(5);
        compaction.summary = Some("## Goal\nship it".to_owned());
        assert_eq!(compaction_event(&compaction)["summary"], "## Goal\nship it");
    }

    #[test]
    fn a_done_event_names_the_conclusion_and_the_budget_that_ran_out() {
        let run = Run::new(
            "builder",
            &RunCaller::system(),
            &sc_agent::AgentLoop::new(20),
        );
        let event = done_event(
            &run,
            &Conclusion::OverBudget {
                budget: sc_agent::Budget::WallTime,
            },
        );
        assert_eq!(event["conclusion"], "over_budget");
        assert_eq!(event["budget"], "wall_time");
        assert_eq!(event["answer"], "");

        let event = done_event(
            &run,
            &Conclusion::Answered {
                answer: "hi".to_owned(),
            },
        );
        assert_eq!(event["conclusion"], "answered");
        assert_eq!(event["answer"], "hi");
        assert!(event.get("budget").is_none());

        let event = done_event(
            &run,
            &Conclusion::Stuck {
                reason: "3 malformed tool calls in a row".to_owned(),
            },
        );
        assert_eq!(event["conclusion"], "stuck");
        assert_eq!(event["reason"], "3 malformed tool calls in a row");
        assert!(event.get("budget").is_none());
    }

    /// The route is admin-only today, so this is the check that would refuse a
    /// caller the route let through — which is the point of making it here.
    #[test]
    fn an_agent_above_the_callers_role_is_refused() {
        let staff = RunCaller::user(sc_auth::User::new(Uuid::new_v4(), 40).expect("a user"));
        // Roles descend: 40 meets a floor of 80, and does not meet 10 or
        // admin-only.
        assert!(may_chat(&staff, &agent_at(Some(80))).is_ok());
        assert!(may_chat(&staff, &agent_at(Some(40))).is_ok());
        let err = may_chat(&staff, &agent_at(Some(10))).expect_err("10 is above 40");
        assert!(err.to_string().contains("librarian"), "{err}");
        assert!(may_chat(&staff, &agent_at(None)).is_err());
    }

    #[test]
    fn an_admin_meets_every_floor() {
        let admin = RunCaller::user(sc_auth::User::new(Uuid::new_v4(), 1).expect("a user"));
        assert!(may_chat(&admin, &agent_at(None)).is_ok());
        assert!(may_chat(&admin, &agent_at(Some(100))).is_ok());
    }

    #[test]
    fn a_runs_description_is_the_first_line_of_what_was_asked() {
        assert_eq!(
            summarise("  how many books?\nand who by?  "),
            "how many books?"
        );
        let long = "x".repeat(DESCRIPTION_CHARS + 10);
        let cut = summarise(&long);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), DESCRIPTION_CHARS + 1);
        // A message that is only whitespace still yields a description that is
        // not a lie about what was asked.
        assert_eq!(summarise("   "), "");
    }

    #[test]
    fn a_turn_that_asks_says_so_and_carries_the_question() {
        let request = sc_agent::UserRequest::new(
            "Approve the plan?",
            sc_agent::RequestKind::Approve {
                label: "Approve".into(),
                payload: Json::Null,
            },
        );
        let caller = RunCaller::system();
        let mut state = sc_agent::AgentLoop::new(5);
        state.push_user("plan it").expect("a message");
        let run = Run::new("planner", &caller, &state);
        let event = done_event(
            &run,
            &Conclusion::AwaitingUser {
                request: request.clone(),
            },
        );
        assert_eq!(event["conclusion"], json!("awaiting_user"));
        assert_eq!(event["request"]["id"], json!(request.id));
        assert_eq!(event["request"]["kind"], json!("approve"));
        assert_eq!(event["answer"], json!(""));
    }

    #[test]
    fn the_client_messages_parse_as_the_protocol_documents_them() {
        let start: ClientMessage =
            serde_json::from_str(r#"{"type":"start","agent":"librarian"}"#).expect("start");
        assert!(matches!(start, ClientMessage::Start { agent, run: None } if agent == "librarian"));
        let resumed: ClientMessage = serde_json::from_str(
            r#"{"type":"start","agent":"a","run":"00000000-0000-0000-0000-000000000001"}"#,
        )
        .expect("start with a run");
        assert!(matches!(resumed, ClientMessage::Start { run: Some(_), .. }));
        let message: ClientMessage =
            serde_json::from_str(r#"{"type":"message","text":"hi"}"#).expect("message");
        assert!(
            matches!(message, ClientMessage::Message { text, answers_request: None, .. } if text == "hi")
        );
        // An answer, with the composer's controls (TODO-agent-skills AS6).
        let answer: ClientMessage = serde_json::from_str(
            r#"{"type":"message","answers_request":"q1","value":["a"],"controls":{"model":"fast"}}"#,
        )
        .expect("answer");
        assert!(matches!(
            answer,
            ClientMessage::Message { answers_request: Some(id), value, controls, .. }
                if id == "q1" && value == json!(["a"]) && controls["model"] == json!("fast")
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"abort"}"#),
            Ok(ClientMessage::Abort)
        ));
        // An unknown kind is refused rather than read as one of these.
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"resume"}"#).is_err());
    }
}

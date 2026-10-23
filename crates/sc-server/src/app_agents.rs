//! An application's exposed agents (§11.7): chat, single prompts and the
//! conversation history, at `{mount}/_agents/{name}/…`.
//!
//! **Why server-sent events and not the admin chat's WebSocket.** A turn is one
//! request with a streamed answer: the person's message goes up once, and
//! everything after it comes down. That is the shape a `POST` answered with
//! `text/event-stream` already has, so a turn goes through the application's
//! ordinary request path — its session, its CSRF check, its admission rules —
//! and is stopped by aborting the `fetch`. The admin socket's two-way channel
//! would buy nothing here but a second way in.
//!
//! **The events are the admin socket's** (`text`, `reasoning`, `tool_call`,
//! `tool_result`, `error`, `done`), plus `run` first, so a new conversation
//! has its id before the agent says anything. How much of a tool the client is
//! told is the exposure's [`ToolDetail`].
//!
//! **A client that goes away stops the agent.** The turn runs in a task that
//! watches the response's channel; when the response is dropped — the page was
//! closed, the `fetch` aborted — the drive is dropped with it and the run is
//! written as `aborted`, exactly as the admin chat does on a closed socket.
//!
//! **Whose a run is.** A run started here records the application, the mode
//! (`chat` or `prompt`) and its owner — `user:<id>`, or for somebody who is
//! not signed in a hash of the browser's cookie, as an upload's uploader is —
//! in its attributes. Only its owner sees it through this route, in this
//! application; anybody else asking for it gets the 404 a run that does not
//! exist gets. The admin chat still lists it under the agent, as a record.

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::CookieJar;
use sc_agent::validate::Agents;
use sc_agent::{
    Agent, ModelRole, Run, RunCaller, RunId, RunObserver, RunState, Runner, abort_run, save_run,
};
use sc_app::{AgentExposure, AgentRequest, AgentRoute, ToolDetail};
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::{Error, ErrorKind, Result};
use sc_llm::{LlmDelta, LlmMessage, ToolCall};
use serde_json::{Value as Json, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::agents::AgentServices;
use crate::apps::MountedApp;
use crate::chat::{done_event, error_event, may_chat, summarise};
use crate::router::{AppState, app_caller, error_status, json_error, log_failure, parse_query};

/// The run attribute naming the application a run was started through.
pub(crate) const ATTR_APP: &str = "app";
/// The run attribute naming whose run it is: `user:<id>` or `anon:<hash>`.
pub(crate) const ATTR_OWNER: &str = "owner";
/// The run attribute saying how it was started: `chat` or `prompt`.
pub(crate) const ATTR_APP_MODE: &str = "app_mode";

/// The longest title a person may give a conversation.
const MAX_TITLE_CHARS: usize = 200;

/// Serve one request to an exposed agent.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle(
    state: &AppState,
    app: &Arc<MountedApp>,
    request: AgentRequest,
    method: &Method,
    uri: &Uri,
    jar: &CookieJar,
    anonymous: bool,
    body: &Bytes,
) -> Response {
    match answer(state, app, request, method, uri, jar, anonymous, body).await {
        Ok(response) => response,
        Err(e) => refusal(&e),
    }
}

/// An error as the JSON response a client reads: the message for a refusal
/// the caller can act on, a generic line (and the log) for a server fault.
fn refusal(e: &Error) -> Response {
    let status = error_status(e);
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        log_failure("an exposed agent's request failed", e);
        return json_error(
            status,
            "the agent could not be reached; the reason is in the server log",
        );
    }
    json_error(status, sc_error::format_causes(e))
}

/// Who is asking, and with what authority.
struct Asker {
    /// What every tool runs as.
    caller: RunCaller,
    /// Whose runs these are, when the caller can be told apart from anybody
    /// else: a signed-in user, or a browser carrying a cookie.
    owner: Option<String>,
    /// The signed-in user's id, which is also the run's `user_id`.
    user: Option<uuid::Uuid>,
}

#[allow(clippy::too_many_arguments)]
async fn answer(
    state: &AppState,
    app: &Arc<MountedApp>,
    request: AgentRequest,
    method: &Method,
    uri: &Uri,
    jar: &CookieJar,
    anonymous: bool,
    body: &Bytes,
) -> Result<Response> {
    let catalog = state
        .apps
        .catalog()
        .cloned()
        .ok_or_else(|| Error::msg("no catalog for application agents"))?;
    let Some(services) = state.apps.agents().cloned() else {
        return Ok(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "this server has no agents installed",
        ));
    };
    let caller = app_caller(state, &app.app, jar, anonymous)
        .await
        .map_err(|_| Error::msg("session lookup failed"))?;
    // An agent is not part of anybody's sign-in (TODO F9).
    if let Some(refused) = caller.refused {
        return Ok(refused.response());
    }
    let exposure = &request.exposure;
    let asker = asker(exposure, caller.user, jar)?;

    // The agent as it is now, and the agent's own floor: the one rule about who
    // may talk to an agent, wherever it is reached from.
    let agents = Agents::load(&catalog, services.registry()).await?;
    let agent = agents
        .require(&exposure.name)
        .map_err(|e| Error::invalid(format!("agent `{}` is not available: {e}", exposure.name)))?
        .clone();
    may_chat(&asker.caller, &agent)?;

    let ctx = AgentCtx {
        catalog,
        services,
        state: state.clone(),
        app: Arc::clone(app),
        exposure: exposure.clone(),
        agent,
        asker,
    };
    match (&request.route, method) {
        (AgentRoute::Chat, &Method::POST) if exposure.chat => chat(ctx, body).await,
        (AgentRoute::Prompt, &Method::POST) if exposure.prompt => {
            let stream = parse_query(uri)
                .iter()
                .any(|(k, v)| k == "stream" && (v == "1" || v == "true"));
            prompt(ctx, body, stream).await
        }
        (AgentRoute::Runs, &Method::GET) if exposure.chat => list(&ctx).await,
        (AgentRoute::Run(id), &Method::GET) if exposure.chat => get(&ctx, id).await,
        (AgentRoute::Run(id), &Method::PATCH) if exposure.chat => rename(&ctx, id, body).await,
        (AgentRoute::Run(id), &Method::DELETE) if exposure.chat => delete(&ctx, id).await,
        // A route this exposure does not open is not there at all.
        (AgentRoute::Chat | AgentRoute::Runs | AgentRoute::Run(_), _) if !exposure.chat => {
            Err(Error::not_found("not found"))
        }
        (AgentRoute::Prompt, _) if !exposure.prompt => Err(Error::not_found("not found")),
        _ => Ok(json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed for this route",
        )),
    }
}

/// Decide who is asking: a signed-in user as themselves, or — only where the
/// application lets this agent be used signed out — an anonymous visitor at
/// the public role.
fn asker(exposure: &AgentExposure, user: Option<User>, jar: &CookieJar) -> Result<Asker> {
    let owner = crate::uploads::uploader_of(user.as_ref(), jar);
    match user {
        Some(user) => Ok(Asker {
            user: Some(user.id),
            caller: RunCaller::user(user),
            owner,
        }),
        None if exposure.anonymous => Ok(Asker {
            caller: RunCaller::anonymous(),
            owner,
            user: None,
        }),
        None => Err(Error::auth(format!(
            "sign in to use agent `{}`",
            exposure.name
        ))),
    }
}

/// Everything one request needs, owned, so a turn can run in its own task.
struct AgentCtx {
    catalog: Arc<Catalog>,
    services: AgentServices,
    state: AppState,
    app: Arc<MountedApp>,
    exposure: AgentExposure,
    agent: Agent,
    asker: Asker,
}

impl AgentCtx {
    /// Whether `run` is this caller's, of this agent, started in this
    /// application in this mode. A run that is not is answered as one that
    /// does not exist.
    fn owns(&self, run: &Run, mode: &str) -> bool {
        let attr = |key: &str| run.attributes.get(key).and_then(Json::as_str);
        run.subject == self.agent.name
            && attr(ATTR_APP) == Some(self.app.app.id.0.to_string().as_str())
            && attr(ATTR_APP_MODE) == Some(mode)
            && self.asker.owner.is_some()
            && attr(ATTR_OWNER) == self.asker.owner.as_deref()
            && run.user == self.asker.user
    }

    /// This caller's chat run `id`, or the 404 any other id gets.
    async fn own_run(&self, id: &str) -> Result<Run> {
        let missing = || Error::not_found("no such conversation");
        let id = uuid::Uuid::parse_str(id).map_err(|_| missing())?;
        let run = sc_agent::load_run(&self.catalog, RunId(id))
            .await?
            .ok_or_else(missing)?;
        if !self.owns(&run, "chat") {
            return Err(missing());
        }
        Ok(run)
    }

    /// Mark a new run as this application's and this caller's.
    fn claim(&self, run: Run, mode: &str) -> Run {
        let mut run = run;
        run.attributes
            .insert(ATTR_APP.to_owned(), json!(self.app.app.id.0.to_string()));
        run.attributes.insert(ATTR_APP_MODE.to_owned(), json!(mode));
        if let Some(owner) = &self.asker.owner {
            run.attributes.insert(ATTR_OWNER.to_owned(), json!(owner));
        }
        run
    }
}

/// The body of a request, as an object.
fn body_object(body: &Bytes) -> Result<serde_json::Map<String, Json>> {
    if body.is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_slice::<Json>(body) {
        Ok(Json::Object(obj)) => Ok(obj),
        Ok(other) => Err(Error::invalid(format!(
            "the body should be a JSON object, got {other}"
        ))),
        Err(e) => Err(Error::invalid(format!("invalid JSON body: {e}"))),
    }
}

/// `POST …/chat`: one turn, `{ message, run? }`, streamed.
async fn chat(ctx: AgentCtx, body: &Bytes) -> Result<Response> {
    let obj = body_object(body)?;
    let text = match obj.get("message") {
        Some(Json::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => return Err(Error::invalid("a chat turn needs a `message`, as text")),
    };
    // Continuing needs somebody to continue as: a conversation nobody can be
    // told apart from anybody else by is one nobody can come back to.
    if ctx.asker.owner.is_none() {
        return Err(Error::auth(
            "a conversation needs a session or the CSRF cookie, so it can be continued \
             by whoever began it",
        ));
    }
    let existing = match obj.get("run") {
        None | Some(Json::Null) => None,
        Some(Json::String(id)) => {
            let run = ctx.own_run(id).await?;
            if run.state == RunState::Running {
                return Ok(json_error(
                    StatusCode::CONFLICT,
                    "the agent is still answering in this conversation; wait for it to \
                     finish or stop it",
                ));
            }
            Some(run)
        }
        Some(other) => {
            return Err(Error::invalid(format!(
                "`run` should be a conversation's id, got {other}"
            )));
        }
    };
    Ok(sse(start_turn(ctx, existing, text, "chat")))
}

/// `POST …/prompt`: `{ prompt }` or `{ input }`, answered as JSON — or as a
/// stream with `?stream=1`.
async fn prompt(ctx: AgentCtx, body: &Bytes, stream: bool) -> Result<Response> {
    let obj = body_object(body)?;
    let prompt = match obj.get("prompt") {
        None | Some(Json::Null) => None,
        Some(Json::String(s)) => Some(s.as_str()),
        Some(other) => {
            return Err(Error::invalid(format!(
                "`prompt` should be text, got {other}"
            )));
        }
    };
    let text = ctx.exposure.prompt_text(prompt, obj.get("input"))?;
    let rx = start_turn(ctx, None, text, "prompt");
    if stream {
        return Ok(sse(rx));
    }
    Ok(collect_answer(rx).await)
}

/// Run one turn in its own task, returning the channel its events arrive on.
/// Dropping the receiver — the response going away — stops the agent.
fn start_turn(
    ctx: AgentCtx,
    existing: Option<Run>,
    text: String,
    mode: &'static str,
) -> tokio::sync::mpsc::UnboundedReceiver<Json> {
    let (tx, rx) = unbounded_channel::<Json>();
    tokio::spawn(async move {
        if let Err(e) = drive_turn(&ctx, existing, text, mode, &tx).await {
            let message = match e.kind() {
                ErrorKind::System => {
                    log_failure("an exposed agent's turn failed", &e);
                    "the agent failed; the reason is in the server log".to_owned()
                }
                ErrorKind::Application => sc_error::format_causes(&e),
            };
            let _ = tx.send(error_event(message));
            let _ = tx.send(json!({
                "type": "done", "run": Json::Null, "state": "failed", "answer": "",
            }));
        }
    });
    rx
}

/// What ended a turn's drive.
enum Ending {
    Finished(Result<sc_agent::Conclusion>),
    Gone,
}

/// Drive one turn to its end, sending its events.
async fn drive_turn(
    ctx: &AgentCtx,
    existing: Option<Run>,
    text: String,
    mode: &'static str,
    tx: &UnboundedSender<Json>,
) -> Result<()> {
    let agent = &ctx.agent;
    let executor = ctx
        .services
        .providers()
        .connect(&ctx.catalog, agent, ModelRole::Executor)
        .await?;
    let observer = AppObserver {
        tx: tx.clone(),
        tools: ctx.exposure.tools,
    };
    let mut runner = Runner::new(
        &ctx.catalog,
        ctx.services.registry(),
        agent,
        executor,
        ctx.asker.caller.clone(),
    )
    .observing(&observer)
    .with_connector(ctx.services.providers());
    let evaluator = ctx.state.apps.evaluator();
    if let Some(evaluator) = &evaluator {
        runner = runner.with_evaluator(evaluator);
    }
    let triggers = ctx.state.apps.triggers().cloned();
    if let Some(triggers) = &triggers {
        runner = runner.with_triggers(triggers);
    }

    let mut run = match existing {
        Some(mut run) => {
            let mut state = run.agent_loop()?;
            state.push_user(&text)?;
            run.record(&state);
            save_run(&ctx.catalog, &run).await?;
            run
        }
        None => {
            let run = runner.new_run(&text)?.description(summarise(&text));
            let run = ctx.claim(run, mode);
            save_run(&ctx.catalog, &run).await?;
            run
        }
    };
    let _ = tx.send(json!({ "type": "run", "run": run.id.0.to_string() }));

    let ending = {
        let driving = runner.drive(&mut run);
        tokio::pin!(driving);
        tokio::select! {
            outcome = &mut driving => Ending::Finished(outcome),
            () = tx.closed() => Ending::Gone,
        }
    };
    match ending {
        Ending::Finished(Ok(conclusion)) => {
            let _ = tx.send(done_event(&run, &conclusion));
        }
        Ending::Finished(Err(e)) => {
            // The driver has written the failure to the row; the person is
            // told what they can act on, and the rest goes to the log.
            let message = match e.kind() {
                ErrorKind::System => {
                    log_failure("an exposed agent's run failed", &e);
                    "the agent failed; the reason is in the server log".to_owned()
                }
                ErrorKind::Application => sc_error::format_causes(&e),
            };
            let _ = tx.send(error_event(message));
            let _ = tx.send(json!({
                "type": "done",
                "run": run.id.0.to_string(),
                "state": run.state.as_str(),
                "answer": "",
            }));
        }
        // Nobody is listening any more: stop where it stands, so the row does
        // not say `running` for ever (§11.4's rule for a closed socket).
        Ending::Gone => abort_run(&ctx.catalog, &mut run).await?,
    }
    Ok(())
}

/// A turn's events as a `text/event-stream` response.
fn sse(rx: tokio::sync::mpsc::UnboundedReceiver<Json>) -> Response {
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| {
            (
                Ok::<_, Infallible>(Event::default().data(event.to_string())),
                rx,
            )
        })
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    // A proxy that buffers the response would deliver the whole answer at
    // once, which is the one thing streaming it was for.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// Wait for a turn's `done`, answering with what it came to.
async fn collect_answer(mut rx: tokio::sync::mpsc::UnboundedReceiver<Json>) -> Response {
    let mut run: Option<String> = None;
    let mut tools: Vec<Json> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    while let Some(event) = rx.recv().await {
        match event["type"].as_str() {
            Some("run") => run = event["run"].as_str().map(str::to_owned),
            Some("tool_result") => tools.push(json!({
                "name": event["name"],
                "is_error": event["is_error"],
            })),
            Some("error") => errors.push(event["message"].as_str().unwrap_or("").to_owned()),
            Some("done") => {
                let run = event["run"].as_str().map(str::to_owned).or(run);
                if event["state"] == "failed" {
                    let mut body = json!({ "error": errors.join("; ") });
                    if let Some(run) = run {
                        body["run"] = json!(run);
                    }
                    return (StatusCode::BAD_GATEWAY, axum::Json(body)).into_response();
                }
                return axum::Json(json!({
                    "run": run,
                    "state": event["state"],
                    "answer": event["answer"],
                    "conclusion": event.get("conclusion").cloned().unwrap_or(Json::Null),
                    "tools": tools,
                }))
                .into_response();
            }
            _ => {}
        }
    }
    json_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the agent stopped without answering",
    )
}

/// `GET …/runs`: the caller's conversations with this agent, newest first.
async fn list(ctx: &AgentCtx) -> Result<Response> {
    if ctx.asker.owner.is_none() {
        return Ok(axum::Json(json!([])).into_response());
    }
    let runs = sc_agent::list_runs_for(&ctx.catalog, &ctx.agent.name, ctx.asker.user).await?;
    let listed: Vec<Json> = runs
        .iter()
        .filter(|run| ctx.owns(run, "chat"))
        .map(run_summary)
        .collect();
    Ok(axum::Json(Json::Array(listed)).into_response())
}

/// `GET …/runs/{id}`: one conversation, with its transcript.
async fn get(ctx: &AgentCtx, id: &str) -> Result<Response> {
    let run = ctx.own_run(id).await?;
    let mut out = run_summary(&run);
    out["messages"] = Json::Array(transcript(&run, ctx.exposure.tools)?);
    Ok(axum::Json(out).into_response())
}

/// `PATCH …/runs/{id}`: `{ title }`.
async fn rename(ctx: &AgentCtx, id: &str, body: &Bytes) -> Result<Response> {
    let mut run = ctx.own_run(id).await?;
    let obj = body_object(body)?;
    let title = match obj.get("title") {
        Some(Json::String(t)) if !t.trim().is_empty() => t.trim().to_owned(),
        _ => {
            return Err(Error::invalid(
                "a conversation's new `title` should be text",
            ));
        }
    };
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(Error::invalid(format!(
            "a title is at most {MAX_TITLE_CHARS} characters"
        )));
    }
    // The title is all that changes, and a turn in progress writes the row
    // after every step: renaming under it would be undone by the next one.
    if run.state == RunState::Running {
        return Ok(json_error(
            StatusCode::CONFLICT,
            "the agent is still answering in this conversation; rename it when it has finished",
        ));
    }
    run.description = title;
    save_run(&ctx.catalog, &run).await?;
    Ok(axum::Json(run_summary(&run)).into_response())
}

/// `DELETE …/runs/{id}`.
async fn delete(ctx: &AgentCtx, id: &str) -> Result<Response> {
    let run = ctx.own_run(id).await?;
    // A turn in progress saves the run after every step, which would put a
    // deleted conversation back.
    if run.state == RunState::Running {
        return Ok(json_error(
            StatusCode::CONFLICT,
            "the agent is still answering in this conversation; stop it first",
        ));
    }
    sc_agent::delete_run(&ctx.catalog, run.id).await?;
    Ok(axum::Json(json!({ "deleted": true })).into_response())
}

/// A conversation as a history list shows it.
fn run_summary(run: &Run) -> Json {
    json!({
        "id": run.id.0.to_string(),
        "title": run.description,
        "state": run.state.as_str(),
        "created_at": run.created_at.to_rfc3339(),
        "updated_at": run.updated_at.to_rfc3339(),
    })
}

/// A run's conversation as the client's `AgentMessage[]`: each thing the
/// person said, and after it one message holding everything the agent did in
/// answer — its words and its tool calls, settled by their results — to the
/// extent `tools` shows them.
///
/// The system prompt, the compaction summaries and the vendors' opaque replay
/// items are the loop's and never leave the server.
pub(crate) fn transcript(run: &Run, tools: ToolDetail) -> Result<Vec<Json>> {
    Ok(transcript_of(run.agent_loop()?.messages(), tools))
}

/// One message being built: its id, whether it is the agent's, its parts.
struct Building {
    id: String,
    assistant: bool,
    parts: Vec<Json>,
}

/// [`transcript`] over the messages themselves.
fn transcript_of(messages: &[LlmMessage], tools: ToolDetail) -> Vec<Json> {
    let mut out: Vec<Building> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            LlmMessage::User { content } => out.push(Building {
                id: format!("m{index}"),
                assistant: false,
                parts: vec![json!({ "type": "text", "text": content })],
            }),
            LlmMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let parts = assistant_parts(&mut out, index);
                if !content.is_empty() {
                    parts.push(json!({ "type": "text", "text": content }));
                }
                if tools != ToolDetail::None {
                    for call in tool_calls {
                        parts.push(tool_part(call, tools));
                    }
                }
            }
            LlmMessage::ToolResult {
                tool_call_id,
                name,
                content,
                ..
            } => {
                if tools == ToolDetail::None {
                    continue;
                }
                let parts = assistant_parts(&mut out, index);
                let settled = parts
                    .iter_mut()
                    .find(|p| p["type"] == "tool" && p["id"] == tool_call_id.as_str());
                // The stored result carries no error flag, so a result is
                // `done`; the live `tool_result` event is what says `error`.
                match settled {
                    Some(part) => {
                        part["state"] = json!("done");
                        if tools == ToolDetail::Full {
                            part["result"] = json!(content);
                        }
                    }
                    None => {
                        let mut part = json!({
                            "type": "tool", "id": tool_call_id, "name": name, "state": "done",
                        });
                        if tools == ToolDetail::Full {
                            part["result"] = json!(content);
                        }
                        parts.push(part);
                    }
                }
            }
        }
    }
    out.into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "role": if m.assistant { "assistant" } else { "user" },
                "parts": m.parts,
            })
        })
        .collect()
}

/// The parts of the agent's message being built: the last message's when it
/// is the agent's, otherwise a new one's.
fn assistant_parts(out: &mut Vec<Building>, index: usize) -> &mut Vec<Json> {
    match out.last() {
        Some(last) if last.assistant => {}
        _ => out.push(Building {
            id: format!("m{index}"),
            assistant: true,
            parts: Vec::new(),
        }),
    }
    let at = out.len() - 1;
    &mut out[at].parts
}

/// A tool call as a part, still running until its result settles it.
fn tool_part(call: &ToolCall, tools: ToolDetail) -> Json {
    let mut part = json!({
        "type": "tool", "id": call.id, "name": call.name, "state": "running",
    });
    if tools == ToolDetail::Full {
        part["arguments"] = call.arguments.clone();
    }
    part
}

/// The [`RunObserver`] that turns the loop's events into an application's
/// events, holding back what the exposure does not show.
struct AppObserver {
    tx: UnboundedSender<Json>,
    tools: ToolDetail,
}

impl RunObserver for AppObserver {
    fn on_delta(&self, delta: &LlmDelta) {
        let event = match delta {
            LlmDelta::Text(text) => json!({ "type": "text", "delta": text }),
            LlmDelta::Reasoning(text) => json!({ "type": "reasoning", "delta": text }),
            // A tool call is sent from `on_tool_call`, once (§11.4); the
            // vendor's replay items and the stop are not the client's.
            LlmDelta::ToolCall(_) | LlmDelta::ProviderItem(_) | LlmDelta::Stop { .. } => return,
        };
        let _ = self.tx.send(event);
    }

    fn on_tool_call(&self, call: &ToolCall) {
        if self.tools == ToolDetail::None {
            return;
        }
        let mut event = json!({ "type": "tool_call", "id": call.id, "name": call.name });
        if self.tools == ToolDetail::Full {
            event["arguments"] = call.arguments.clone();
        }
        let _ = self.tx.send(event);
    }

    fn on_tool_result(&self, outcome: &sc_agent::ToolOutcome) {
        if self.tools == ToolDetail::None {
            return;
        }
        let mut event = json!({
            "type": "tool_result",
            "id": outcome.call.id,
            "name": outcome.call.name,
            "is_error": outcome.is_error,
        });
        if self.tools == ToolDetail::Full {
            event["content"] = json!(outcome.content);
        }
        let _ = self.tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation() -> Vec<LlmMessage> {
        vec![
            LlmMessage::User {
                content: "Where is order 7?".to_owned(),
            },
            LlmMessage::Assistant {
                content: "Let me look.".to_owned(),
                tool_calls: vec![ToolCall {
                    id: "c1".to_owned(),
                    name: "query_orders".to_owned(),
                    arguments: json!({ "id": 7 }),
                }],
                provider_items: Vec::new(),
            },
            LlmMessage::ToolResult {
                tool_call_id: "c1".to_owned(),
                name: "query_orders".to_owned(),
                content: "[{\"id\":7,\"status\":\"shipped\"}]".to_owned(),
                images: Vec::new(),
            },
            LlmMessage::Assistant {
                content: "It has shipped.".to_owned(),
                tool_calls: Vec::new(),
                provider_items: Vec::new(),
            },
        ]
    }

    #[test]
    fn a_transcript_is_one_agent_message_per_question_with_tools_as_shown() {
        let names = transcript_of(&conversation(), ToolDetail::Names);
        assert_eq!(names.len(), 2, "{names:#?}");
        assert_eq!(names[0]["role"], "user");
        assert_eq!(names[1]["role"], "assistant");
        assert_eq!(
            names[1]["parts"],
            json!([
                { "type": "text", "text": "Let me look." },
                { "type": "tool", "id": "c1", "name": "query_orders", "state": "done" },
                { "type": "text", "text": "It has shipped." },
            ])
        );

        let full = transcript_of(&conversation(), ToolDetail::Full);
        assert_eq!(full[1]["parts"][1]["arguments"], json!({ "id": 7 }));
        assert_eq!(
            full[1]["parts"][1]["result"],
            "[{\"id\":7,\"status\":\"shipped\"}]"
        );

        let none = transcript_of(&conversation(), ToolDetail::None);
        assert_eq!(
            none[1]["parts"],
            json!([
                { "type": "text", "text": "Let me look." },
                { "type": "text", "text": "It has shipped." },
            ])
        );
    }

    #[test]
    fn the_observer_holds_back_what_the_exposure_does_not_show() {
        let call = ToolCall {
            id: "c1".to_owned(),
            name: "query_orders".to_owned(),
            arguments: json!({ "id": 7 }),
        };
        let outcome = sc_agent::ToolOutcome::ok(call.clone(), &json!("secret rows"));
        let seen = |tools: ToolDetail| {
            let (tx, mut rx) = unbounded_channel();
            let observer = AppObserver { tx, tools };
            observer.on_tool_call(&call);
            observer.on_tool_result(&outcome);
            observer.on_delta(&LlmDelta::Text("hi".to_owned()));
            drop(observer);
            let mut events = Vec::new();
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
            events
        };
        let none = seen(ToolDetail::None);
        assert_eq!(none, vec![json!({ "type": "text", "delta": "hi" })]);
        let names = seen(ToolDetail::Names);
        assert_eq!(names.len(), 3);
        assert!(names[0].get("arguments").is_none());
        assert!(names[1].get("content").is_none());
        let full = seen(ToolDetail::Full);
        assert_eq!(full[0]["arguments"], json!({ "id": 7 }));
        assert!(full[1]["content"].as_str().unwrap().contains("secret rows"));
    }
}

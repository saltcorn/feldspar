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
//! **Interactive chat** (TODO-agent-skills AS6): a tool's display parts — rows,
//! an image, audio for the person and not the model — arrive as a `display`
//! event whatever the [`ToolDetail`], since they are meant to be seen, and are
//! fetched from `…/runs/{id}/parts/{part}`. A turn that ends on a question is
//! `done` with `conclusion: "awaiting_user"` and the `request`; the answer is
//! the next `chat` body, `{ answers_request, value, run }`. A chat body may
//! carry `controls`, the values of what `…/controls` lists.
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
//!
//! **A turn spends the provider's budget**, so starting one is limited
//! (security review #22): per address and per asker by
//! [`sc_ratelimit::global`], and in how many may run at once — per
//! application and per asker — by [`TurnSlots`]. A visitor who is not signed
//! in also gets a smaller step budget ([`ANONYMOUS_MAX_STEPS`]).

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
use crate::chat::{TurnInput, done_event, error_event, may_chat, summarise};
use crate::router::{AppState, app_caller, error_status, json_error, log_failure, parse_query};

/// The run attribute naming the application a run was started through.
pub(crate) const ATTR_APP: &str = "app";
/// The run attribute naming whose run it is: `user:<id>` or `anon:<hash>`.
pub(crate) const ATTR_OWNER: &str = "owner";
/// The run attribute saying how it was started: `chat` or `prompt`.
pub(crate) const ATTR_APP_MODE: &str = "app_mode";

/// The longest title a person may give a conversation.
const MAX_TITLE_CHARS: usize = 200;

/// How many turns one address may start: a person typing, not a script.
const TURNS_PER_IP: sc_ratelimit::Rate = sc_ratelimit::Rate {
    max: 30,
    window: std::time::Duration::from_secs(10 * 60),
};
/// How many turns one asker — a user, or a browser's cookie — may start.
const TURNS_PER_ASKER: sc_ratelimit::Rate = sc_ratelimit::Rate {
    max: 20,
    window: std::time::Duration::from_secs(10 * 60),
};
/// How many turns may run at once in one application.
const MAX_TURNS_PER_APP: usize = 16;
/// How many turns one asker may have running at once.
const MAX_TURNS_PER_ASKER: usize = 2;
/// The most model steps a run started by a visitor who is not signed in may
/// take, whatever the agent's own `max_steps`.
pub(crate) const ANONYMOUS_MAX_STEPS: u32 = 20;

/// The turns running now, counted by key (`app:<id>`, `asker:<app>:<who>`).
/// In memory and per process, like the rate limits.
#[derive(Default)]
struct TurnSlots {
    running: std::sync::Mutex<std::collections::HashMap<String, usize>>,
}

/// One running turn's place in [`TurnSlots`], given back when it is dropped —
/// when the turn's task ends, however it ends.
pub(crate) struct TurnSlot {
    keys: Vec<String>,
}

impl TurnSlots {
    fn global() -> &'static TurnSlots {
        static SLOTS: std::sync::OnceLock<TurnSlots> = std::sync::OnceLock::new();
        SLOTS.get_or_init(TurnSlots::default)
    }

    /// Take a place under every `(key, cap)`, or none and `false` when any is
    /// full.
    fn take(&'static self, limits: &[(String, usize)]) -> Option<TurnSlot> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if limits
            .iter()
            .any(|(key, cap)| running.get(key).copied().unwrap_or(0) >= *cap)
        {
            return None;
        }
        for (key, _) in limits {
            *running.entry(key.clone()).or_insert(0) += 1;
        }
        Some(TurnSlot {
            keys: limits.iter().map(|(k, _)| k.clone()).collect(),
        })
    }
}

impl Drop for TurnSlot {
    fn drop(&mut self) {
        let slots = TurnSlots::global();
        let mut running = slots.running.lock().unwrap_or_else(|e| e.into_inner());
        for key in &self.keys {
            if let Some(n) = running.get_mut(key) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    running.remove(key);
                }
            }
        }
    }
}

/// A `429` saying when to come back.
fn too_many(message: &str, retry_after: Option<u64>) -> Response {
    let mut response = json_error(StatusCode::TOO_MANY_REQUESTS, message);
    if let Some(secs) = retry_after
        && let Ok(value) = HeaderValue::from_str(&secs.to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// Whether this caller may start a turn now: within the rate limits for its
/// address and for itself, and with a place among the turns running. The
/// place is held until the returned slot is dropped.
fn admit_turn(ctx: &AgentCtx) -> std::result::Result<TurnSlot, Box<Response>> {
    let app = ctx.app.app.id.0.to_string();
    let limiter = sc_ratelimit::global();
    if let Some(ip) = ctx.client_ip
        && let Err(limited) = limiter.hit(&format!("agent-turn:{app}:ip:{ip}"), TURNS_PER_IP)
    {
        return Err(Box::new(too_many(
            "too many messages from this address; wait a little and try again",
            Some(limited.retry_after_secs()),
        )));
    }
    // Who is asking, for the per-asker counts: the run owner, else (a prompt
    // with no cookie) the address.
    let who = ctx
        .asker
        .owner
        .clone()
        .or_else(|| ctx.client_ip.map(|ip| format!("ip:{ip}")));
    if let Some(who) = &who
        && let Err(limited) = limiter.hit(&format!("agent-turn:{app}:{who}"), TURNS_PER_ASKER)
    {
        return Err(Box::new(too_many(
            "too many messages; wait a little and try again",
            Some(limited.retry_after_secs()),
        )));
    }
    let mut limits = vec![(format!("app:{app}"), MAX_TURNS_PER_APP)];
    if let Some(who) = who {
        limits.push((format!("asker:{app}:{who}"), MAX_TURNS_PER_ASKER));
    }
    TurnSlots::global().take(&limits).ok_or_else(|| {
        Box::new(too_many(
            "the agent is answering too many messages at once; try again in a moment",
            Some(5),
        ))
    })
}

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
    client_ip: Option<std::net::IpAddr>,
    body: &Bytes,
) -> Response {
    match answer(
        state, app, request, method, uri, jar, anonymous, client_ip, body,
    )
    .await
    {
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
    client_ip: Option<std::net::IpAddr>,
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
    let asker = asker(exposure, caller.user, jar, state.secure_cookies)?;

    // The agent as it is now, and the agent's own floor: the one rule about who
    // may talk to an agent, wherever it is reached from.
    let agents = Agents::load(&catalog, services.registry()).await?;
    let mut agent = agents
        .require(&exposure.name)
        .map_err(|e| Error::invalid(format!("agent `{}` is not available: {e}", exposure.name)))?
        .clone();
    may_chat(&asker.caller, &agent)?;
    // A visitor's run goes round the loop fewer times: each step is paid for,
    // and nobody can be asked to pay for a visitor's.
    if asker.caller.is_anonymous() {
        let steps = agent.max_steps().min(ANONYMOUS_MAX_STEPS);
        agent
            .attributes
            .insert(sc_agent::ATTR_MAX_STEPS.to_owned(), json!(steps));
    }

    let ctx = AgentCtx {
        catalog,
        services,
        state: state.clone(),
        app: Arc::clone(app),
        exposure: exposure.clone(),
        agent,
        asker,
        client_ip,
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
        (AgentRoute::Part(id, part), &Method::GET) if exposure.chat => {
            part_of(&ctx, id, part).await
        }
        (AgentRoute::Controls, &Method::GET) if exposure.chat || exposure.prompt => {
            let controls = sc_agent::composer_controls_for(
                &ctx.catalog,
                &ctx.asker.caller,
                ctx.services.registry(),
                &ctx.agent,
            )
            .await?;
            Ok(axum::Json(json!(controls)).into_response())
        }
        // A route this exposure does not open is not there at all.
        (AgentRoute::Chat | AgentRoute::Runs | AgentRoute::Run(_) | AgentRoute::Part(..), _)
            if !exposure.chat =>
        {
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
fn asker(
    exposure: &AgentExposure,
    user: Option<User>,
    jar: &CookieJar,
    secure: bool,
) -> Result<Asker> {
    let owner = crate::uploads::uploader_of(user.as_ref(), jar, secure);
    match user {
        Some(user) => Ok(Asker {
            user: Some(user.id),
            caller: RunCaller::exposed(user),
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
    /// The address the request came from, for the per-address turn limit.
    client_ip: Option<std::net::IpAddr>,
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

/// `POST …/chat`: one turn, `{ message, run?, controls? }` or
/// `{ answers_request, value, run, controls? }`, streamed.
async fn chat(ctx: AgentCtx, body: &Bytes) -> Result<Response> {
    let obj = body_object(body)?;
    let answer = match obj.get("answers_request") {
        None | Some(Json::Null) => None,
        Some(Json::String(id)) => Some(sc_agent::Answer {
            answers_request: id.clone(),
            value: obj.get("value").cloned().unwrap_or(Json::Null),
        }),
        Some(other) => {
            return Err(Error::invalid(format!(
                "`answers_request` should be a question's id, got {other}"
            )));
        }
    };
    let text = match obj.get("message") {
        Some(Json::String(s)) if !s.trim().is_empty() => s.clone(),
        _ if answer.is_some() => String::new(),
        _ => return Err(Error::invalid("a chat turn needs a `message`, as text")),
    };
    let controls = match obj.get("controls") {
        None | Some(Json::Null) => std::collections::BTreeMap::new(),
        Some(Json::Object(map)) => map.clone().into_iter().collect(),
        Some(other) => {
            return Err(Error::invalid(format!(
                "`controls` should be an object of control values, got {other}"
            )));
        }
    };
    let input = TurnInput {
        text,
        answer,
        controls,
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
    let slot = match admit_turn(&ctx) {
        Ok(slot) => slot,
        Err(refused) => return Ok(*refused),
    };
    Ok(sse(start_turn(ctx, slot, existing, input, "chat")))
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
    let slot = match admit_turn(&ctx) {
        Ok(slot) => slot,
        Err(refused) => return Ok(*refused),
    };
    let input = TurnInput {
        text,
        answer: None,
        controls: std::collections::BTreeMap::new(),
    };
    let rx = start_turn(ctx, slot, None, input, "prompt");
    if stream {
        return Ok(sse(rx));
    }
    Ok(collect_answer(rx).await)
}

/// Run one turn in its own task, returning the channel its events arrive on.
/// Dropping the receiver — the response going away — stops the agent.
fn start_turn(
    ctx: AgentCtx,
    slot: TurnSlot,
    existing: Option<Run>,
    input: TurnInput,
    mode: &'static str,
) -> tokio::sync::mpsc::UnboundedReceiver<Json> {
    let (tx, rx) = unbounded_channel::<Json>();
    tokio::spawn(async move {
        // Held for as long as the turn runs.
        let _slot = slot;
        if let Err(e) = drive_turn(&ctx, existing, input, mode, &tx).await {
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
    input: TurnInput,
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
            match &input.answer {
                Some(answer) => runner.push_answer(&mut run, answer, input.controls).await?,
                None => {
                    runner
                        .push_message(&mut run, &input.text, input.controls)
                        .await?;
                }
            }
            run
        }
        None => {
            if input.answer.is_some() {
                return Err(Error::invalid(
                    "there is no question to answer: name the conversation in `run`",
                ));
            }
            sc_agent::check_controls(&runner.controls().await?, &input.controls)?;
            let text = input.text;
            let run = runner.new_run(&text)?.description(summarise(&text));
            let mut run = ctx.claim(run, mode);
            let mut state = run.agent_loop()?;
            state.set_controls(input.controls);
            run.record(&state);
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

/// `GET …/runs/{id}`: one conversation, with its transcript, and the question
/// it is waiting on, if it is (`awaiting`).
async fn get(ctx: &AgentCtx, id: &str) -> Result<Response> {
    let run = ctx.own_run(id).await?;
    let mut out = run_summary(&run);
    out["messages"] = Json::Array(transcript(&run, ctx.exposure.tools)?);
    out["awaiting"] = json!(run.agent_loop()?.pending_request());
    Ok(axum::Json(out).into_response())
}

/// `GET …/runs/{id}/parts/{part}`: something a tool showed the person in this
/// caller's conversation — rows as JSON, an image or a clip as itself.
async fn part_of(ctx: &AgentCtx, id: &str, part: &str) -> Result<Response> {
    let run = ctx.own_run(id).await?;
    let missing = || Error::not_found("no such part");
    let part = uuid::Uuid::parse_str(part).map_err(|_| missing())?;
    let stored = sc_agent::load_display_part(&ctx.catalog, run.id, part)
        .await?
        .ok_or_else(missing)?;
    let Some(content) = stored.content else {
        return Ok(axum::Json(stored.data).into_response());
    };
    let mut response = (StatusCode::OK, content).into_response();
    if let Ok(value) = HeaderValue::from_str(&stored.reference.mime_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    Ok(response)
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
    let state = run.agent_loop()?;
    let mut out = transcript_of(state.messages(), state.display(), tools);
    // The question the run waits on closes the agent's last message.
    if let Some(request) = state.pending_request()
        && let Some(last) = out.last_mut()
        && last["role"] == "assistant"
        && let Some(parts) = last["parts"].as_array_mut()
    {
        parts.push(json!({ "type": "request", "request": request }));
    }
    Ok(out)
}

/// One message being built: its id, whether it is the agent's, its parts.
struct Building {
    id: String,
    assistant: bool,
    parts: Vec<Json>,
}

/// [`transcript`] over the messages themselves.
fn transcript_of(
    messages: &[LlmMessage],
    display: &std::collections::BTreeMap<String, Vec<sc_agent::DisplayRef>>,
    tools: ToolDetail,
) -> Vec<Json> {
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
                // What the final answer showed: a spoken answer (AS11.2).
                if let Some(shown) = display.get(&sc_agent::answer_display_key(index)) {
                    parts.extend(shown.iter().map(display_part));
                }
            }
            LlmMessage::ToolResult {
                tool_call_id,
                name,
                content,
                ..
            } => {
                // What the tool showed the person is theirs whatever the
                // exposure says of tools.
                if let Some(shown) = display.get(tool_call_id) {
                    let parts = assistant_parts(&mut out, index);
                    parts.extend(shown.iter().map(display_part));
                }
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

/// A display reference as a transcript part.
fn display_part(shown: &sc_agent::DisplayRef) -> Json {
    json!({
        "type": "display",
        "id": shown.id,
        "kind": shown.kind,
        "mime_type": shown.mime_type,
        "summary": shown.summary,
    })
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

    // What the final answer showed (a spoken answer, AS11.2).
    fn on_display(&self, parts: &[sc_agent::DisplayRef]) {
        let _ = self.tx.send(json!({
            "type": "display",
            "parts": parts.iter().map(display_part).collect::<Vec<_>>(),
        }));
    }

    fn on_tool_result(&self, outcome: &sc_agent::ToolOutcome) {
        if !outcome.display.is_empty() {
            let _ = self.tx.send(json!({
                "type": "display",
                "parts": outcome.display.iter().map(display_part).collect::<Vec<_>>(),
            }));
        }
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

    #[test]
    fn turn_slots_cap_what_runs_at_once_and_free_on_drop() {
        let slots = TurnSlots::global();
        let limits = |who: &str| {
            vec![
                ("app:slot-test".to_owned(), 3),
                (format!("asker:slot-test:{who}"), 2),
            ]
        };
        let a1 = slots.take(&limits("a")).expect("first");
        let a2 = slots.take(&limits("a")).expect("second");
        assert!(slots.take(&limits("a")).is_none(), "the asker's cap");
        let b1 = slots.take(&limits("b")).expect("another asker");
        assert!(slots.take(&limits("c")).is_none(), "the application's cap");
        drop(a1);
        let c1 = slots.take(&limits("c")).expect("a place was given back");
        drop((a2, b1, c1));
        assert!(
            !slots
                .running
                .lock()
                .unwrap()
                .keys()
                .any(|k| k.contains("slot-test")),
            "nothing left counted"
        );
    }

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
        let names = transcript_of(
            &conversation(),
            &std::collections::BTreeMap::new(),
            ToolDetail::Names,
        );
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

        let full = transcript_of(
            &conversation(),
            &std::collections::BTreeMap::new(),
            ToolDetail::Full,
        );
        assert_eq!(full[1]["parts"][1]["arguments"], json!({ "id": 7 }));
        assert_eq!(
            full[1]["parts"][1]["result"],
            "[{\"id\":7,\"status\":\"shipped\"}]"
        );

        let none = transcript_of(
            &conversation(),
            &std::collections::BTreeMap::new(),
            ToolDetail::None,
        );
        assert_eq!(
            none[1]["parts"],
            json!([
                { "type": "text", "text": "Let me look." },
                { "type": "text", "text": "It has shipped." },
            ])
        );
    }

    /// A spoken answer (AS11.2) follows the answer it reads, whatever the
    /// exposure says of tools.
    #[test]
    fn what_the_final_answer_showed_follows_it() {
        let audio = sc_agent::DisplayRef {
            id: uuid::Uuid::new_v4(),
            kind: "audio".to_owned(),
            mime_type: "audio/mpeg".to_owned(),
            summary: "answer.mp3 (1 KB)".to_owned(),
        };
        let display = std::collections::BTreeMap::from([(
            sc_agent::answer_display_key(3),
            vec![audio.clone()],
        )]);
        let none = transcript_of(&conversation(), &display, ToolDetail::None);
        assert_eq!(
            none[1]["parts"],
            json!([
                { "type": "text", "text": "Let me look." },
                { "type": "text", "text": "It has shipped." },
                {
                    "type": "display", "id": audio.id, "kind": "audio",
                    "mime_type": "audio/mpeg", "summary": "answer.mp3 (1 KB)",
                },
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
